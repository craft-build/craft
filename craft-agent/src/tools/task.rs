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
use craft_providers::provider;
use craft_providers::tier_map;
use craft_providers::{ContentBlock, Model, ModelError, Role};
use craft_tool_macro::Tool;
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

use super::{DescriptionContext, FileReadTracker, ToolContext, ToolFilter};
use crate::agent;
use crate::template;
use crate::tools::{ToolAudience, ToolRegistry};
use crate::{Agent, AgentInput, AgentMode, AgentParams, AgentRunParams};

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
        description = "Parent context to pass to the subagent:\n- \"none\" (default): fresh, no parent history.\n- \"summary\": last few parent messages for context.\n- \"full\": full parent conversation history."
    )]
    context_mode: Option<String>,
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

        let (model, provider): (Model, Arc<dyn provider::Provider>) = if let Some(ref tier_str) =
            self.model_tier
        {
            let requested: ModelTier = tier_str.parse().map_err(|e: ModelError| e.to_string())?;
            let effective = requested.min(ctx.model.tier);
            if effective == ctx.model.tier {
                (Model::clone(&ctx.model), Arc::clone(&ctx.provider))
            } else {
                let resolved_model = {
                    let map = tier_map::tier_map()
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    map.spec_for_tier(ctx.model.provider, effective)
                        .or_else(|| map.spec_for_tier_any(effective))
                        .and_then(|spec| Model::from_spec(&spec).ok())
                        .or_else(|| {
                            Model::from_tier_dynamic(
                                ctx.model.provider,
                                effective,
                                ctx.model.dynamic_slug.as_deref(),
                            )
                            .ok()
                        })
                        .ok_or_else(|| format!("no model available for tier {effective}"))?
                };
                let resolved_provider = provider::from_model(&resolved_model, ctx.timeouts)
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

        let cwd_owned = vars.apply("{cwd}").into_owned();
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

        let session_id = Uuid::new_v4().to_string();
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
        if let Some(ref id) = ctx.tool_use_id {
            ctx.subagent_cancels.insert(id.clone(), child_trigger);
        } else {
            drop(child_trigger);
        }
        let input = AgentInput {
            message: self.prompt.clone(),
            mode: AgentMode::Build,
            thinking: ctx.opts.thinking,
            fast: ctx.opts.fast,
            ..Default::default()
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
        let mut history = crate::History::new(seeded);
        let agent = Agent::new(
            AgentParams {
                provider,
                model,
                config: ctx.config.clone(),
                tool_output_lines: ToolOutputLines::default(),
                permissions: Arc::clone(&ctx.permissions),
                session_id: Some(session_id),
                timeouts: ctx.timeouts,
                file_tracker: FileReadTracker::fresh(),
                prompt_slots: Arc::clone(&ctx.prompt_slots),
                subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
                compression: ctx.compression.clone(),
                findings_store: None,
                fs: Arc::new(crate::tools::LocalFs),
                doom: Arc::new(std::sync::Mutex::new(crate::DoomTracker::new())),
            },
            AgentRunParams {
                history: &mut history,
                system,
                event_tx: sub_event_tx,
                tools,
                promoted: crate::tools::PromotedTools::new(),
                tool_build: None,
                hooks: None,
            },
        )
        .with_user_response_rx(answer_rx)
        .with_cancel(child_cancel)
        .with_mcp(ctx.mcp.clone());
        let start = Instant::now();
        let result = agent.run(input).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        if let Some(ref id) = ctx.tool_use_id {
            ctx.subagent_cancels.remove(id);
        }
        let success = result.is_ok();
        info!(description = %self.description, duration_ms, success, "subagent completed");
        result.map_err(|e| format!("sub-agent error: {e}"))?;

        let messages = history.into_vec();

        let text = messages
            .iter()
            .rev()
            .filter(|m| matches!(m.role, Role::Assistant))
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("(no response)")
            .to_string();

        if let Some(tool_use_id) = ctx.tool_use_id.clone() {
            let _ = ctx.event_tx.send(AgentEvent::SubagentHistory {
                tool_use_id,
                messages,
            });
        }

        Ok(ToolOutput::Plain(text))
    }

    pub fn start_header(&self) -> String {
        self.description.clone()
    }
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
    use std::collections::BTreeMap;

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

        let expected: BTreeMap<&str, ToolAudience> = BTreeMap::from([
            (super::super::READ_TOOL_NAME, all),
            (super::super::STYLEGUIDE_LIST_TOOL_NAME, all),
            (super::super::STYLEGUIDE_SEARCH_TOOL_NAME, all),
            (super::super::STYLEGUIDE_GET_TOOL_NAME, all),
            (super::super::REPORT_FINDING_TOOL_NAME, MAIN | RES),
            (super::super::REVIEW_TOOL_NAME, MAIN),
            (super::super::READ_FINDINGS_TOOL_NAME, MAIN),
            (crate::agent::retrieve::Retrieve::NAME, MAIN | RES),
            (super::super::WRITE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::EDIT_TOOL_NAME, MAIN | GEN | INT),
            (super::super::MULTIEDIT_TOOL_NAME, MAIN | GEN | INT),
            (super::super::APPLY_PATCH_TOOL_NAME, MAIN | GEN | INT),
            (super::super::BATCH_TOOL_NAME, MAIN | RES | GEN),
            (super::super::CODE_EXECUTION_TOOL_NAME, MAIN | RES | GEN),
            (super::super::LIST_TOOLS_TOOL_NAME, MAIN | RES | GEN),
            (super::super::TASK_TOOL_NAME, MAIN),
            (super::super::OUTLINE_TOOL_NAME, all),
            (super::super::ZOOM_TOOL_NAME, MAIN | GEN | INT),
            (super::super::AST_GREP_TOOL_NAME, MAIN),
            (super::super::CALLGRAPH_TOOL_NAME, all),
            (super::super::CONFLICTS_TOOL_NAME, all),
            (super::super::DELETE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::INSPECT_TOOL_NAME, all),
            (super::super::MOVE_TOOL_NAME, MAIN | GEN | INT),
            (super::super::SAFETY_TOOL_NAME, MAIN),
        ]);

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
