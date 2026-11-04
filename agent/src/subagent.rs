//! Reusable subagent launcher (`task` tool).
//!
//! Spawns a child agent with a restricted tool set and its own context
//! window, runs it to completion on its own [`crate::harness::Conversation`]
//! (never the parent's — per-run layer state would collide), and returns
//! the final assistant text or, when an `output_schema` is supplied, the
//! validated JSON object.

use std::sync::Arc;

use serde_json::Value;

use crate::config::{AgentConfig, ProviderConfig};
use crate::harness;
use crate::history::{StoredMessage, ext};
use crate::model_registry::ModelTier;
use crate::provider::ModelHandle;
use crate::tools::Workspace;
use crate::tools::worktree::Worktree;

/// Which tools a subagent may use. `General` adds the write family to
/// `Research`'s read-only set; neither includes `task`, `question`, or
/// `sessions` (subagents must not spawn interactive surfaces or recurse).
pub const RESEARCH_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "list",
    "inspect",
    "retrieve",
    "skill",
    "view_image",
    "webfetch",
    "websearch",
    "batch",
    "list_tools",
    // Argosy knowledge tools (read-only).
    "search",
    "search_rules",
    "ask",
    "read_document",
    "read_memory",
    "list_skills",
    "get_skill",
    "outline",
    "zoom",
    "callgraph",
    "repomap",
];

pub const GENERAL_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "list",
    "inspect",
    "retrieve",
    "skill",
    "view_image",
    "webfetch",
    "websearch",
    "batch",
    "list_tools",
    "edit",
    "edit_lines",
    "insert_lines",
    "multiedit",
    "apply_patch",
    "write",
    "delete",
    "move_file",
    "bash",
    "bash_status",
    "bash_watch",
    "bash_kill",
    "todo_write",
];

/// The reviewer subagent's budget: the read-only research set
/// plus the argosy review workflow, minus the network tools.
pub const REVIEWER_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "list",
    "inspect",
    "search",
    "search_rules",
    "read_document",
    "read_memory",
    "start_review",
    "review_diff",
    "report_finding",
    "review_findings",
];

pub const RESEARCH_PROMPT: &str = "You are a research subagent launched by a parent agent. \
You have your own context window and read-only tools. Investigate the task you were given and \
report back: your final message is returned verbatim to the calling agent, so end with a \
complete, self-contained answer with file:line references. Be concise.";
pub const GENERAL_PROMPT: &str = "You are a general subagent launched by a parent agent. \
You have your own context window and may modify files. Keep your changes scoped to the task you \
were given. Your final message is returned verbatim to the calling agent, so end with a \
complete, self-contained summary of what you did, with file:line references. Be concise.";

const MAX_SCHEMA_RETRIES: u32 = 3;

/// Which restricted tool budget and preamble a subagent runs with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentKind {
    Research,
    General,
    Reviewer,
}
const SCHEMA_INSTRUCTION_HEAD: &str = "\n\nYou MUST end your final reply with a single JSON object matching this JSON Schema (no prose, no markdown fences, just the JSON object):\n";
const SCHEMA_INSTRUCTION_TAIL: &str = "\nReturn ONLY that JSON object as your final message.";

/// A request to launch one subagent. Mirrors the subset of the `task` tool
/// inputs that programmatic callers need.
#[derive(Debug, Clone)]
pub struct SubagentRequest {
    /// The spawning tool call's id: the key the task-chats view routes
    /// subagent events by. Empty when the caller has no call identity
    /// (tests, programmatic launches).
    pub tool_use_id: String,
    /// Human label, used for event tags and the worktree slug.
    pub description: String,
    /// The prompt body (schema instructions are appended when
    /// `output_schema` is set).
    pub prompt: String,
    /// `"research"` (read-only) or `"general"` (write).
    pub subagent_type: String,
    /// Optional model tier (`"weak"`/`"medium"`/`"strong"`), capped at the
    /// parent's tier; `None` uses the parent model.
    pub model_tier: Option<String>,
    /// `"none"` (fresh), `"summary"` (last 8 parent messages), or `"full"`.
    pub context_mode: String,
    /// Optional structured-output schema; validated with bounded retry.
    pub output_schema: Option<Value>,
    /// `"none"` or `"worktree"`.
    pub isolation: String,
}

impl SubagentRequest {
    pub fn research(description: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            tool_use_id: String::new(),
            description: description.into(),
            prompt: prompt.into(),
            subagent_type: "research".into(),
            model_tier: None,
            context_mode: "none".into(),
            output_schema: None,
            isolation: "none".into(),
        }
    }
}

/// The result of a subagent run: the final assistant text, or the validated
/// JSON object when an `output_schema` was requested.
#[derive(Debug, PartialEq)]
pub enum SubagentResult {
    Text(String),
    Json(Value),
}

/// Everything the launcher needs to spawn a child. Hosts build one per turn
/// (like the approval gate and question asker) and install it on the
/// workspace via `with_subagents`.
#[derive(Clone)]
pub struct SubagentLauncher {
    /// The parent conversation: model source, history for `context_mode`,
    /// and the settings the child inherits.
    pub parent: crate::harness::Conversation,
    /// Provider name (key into `providers`).
    pub provider: String,
    /// Provider configs from which alternate-tier models are built.
    pub providers: std::collections::BTreeMap<String, ProviderConfig>,
    /// Agent-level run settings for the child.
    pub agent: AgentConfig,
    /// Compression settings shared with the parent's run.
    pub compression: crate::compression::CompressionConfig,
    /// The parent's system-preamble base (user preamble + instructions),
    /// extended with the kind-specific briefing for the child.
    pub base_prompt: String,
    /// The parent workspace (cloned; subagent tool tables are built from it).
    pub workspace: Workspace,
    /// The parent's per-turn cancellation cell: the surface stores each
    /// turn's cancellation into it, and the child links under whatever the
    /// cell holds when it spawns.
    pub cancel: Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    /// Per-tool-call child cancellation: every spawn registers
    /// its child trigger here under the spawning call's id, so a surface
    /// can cancel one subagent without stopping the parent turn.
    pub cancels: Arc<harness::cancel::CancelMap<String>>,
    /// The parent conversation's event sink; filtered child events are
    /// forwarded into it as `CraftEvent::Subagent`.
    pub sink: crate::harness::EventSink,
    /// The parent's approval seam: subagent tool calls flow through
    /// the same permission rules and the same interaction, so a `general`
    /// subagent cannot bypass approvals by running `bash` itself.
    pub policy: crate::harness::PolicyLayerInputs,
    /// The working directory as the child's prompt shows it.
    pub cwd: String,
}

impl crate::tools::SpawnSubagent for SubagentLauncher {
    fn spawn(
        &self,
        req: SubagentRequest,
    ) -> crate::harness::BoxFuture<std::result::Result<SubagentResult, String>> {
        let launcher = self.clone();
        Box::pin(async move { launcher.spawn(&req).await })
    }
}

impl SubagentLauncher {
    /// Resolve the child's model and billing spec: the requested tier
    /// capped at the parent's tier, falling back to the parent model
    /// whenever resolution fails.
    pub async fn resolve_model(
        &self,
        requested: Option<&str>,
    ) -> Result<(ModelHandle, std::sync::Arc<str>), String> {
        let parent_spec = self.parent.model_spec();
        let parent = (self.parent.model(), parent_spec.as_str().into());
        let Some(tier_str) = requested else {
            return Ok(parent);
        };
        let requested = parse_tier(tier_str)?;
        // Compaction-tier models are internal; a subagent may not request one.
        if requested == ModelTier::Compaction {
            return Err("model tier \"compaction\" is not available for subagents".into());
        }
        let parent_tier = crate::model_registry::tier_for(&parent_spec, &self.provider, None);
        let effective = requested.min(parent_tier);
        if effective == parent_tier {
            return Ok(parent);
        }
        let Some(spec) = crate::model_registry::spec_for_tier(&self.provider, effective) else {
            return Ok(parent);
        };
        let Some(provider_config) = self.providers.get(&self.provider) else {
            return Ok(parent);
        };
        let catalog = crate::provider::load_catalog().await;
        let provider =
            crate::provider::ProviderHandle::from_config(provider_config, Some(&catalog))
                .await
                .map_err(|e| e.to_string())?;
        let model = provider
            .model(provider_config, Some(&catalog), &spec)
            .await
            .map_err(|e| e.to_string())?;
        Ok((model, spec.as_str().into()))
    }

    /// Run one subagent to completion (reference `run_subagent`).
    pub async fn spawn(&self, req: &SubagentRequest) -> Result<SubagentResult, String> {
        let kind = match req.subagent_type.as_str() {
            "research" => SubagentKind::Research,
            "general" => SubagentKind::General,
            "reviewer" => SubagentKind::Reviewer,
            other => return Err(format!("unknown subagent type: {other}")),
        };
        let general = kind == SubagentKind::General;
        let (model, child_spec) = self.resolve_model(req.model_tier.as_deref()).await?;

        tracing::info!(
            description = %req.description,
            subagent_type = %req.subagent_type,
            "subagent spawning"
        );

        let seeded: Vec<StoredMessage> = match req.context_mode.as_str() {
            "none" => Vec::new(),
            "summary" => {
                let parent = self.parent.history().snapshot().await;
                ext::stored_of_entries(parent.entries())
                    .into_iter()
                    .rev()
                    .take(8)
                    .rev()
                    .collect()
            }
            "full" => {
                let parent = self.parent.history().snapshot().await;
                ext::stored_of_entries(parent.entries())
            }
            other => return Err(format!("unknown context_mode: {other}")),
        };

        // Worktree isolation roots the child's whole
        // execution state at a fresh linked worktree: its tools, snapshots,
        // bash cwd and claim normalization all anchor at the child root,
        // and creation failure is an error — never a silent fall back to
        // editing the parent tree. Every other child runs on the parent
        // workspace.
        let (worktree, workspace, cwd) = match req.isolation.as_str() {
            "none" => (None, self.workspace.clone(), self.cwd.clone()),
            "worktree" if !general => {
                return Err("worktree isolation requires subagent_type \"general\"".into());
            }
            "worktree" => {
                let worktree = Worktree::create(self.workspace.root(), &req.description)
                    .map_err(|e| e.to_string())?;
                let cwd = worktree.path().display().to_string();
                let workspace = self
                    .workspace
                    .child_workspace(worktree.path())
                    .map_err(|e| e.to_string())?;
                (Some(worktree), workspace, cwd)
            }
            other => return Err(format!("unknown isolation mode: {other}")),
        };

        let schema = match req.output_schema.as_ref() {
            Some(v) => {
                if !v.is_object() {
                    return Err("output_schema must be a JSON Schema object".into());
                }
                Some(v.clone())
            }
            None => None,
        };
        let mut prompt_text = req.prompt.clone();
        if let Some(schema) = &schema {
            prompt_text.push_str(SCHEMA_INSTRUCTION_HEAD);
            prompt_text.push_str(&serde_json::to_string_pretty(schema).map_err(|e| e.to_string())?);
            prompt_text.push_str(SCHEMA_INSTRUCTION_TAIL);
        }

        // The child's config: defaults plus exactly the knobs the parent
        // passes down, so its request pipeline behaves like the parent's.
        // The role rides as a `Subagent role` prompt section (appended
        // below), not inside the preamble string.
        let mut config = crate::config::Config::default();
        config.agent.preamble = self.base_prompt.clone();
        config.agent.temperature = self.agent.temperature;
        config.agent.max_tokens = self.agent.max_tokens;
        config.agent.thinking = self.agent.thinking;
        config.agent.advisor = self.agent.advisor.clone();
        config.agent.prompt_format = self.agent.prompt_format;
        config.compression = self.compression.clone();

        // The child's own history: retrieve reads the child's entry
        // sources, seeded from the parent snapshot below.
        let history = animist::history::History::from_entries(ext::entries(seeded));
        let tools = match kind {
            SubagentKind::General => workspace.subagent_registry(true, &history),
            SubagentKind::Research => workspace.subagent_registry(false, &history),
            SubagentKind::Reviewer => workspace.reviewer_registry(&history),
        }
        .map_err(|e| e.to_string())?;
        // A child conversation with its own layer instances — never the
        // parent's, whose per-run state would collide on run ids. The
        // layers are built from the parent's shared services (permission
        // rules, approval responder, auto-review reviewer); no persistence
        // layer, because the parent stores the child's entries when it
        // ends. The parent's approval gate governs the child's tool calls
        // too.
        let (sink, stream) = crate::harness::EventSink::new();
        let mut builder =
            crate::harness::ConversationBuilder::new(&config, &cwd, "", model, tools, sink)
                .history(history)
                .max_turns(crate::harness::MaxTurns::Unbounded)
                .workspace(workspace)
                .section(crate::prompt::subagent_role(kind));
        builder = builder.policy(self.policy.clone());
        let conversation = builder.build().map_err(|e| e.to_string())?;

        // Filtered child events forward into the parent's sink, tagged with
        // the spawning call: the parent derives its own Done/Error and live
        // tool output; forwarding the child's would duplicate. ToolFinished
        // passes so the task chat can complete its tool cards.
        let forwarder = {
            let parent_sink = self.sink.clone();
            let tool_use_id = req.tool_use_id.clone();
            let description = req.description.clone();
            let mut stream = stream;
            tokio::spawn(async move {
                while let Some(event) = stream.next().await {
                    if matches!(
                        &event,
                        crate::harness::Event::Craft(
                            crate::harness::CraftEvent::Done { .. }
                                | crate::harness::CraftEvent::Error(_)
                                | crate::harness::CraftEvent::PersistenceFailed(_)
                        ) | crate::harness::Event::Agent(
                            animist::agent::AgentEvent::ToolProgress { .. }
                                | animist::agent::AgentEvent::Finished { .. },
                        )
                    ) {
                        continue;
                    }
                    parent_sink.craft(crate::harness::CraftEvent::Subagent {
                        tool_use_id: tool_use_id.clone(),
                        description: description.clone(),
                        event: Box::new(event),
                    });
                }
            })
        };

        let child_cancel = animist::agent::Cancellation::default();
        let parent_cancel = (*self.cancel.load_full()).clone();
        // The cancel-map slot: the surface can cancel one
        // subagent without stopping the parent turn. Retiring the slot at
        // exit drops the trigger, which fires the token — the linked run
        // observes both through the same select.
        let mut cancel_slot = None;
        let mut slot_token = None;
        if !req.tool_use_id.is_empty() {
            let (trigger, token) = crate::harness::cancel::cancel_pair();
            cancel_slot = Some(self.cancels.insert(req.tool_use_id.clone(), trigger));
            slot_token = Some(token);
        }
        // Stop the child at every exit path, whether it is owned by the
        // cancel map (retire drops the trigger) or linked only to the
        // parent.
        macro_rules! stop_child {
            () => {
                if let Some(slot) = cancel_slot {
                    self.cancels.retire(&req.tool_use_id, slot);
                } else {
                    child_cancel.cancel();
                }
            };
        }

        let max_attempts = if schema.is_some() {
            MAX_SCHEMA_RETRIES + 1
        } else {
            1
        };
        let mut validated: Option<Value> = None;
        let mut last_error = String::from("no valid JSON produced");

        for attempt in 0..max_attempts {
            let message = if attempt == 0 {
                prompt_text.clone()
            } else {
                format!(
                    "Your previous response was not valid JSON matching the required schema: \
                     {last_error}\n\nReply again with ONLY a single JSON object matching the \
                     schema. If you previously returned a bare array, wrap it inside the \
                     object's array field."
                )
            };

            let outcome = run_linked(
                &conversation,
                &message,
                &child_cancel,
                &parent_cancel,
                slot_token.as_ref(),
            )
            .await;
            if matches!(
                outcome,
                crate::harness::TurnOutcome::Failed(_) | crate::harness::TurnOutcome::Cancelled
            ) {
                let message = match &outcome {
                    crate::harness::TurnOutcome::Failed(e) => e.clone(),
                    _ => "cancelled".to_string(),
                };
                // A run cut short after streaming some text still has the
                // transcript: half of it beats a bare error.
                let partial = partial_text(&ext::stored_of_entries(
                    conversation.history().snapshot().await.entries(),
                ));
                stop_child!();
                let _ = conversation.shutdown().await;
                self.persist_child(req, kind, &child_spec, &conversation.history())
                    .await;
                // Drop the conversation (the stream's last sender) so the
                // forwarder's drain can end; awaiting it first would hang.
                drop(conversation);
                let _ = forwarder.await;
                if partial.is_empty() {
                    return Err(format!("sub-agent error: {message}"));
                }
                return Err(format!(
                    "sub-agent interrupted ({message}). Partial output:\n{partial}"
                ));
            }
            let reply = match &outcome {
                crate::harness::TurnOutcome::Done { reply }
                | crate::harness::TurnOutcome::MaxTokens { reply } => reply.clone(),
                _ => final_text(&ext::stored_of_entries(
                    conversation.history().snapshot().await.entries(),
                )),
            };

            if let Some(schema) = &schema {
                match crate::json_repair::extract_json(&reply)
                    .and_then(|v| validate_schema(schema, &v).map(|_| v))
                {
                    Ok(v) => {
                        validated = Some(v);
                        break;
                    }
                    Err(e) => {
                        last_error.clone_from(&e);
                        tracing::warn!(
                            description = %req.description,
                            attempt, error = %e,
                            "subagent output schema validation failed"
                        );
                    }
                }
            } else {
                stop_child!();
                let _ = conversation.shutdown().await;
                self.persist_child(req, kind, &child_spec, &conversation.history())
                    .await;
                drop(conversation);
                let _ = forwarder.await;
                return Ok(SubagentResult::Text(reply));
            }
        }
        stop_child!();
        let _ = conversation.shutdown().await;
        self.persist_child(req, kind, &child_spec, &conversation.history())
            .await;
        drop(conversation);
        let _ = forwarder.await;
        drop(worktree);

        match validated {
            Some(v) => Ok(SubagentResult::Json(v)),
            None => Err(format!(
                "subagent did not produce schema-valid JSON: {last_error}"
            )),
        }
    }

    /// Store the finished child's entries in the session's subagent
    /// histories: the child runs no persistence layer, so the parent
    /// records what it produced. Programmatic launches (no call id) and
    /// unpersisted parents (no state dir) store nothing.
    async fn persist_child(
        &self,
        req: &SubagentRequest,
        kind: SubagentKind,
        child_spec: &str,
        history: &animist::history::History,
    ) {
        if req.tool_use_id.is_empty() {
            return;
        }
        let Some(persister) = self.parent.persister() else {
            return;
        };
        let entries = history.snapshot().await.entries().to_vec();
        let record = crate::storage::sessions::StoredSubagent {
            tool_use_id: req.tool_use_id.clone(),
            name: req.description.clone(),
            prompt: Some(req.prompt.clone()),
            model: Some(child_spec.to_owned()),
            lifecycle: Some(
                match kind {
                    SubagentKind::Research => "research",
                    SubagentKind::General => "general",
                    SubagentKind::Reviewer => "reviewer",
                }
                .to_owned(),
            ),
            context_mode: Some(req.context_mode.clone()),
        };
        let mut persister = persister.lock().await;
        if let Err(error) = persister.record_subagent(record, entries).await {
            tracing::warn!(
                tool_use_id = %req.tool_use_id,
                "subagent history persist failed: {error}"
            );
            self.sink
                .craft(crate::harness::CraftEvent::PersistenceFailed(
                    error.to_string(),
                ));
        }
    }
}

fn parse_tier(tier: &str) -> Result<ModelTier, String> {
    match tier {
        "weak" => Ok(ModelTier::Weak),
        "medium" => Ok(ModelTier::Medium),
        "strong" => Ok(ModelTier::Strong),
        "compaction" => Ok(ModelTier::Compaction),
        other => Err(format!(
            "unknown model tier {other:?}; expected weak, medium, or strong"
        )),
    }
}

/// The last assistant text of the run: what the tool returns verbatim.
fn final_text(messages: &[StoredMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.message().role, crate::history::Role::Assistant))
        .map(ext::message_text)
        .unwrap_or_default()
}

/// Every assistant text of the run, unlike [`final_text`]'s last one: a
/// cut-short run's value is everything it managed to say.
fn partial_text(messages: &[StoredMessage]) -> String {
    messages
        .iter()
        .filter(|m| matches!(m.message().role, crate::history::Role::Assistant))
        .map(ext::message_text)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run one child attempt with its cancellation linked to the parent turn's
/// and to the spawn's cancel-map slot: whichever fires first cancels the
/// child, and the run itself is always awaited to completion — never
/// dropped mid-flight, so its partial results and cleanup still happen.
async fn run_linked(
    conversation: &crate::harness::Conversation,
    message: &str,
    child: &animist::agent::Cancellation,
    parent: &animist::agent::Cancellation,
    slot: Option<&crate::harness::cancel::CancelToken>,
) -> crate::harness::TurnOutcome {
    // A link that already fired resolves the race at once, before the run
    // spends a model call.
    if parent.is_cancelled() || slot.is_some_and(|slot| slot.cancelled()) {
        child.cancel();
    }
    let run = crate::harness::run_turn(conversation, message, &[], child.clone());
    tokio::pin!(run);
    let link = async {
        tokio::select! {
            biased;
            () = parent.cancelled() => {}
            () = async {
                match slot {
                    Some(slot) => slot.wait().await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    };
    tokio::select! {
        biased;
        () = link => {
            child.cancel();
            (&mut run).await
        }
        outcome = &mut run => outcome,
    }
}

/// Validate `value` against the structural subset of JSON Schema the
/// `output_schema` parameter accepts: `type` (string or list), `properties`,
/// `required`, `items`, and `enum`. No JSON-Schema validator crate is a
/// dependency, and the schemas models compose are simple objects, so a
/// small recursive check suffices.
fn validate_schema(schema: &Value, value: &Value) -> Result<(), String> {
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };
    if let Some(types) = obj.get("type") {
        check_type(types, value)?;
    }
    if let Some(allowed) = obj.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!(
            "value {value} is not one of the allowed enum values"
        ));
    }
    if let (Some(properties), Value::Object(map)) = (obj.get("properties"), value) {
        for (key, sub) in properties.as_object().into_iter().flatten() {
            match map.get(key) {
                Some(v) => validate_schema(sub, v).map_err(|e| format!("property {key:?}: {e}"))?,
                None if obj
                    .get("required")
                    .and_then(Value::as_array)
                    .is_some_and(|r| r.iter().any(|k| k.as_str() == Some(key))) =>
                {
                    return Err(format!("missing required property {key:?}"));
                }
                None => {}
            }
        }
    }
    if let (Some(required), Value::Object(map)) =
        (obj.get("required").and_then(Value::as_array), value)
    {
        for key in required {
            if let Some(key) = key.as_str()
                && !map.contains_key(key)
            {
                return Err(format!("missing required property {key:?}"));
            }
        }
    }
    if let (Some(items), Value::Array(elements)) = (obj.get("items"), value) {
        for (index, element) in elements.iter().enumerate() {
            validate_schema(items, element).map_err(|e| format!("item {index}: {e}"))?;
        }
    }
    Ok(())
}

fn check_type(types: &Value, value: &Value) -> Result<(), String> {
    let matches = |ty: &str| match (ty, value) {
        ("object", Value::Object(_))
        | ("array", Value::Array(_))
        | ("string", Value::String(_))
        | ("boolean", Value::Bool(_))
        | ("null", Value::Null) => true,
        ("number", Value::Number(_)) => true,
        ("integer", Value::Number(n)) => n.is_i64() || n.is_u64(),
        _ => false,
    };
    let ok = match types {
        Value::String(ty) => matches(ty),
        Value::Array(list) => list.iter().filter_map(Value::as_str).any(matches),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("expected type {}, got {}", types, type_name(value)))
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assistant(text: &str) -> StoredMessage {
        ext::stored(ext::assistant_text(text))
    }

    #[test]
    fn final_text_returns_last_assistant_text() {
        let convo = vec![
            assistant("older"),
            ext::stored_user_text("q"),
            assistant("newer"),
        ];
        assert_eq!(final_text(&convo), "newer");
    }

    #[test]
    fn final_text_is_empty_without_assistant_text() {
        assert_eq!(final_text(&[]), "");
        assert_eq!(final_text(&[ext::stored_user_text("q")]), "");
    }

    #[test]
    fn partial_text_joins_all_assistant_replies() {
        let convo = vec![
            assistant("one"),
            ext::stored_user_text("q"),
            assistant("two"),
        ];
        assert_eq!(partial_text(&convo), "one\ntwo");
    }

    #[test]
    fn validate_schema_checks_types_and_required() {
        let schema = json!({
            "type": "object",
            "required": ["summary"],
            "properties": {
                "summary": {"type": "string"},
                "count": {"type": "integer"},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        });
        assert!(validate_schema(&schema, &json!({"summary": "ok", "count": 2})).is_ok());
        assert!(validate_schema(&schema, &json!({"count": 2})).is_err());
        assert!(validate_schema(&schema, &json!({"summary": 5})).is_err());
        assert!(validate_schema(&schema, &json!({"summary": "s", "count": 1.5})).is_err());
        assert!(validate_schema(&schema, &json!({"summary": "s", "tags": ["a", 1]})).is_err());
    }

    #[test]
    fn validate_schema_allows_enum() {
        let schema = json!({"enum": ["red", "green"]});
        assert!(validate_schema(&schema, &json!("red")).is_ok());
        assert!(validate_schema(&schema, &json!("blue")).is_err());
    }

    #[test]
    fn tool_sets_exclude_interactive_and_recursive_surfaces() {
        for set in [RESEARCH_TOOLS, GENERAL_TOOLS] {
            for banned in ["task", "question", "sessions"] {
                assert!(
                    !set.contains(&banned),
                    "{banned} leaked into subagent tools"
                );
            }
        }
        assert!(!RESEARCH_TOOLS.contains(&"write"));
        assert!(GENERAL_TOOLS.contains(&"write"));
    }

    // ---- integration: nested run against a mock model ----

    use std::sync::Mutex as StdMutex;

    fn mock(turns: Vec<crate::testing::Attempt>) -> crate::provider::ModelHandle {
        mock_with_provider(turns).0
    }

    fn mock_with_provider(
        turns: Vec<crate::testing::Attempt>,
    ) -> (
        crate::provider::ModelHandle,
        crate::testing::ScriptedProvider,
    ) {
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        for turn in turns {
            provider.push(turn);
        }
        let model = crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("mock", 200_000, 8_192, false, false),
        ));
        (model, provider)
    }

    /// One tool-call spec for `crate::testing::tool_turn`.
    fn tool_event(id: &str, name: &str, args: serde_json::Value) -> crate::testing::ToolCallSpec {
        (id.to_owned(), name.to_owned(), args)
    }

    type HarnessEvent = crate::harness::Event;

    struct Captured(StdMutex<Vec<HarnessEvent>>);

    /// A launcher over a real parent conversation whose event stream is
    /// captured: the child forwards into the parent sink, the capture task
    /// drains the parent stream into the shared vec.
    fn launcher(model: crate::provider::ModelHandle, captured: Arc<Captured>) -> SubagentLauncher {
        let tmp = tempfile::TempDir::new().unwrap();
        // The workspace's tool calls read the root, so the directory must
        // outlive the test: `keep` parks cleanup on the OS.
        let root = tmp.keep();
        let workspace = Workspace::new(&root).unwrap();
        let (sink, stream) = crate::harness::EventSink::new();
        let capture = Arc::clone(&captured);
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                capture.0.lock().unwrap().push(event);
            }
        });
        let cwd = root.display().to_string();
        let parent = crate::harness::ConversationBuilder::new(
            &crate::config::Config::parse("").expect("default config"),
            &cwd,
            "",
            model,
            workspace
                .clone()
                .registry(&animist::history::History::new())
                .unwrap(),
            sink.clone(),
        )
        .build()
        .expect("parent conversation builds");
        launcher_over(parent, sink, workspace, cwd)
    }

    /// A launcher over a caller-built parent conversation.
    fn launcher_over(
        parent: crate::harness::Conversation,
        sink: crate::harness::EventSink,
        workspace: Workspace,
        cwd: String,
    ) -> SubagentLauncher {
        let root = workspace.root().to_path_buf();
        SubagentLauncher {
            parent,
            provider: "anthropic".into(),
            providers: Default::default(),
            agent: Default::default(),
            compression: Default::default(),
            base_prompt: String::new(),
            workspace,
            cancel: Arc::new(arc_swap::ArcSwap::from_pointee(
                animist::agent::Cancellation::default(),
            )),
            cancels: Arc::new(harness::cancel::CancelMap::new()),
            sink,
            policy: crate::harness::PolicyLayerInputs::new(
                Arc::new(crate::permissions::PermissionManager::new(
                    crate::permissions::PermissionsConfig::default(),
                    root,
                )),
                Arc::new(crate::headless::HeadlessGate),
            ),
            cwd,
        }
    }

    /// The captured events once the asynchronous hops settle: the child's
    /// forwarder finished inside `spawn`, so this only waits for the
    /// capture task to drain what is already queued. Bounded, so a lost
    /// event fails the test instead of hanging it.
    async fn settled_events(captured: &Captured) -> Vec<HarnessEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let count = captured.0.lock().unwrap().len();
            if count > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                if captured.0.lock().unwrap().len() == count {
                    return captured.0.lock().unwrap().clone();
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no subagent events arrived"
            );
        }
    }

    #[tokio::test]
    async fn approval_policy_governs_child_tool_calls() {
        // A deny rule on the parent's permission manager must hold for the
        // child's own tool calls: the child's conversation runs the same
        // policy layer over the same rules, so a research subagent cannot
        // read through a denial the parent enforced.
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(
            mock(vec![
                crate::testing::tool_turn(vec![tool_event(
                    "t1",
                    "list",
                    serde_json::json!({"path": "."}),
                )]),
                crate::testing::text_turn("done"),
            ]),
            Arc::clone(&captured),
        );
        l.policy
            .permissions
            .add_session_rule(crate::permissions::PermissionRule {
                tool: crate::permissions::ToolKey::native("list"),
                scope: None,
                effect: crate::permissions::Effect::Deny,
            });
        let result = l
            .spawn(&SubagentRequest::research("find", "list files"))
            .await;
        assert!(result.is_ok(), "{result:?}");
        let forwarded = settled_events(&captured).await;
        let denied = forwarded.iter().any(|event| match event {
            HarnessEvent::Craft(crate::harness::CraftEvent::Subagent { event, .. }) => {
                match &**event {
                    HarnessEvent::Agent(animist::agent::AgentEvent::ToolFinished {
                        call,
                        output,
                    }) if call.name == "list" => {
                        crate::history::ext::tool_content_text(&output.content)
                            .starts_with(crate::permissions::PERMISSION_DENIED_PREFIX)
                    }
                    _ => false,
                }
            }
            _ => false,
        });
        assert!(
            denied,
            "the child's call must carry the denial: {forwarded:?}"
        );
    }

    /// Task 96: forwarded events are keyed by the spawning call's id, so
    /// the task-chats view can route them without guessing.
    #[tokio::test]
    async fn forwarded_events_carry_the_call_id() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(
            mock(vec![crate::testing::text_turn("working on it")]),
            captured.clone(),
        );
        let mut req = SubagentRequest::research("find", "search");
        req.tool_use_id = "toolu_01".into();
        let result = l.spawn(&req).await;
        assert!(result.is_ok(), "{result:?}");
        let forwarded = settled_events(&captured).await;
        assert!(!forwarded.is_empty());
        assert!(forwarded.iter().all(
            |e| matches!(e, HarnessEvent::Craft(crate::harness::CraftEvent::Subagent { tool_use_id, .. })
                if tool_use_id == "toolu_01")
        ));
    }

    #[tokio::test]
    async fn research_subagent_returns_final_text() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(
            mock(vec![crate::testing::text_turn("found it at src/lib.rs:12")]),
            captured.clone(),
        );
        let result = l.spawn(&SubagentRequest::research("find", "search")).await;
        assert_eq!(
            result.unwrap(),
            SubagentResult::Text("found it at src/lib.rs:12".into())
        );
        // The child's Done is filtered; forwarded events carry the tag.
        let forwarded = settled_events(&captured).await;
        assert!(forwarded.iter().all(|e| matches!(
            e,
            HarnessEvent::Craft(crate::harness::CraftEvent::Subagent { .. })
        )));
        assert!(!forwarded.iter().any(
            |e| matches!(e,
                HarnessEvent::Craft(crate::harness::CraftEvent::Subagent { event, .. })
                if matches!(&**event, HarnessEvent::Craft(crate::harness::CraftEvent::Done { .. })))
        ));
    }

    #[tokio::test]
    async fn output_schema_retries_until_valid_json() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let model = mock(vec![
            crate::testing::text_turn("sure, here you go"),
            crate::testing::text_turn(
                r#"```json
            {"summary": "ok"}
            ```"#,
            ),
        ]);
        let l = launcher(model, captured);
        let mut req = SubagentRequest::research("summarize", "summarize the diff");
        req.output_schema = Some(json!({
            "type": "object",
            "required": ["summary"],
            "properties": {"summary": {"type": "string"}}
        }));
        let result = l.spawn(&req).await.unwrap();
        assert_eq!(result, SubagentResult::Json(json!({"summary": "ok"})));
    }

    #[tokio::test]
    async fn cancelled_parent_short_circuits_child() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(
            mock(vec![crate::testing::text_turn("never runs")]),
            captured,
        );
        l.cancel.load().cancel();
        let err = l
            .spawn(&SubagentRequest::research("find", "search"))
            .await
            .unwrap_err();
        assert!(err.contains("sub-agent"), "{err}");
    }

    #[tokio::test]
    async fn unknown_enum_inputs_are_rejected() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(mock(vec![]), captured);
        let mut req = SubagentRequest::research("find", "search");
        req.subagent_type = "ninja".into();
        assert!(
            l.spawn(&req)
                .await
                .unwrap_err()
                .contains("unknown subagent type")
        );
        let mut req = SubagentRequest::research("find", "search");
        req.context_mode = "telepathy".into();
        assert!(
            l.spawn(&req)
                .await
                .unwrap_err()
                .contains("unknown context_mode")
        );
        let mut req = SubagentRequest::research("find", "search");
        req.isolation = "container".into();
        assert!(
            l.spawn(&req)
                .await
                .unwrap_err()
                .contains("unknown isolation")
        );
        let mut req = SubagentRequest::research("find", "search");
        req.model_tier = Some("turbo".into());
        assert!(
            l.spawn(&req)
                .await
                .unwrap_err()
                .contains("unknown model tier")
        );
    }

    #[test]
    fn subagent_tables_are_restricted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = Workspace::new(tmp.path()).unwrap();
        let registry_names = |registry: &animist::agent::tool::ToolRegistry| {
            registry
                .definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>()
        };
        let research = registry_names(
            &workspace
                .subagent_registry(false, &animist::history::History::new())
                .unwrap(),
        );
        let general = registry_names(
            &workspace
                .subagent_registry(true, &animist::history::History::new())
                .unwrap(),
        );
        for banned in ["task", "question", "sessions"] {
            assert!(
                !research.contains(&banned.to_string()),
                "{banned} in research"
            );
            assert!(
                !general.contains(&banned.to_string()),
                "{banned} in general"
            );
        }
        assert!(!research.contains(&"write".to_string()));
        assert!(research.contains(&"read".to_string()));
        assert!(general.contains(&"write".to_string()));
        assert!(general.contains(&"bash".to_string()));
    }

    #[tokio::test]
    async fn tier_capping_falls_back_to_parent_without_provider_config() {
        // No provider configs installed, so any tier request must resolve
        // to the parent model rather than erroring.
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(mock(vec![]), captured);
        let (model, spec) = l.resolve_model(Some("weak")).await.unwrap();
        assert!(
            !spec.as_ref().is_empty(),
            "the parent spec resolves even without provider configs"
        );
        let _ = model;
    }

    // ---- 10: linked cancellation, schema exhaustion, filtering ----

    /// The child's model call blocks mid-stream (scripted blocked attempt);
    /// a parent cancellation that lands mid-run must cancel the child
    /// promptly and end the spawn, not park it forever.
    #[tokio::test]
    async fn parent_cancellation_mid_run_cancels_the_child_promptly() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let (model, provider) =
            mock_with_provider(vec![crate::testing::text_turn("half a reply").blocked()]);
        let l = launcher(model, captured.clone());
        let cell = l.cancel.clone();
        let spawned =
            tokio::spawn(
                async move { l.spawn(&SubagentRequest::research("find", "search")).await },
            );
        // Wait until the child is really streaming before cancelling.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while captured.0.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "child never started");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cell.load().cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), spawned)
            .await
            .expect("the cancelled child returns promptly")
            .unwrap();
        let error = result.expect_err("a cancelled child reports an error");
        assert!(error.contains("sub-agent"), "{error}");
        assert!(
            error.contains("cancelled") || error.contains("interrupted"),
            "{error}"
        );
        provider.release();
    }

    /// Every schema retry spent without valid JSON ends the spawn with the
    /// last validation error.
    #[tokio::test]
    async fn output_schema_exhaustion_reports_the_last_error() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let attempts = (0..=MAX_SCHEMA_RETRIES)
            .map(|_| crate::testing::text_turn("not json at all"))
            .collect();
        let l = launcher(mock(attempts), captured);
        let mut req = SubagentRequest::research("summarize", "summarize");
        req.output_schema = Some(json!({
            "type": "object",
            "required": ["summary"],
            "properties": {"summary": {"type": "string"}}
        }));
        let error = l.spawn(&req).await.unwrap_err();
        assert!(
            error.contains("did not produce schema-valid JSON"),
            "{error}"
        );
        assert!(error.contains("no JSON object found"), "{error}");
    }

    /// The forwarded stream keeps tool completion (the task chat needs it)
    /// and drops the child's terminal and progress noise.
    #[tokio::test]
    async fn child_noise_events_are_filtered_and_completion_passes() {
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        // A general child running a bash command longer than the progress
        // cadence streams ToolProgress notes while it works.
        let l = launcher(
            mock(vec![
                crate::testing::tool_turn(vec![tool_event(
                    "b1",
                    "bash",
                    serde_json::json!({"command": "sleep 1; echo done"}),
                )]),
                crate::testing::text_turn("finished"),
            ]),
            captured.clone(),
        );
        l.policy
            .permissions
            .add_session_rule(crate::permissions::PermissionRule {
                tool: crate::permissions::ToolKey::native("bash"),
                scope: None,
                effect: crate::permissions::Effect::Allow,
            });
        let mut req = SubagentRequest::research("run", "run the command");
        req.subagent_type = "general".into();
        let result = l.spawn(&req).await;
        assert!(result.is_ok(), "{result:?}");
        let forwarded = settled_events(&captured).await;
        let mut saw_finished = false;
        for event in &forwarded {
            let HarnessEvent::Craft(crate::harness::CraftEvent::Subagent { event, .. }) = event
            else {
                panic!("every forwarded event must be tagged: {event:?}");
            };
            match &**event {
                HarnessEvent::Craft(
                    crate::harness::CraftEvent::Done { .. }
                    | crate::harness::CraftEvent::Error(_)
                    | crate::harness::CraftEvent::PersistenceFailed(_),
                ) => panic!("terminal craft events are filtered: {event:?}"),
                HarnessEvent::Agent(animist::agent::AgentEvent::ToolProgress { .. }) => {
                    panic!("progress notes are filtered: {event:?}")
                }
                HarnessEvent::Agent(animist::agent::AgentEvent::Finished { .. }) => {
                    panic!("the child's finished status is filtered: {event:?}")
                }
                HarnessEvent::Agent(animist::agent::AgentEvent::ToolFinished { call, .. })
                    if call.name == "bash" =>
                {
                    saw_finished = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_finished,
            "the bash completion must pass through: {forwarded:?}"
        );
    }

    // ---- 10: layer instances are never shared ----

    /// A layer instance counting `turn_start` calls: the parent's instance
    /// counts the parent's runs and never the child's, proving the child
    /// conversation builds its own instances from the shared services.
    struct TurnStarts(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl animist::agent::LayerImpl for TurnStarts {
        async fn turn_start(
            &self,
            _agent: &animist::agent::Agent,
            _run: &animist::agent::RunContext,
            message: &StoredMessage,
        ) -> std::result::Result<StoredMessage, animist::agent::AgentError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(message.clone())
        }
    }

    #[tokio::test]
    async fn no_layer_instance_reaches_both_parent_and_child() {
        let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.keep();
        let workspace = Workspace::new(&root).unwrap();
        let (sink, stream) = crate::harness::EventSink::new();
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                let _ = event;
            }
        });
        let cwd = root.display().to_string();
        // One script serves both: the parent turn first, then the child's.
        let parent = crate::harness::ConversationBuilder::new(
            &crate::config::Config::parse("").expect("default config"),
            &cwd,
            "",
            mock(vec![
                crate::testing::text_turn("parent turn"),
                crate::testing::text_turn("child done"),
            ]),
            workspace
                .clone()
                .registry(&animist::history::History::new())
                .unwrap(),
            sink.clone(),
        )
        .test_layer(animist::agent::Layer::new(TurnStarts(Arc::clone(&turns))))
        .build()
        .expect("parent conversation builds");
        let l = launcher_over(parent.clone(), sink, workspace, cwd);
        let outcome = crate::harness::run_turn(
            &parent,
            "hello",
            &[],
            animist::agent::Cancellation::default(),
        )
        .await;
        assert!(matches!(outcome, crate::harness::TurnOutcome::Done { .. }));
        assert_eq!(
            turns.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the parent's run counted"
        );
        let result = l.spawn(&SubagentRequest::research("find", "search")).await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            turns.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the child's runs must never reach the parent's layer instance"
        );
    }

    // ---- worktree isolation ----

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// A committed temporary repository: worktree children need a real
    /// git root to link from.
    fn git_repo() -> tempfile::TempDir {
        let repo = tempfile::TempDir::new().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(args)
                .status()
                .unwrap()
                .success()
        };
        assert!(git(&["init", "-q"]), "git init");
        std::fs::write(repo.path().join("committed.txt"), "parent tree\n").unwrap();
        assert!(git(&["add", "."]), "git add");
        assert!(git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init"
        ]));
        repo
    }

    fn allow(l: &SubagentLauncher, tool: &str) {
        l.policy
            .permissions
            .add_session_rule(crate::permissions::PermissionRule {
                tool: crate::permissions::ToolKey::native(tool),
                scope: None,
                effect: crate::permissions::Effect::Allow,
            });
    }

    /// The text of every tool result for `name` in a child's persisted
    /// entries.
    fn child_tool_results(messages: &[StoredMessage], name: &str) -> Vec<String> {
        messages
            .iter()
            .flat_map(|message| message.message().content.iter())
            .filter_map(|block| match block {
                crate::history::ContentBlock::ToolResult {
                    name: Some(block_name),
                    content,
                    ..
                } if block_name == name => Some(ext::tool_content_text(content)),
                _ => None,
            })
            .collect()
    }

    /// The test session id derived from `id`'s two trailing hex digits.
    fn session_ref_for(id: &str) -> crate::id::SessionRef {
        crate::id::SessionRef::from_id(
            format!("01965087-4c71-7f00-8000-0000000000{id}")
                .parse()
                .unwrap(),
        )
    }

    /// A persisted launcher rooted at `root`: the session file records the
    /// child histories, which is how the isolation assertions read back
    /// what the child's tools actually saw.
    async fn persisted_launcher(
        root: &std::path::Path,
        id: &str,
        turns: Vec<crate::testing::Attempt>,
    ) -> SubagentLauncher {
        let session_ref = session_ref_for(id);
        let persister = crate::harness::Persister::open_in(
            crate::storage::StateDir::from_path(root.join("state")),
            session_ref,
            &root.display().to_string(),
            "scripted/mock",
        )
        .await
        .expect("persister opens");
        let workspace = Workspace::new(root).unwrap();
        let (sink, stream) = crate::harness::EventSink::new();
        tokio::spawn(async move {
            let mut stream = stream;
            while let Some(event) = stream.next().await {
                let _ = event;
            }
        });
        let cwd = root.display().to_string();
        let parent = crate::harness::ConversationBuilder::new(
            &crate::config::Config::parse("").expect("default config"),
            &cwd,
            "",
            mock(turns),
            workspace
                .clone()
                .registry(&animist::history::History::new())
                .unwrap(),
            sink.clone(),
        )
        .history(persister.history())
        .persist(Some(Arc::new(tokio::sync::Mutex::new(persister))))
        .build()
        .expect("parent conversation builds");
        launcher_over(parent, sink, workspace, cwd)
    }

    /// Child tools operate in the worktree, not the parent checkout: bash
    /// resolves its cwd there, writes land there, and the parent tree is
    /// untouched after the worktree is cleaned up.
    #[tokio::test]
    async fn worktree_children_run_in_the_child_root() {
        if !git_available() {
            return;
        }
        let repo = git_repo();
        let root = repo.path().to_path_buf();
        let l = persisted_launcher(
            &root,
            "aa",
            vec![
                crate::testing::tool_turn(vec![
                    tool_event("b1", "bash", serde_json::json!({"command": "pwd"})),
                    tool_event(
                        "w1",
                        "write",
                        serde_json::json!({"path": "child.txt", "content": "isolation works"}),
                    ),
                ]),
                crate::testing::text_turn("done"),
            ],
        )
        .await;
        allow(&l, "bash");
        allow(&l, "write");
        let mut req = SubagentRequest::research("isolate", "work inside the worktree");
        req.subagent_type = "general".into();
        req.isolation = "worktree".into();
        req.tool_use_id = "toolu_wt".into();
        let result = l.spawn(&req).await;
        assert!(result.is_ok(), "{result:?}");

        assert!(
            !root.join("child.txt").exists(),
            "the parent checkout must stay untouched"
        );
        assert!(root.join("committed.txt").exists());
        let entries = load_child_entries(&root, "aa", "toolu_wt");
        let pwd = child_tool_results(&entries, "bash").join("");
        let worktrees = std::env::temp_dir()
            .join("craft-worktrees")
            .display()
            .to_string();
        assert!(
            pwd.contains(worktrees.as_str()),
            "bash must run inside the worktree, got {pwd:?}"
        );
        assert!(
            !pwd.contains(root.display().to_string().as_str()),
            "not the parent root: {pwd:?}"
        );
    }

    /// Permission scopes resolve against the child root for a worktree
    /// child: an allow rule scoped to the parent tree must not silently
    /// auto-approve a write inside the isolated copy (the child shares the
    /// rules, not the root they resolve against).
    #[tokio::test]
    async fn worktree_children_resolve_scopes_against_the_child_root() {
        if !git_available() {
            return;
        }
        let repo = git_repo();
        let root = repo.path().to_path_buf();
        let l = persisted_launcher(
            &root,
            "cc",
            vec![
                crate::testing::tool_turn(vec![tool_event(
                    "w1",
                    "write",
                    serde_json::json!({"path": "child.txt", "content": "isolation works"}),
                )]),
                crate::testing::text_turn("done"),
            ],
        )
        .await;
        // An absolute allow scoped to the parent checkout, as a user config
        // would express it.
        l.policy
            .permissions
            .add_session_rule(crate::permissions::PermissionRule {
                tool: crate::permissions::ToolKey::native("write"),
                scope: Some(format!("{}/child.txt", root.display())),
                effect: crate::permissions::Effect::Allow,
            });
        let mut req = SubagentRequest::research("isolate", "write the file");
        req.subagent_type = "general".into();
        req.isolation = "worktree".into();
        req.tool_use_id = "toolu_scopes".into();
        let result = l.spawn(&req).await;
        assert!(result.is_ok(), "{result:?}");
        let entries = load_child_entries(&root, "cc", "toolu_scopes");
        let denial = child_tool_results(&entries, "write").join("");
        assert!(
            denial.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
            "a parent-rooted allow must not auto-approve a worktree write: {denial:?}"
        );
    }

    /// Isolation is explicit about failure: outside a git repository the
    /// spawn errors instead of silently editing the parent tree.
    #[tokio::test]
    async fn worktree_isolation_fails_explicitly_outside_a_git_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(
            mock(vec![crate::testing::text_turn("done")]),
            captured.clone(),
        );
        let mut req = SubagentRequest::research("isolate", "mutate things");
        req.subagent_type = "general".into();
        req.isolation = "worktree".into();
        let error = l.spawn(&req).await.unwrap_err();
        assert!(error.contains("not a git repository"), "{error}");
        assert!(
            !tmp.path().join("child.txt").exists(),
            "nothing ran in the parent tree"
        );
        let _ = tmp;
    }

    /// Concurrent isolated children each get their own root: both spawns
    /// run at once, report distinct worktree cwds, and leave the process
    /// working directory where it was.
    #[tokio::test]
    async fn concurrent_isolated_children_get_their_own_roots() {
        if !git_available() {
            return;
        }
        let repo = git_repo();
        let root = repo.path().to_path_buf();
        let before = std::env::current_dir().unwrap();
        let make = |session: &'static str, call: &'static str| {
            let root = root.clone();
            async move {
                let l = persisted_launcher(
                    &root,
                    session,
                    vec![
                        crate::testing::tool_turn(vec![tool_event(
                            "b1",
                            "bash",
                            serde_json::json!({"command": "pwd"}),
                        )]),
                        crate::testing::text_turn("done"),
                    ],
                )
                .await;
                allow(&l, "bash");
                let mut req = SubagentRequest::research("isolate", "own root");
                req.subagent_type = "general".into();
                req.isolation = "worktree".into();
                req.tool_use_id = call.into();
                l.spawn(&req).await
            }
        };
        // Distinct sessions and call ids: concurrent children are separate
        // spawns, never two writers over one session file.
        let (first, second) = tokio::join!(make("b1", "toolu_first"), make("b2", "toolu_second"));
        assert!(first.is_ok(), "{first:?}");
        assert!(second.is_ok(), "{second:?}");
        assert_eq!(
            std::env::current_dir().unwrap(),
            before,
            "the process working directory never moved"
        );
    }

    // ---- 10: the child history lands in the session ----

    fn load_child_entries(root: &std::path::Path, session: &str, id: &str) -> Vec<StoredMessage> {
        let state = crate::storage::StateDir::from_path(root.join("state"));
        let session =
            crate::storage::sessions::StoredSession::load(session_ref_for(session).id(), &state)
                .expect("session reloads");
        ext::stored_of_entries(
            session
                .subagent_messages()
                .get(id)
                .unwrap_or_else(|| panic!("child history {id} recorded"))
                .as_slice(),
        )
    }

    #[tokio::test]
    async fn the_child_history_is_persisted_when_it_succeeds() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.keep();
        let l = persisted_launcher(
            &root,
            "aa",
            vec![crate::testing::text_turn("found it at src/lib.rs:3")],
        )
        .await;
        let mut req = SubagentRequest::research("find", "search");
        req.tool_use_id = "toolu_ok".into();
        let result = l.spawn(&req).await;
        assert!(result.is_ok(), "{result:?}");
        let entries = load_child_entries(&root, "aa", "toolu_ok");
        assert!(
            entries.len() >= 2,
            "the prompt and the reply are recorded: {:?}",
            entries.len()
        );
        assert_eq!(final_text(&entries), "found it at src/lib.rs:3");
    }

    #[tokio::test]
    async fn the_child_history_is_persisted_when_it_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.keep();
        // A permanent request failure: the child's first model call fails.
        let l =
            persisted_launcher(&root, "aa", vec![crate::testing::http_error(400, "boom")]).await;
        let mut req = SubagentRequest::research("find", "search");
        req.tool_use_id = "toolu_err".into();
        let error = l.spawn(&req).await.unwrap_err();
        assert!(error.contains("sub-agent"), "{error}");
        let entries = load_child_entries(&root, "aa", "toolu_err");
        assert!(
            !entries.is_empty(),
            "a failed child still records what it admitted"
        );
    }

    /// A programmatic launch (no call id) persists nothing.
    #[tokio::test]
    async fn a_call_without_an_id_persists_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.keep();
        let l = persisted_launcher(&root, "aa", vec![crate::testing::text_turn("anon")]).await;
        let result = l.spawn(&SubagentRequest::research("find", "search")).await;
        assert!(result.is_ok(), "{result:?}");
        let persister = l.parent.persister().unwrap();
        let persister = persister.lock().await;
        assert!(
            persister.store().session().subagent_messages().is_empty(),
            "no id, no record"
        );
    }

    // ---- 10: the reviewer budget ----

    #[test]
    fn reviewer_registry_serves_exactly_the_reviewer_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = Workspace::new(tmp.path()).unwrap();
        let registry = workspace
            .reviewer_registry(&animist::history::History::new())
            .unwrap();
        let mut names: Vec<String> = registry
            .definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        names.sort();
        let mut expected: Vec<String> =
            REVIEWER_TOOLS.iter().map(|tool| tool.to_string()).collect();
        expected.extend(["list_tools"].map(str::to_string));
        expected.sort();
        assert_eq!(names, expected);
        for banned in ["task", "question", "sessions", "write", "bash", "batch"] {
            assert!(
                !names.contains(&banned.to_string()),
                "{banned} for reviewer"
            );
        }
    }
}
