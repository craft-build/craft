//! Reusable subagent launcher (A.5 `task` tool).
//!
//! Spawns a child agent with a restricted tool set and its own context
//! window, runs it to completion on its own [`crate::harness::Conversation`]
//! (never the parent's — per-run layer state would collide), and returns
//! the final assistant text or, when an `output_schema` is supplied, the
//! validated JSON object.
//!
//! Ported from the reference `craft-agent/src/tools/subagent.rs` +
//! `task.rs`; the host seams (TUI, headless) install a
//! [`SubagentLauncher`] on the [`crate::tools::Workspace`] the same way
//! the `question` tool installs its asker.

use std::sync::Arc;

use serde_json::Value;

use crate::config::{AgentConfig, ProviderConfig};
use crate::history::{StoredMessage, ext};
use crate::model_registry::ModelTier;
use crate::provider::ModelHandle;
use crate::run;
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
    // Argosy knowledge tools (read-only, Phase 1).
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

/// The reviewer subagent's budget (Phase 5): the read-only research set
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
enum SubagentKind {
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
    /// Per-tool-call child cancellation (task 96): every spawn registers
    /// its child trigger here under the spawning call's id, so a surface
    /// can cancel one subagent without stopping the parent turn.
    pub cancels: Arc<run::cancel::CancelMap<String>>,
    /// The parent conversation's event sink; filtered child events are
    /// forwarded into it as `CraftEvent::Subagent`.
    pub sink: crate::harness::EventSink,
    /// The parent's approval seam (B.1): subagent tool calls flow through
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
    ) -> crate::run::BoxFuture<std::result::Result<SubagentResult, String>> {
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
        let (model, _child_spec) = self.resolve_model(req.model_tier.as_deref()).await?;

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

        let worktree = match req.isolation.as_str() {
            "none" => None,
            "worktree" => match general {
                true => Worktree::create(self.workspace.root(), &req.description),
                false => {
                    return Err("worktree isolation requires subagent_type \"general\"".into());
                }
            },
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

        let base = format!(
            "{}\n\n{}",
            self.base_prompt,
            match kind {
                SubagentKind::General => GENERAL_PROMPT,
                SubagentKind::Reviewer => crate::tools::REVIEWER_PROMPT,
                SubagentKind::Research => RESEARCH_PROMPT,
            }
        );
        // The child's config: defaults plus exactly the knobs the parent
        // passes down, so its request pipeline behaves like the parent's.
        let mut config = crate::config::Config::default();
        config.agent.preamble = base;
        config.agent.temperature = self.agent.temperature;
        config.agent.max_tokens = self.agent.max_tokens;
        config.agent.thinking = self.agent.thinking;
        config.agent.advisor = self.agent.advisor.clone();
        config.compression = self.compression.clone();

        // The child's own history: retrieve reads the child's entry
        // sources, seeded from the parent snapshot below.
        let history = animist::history::History::from_entries(ext::entries(seeded));
        let tools = match kind {
            SubagentKind::General => self.workspace.subagent_registry(true, &history),
            SubagentKind::Research => self.workspace.subagent_registry(false, &history),
            SubagentKind::Reviewer => self.workspace.reviewer_registry(&history),
        }
        .map_err(|e| e.to_string())?;
        // A child conversation with its own layer instances — never the
        // parent's, whose per-run state would collide on run ids. The
        // parent's approval gate governs the child's tool calls too
        // (reference shares ctx.permissions with the spawned agent).
        let (sink, stream) = crate::harness::EventSink::new();
        let mut builder =
            crate::harness::ConversationBuilder::new(&config, &self.cwd, "", model, tools, sink)
                .history(history)
                .max_turns(crate::harness::MaxTurns::Unbounded)
                .workspace(self.workspace.clone());
        builder = builder.policy(self.policy.clone());
        let conversation = builder.build().map_err(|e| e.to_string())?;

        // Filtered child events forward into the parent's sink, tagged with
        // the spawning call: the parent derives its own Done/Error and live
        // tool output; forwarding the child's would duplicate. ToolFinished
        // passes so the task chat (task 96) can complete its tool cards.
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

        // The child cancels with its parent turn or with its own cancel-map
        // slot; the map speaks the trigger vocabulary, so a watcher
        // translates the slot firing into the child's cancellation.
        let child_cancel = animist::agent::Cancellation::default();
        let parent_cancel = (*self.cancel.load_full()).clone();
        let linked = child_cancel.clone();
        let parent_link = tokio::spawn(async move {
            parent_cancel.cancelled().await;
            linked.cancel();
        });
        let mut cancel_slot = None;
        if !req.tool_use_id.is_empty() {
            let (trigger, slot_token) = crate::run::cancel::cancel_pair();
            cancel_slot = Some(self.cancels.insert(req.tool_use_id.clone(), trigger));
            let linked = child_cancel.clone();
            tokio::spawn(async move {
                slot_token.wait().await;
                linked.cancel();
            });
        }
        // Stop the child at every exit path, whether it is owned by the
        // cancel map (retire drops the trigger, firing the watcher) or
        // linked only to the parent.
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

            let outcome = run_isolated(
                || crate::harness::run_turn(&conversation, &message, &[], child_cancel.clone()),
                worktree.as_ref(),
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
                parent_link.abort();
                let _ = conversation.shutdown().await;
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
                parent_link.abort();
                let _ = conversation.shutdown().await;
                drop(conversation);
                let _ = forwarder.await;
                return Ok(SubagentResult::Text(reply));
            }
        }
        stop_child!();
        parent_link.abort();
        let _ = conversation.shutdown().await;
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
pub fn final_text(messages: &[StoredMessage]) -> String {
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

/// Run an isolated subagent inside a linked git worktree. Because the
/// process has a single working directory, isolated subagents are
/// serialized on a global mutex so sibling worktrees never race on
/// `chdir`. When `worktree` is `None`, the run proceeds normally.
async fn run_isolated<F, Fut>(run: F, worktree: Option<&Worktree>) -> crate::harness::TurnOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = crate::harness::TurnOutcome>,
{
    let Some(wt) = worktree else {
        return run().await;
    };
    static WORKTREE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = WORKTREE_GUARD.lock().await;
    let prev = std::env::current_dir().ok();
    if std::env::set_current_dir(wt.path()).is_err() {
        return run().await;
    }
    let result = run().await;
    if let Some(prev) = prev {
        let _ = std::env::set_current_dir(prev);
    }
    result
}

/// Validate `value` against the structural subset of JSON Schema the
/// `output_schema` parameter accepts: `type` (string or list), `properties`,
/// `required`, `items`, and `enum`. No JSON-Schema validator crate is a
/// dependency, and the schemas models compose are simple objects, so a
/// small recursive check covers the reference's `validate` behavior for
/// this seam.
pub fn validate_schema(schema: &Value, value: &Value) -> Result<(), String> {
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
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        for turn in turns {
            provider.push(turn);
        }
        crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("mock", 200_000, 8_192, false, false),
        ))
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
            cancels: Arc::new(run::cancel::CancelMap::new()),
            sink,
            policy: crate::harness::PolicyLayerInputs::new(
                Arc::new(crate::permissions::PermissionManager::new(
                    crate::permissions::PermissionsConfig::default(),
                    root.to_path_buf(),
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
}
