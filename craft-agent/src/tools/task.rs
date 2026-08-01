//! Spawns a child agent (subagent) with a restricted tool set.
//!
//! The child's model tier is capped at the parent's tier, so a weak parent cannot spawn a strong child.
//! Events are forwarded to the parent with `SubagentInfo` attached; Done/Error/ToolOutput/ToolPending are filtered.
//! Child cancellation is linked to the parent via `cancel.child()`, so parent cancellation propagates.

use std::sync::Arc;
use std::time::Instant;

use crate::{AgentEvent, EventSender, SubagentInfo, ToolOutput};
use craft_config::ToolOutputLines;
use craft_providers::model::ModelTier;
use craft_providers::model_registry;
use craft_providers::provider;
use craft_providers::{ContentBlock, Model, ModelError, Role};
use craft_storage::id::SessionRef;
use craft_tool_macro::Tool;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use super::schema as tool_schema;
use super::worktree::Worktree;
use super::{DescriptionContext, FileReadTracker, ToolContext, ToolFilter};
use crate::agent;
use crate::template;
use crate::tools::{ToolAudience, ToolRegistry};
use crate::{Agent, AgentInput, AgentMode, AgentParams, AgentRunParams};

/// Flow-mode context for a `task`-spawned child agent: the child's `ThreadId`
/// and the shared handles it inherits from the parent. `None` in Build/Plan,
/// where `task` is the ordinary subagent with no thread registration.
struct FlowChild {
    workstream: String,
    child_thread_id: crate::agent::typed_log::ThreadId,
    parent_id: crate::agent::typed_log::ThreadId,
    thread_history: Option<Arc<std::sync::Mutex<crate::agent::typed_log::ThreadHistory>>>,
    thread_manager: Option<Arc<std::sync::Mutex<crate::agent::threads::ThreadManager>>>,
    progress_tx: Option<flume::Sender<crate::FlowProgress>>,
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Task {
    #[param(description = "Short (3-5 words) description of the task")]
    description: String,
    #[param(description = "Detailed task prompt for the agent")]
    prompt: String,
    #[param(
        description = "Subagent type: \"research\" (read-only, default) or \"general\" (can modify files)"
    )]
    subagent_type: Option<String>,
    #[param(
        description = "Model tier (optional, omit to use current model, capped at current tier):\n- \"strong\" (e.g. Opus): Deep reasoning, complex architecture, subtle bugs, most critical sections. ~5x cost of medium.\n- \"medium\" (e.g. Sonnet): Balanced. Refactors, features, multi-file changes.\n- \"weak\" (e.g. Haiku): Fast/cheap. Search, summarize, boilerplate, simple edits."
    )]
    model_tier: Option<String>,
    #[param(
        description = "Model role (optional, mutually exclusive with model_tier). When set, resolves the subagent's model from model_roles.toml by role name (e.g. \"scout\", \"advisor\"). Unset roles fall back to the current model. Cannot be combined with model_tier."
    )]
    model_role: Option<String>,
    #[param(
        description = "Parent context to pass to the subagent:\n- \"none\" (default): fresh, no parent history.\n- \"summary\": last few parent messages for context.\n- \"full\": full parent conversation history."
    )]
    context_mode: Option<String>,
    #[param(
        description = "Optional JSON Schema (object) describing the structured object the subagent must return as its final message. When set, the subagent is told to emit a final JSON object matching the schema; that object is validated and returned to you as structured data instead of prose. On validation failure the subagent is re-prompted (bounded), then a clean error is surfaced."
    )]
    output_schema: Option<Value>,
    #[param(
        description = "Isolation mode for a general subagent:\n- \"none\" (default): run in the current working tree.\n- \"worktree\": run inside a fresh linked git worktree so file mutations do not touch the parent tree (sibling subagents cannot clobber each other). Requires a git repo; falls back to none otherwise."
    )]
    isolation: Option<String>,
}

impl Task {
    pub const NAME: &str = "task";
    pub const DESCRIPTION: &str = include_str!("task.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"description": "Find auth middleware", "prompt": "Search the codebase for authentication middleware. Return file paths and a summary of how auth is implemented.", "model_tier": "weak"}]"#,
    );

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let vars = template::env_vars();
        let agent_type = self.subagent_type.as_deref().unwrap_or("research");
        let (prompt_id, audience) = match agent_type {
            "research" => (
                crate::prompt::PromptId::Research,
                ToolAudience::RESEARCH_SUB,
            ),
            "general" => (crate::prompt::PromptId::General, ToolAudience::GENERAL_SUB),
            other => return Err(format!("unknown subagent type: {other}")),
        };

        if self.model_tier.is_some() && self.model_role.is_some() {
            return Err(
                "model_tier and model_role are mutually exclusive; set only one".to_string(),
            );
        }

        let (model, provider): (Model, Arc<dyn provider::Provider>) = if let Some(ref role_str) =
            self.model_role
        {
            let role: craft_config::model_roles::ModelRole =
                role_str.parse().map_err(|e: String| e)?;
            let resolved = craft_providers::roles::resolve_role(
                role,
                Model::clone(&ctx.model),
                Arc::clone(&ctx.provider),
                ctx.timeouts,
            )
            .await;
            (resolved.primary.model, resolved.primary.provider)
        } else if let Some(ref tier_str) = self.model_tier {
            let requested: ModelTier = tier_str.parse().map_err(|e: ModelError| e.to_string())?;
            let effective = requested.min(ctx.model.tier);
            if effective == ctx.model.tier {
                (Model::clone(&ctx.model), Arc::clone(&ctx.provider))
            } else {
                let mut resolved_model = {
                    let slug = &ctx.model.provider;
                    let map = model_registry::model_registry()
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    map.spec_for_tier(slug, effective)
                        .or_else(|| map.spec_for_tier_any(effective))
                        .and_then(|spec| Model::from_spec(&spec).ok())
                        .or_else(|| Model::from_tier_dynamic(slug, effective).ok())
                        .ok_or_else(|| format!("no model available for tier {effective}"))?
                };
                let resolved_provider = provider::from_model(&mut resolved_model, ctx.timeouts)
                    .await
                    .map_err(|e| e.to_string())?;
                (resolved_model, Arc::from(resolved_provider))
            }
        } else {
            (Model::clone(&ctx.model), Arc::clone(&ctx.provider))
        };

        info!(
            description = %self.description,
            subagent_type = agent_type,
            model = %model.id,
            "subagent spawning",
        );

        let vars_cwd = vars.apply("{cwd}").into_owned();
        let cwd_owned = vars_cwd.clone();
        let instructions =
            tokio::task::spawn_blocking(move || agent::load_instruction_text(&cwd_owned))
                .await
                .map_err(|e| format!("task failed: {e}"))?;
        let system = vars
            .apply(&crate::prompt::assemble(
                prompt_id,
                &ctx.prompt_slots,
                &instructions,
            ))
            .into_owned();
        let snapshot = ToolRegistry::native().iter();
        let tool_names: Vec<String> = snapshot
            .iter()
            .filter(|e| {
                e.tool.audience().contains(audience)
                    && super::is_tool_enabled(&ctx.config, e.name())
            })
            .map(|e| e.name().to_owned())
            .collect();
        let filter = ToolFilter::Only(tool_names);
        let ctx_desc = DescriptionContext { filter: &filter };
        let mut tools =
            ToolRegistry::native().definitions(&vars, &ctx_desc, model.supports_tool_examples());
        if let Some(ref mcp) = ctx.mcp {
            mcp.extend_tools(&mut tools);
        }

        let session_id = ctx
            .session_id
            .as_ref()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(SessionRef::generate);
        let (sub_tx, sub_rx) = flume::unbounded::<crate::Envelope>();
        let sub_event_tx = EventSender::new(sub_tx, ctx.event_tx.run_id());
        let parent_tx = ctx.event_tx.clone();
        let (answer_tx, answer_rx) = flume::unbounded::<String>();
        let answer_rx = Arc::new(tokio::sync::Mutex::new(answer_rx));
        let subagent_info = ctx.tool_use_id.as_ref().map(|id| SubagentInfo {
            parent_tool_use_id: id.to_owned(),
            name: self.description.clone(),
            prompt: Some(self.prompt.clone()),
            model: Some(model.spec()),
            answer_tx: Some(answer_tx),
        });
        tokio::spawn(async move {
            while let Ok(mut envelope) = sub_rx.recv_async().await {
                if matches!(
                    envelope.event,
                    AgentEvent::Done { .. }
                        | AgentEvent::Error { .. }
                        | AgentEvent::ToolOutput { .. }
                        | AgentEvent::ToolPending { .. }
                        | AgentEvent::SubagentHistory { .. }
                ) {
                    continue;
                }
                envelope.subagent = subagent_info.clone();
                let _ = parent_tx.send_envelope(envelope);
            }
        });

        let (child_trigger, child_cancel) = ctx.cancel.child();
        let cancel_slot = match ctx.tool_use_id.as_ref() {
            Some(id) => Some((
                id.clone(),
                ctx.subagent_cancels.insert(id.clone(), child_trigger),
            )),
            None => {
                drop(child_trigger);
                None
            }
        };

        let ctx_mode = self.context_mode.as_deref().unwrap_or("none");
        let seeded: Vec<craft_providers::Message> = match ctx_mode {
            "none" => Vec::new(),
            "summary" => ctx
                .parent_messages
                .iter()
                .rev()
                .take(8)
                .rev()
                .cloned()
                .collect(),
            "full" => ctx.parent_messages.to_vec(),
            other => return Err(format!("unknown context_mode: {other}")),
        };

        let output_schema = match self.output_schema.as_ref() {
            Some(v) => Some(tool_schema::try_from_json(v).map_err(|e| e.to_string())?),
            None => None,
        };

        let isolation = self.isolation.as_deref().unwrap_or("none");
        let worktree = match isolation {
            "none" => None,
            "worktree" => {
                match Worktree::create(std::path::Path::new(&vars_cwd), &self.description) {
                    Some(wt) => Some(wt),
                    None => {
                        warn!(
                            description = %self.description,
                            "worktree isolation requested but unavailable; running in parent cwd"
                        );
                        None
                    }
                }
            }
            other => return Err(format!("unknown isolation mode: {other}")),
        };

        let mut prompt_text = self.prompt.clone();
        if let Some(schema) = output_schema {
            let schema_json = tool_schema::to_json_schema(schema);
            prompt_text.push_str(SCHEMA_INSTRUCTION_HEAD);
            prompt_text
                .push_str(&serde_json::to_string_pretty(&schema_json).map_err(|e| e.to_string())?);
            prompt_text.push_str(SCHEMA_INSTRUCTION_TAIL);
        }

        let start = Instant::now();
        let mut conversation: Vec<craft_providers::Message> = seeded.clone();
        let mut validated: Option<Value> = None;
        let mut last_text = String::new();

        // Flow mode: register a child Thread for this subagent under the parent
        // and emit ThreadSpawn. The child agent shares the parent's typed log,
        // thread manager, and progress channel; it runs its own shift-enabled
        // loop against the child ThreadId. No chunks, no DAG.
        let flow_child =
            if let (AgentMode::Flow(workstream), Some(parent_id), Some(mgr), Some(progress_tx)) = (
                &ctx.mode,
                ctx.flow_thread_id.as_ref(),
                ctx.flow_thread_manager.as_ref(),
                ctx.flow_progress_tx.as_ref(),
            ) {
                let child_id = ctx
                    .tool_use_id
                    .clone()
                    .unwrap_or_else(|| format!("{parent_id}-task-{}", start.elapsed().as_nanos()));
                let (child_thread_id, turn_type) = {
                    let mut m = mgr.lock().unwrap_or_else(|e| e.into_inner());
                    let id = m.spawn(parent_id, agent::turn_type::TurnType::Req, &child_id);
                    (id, agent::turn_type::TurnType::Req)
                };
                let _ = progress_tx.send(crate::FlowProgress::ThreadSpawn {
                    thread_id: child_thread_id.to_string(),
                    parent_id: parent_id.to_string(),
                    turn_type,
                });
                Some(FlowChild {
                    workstream: workstream.clone(),
                    child_thread_id,
                    parent_id: parent_id.clone(),
                    thread_history: ctx.flow_thread_history.clone(),
                    thread_manager: ctx.flow_thread_manager.clone(),
                    progress_tx: ctx.flow_progress_tx.clone(),
                })
            } else {
                None
            };
        if flow_child.is_some() {
            let original = std::mem::take(&mut prompt_text);
            prompt_text.push_str(FLOW_SUBTASK_PREAMBLE);
            prompt_text.push_str(&original);
        }

        let max_attempts = if output_schema.is_some() {
            MAX_SCHEMA_RETRIES + 1
        } else {
            1
        };
        for attempt in 0..max_attempts {
            let message = if attempt == 0 {
                prompt_text.clone()
            } else {
                format!(
                    "{RETRY_PREAMBLE}\n\nReply again with ONLY a JSON object matching the schema."
                )
            };
            let input = AgentInput {
                message,
                mode: flow_child
                    .as_ref()
                    .map(|c| AgentMode::Flow(c.workstream.clone()))
                    .unwrap_or(AgentMode::Build),
                thinking: ctx.opts.thinking,
                fast: ctx.opts.fast,
                ..Default::default()
            };

            let mut history = crate::History::restored(conversation.clone());
            let agent = Agent::new(
                AgentParams {
                    provider: Arc::clone(&provider),
                    model: Model::clone(&model),
                    config: ctx.config.clone(),
                    tool_output_lines: ToolOutputLines::default(),
                    permissions: Arc::clone(&ctx.permissions),
                    session_id: Some(session_id.clone()),
                    mailbox: None,
                    timeouts: ctx.timeouts,
                    file_tracker: FileReadTracker::fresh(),
                    prompt_slots: Arc::clone(&ctx.prompt_slots),
                    subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                    registry: Arc::clone(ToolRegistry::native_arc()),
                    compression: ctx.compression.clone(),
                    findings_store: None,
                    fs: Arc::new(crate::tools::LocalFs),
                    doom: Arc::new(std::sync::Mutex::new(crate::DoomTracker::new())),
                    flow_thread_history: flow_child.as_ref().and_then(|c| c.thread_history.clone()),
                    flow_thread_manager: flow_child.as_ref().and_then(|c| c.thread_manager.clone()),
                    flow_advisor: None,
                    flow_progress_tx: flow_child.as_ref().and_then(|c| c.progress_tx.clone()),
                },
                AgentRunParams {
                    history: &mut history,
                    system: system.clone(),
                    event_tx: sub_event_tx.clone(),
                    tools: tools.clone(),
                    promoted: crate::tools::PromotedTools::new(),
                    tool_build: None,
                    hooks: None,
                },
            )
            .with_user_response_rx(Arc::clone(&answer_rx))
            .with_cancel(child_cancel.clone())
            .with_mcp(ctx.mcp.clone());
            let agent = if let Some(ref child) = flow_child {
                agent.with_flow_thread_id(child.child_thread_id.clone())
            } else {
                agent
            };

            run_isolated(agent, input, worktree.as_ref())
                .await
                .map_err(|e| format!("sub-agent error: {e}"))?;

            conversation = history.into_vec();
            last_text = final_text(&conversation);

            if let Some(schema) = output_schema {
                match extract_json(&last_text)
                    .and_then(|v| tool_schema::validate(schema, v).map_err(|e| e.to_string()))
                {
                    Ok(v) => {
                        validated = Some(v);
                        break;
                    }
                    Err(e) => {
                        conversation.push(craft_providers::Message::user(format!(
                            "Your previous response did not match the required output schema: {e}"
                        )));
                        warn!(
                            description = %self.description,
                            attempt, error = %e,
                            "subagent output schema validation failed"
                        );
                    }
                }
            } else {
                break;
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        if let Some((id, slot)) = cancel_slot {
            ctx.subagent_cancels.retire(&id, slot);
        }
        // Flow mode: close the child Thread and emit ThreadExit so the host's
        // status line reflects the child completing.
        if let Some(child) = flow_child {
            if let Some(mgr) = child.thread_manager.as_ref() {
                mgr.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .exit(&child.child_thread_id);
            }
            if let Some(tx) = child.progress_tx.as_ref() {
                let _ = tx.send(crate::FlowProgress::ThreadExit {
                    thread_id: child.child_thread_id.to_string(),
                    returning_to: child.parent_id.to_string(),
                });
            }
        }
        drop(worktree);
        info!(description = %self.description, duration_ms, "subagent completed");

        if let Some(tool_use_id) = ctx.tool_use_id.clone() {
            let _ = ctx.event_tx.send(AgentEvent::SubagentHistory {
                tool_use_id,
                messages: conversation.clone(),
            });
        }

        if let Some(v) = validated {
            let pretty = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
            return Ok(ToolOutput::Plain(pretty));
        }
        let _ = last_text;
        Ok(ToolOutput::Plain(final_text(&conversation)))
    }

    pub fn start_header(&self) -> String {
        self.description.clone()
    }
}

const MAX_SCHEMA_RETRIES: u32 = 1;
const SCHEMA_INSTRUCTION_HEAD: &str = "\n\nYou MUST end your final reply with a single JSON object matching this JSON Schema (no prose, no markdown fences, just the JSON object):\n";
const SCHEMA_INSTRUCTION_TAIL: &str = "\nReturn ONLY that JSON object as your final message.";
const RETRY_PREAMBLE: &str =
    "Your previous response was not valid JSON matching the required schema.";

/// Prepended to a `task`-spawned child's prompt when the parent runs in Flow
/// mode. The child is a per-chunk thread (started at `req`), and without this
/// it has no signal to use the `shift` tool or knowledge of the chunk cycle.
const FLOW_SUBTASK_PREAMBLE: &str = "\n\nYou are a Flow child thread on this workstream, started at the `req` turn type. Drive the chunk through its cycle with the `shift` tool:\n\n- `req` — write the spec for this chunk, then `shift` to `execute`.\n- `execute` — implement the chunk. The Execute -> Review transition requires the diff to compile (an objective gate runs), so ensure `cargo check` passes before you `shift` to `review`.\n- `review` — check the implementation against the spec. If you find P0 or P1 issues, `shift` back to `execute`. Otherwise `shift` to `qa`.\n- `qa` — run builds and tests. The QA -> Report transition requires tests to pass (an objective gate runs). Fix failures (shifting back to `execute` if needed) before you `shift` to `report`.\n- `report` — write the chunk's outcome summary and stop; the report transition exits this child thread.\n\nCall `shift` with a `target` and a short `rationale`. Read prior entries (the plan, this chunk's requirement, review findings) from the typed log with `flow_search` or `read path=\"flow://...\"` rather than re-deriving them. Keep your work scoped to this chunk.\n\nYour task:\n";

/// Extract the last JSON object/array from the model's text. Tolerates leading
/// prose and markdown fences. Returns `Err` with a short reason when no JSON can
/// be recovered.
fn extract_json(text: &str) -> Result<Value, String> {
    use jsonrepair::{Options as RepairOpts, loads as repair_loads};
    let trimmed = text.trim();
    if let Ok(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
        serde_json::from_str::<Value>(trimmed)
    {
        return Ok(v);
    }
    let start = trimmed
        .rfind(['{', '['])
        .ok_or("no JSON object found in subagent response")?;
    let open = trimmed.as_bytes()[start];
    let close = if open == b'{' { '}' } else { ']' };
    let end = trimmed
        .rfind(close)
        .ok_or("unterminated JSON in subagent response")?;
    if end <= start {
        return Err("malformed JSON in subagent response".into());
    }
    let slice = &trimmed[start..=end];
    if let Ok(v) = serde_json::from_str::<Value>(slice) {
        return Ok(v);
    }
    repair_loads(slice, &RepairOpts::default())
        .map_err(|e| format!("invalid JSON: {e}"))
        .and_then(|v| match v {
            Value::Object(_) | Value::Array(_) => Ok(v),
            other => Err(format!("expected JSON object or array, got {}", other)),
        })
}

fn final_text(messages: &[craft_providers::Message]) -> String {
    messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role, Role::Assistant))
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("(no response)")
        .to_string()
}

/// Run an isolated subagent inside a linked git worktree. Because the process
/// has a single working directory, isolated subagents are serialized on a
/// global mutex so sibling worktrees never race on `chdir`. When `worktree` is
/// `None`, the agent runs normally.
async fn run_isolated<'h>(
    agent: Agent<'h>,
    input: AgentInput,
    worktree: Option<&Worktree>,
) -> Result<(), crate::AgentError> {
    static WORKTREE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let Some(wt) = worktree else {
        return agent.run(input).await;
    };
    let _guard = WORKTREE_GUARD.lock().await;
    let prev = std::env::current_dir().ok();
    let target = wt.path();
    if std::env::set_current_dir(target).is_err() {
        return agent.run(input).await;
    }
    let result = agent.run(input).await;
    if let Some(prev) = prev {
        let _ = std::env::set_current_dir(prev);
    }
    result
}

super::impl_tool!(
    Task,
    audience = super::ToolAudience::MAIN,
    kind = "think",
    tier = super::ToolTier::Core
);

impl super::ToolInvocation for Task {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Task::start_header(self)))
    }
    fn permission_scopes(&self) -> super::BoxFuture<'_, Option<super::PermissionScopes>> {
        Box::pin(std::future::ready(Some(super::PermissionScopes::single(
            format!("task:{}", self.description),
        ))))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Task::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use test_case::test_case;

    #[test_case(r#"{"a": 1}"#, 1 ; "pure_json_object")]
    #[test_case(r#"Here is the result: {"a": 2}"#, 2 ; "trailing_prose")]
    #[test_case(r#"```json
    {"a": 3}
    ```"#, 3 ; "markdown_fenced")]
    fn extract_json_recovers_value(input: &str, expected: i64) {
        let v = extract_json(input).expect("should parse JSON");
        assert_eq!(v["a"], json!(expected));
    }

    #[test]
    fn extract_json_recovers_array_with_prose() {
        let v = extract_json("summary text [1, 2, 3] trailing").expect("should parse array");
        assert!(v.is_array());
        assert_eq!(v[0], json!(1));
    }

    #[test]
    fn extract_json_rejects_when_absent() {
        assert!(extract_json("just prose, no json here").is_err());
    }

    /// The audience bitmask decides which agents can call each tool, so flipping a flag is
    /// a behavior change (letting `memory` into the interpreter, say, hands subagents a new
    /// power). To move a tool between audiences, change the tool file and this map together.
    #[test]
    fn audience_matrix_is_locked() {
        const MAIN: ToolAudience = ToolAudience::MAIN;
        const RES: ToolAudience = ToolAudience::RESEARCH_SUB;
        const GEN: ToolAudience = ToolAudience::GENERAL_SUB;
        const INT: ToolAudience = ToolAudience::INTERPRETER;
        let all = MAIN | RES | GEN | INT;

        let mut expected: BTreeMap<&str, ToolAudience> = BTreeMap::from([
            (super::super::READ_TOOL_NAME, all),
            (super::super::STYLEGUIDE_LIST_TOOL_NAME, all),
            (super::super::STYLEGUIDE_SEARCH_TOOL_NAME, all),
            (super::super::STYLEGUIDE_GET_TOOL_NAME, all),
            (super::super::REPORT_FINDING_TOOL_NAME, MAIN | RES),
            (super::super::REVIEW_TOOL_NAME, MAIN),
            (super::super::READ_FINDINGS_TOOL_NAME, MAIN),
            (crate::agent::retrieve::Retrieve::NAME, MAIN | RES),
            (crate::agent::vcc_recall::VccRecall::NAME, MAIN | RES),
            (super::super::WRITE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::EDIT_TOOL_NAME, MAIN | GEN | INT),
            (super::super::EDIT_LINES_TOOL_NAME, MAIN | GEN | INT),
            (super::super::INSERT_LINES_TOOL_NAME, MAIN | GEN | INT),
            (super::super::MULTIEDIT_TOOL_NAME, MAIN | GEN | INT),
            (super::super::APPLY_PATCH_TOOL_NAME, MAIN | GEN | INT),
            (super::super::BATCH_TOOL_NAME, MAIN | RES | GEN),
            (super::super::CODE_EXECUTION_TOOL_NAME, MAIN | RES | GEN),
            (super::super::LIST_TOOLS_TOOL_NAME, MAIN | RES | GEN),
            (super::super::TASK_TOOL_NAME, MAIN),
            (super::super::BROWSER_TOOL_NAME, MAIN),
            (super::super::OUTLINE_TOOL_NAME, all),
            (super::super::ZOOM_TOOL_NAME, MAIN | GEN | INT),
            (super::super::AST_GREP_TOOL_NAME, MAIN),
            (super::super::AST_EDIT_TOOL_NAME, MAIN),
            (super::super::CALLGRAPH_TOOL_NAME, all),
            (super::super::CONFLICTS_TOOL_NAME, all),
            (super::super::DELETE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::INSPECT_TOOL_NAME, all),
            (super::super::MOVE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::RESOLVE_TOOL_NAME, MAIN),
            (super::super::SAFETY_TOOL_NAME, MAIN),
            (super::super::WIKI_READ_TOOL_NAME, all),
            (super::super::WIKI_APPEND_TOOL_NAME, all),
            (super::super::SHIFT_TOOL_NAME, MAIN),
        ]);

        // `flow_search` is always registered; the matrix must reflect the registry.
        expected.insert(super::super::FLOW_SEARCH_TOOL_NAME, MAIN | RES | GEN);

        let snapshot = ToolRegistry::native().iter();
        let actual: BTreeMap<String, ToolAudience> = snapshot
            .iter()
            .map(|e| (e.name().to_owned(), e.tool.audience()))
            .collect();

        assert_eq!(
            actual.len(),
            expected.len(),
            "native tool count drift: expected {}, got {} ({:?})",
            expected.len(),
            actual.len(),
            actual.keys().collect::<Vec<_>>()
        );

        for (name, want) in &expected {
            let got = actual
                .get(*name)
                .unwrap_or_else(|| panic!("missing tool '{name}'"));
            assert_eq!(
                got.bits(),
                want.bits(),
                "audience drift for '{name}': expected {want:?}, got {got:?}"
            );
        }
    }
}
