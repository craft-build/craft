//! Reusable subagent launcher. Spawns a child `Agent` with a restricted tool
//! set, runs it to completion, and returns the final assistant text (or, when
//! an `output_schema` is supplied, the validated JSON object).
//!
//! The `task` tool and the Flow `TaskStageRunner` both build on this so the
//! subagent construction (model-role resolution, worktree isolation, output
//! schema validation, event forwarding) stays in one place.

use std::sync::Arc;

use craft_config::ToolOutputLines;
use craft_providers::{ContentBlock, Message, Model, Role};
use craft_storage::id::SessionRef;
use serde_json::Value;
use tracing::{info, warn};

use crate::agent;
use crate::cancel::CancelMap;
use crate::prompt::PromptId;
use crate::template::Vars;
use crate::tools::schema as tool_schema;
use crate::tools::worktree::Worktree;
use crate::tools::{
    DescriptionContext, FileReadTracker, ToolAudience, ToolContext, ToolFilter, ToolRegistry,
};
use crate::{
    Agent, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, EventSender,
    SubagentInfo,
};

const SCHEMA_INSTRUCTION_HEAD: &str = "\n\nYou MUST end your final reply with a single JSON object matching this JSON Schema (no prose, no markdown fences, just the JSON object):\n";
const SCHEMA_INSTRUCTION_TAIL: &str = "\nReturn ONLY that JSON object as your final message.";
const MAX_SCHEMA_RETRIES: u32 = 3;

/// A request to launch one subagent. Mirrors the subset of the `task` tool
/// inputs that the Flow pipeline and other programmatic callers need.
pub struct SubagentRequest<'a> {
    /// Human label forwarded as `SubagentInfo.name` and used for the worktree slug.
    pub description: String,
    /// The prompt body (schema instructions are appended when `output_schema` is set).
    pub prompt: String,
    /// `"research"` (read-only) or `"general"` (write).
    pub subagent_type: &'a str,
    /// Model role name (e.g. `"scout"`), resolved via `model_roles.toml`.
    /// When `None`, falls back to the parent model.
    pub model_role: Option<&'a str>,
    /// Optional structured-output schema. When set, the subagent is told to
    /// emit a final JSON object matching it; on validation failure it is
    /// re-prompted (bounded), then the runner surfaces a clean error.
    pub output_schema: Option<Value>,
    /// `"none"` or `"worktree"`. Worktree isolation runs file-mutating
    /// subagents in a linked git worktree so siblings cannot clobber each
    /// other. Falls back to `none` when git is unavailable.
    pub isolation: &'a str,
}

/// The result of a subagent run: either the final assistant text, or the
/// validated JSON object when an `output_schema` was requested.
#[derive(Debug)]
pub enum SubagentResult {
    Text(String),
    Json(Value),
}

impl SubagentResult {
    /// The textual form to feed into the next stage. JSON is pretty-printed
    /// so downstream JSON-tolerant parsers still recover the object.
    pub fn into_stage_text(self) -> String {
        match self {
            Self::Text(s) => s,
            Self::Json(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| String::new()),
        }
    }
}

/// Launch a subagent under `ctx` and run it to completion. Mirrors the
/// `task` tool's construction: model-role resolution, worktree isolation,
/// output-schema validation with bounded retry, and event forwarding.
pub async fn run_subagent(
    req: SubagentRequest<'_>,
    ctx: &ToolContext,
) -> Result<SubagentResult, String> {
    let vars = crate::template::env_vars();
    let agent_type = req.subagent_type;
    let (prompt_id, audience) = match agent_type {
        "research" => (PromptId::Research, ToolAudience::RESEARCH_SUB),
        "general" => (PromptId::General, ToolAudience::GENERAL_SUB),
        other => return Err(format!("unknown subagent type: {other}")),
    };

    let (model, provider) = resolve_model(req.model_role, ctx).await?;

    info!(
        description = %req.description,
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
    let mut system = vars
        .apply(&crate::prompt::assemble(
            prompt_id,
            &ctx.prompt_slots,
            &instructions,
        ))
        .into_owned();
    // Stage subagents launched under a Flow ToolContext (mode = Flow) get the
    // same Flow context the root agent does, so they know they are one stage of
    // a pipeline and where their work lands. Without this, `run_subagent` built
    // the system prompt from the bare research/general template and a Flow stage
    // subagent had no idea it was in a Flow workstream.
    if let Some(flow) = crate::prompt::flow_section(&ctx.mode) {
        system.push_str(&flow);
    }

    let tools = build_tools(audience, ctx, &vars, &model);

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
    let subagent_key = ctx
        .tool_use_id
        .clone()
        .unwrap_or_else(|| format!("subagent:{}", req.description));
    let subagent_info = Some(SubagentInfo {
        parent_tool_use_id: subagent_key,
        name: req.description.clone(),
        prompt: Some(req.prompt.clone()),
        model: Some(model.spec()),
        answer_tx: ctx.tool_use_id.as_ref().map(|_| answer_tx),
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
    let (cancel_slot, _hold_trigger): (
        Option<(String, crate::cancel::CancelSlot)>,
        Option<crate::cancel::CancelTrigger>,
    ) = match ctx.tool_use_id.as_ref() {
        Some(id) => (
            Some((
                id.clone(),
                ctx.subagent_cancels.insert(id.clone(), child_trigger),
            )),
            None,
        ),
        None => (None, Some(child_trigger)),
    };

    let worktree = match req.isolation {
        "none" => None,
        "worktree" => match Worktree::create(std::path::Path::new(&vars_cwd), &req.description) {
            Some(wt) => Some(wt),
            None => {
                warn!(
                    description = %req.description,
                    "worktree isolation requested but unavailable; running in parent cwd"
                );
                None
            }
        },
        other => return Err(format!("unknown isolation mode: {other}")),
    };

    let schema = match req.output_schema.as_ref() {
        Some(v) => Some(tool_schema::try_from_json(v).map_err(|e| e.to_string())?),
        None => None,
    };

    let mut prompt_text = req.prompt.clone();
    if let Some(schema) = schema {
        let schema_json = tool_schema::to_json_schema(schema);
        prompt_text.push_str(SCHEMA_INSTRUCTION_HEAD);
        prompt_text
            .push_str(&serde_json::to_string_pretty(&schema_json).map_err(|e| e.to_string())?);
        prompt_text.push_str(SCHEMA_INSTRUCTION_TAIL);
    }

    let max_attempts = if schema.is_some() {
        MAX_SCHEMA_RETRIES + 1
    } else {
        1
    };
    let mut conversation: Vec<Message> = Vec::new();
    let mut validated: Option<Value> = None;
    let mut last_error = String::from("no valid JSON produced");

    for attempt in 0..max_attempts {
        let message = if attempt == 0 {
            prompt_text.clone()
        } else {
            let mut m = format!(
                "Your previous response was not valid JSON matching the required schema: {last_error}\n\n"
            );
            if let Some(schema) = schema {
                let schema_json = tool_schema::to_json_schema(schema);
                m.push_str("The required JSON Schema is:\n");
                m.push_str(&serde_json::to_string_pretty(&schema_json).unwrap_or_default());
                m.push_str("\n\nReply again with ONLY a single JSON object matching this schema. If you previously returned a bare array, wrap it inside the object's array field.");
            }
            m
        };
        let input = AgentInput {
            message,
            mode: AgentMode::Build,
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
                subagent_cancels: Arc::new(CancelMap::new()),
                compression: ctx.compression.clone(),
                model_policy: Arc::clone(&ctx.model_policy),
                findings_store: None,
                fs: Arc::new(crate::tools::LocalFs),
                doom: Arc::new(std::sync::Mutex::new(crate::DoomTracker::new())),
                registry: Arc::clone(crate::tools::ToolRegistry::native_arc()),
                flow_thread_history: None,
                flow_thread_manager: None,
                flow_advisor: None,
                flow_progress_tx: None,
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
        .with_mcp(ctx.mcp.clone())
        .with_flow_search(ctx.flow_search.clone());

        run_isolated(agent, input, worktree.as_ref())
            .await
            .map_err(|e| format!("sub-agent error: {e}"))?;

        conversation = history.into_vec();
        let last_text = final_text(&conversation);

        if let Some(schema) = schema {
            match extract_json(&last_text)
                .and_then(|v| tool_schema::validate(schema, v).map_err(|e| e.to_string()))
            {
                Ok(v) => {
                    validated = Some(v);
                    break;
                }
                Err(e) => {
                    last_error = e.clone();
                    conversation.push(Message::user(format!(
                        "Your previous response did not match the required output schema: {e}"
                    )));
                    warn!(
                        description = %req.description,
                        attempt, error = %e,
                        "subagent output schema validation failed"
                    );
                }
            }
        } else {
            return Ok(SubagentResult::Text(last_text));
        }
    }

    if let Some((id, slot)) = cancel_slot {
        ctx.subagent_cancels.retire(&id, slot);
    }
    drop(worktree);

    if let Some(tool_use_id) = ctx.tool_use_id.clone() {
        let _ = ctx.event_tx.send(AgentEvent::SubagentHistory {
            tool_use_id,
            messages: conversation.clone(),
        });
    }

    match validated {
        Some(v) => Ok(SubagentResult::Json(v)),
        None => {
            let msg = format!("subagent did not produce schema-valid JSON: {last_error}");
            let (kind, action) = crate::agent::recovery::classify_subagent_error(&msg);
            warn!(description = %req.description, ?kind, ?action, "subagent schema validation exhausted");
            // action is Stop (schema failures are deterministic) — no retry.
            Err(msg)
        }
    }
}

async fn resolve_model(
    role: Option<&str>,
    ctx: &ToolContext,
) -> Result<(Model, Arc<dyn craft_providers::provider::Provider>), String> {
    if let Some(role_str) = role {
        let role: craft_config::model_roles::ModelRole = role_str.parse().map_err(|e: String| e)?;
        let resolved = craft_providers::roles::resolve_role(
            role,
            Model::clone(&ctx.model),
            Arc::clone(&ctx.provider),
            ctx.timeouts,
        )
        .await;
        Ok((resolved.primary.model, resolved.primary.provider))
    } else {
        Ok((Model::clone(&ctx.model), Arc::clone(&ctx.provider)))
    }
}

fn build_tools(audience: ToolAudience, ctx: &ToolContext, vars: &Vars, model: &Model) -> Value {
    let snapshot = ToolRegistry::native().iter();
    let tool_names: Vec<String> = snapshot
        .iter()
        .filter(|e| {
            e.tool.audience().contains(audience)
                && crate::tools::is_tool_enabled(&ctx.config, e.name())
        })
        .map(|e| e.name().to_owned())
        .collect();
    let filter = ToolFilter::Only(tool_names);
    let ctx_desc = DescriptionContext {
        filter: &filter,
        mcp: ctx.mcp.is_some(),
    };
    let mut tools =
        ToolRegistry::native().definitions(vars, &ctx_desc, model.supports_tool_examples());
    if let Some(ref mcp) = ctx.mcp {
        mcp.extend_tools(&mut tools);
    }
    tools
}

fn final_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::Assistant))
        .and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        })
        .unwrap_or_default()
        .to_string()
}

async fn run_isolated<'h>(
    agent: Agent<'h>,
    input: AgentInput,
    worktree: Option<&Worktree>,
) -> Result<crate::DoneReason, crate::AgentError> {
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

fn extract_json(text: &str) -> Result<Value, String> {
    use jsonrepair::{Options as RepairOpts, loads as repair_loads};
    let trimmed = text.trim();
    if let Ok(v @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(trimmed) {
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
    repair_loads(slice, &RepairOpts::default())
        .map_err(|e| format!("invalid JSON: {e}"))
        .and_then(|v| match v {
            Value::Object(_) | Value::Array(_) => Ok(v),
            other => Err(format!("expected JSON object or array, got {}", other)),
        })
}
