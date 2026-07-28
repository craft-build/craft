//! The `shift` tool (Flow mode only).
//!
//! Flow mode folds the turn-typed loop into the normal agent loop. The agent
//! starts `General` and runs `Agent::run_loop`. At turn boundaries it drains
//! the last `shift` tool call from the just-completed turn and runs it through
//! `transitions::resolve` against the current type's declared `TransitionRule`
//! set (with the Advisor's forced transition as the override).
//!
//! This tool does **not** mutate `Agent` state. It just returns a sentinel
//! `ToolOutput::ShiftTurnType`. The shift is applied at the turn boundary in
//! `Agent::apply_shift_if_requested` (run.rs). "Last shift wins": the last
//! `shift` call in a tool batch is the one applied, because
//! `Agent::last_shift_request` scans the most recent assistant turn and takes
//! the final `shift` result.

use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::TurnType;
use crate::tools::ToolInvocation;
use crate::types::ToolOutput;

const SHIFT_DESCRIPTION: &str = "\
Shift the Flow run into a different turn type (Flow mode only). The shift is \
applied at the next turn boundary after running through the current type's \
declared transition rules: it is accepted, or rejected as illegal (the current \
type does not declare that target). Use this to enter narrow stages like \
`scout`, `tpm`, `plan`, `req`, `execute`, `review`, `qa`, `report`, \
`integrator`, `verifier`, or to return to `general`. The user chose Flow mode \
to get the pipeline, so default to shifting: `scout` then `tpm` for any task \
that edits code, and `plan` after the goal is approved. Reserve `general` for \
tasks that write no code.";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Shift {
    #[param(
        description = "The turn type to shift into: scout, tpm, plan, req, execute, review, qa, report, integrator, verifier, general"
    )]
    target: String,
    #[param(
        description = "Why this shift is warranted (one or two sentences). Recorded in the typed log."
    )]
    rationale: String,
}

impl Shift {
    pub const NAME: &str = "shift";
    pub const DESCRIPTION: &str = SHIFT_DESCRIPTION;
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"target": "scout", "rationale": "Need a codebase map before planning."}]"#);

    pub fn start_header(&self) -> String {
        format!("shift({})", self.target)
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let target = TurnType::parse(&self.target)
            .ok_or_else(|| format!("unknown turn type '{}'", self.target))?;
        Ok(ToolOutput::ShiftTurnType {
            target,
            rationale: self.rationale.clone(),
        })
    }
}

super::impl_tool!(
    Shift,
    audience = super::ToolAudience::MAIN,
    kind = "switch_mode"
);

impl ToolInvocation for Shift {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Shift::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Shift::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    fn ctx() -> ToolContext {
        stub_ctx(&AgentMode::Flow("test".into()))
    }

    #[test]
    fn shift_returns_sentinel_for_valid_target() {
        let tool = Shift {
            target: "scout".to_string(),
            rationale: "need a codebase map".to_string(),
        };
        let out = futures::executor::block_on(tool.execute(&ctx())).unwrap();
        match out {
            ToolOutput::ShiftTurnType { target, rationale } => {
                assert_eq!(target, TurnType::Scout);
                assert_eq!(rationale, "need a codebase map");
            }
            other => panic!("expected ShiftTurnType, got {other:?}"),
        }
    }

    #[test]
    fn shift_rejects_unknown_target() {
        let tool = Shift {
            target: "nope".to_string(),
            rationale: String::new(),
        };
        let err = futures::executor::block_on(tool.execute(&ctx())).unwrap_err();
        assert!(err.contains("unknown turn type"), "got: {err}");
    }

    #[test]
    fn tool_output_shift_turn_type_round_trips() {
        let v = ToolOutput::ShiftTurnType {
            target: TurnType::Plan,
            rationale: "goal approved, decompose".to_string(),
        };
        let text = v.as_display_text();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["shift"]["target"], "plan");
        assert_eq!(back["shift"]["rationale"], "goal approved, decompose");
    }

    #[test]
    fn tool_output_shift_turn_type_serializes_round_trip() {
        let v = ToolOutput::ShiftTurnType {
            target: TurnType::Scout,
            rationale: "x".to_string(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: ToolOutput = serde_json::from_str(&json).unwrap();
        match back {
            ToolOutput::ShiftTurnType { target, rationale } => {
                assert_eq!(target, TurnType::Scout);
                assert_eq!(rationale, "x");
            }
            other => panic!("expected ShiftTurnType, got {other:?}"),
        }
    }
}
