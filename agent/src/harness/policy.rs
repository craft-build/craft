//! Interim approval layer: today's `BeforeExecute` gate plus the plan-mode
//! write gate, mapped onto animist decisions. 07 replaces this body with the
//! rules → auto-review → responder → plan-mode chain.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;

use animist::agent::tool::{ToolContext, ToolOutput};
use animist::agent::{Agent, LayerImpl, Result, ToolDecision};

use crate::history::ContentBlock;
use crate::run::dispatch::{BeforeExecute, Decision};

use super::conversation::TurnSettings;

pub(crate) struct PolicyLayer {
    approval: Option<Arc<dyn BeforeExecute>>,
    settings: Arc<ArcSwap<TurnSettings>>,
    write_root: Option<PathBuf>,
}

impl PolicyLayer {
    pub(crate) fn new(
        approval: Option<Arc<dyn BeforeExecute>>,
        settings: Arc<ArcSwap<TurnSettings>>,
        write_root: Option<PathBuf>,
    ) -> Self {
        Self {
            approval,
            settings,
            write_root,
        }
    }

    /// The model-facing `ToolUse` block the approval gate expects; the
    /// executor's decoded call carries the same identity.
    fn call_block(context: &ToolContext<'_>) -> ContentBlock {
        ContentBlock::ToolUse {
            id: context.call.id.clone(),
            name: context.call.name.clone(),
            input: context.call.input.clone(),
            thought_signature: None,
        }
    }
}

#[async_trait]
impl LayerImpl for PolicyLayer {
    async fn pre_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
    ) -> Result<ToolDecision> {
        if let Some(approval) = &self.approval {
            match approval.decide(Self::call_block(context)).await {
                Decision::Run => {}
                // A skip answers the call without running it; the reason is
                // a normal (non-error) result, exactly what the gate
                // produced before the migration.
                Decision::Skip(reason) => {
                    return Ok(ToolDecision::Respond {
                        output: ToolOutput::builder().content(reason).build(),
                    });
                }
                Decision::Stop(reason) => return Ok(ToolDecision::Stop { reason }),
            }
        }
        let settings = self.settings.load();
        Ok(
            match settings.mode.plan_block_reason(
                self.write_root.as_deref(),
                &context.call.name,
                &context.call.input,
            ) {
                Some(reason) => ToolDecision::Deny { reason },
                None => ToolDecision::Run,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn plan_gate_blocks_writes_outside_the_plan_file() {
        let mode = crate::run::mode::AgentMode::Plan(PathBuf::from("/w/plans/a.md"));
        let blocked = mode.plan_block_reason(
            Some(Path::new("/w")),
            "write",
            &serde_json::json!({ "path": "src/main.rs", "content": "hi" }),
        );
        assert_eq!(
            blocked.as_deref(),
            Some(crate::run::mode::PLAN_WRITE_RESTRICTED)
        );
        let allowed = mode.plan_block_reason(
            Some(Path::new("/w")),
            "write",
            &serde_json::json!({ "path": "/w/plans/a.md", "content": "hi" }),
        );
        assert_eq!(allowed, None);
        let reads = mode.plan_block_reason(
            Some(Path::new("/w")),
            "read",
            &serde_json::json!({ "path": "src/main.rs" }),
        );
        assert_eq!(reads, None);
    }
}
