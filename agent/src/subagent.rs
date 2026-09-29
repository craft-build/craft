//! Reusable subagent launcher (A.5 `task` tool).
//!
//! Spawns a child agent with a restricted tool set and its own context
//! window, runs it to completion through the ordinary [`crate::run::run`]
//! seam (Rig owns the loop — a subagent is a nested run with a child tool
//! table and a child cancel token, not a separate agent object), and
//! returns the final assistant text or, when an `output_schema` is
//! supplied, the validated JSON object.
//!
//! Ported from the reference `craft-agent/src/tools/subagent.rs` +
//! `task.rs`; the host seams (TUI, headless) install a
//! [`SubagentLauncher`] on the [`crate::tools::Workspace`] the same way
//! the `question` tool installs its asker.

use std::sync::Arc;

use serde_json::Value;

use crate::config::{AgentConfig, ProviderConfig};
use crate::history::Message;
use crate::model_registry::ModelTier;
use crate::providers::DynamicModel;
use crate::run::{self, CancelToken, Event, RunParams};
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

pub const RESEARCH_PROMPT: &str = "You are a research subagent launched by a parent agent. \
You have your own context window and read-only tools. Investigate the task you were given and \
report back: your final message is returned verbatim to the calling agent, so end with a \
complete, self-contained answer with file:line references. Be concise.";

pub const GENERAL_PROMPT: &str = "You are a general subagent launched by a parent agent. \
You have your own context window and may modify files. Keep your changes scoped to the task you \
were given. Your final message is returned verbatim to the calling agent, so end with a \
complete, self-contained summary of what you did, with file:line references. Be concise.";

const MAX_SCHEMA_RETRIES: u32 = 3;
const SCHEMA_INSTRUCTION_HEAD: &str = "\n\nYou MUST end your final reply with a single JSON object matching this JSON Schema (no prose, no markdown fences, just the JSON object):\n";
const SCHEMA_INSTRUCTION_TAIL: &str = "\nReturn ONLY that JSON object as your final message.";

/// A request to launch one subagent. Mirrors the subset of the `task` tool
/// inputs that programmatic callers need.
#[derive(Debug, Clone)]
pub struct SubagentRequest {
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
    /// The parent's resolved model; the child's fallback.
    pub parent_model: DynamicModel,
    /// `provider/model` spec of the parent, for tier resolution and pricing.
    pub parent_spec: String,
    /// Provider name (key into `providers`).
    pub provider: String,
    /// Provider configs from which alternate-tier models are built.
    pub providers: std::collections::BTreeMap<String, ProviderConfig>,
    /// Agent-level run settings for the child.
    pub agent: AgentConfig,
    /// Compression settings shared with the parent's run.
    pub compression: crate::compression::CompressionConfig,
    /// The parent's system-preamble base (user preamble + instructions).
    pub base_prompt: String,
    /// The parent workspace (cloned; subagent tool tables are built from it).
    pub workspace: Workspace,
    /// Parent history snapshot for `context_mode`.
    pub history: Vec<Message>,
    /// Parent cancel token; the child is linked under it.
    pub cancel: CancelToken,
    /// Where filtered child events are forwarded.
    pub emit: Arc<dyn Fn(Event) + Send + Sync>,
    /// The parent's approval gate (B.1): subagent tool calls flow through
    /// the same permission decisions as the parent's, so a `general`
    /// subagent cannot bypass approvals by running `bash` itself.
    pub before: Option<Arc<dyn run::dispatch::BeforeExecute>>,
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
    ) -> Result<(DynamicModel, std::sync::Arc<str>), String> {
        let parent = (self.parent_model.clone(), self.parent_spec.as_str().into());
        let Some(tier_str) = requested else {
            return Ok(parent);
        };
        let requested = parse_tier(tier_str)?;
        // Compaction-tier models are internal; a subagent may not request one.
        if requested == ModelTier::Compaction {
            return Err("model tier \"compaction\" is not available for subagents".into());
        }
        let parent_tier = crate::model_registry::tier_for(&self.parent_spec, &self.provider, None);
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
        let provider =
            crate::providers::Provider::from_config(provider_config).map_err(|e| e.to_string())?;
        let model = provider
            .completion_model(&spec)
            .map_err(|e| e.to_string())?;
        Ok((model, spec.as_str().into()))
    }

    /// Run one subagent to completion (reference `run_subagent`).
    pub async fn spawn(&self, req: &SubagentRequest) -> Result<SubagentResult, String> {
        let general = match req.subagent_type.as_str() {
            "research" => false,
            "general" => true,
            other => return Err(format!("unknown subagent type: {other}")),
        };
        let (model, child_spec) = self.resolve_model(req.model_tier.as_deref()).await?;

        tracing::info!(
            description = %req.description,
            subagent_type = %req.subagent_type,
            "subagent spawning"
        );

        let seeded: Vec<Message> = match req.context_mode.as_str() {
            "none" => Vec::new(),
            "summary" => self.history.iter().rev().take(8).rev().cloned().collect(),
            "full" => self.history.clone(),
            other => return Err(format!("unknown context_mode: {other}")),
        };

        let worktree = match req.isolation.as_str() {
            "none" => None,
            "worktree" => match general {
                true => Worktree::create(self.workspace.root(), &req.description),
                false => {
                    return Err("worktree isolation requires subagent_type \"general\"".to_string());
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
            if general {
                GENERAL_PROMPT
            } else {
                RESEARCH_PROMPT
            }
        );
        let preamble = crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", self.workspace.root().display().to_string())
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &base,
            &crate::prompt::ResolvedSlots::default(),
            None,
        );
        let params = RunParams {
            preamble: Some(preamble),
            temperature: self.agent.temperature,
            max_tokens: self.agent.max_tokens,
            max_turns: RunParams::UNBOUNDED,
            recency: None,
            compression: self.compression.clone(),
            max_continuation_turns: RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
            compaction: None,
            retry: run::RetryCtx::default(),
            reauth: None,
            model_spec: Some(child_spec),
            fast: false,
        };

        let mut tools = self.workspace.register_subagent(general);
        // The parent's approval gate governs the child's tool calls too
        // (reference shares ctx.permissions with the spawned agent).
        if let Some(hook) = &self.before {
            tools = tools.with_before(Arc::clone(hook));
        }
        let (child_trigger, child_cancel) = self.cancel.child();
        let description = req.description.clone();
        let forward = {
            let emit = Arc::clone(&self.emit);
            move |event: Event| {
                // Reference filter: the parent derives its own Done/Error and
                // live tool output; forwarding the child's would duplicate.
                if matches!(
                    event,
                    Event::Done { .. }
                        | Event::Error(_)
                        | Event::ToolOutput { .. }
                        | Event::ToolPending { .. }
                        | Event::StreamClosed
                ) {
                    return;
                }
                emit(Event::Subagent {
                    description: description.clone(),
                    event: Box::new(event),
                });
            }
        };

        let max_attempts = if schema.is_some() {
            MAX_SCHEMA_RETRIES + 1
        } else {
            1
        };
        let mut conversation = seeded;
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

            let mut history = conversation.clone();
            let outcome = run_isolated(
                || {
                    run::run(
                        &model,
                        &params,
                        &tools,
                        &mut history,
                        &message,
                        &child_cancel,
                        &forward,
                    )
                },
                worktree.as_ref(),
            )
            .await;
            if matches!(
                outcome,
                run::RunOutcome::Failed(_) | run::RunOutcome::Cancelled
            ) {
                let message = match &outcome {
                    run::RunOutcome::Failed(e) => e.clone(),
                    _ => "cancelled".to_string(),
                };
                // A run cut short after streaming some text still has the
                // transcript: half of it beats a bare error.
                let partial = partial_text(&history);
                child_trigger.cancel();
                if partial.is_empty() {
                    return Err(format!("sub-agent error: {message}"));
                }
                return Err(format!(
                    "sub-agent interrupted ({message}). Partial output:\n{partial}"
                ));
            }
            drop(outcome);
            conversation = history;

            let last_text = final_text(&conversation);
            if let Some(schema) = &schema {
                match crate::json_repair::extract_json(&last_text)
                    .and_then(|v| validate_schema(schema, &v).map(|_| v))
                {
                    Ok(v) => {
                        validated = Some(v);
                        break;
                    }
                    Err(e) => {
                        last_error.clone_from(&e);
                        conversation.push(Message::user(format!(
                            "Your previous response did not match the required output schema: {e}"
                        )));
                        tracing::warn!(
                            description = %req.description,
                            attempt, error = %e,
                            "subagent output schema validation failed"
                        );
                    }
                }
            } else {
                child_trigger.cancel();
                return Ok(SubagentResult::Text(last_text));
            }
        }
        child_trigger.cancel();
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
pub fn final_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m, Message::Assistant { .. }))
        .map(|m| m.text())
        .unwrap_or_default()
}

/// Every assistant text of the run, unlike [`final_text`]'s last one: a
/// cut-short run's value is everything it managed to say.
fn partial_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| matches!(m, Message::Assistant { .. }))
        .map(|m| m.text())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run an isolated subagent inside a linked git worktree. Because the
/// process has a single working directory, isolated subagents are
/// serialized on a global mutex so sibling worktrees never race on
/// `chdir`. When `worktree` is `None`, the run proceeds normally.
async fn run_isolated<F, Fut>(run: F, worktree: Option<&Worktree>) -> run::RunOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = run::RunOutcome>,
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

    fn assistant(text: &str) -> Message {
        Message::assistant(text)
    }

    #[test]
    fn final_text_returns_last_assistant_text() {
        let convo = vec![assistant("older"), Message::user("q"), assistant("newer")];
        assert_eq!(final_text(&convo), "newer");
    }

    #[test]
    fn final_text_is_empty_without_assistant_text() {
        assert_eq!(final_text(&[]), "");
        assert_eq!(final_text(&[Message::user("q")]), "");
    }

    #[test]
    fn partial_text_joins_all_assistant_replies() {
        let convo = vec![assistant("one"), Message::user("q"), assistant("two")];
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

    fn mock(turns: Vec<Vec<rig_core::test_utils::MockStreamEvent>>) -> DynamicModel {
        DynamicModel::wrap(
            Some("mock"),
            rig_core::test_utils::MockCompletionModel::from_stream_turns(turns),
        )
    }

    struct Captured(StdMutex<Vec<Event>>);

    fn launcher(
        model: DynamicModel,
        captured: Arc<Captured>,
        cancel: &CancelToken,
    ) -> SubagentLauncher {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = Workspace::new(tmp.path()).unwrap();
        // Leak the tempdir's workspace root validity for the test's lifetime:
        // TempDir must outlive the launcher, so box it alongside.
        let workspace = workspace;
        let emit_capture = Arc::clone(&captured);
        SubagentLauncher {
            parent_model: model,
            parent_spec: "anthropic/mock-model".into(),
            provider: "anthropic".into(),
            providers: Default::default(),
            agent: Default::default(),
            compression: Default::default(),
            base_prompt: String::new(),
            workspace,
            history: Vec::new(),
            cancel: cancel.clone(),
            emit: Arc::new(move |event| emit_capture.0.lock().unwrap().push(event)),
            before: None,
        }
    }

    fn text_turn(text: &str) -> Vec<rig_core::test_utils::MockStreamEvent> {
        vec![
            rig_core::test_utils::MockStreamEvent::text(text),
            rig_core::test_utils::MockStreamEvent::final_response_with_total_tokens(1),
        ]
    }

    fn events(captured: &Captured) -> Vec<Event> {
        captured.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn approval_gate_governs_child_tool_calls() {
        use crate::history::ToolCall;
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Clone)]
        struct CountingGate(Arc<AtomicUsize>);
        impl run::dispatch::BeforeExecute for CountingGate {
            fn decide(&self, call: ToolCall) -> run::BoxFuture<run::Decision> {
                self.0.fetch_add(1, Ordering::SeqCst);
                assert_eq!(call.function.name, "list");
                Box::pin(async { run::Decision::Run })
            }
        }

        let (_flag, cancel) = run::cancel_channel();
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let seen = Arc::new(AtomicUsize::new(0));
        let mut l = launcher(
            mock(vec![
                vec![
                    rig_core::test_utils::MockStreamEvent::tool_call(
                        "t1",
                        "list",
                        serde_json::json!({"path": "."}),
                    ),
                    rig_core::test_utils::MockStreamEvent::final_response_with_total_tokens(1),
                ],
                text_turn("done"),
            ]),
            captured,
            &cancel,
        );
        l.before = Some(Arc::new(CountingGate(Arc::clone(&seen))));
        let result = l
            .spawn(&SubagentRequest::research("find", "list files"))
            .await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "gate must see child tool calls"
        );
    }

    #[tokio::test]
    async fn research_subagent_returns_final_text() {
        let (flag, cancel) = run::cancel_channel();
        drop(flag);
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(
            mock(vec![text_turn("found it at src/lib.rs:12")]),
            captured.clone(),
            &cancel,
        );
        let result = l.spawn(&SubagentRequest::research("find", "search")).await;
        assert_eq!(
            result.unwrap(),
            SubagentResult::Text("found it at src/lib.rs:12".into())
        );
        // The child's Done is filtered; forwarded events carry the tag.
        let forwarded = events(&captured);
        assert!(
            forwarded
                .iter()
                .all(|e| matches!(e, Event::Subagent { .. }))
        );
        assert!(!forwarded.iter().any(
            |e| matches!(e, Event::Subagent { event, .. } if matches!(&**event, Event::Done { .. }))
        ));
    }

    #[tokio::test]
    async fn output_schema_retries_until_valid_json() {
        let (flag, cancel) = run::cancel_channel();
        drop(flag);
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let model = mock(vec![
            text_turn("sure, here you go"),
            text_turn(
                r#"```json
            {"summary": "ok"}
            ```"#,
            ),
        ]);
        let l = launcher(model, captured, &cancel);
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
        let (flag, cancel) = run::cancel_channel();
        flag.set(true);
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(mock(vec![text_turn("never runs")]), captured, &cancel);
        let err = l
            .spawn(&SubagentRequest::research("find", "search"))
            .await
            .unwrap_err();
        assert!(err.contains("sub-agent"), "{err}");
    }

    #[tokio::test]
    async fn unknown_enum_inputs_are_rejected() {
        let (_flag, cancel) = run::cancel_channel();
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(mock(vec![]), captured, &cancel);
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
        let research = workspace.register_subagent(false).names();
        let general = workspace.register_subagent(true).names();
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
        let (_flag, cancel) = run::cancel_channel();
        let captured = Arc::new(Captured(StdMutex::new(Vec::new())));
        let l = launcher(mock(vec![]), captured, &cancel);
        let (model, spec) = l.resolve_model(Some("weak")).await.unwrap();
        assert_eq!(spec.as_ref(), "anthropic/mock-model");
        let _ = model;
    }
}
