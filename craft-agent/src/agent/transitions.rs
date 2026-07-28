//! Transitions: local declarations, objective gates, and precedence.
//!
//! Each turn type declares its own small set of legal next types; the overall
//! shape of a run emerges from those local declarations rather than a global
//! dispatcher (design §2). A turn's write carries content plus a proposed next
//! step, but whether that proposal is honored runs through an objective check
//! wherever one exists, and only falls back to self-report where nothing
//! objective is available.
//!
//! Precedence (highest to lowest), resolved in [`resolve`]:
//! - Advisor-forced transition (Phase 3): overrides everything; bypasses
//!   objective gates only when the force target is `General` in a parent
//!   thread, since General re-enters with full picture and re-evaluates.
//! - Objective-gate failure: blocks a self-proposed advance that fails its
//!   check.
//! - Turn's self-proposed transition: honored when no higher authority
//!   overrides.
//!
//! Design ref: `turn-type-agent-loop-design.md` §2 (Transitions) and the plan's
//! "Precedence rule" section.

//! The precedence resolver + helpers are reserved for Phase 3's Advisor
//! forced-transition authority; Phase 2's driver evaluates objective gates
//! directly. Kept compiled so the seam is ready.
#![allow(dead_code)]

use super::turn_type::{Gate, GateFn, ThreadAction, TurnType};

/// The transition a turn proposed, parsed from its write (the model's stated
/// next step plus its self-assessment). Phase 2's self-report path fills this
/// from the subagent's textual output; Phase 3's Advisor can override it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnProposal {
    pub target: TurnType,
    pub action: ThreadAction,
    /// Free-text self-assessment carried for the gate; ignored when the gate
    /// is objective.
    pub self_report: String,
}

impl TurnProposal {
    pub fn self_report(target: TurnType, action: ThreadAction, report: impl Into<String>) -> Self {
        Self {
            target,
            action,
            self_report: report.into(),
        }
    }
}

/// The outcome of resolving a [`TurnProposal`] against a turn type's declared
/// [`super::turn_type::TransitionRule`] set + precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedTransition {
    /// The proposal is honored: this transition is legal and its gate (if any)
    /// passed.
    Accepted {
        target: TurnType,
        action: ThreadAction,
    },
    /// An objective gate blocked the proposal. The turn stays in its current
    /// type (design §2: "a turn doesn't get to certify its own transition
    /// unchecked").
    Blocked { reason: String },
    /// The proposed target is not in this type's declared transition set.
    Illegal { proposed: TurnType },
}

/// Resolve a turn's proposed next step against the declared transition rules
/// and precedence. The Advisor override is `None` in Phase 2 (Phase 3 supplies
/// it); when present and forced, it takes precedence over self-report and
/// objective gates (the escape hatch). A forced transition bypasses the
/// objective gate only because its target is `General` in a parent thread,
/// which re-enters with full picture and re-evaluates (plan: "forced target is
/// always `General` in a parent thread").
pub fn resolve(
    rules: &[super::turn_type::TransitionRule],
    proposal: &TurnProposal,
    advisor_override: Option<&TurnProposal>,
) -> ResolvedTransition {
    if let Some(forced) = advisor_override {
        return ResolvedTransition::Accepted {
            target: forced.target,
            action: forced.action,
        };
    }
    let Some(rule) = rules.iter().find(|r| r.target == proposal.target) else {
        return ResolvedTransition::Illegal {
            proposed: proposal.target,
        };
    };
    match &rule.gate {
        Gate::SelfReport => ResolvedTransition::Accepted {
            target: rule.target,
            action: rule.action,
        },
        Gate::Objective(gate) => match gate() {
            Ok(()) => ResolvedTransition::Accepted {
                target: rule.target,
                action: rule.action,
            },
            Err(reason) => ResolvedTransition::Blocked { reason },
        },
    }
}

/// A gate that always passes (for tests and self-report edges).
pub fn always_pass() -> GateFn {
    std::sync::Arc::new(|| Ok(()))
}

/// A gate that always fails with `reason` (for tests).
pub fn always_fail(reason: &'static str) -> GateFn {
    std::sync::Arc::new(move || Err(reason.to_string()))
}

/// Default post-merge build/test gate: runs `cargo nextest` (falling back to
/// `cargo test`) and `cargo clippy` with `-D warnings`. Migrated from the
/// former `craft_flow::default_post_merge_gate`; used as one objective gate
/// (Integrator/Verifier drift check, plan migration table).
pub fn default_post_merge_gate() -> Result<(), String> {
    let test = std::process::Command::new("cargo")
        .args(["nextest", "run", "--all-features", "--workspace"])
        .output()
        .or_else(|_| {
            std::process::Command::new("cargo")
                .args(["test", "--all-features", "--workspace"])
                .output()
        })
        .map_err(|e| format!("failed to run test gate: {e}"))?;
    if !test.status.success() {
        return Err("post-merge tests failed".to_string());
    }
    let clippy = std::process::Command::new("cargo")
        .args([
            "clippy",
            "--all-features",
            "--all",
            "--tests",
            "--",
            "-D",
            "warnings",
        ])
        .output()
        .map_err(|e| format!("failed to run clippy gate: {e}"))?;
    if !clippy.status.success() {
        return Err("post-merge clippy failed".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::turn_type::{ThreadAction, TurnType};
    use super::*;

    fn rule(
        target: TurnType,
        action: ThreadAction,
        gate: Gate,
    ) -> super::super::turn_type::TransitionRule {
        super::super::turn_type::TransitionRule {
            target,
            action,
            gate,
        }
    }

    #[test]
    fn self_report_proposal_is_accepted() {
        let rules = vec![rule(
            TurnType::Scout,
            ThreadAction::Advance,
            Gate::SelfReport,
        )];
        let p = TurnProposal::self_report(TurnType::Scout, ThreadAction::Advance, "done");
        assert_eq!(
            resolve(&rules, &p, None),
            ResolvedTransition::Accepted {
                target: TurnType::Scout,
                action: ThreadAction::Advance
            }
        );
    }

    #[test]
    fn objective_gate_pass_accepts() {
        let rules = vec![rule(
            TurnType::Review,
            ThreadAction::Advance,
            Gate::Objective(always_pass()),
        )];
        let p = TurnProposal::self_report(TurnType::Review, ThreadAction::Advance, "compiles");
        assert!(matches!(
            resolve(&rules, &p, None),
            ResolvedTransition::Accepted { .. }
        ));
    }

    #[test]
    fn objective_gate_fail_blocks() {
        let rules = vec![rule(
            TurnType::Review,
            ThreadAction::Advance,
            Gate::Objective(always_fail("diff does not compile")),
        )];
        let p = TurnProposal::self_report(TurnType::Review, ThreadAction::Advance, "done");
        assert_eq!(
            resolve(&rules, &p, None),
            ResolvedTransition::Blocked {
                reason: "diff does not compile".to_string()
            }
        );
    }

    #[test]
    fn proposal_outside_transition_set_is_illegal() {
        let rules = vec![rule(
            TurnType::Scout,
            ThreadAction::Advance,
            Gate::SelfReport,
        )];
        let p = TurnProposal::self_report(TurnType::Verifier, ThreadAction::Exit, "skip");
        assert_eq!(
            resolve(&rules, &p, None),
            ResolvedTransition::Illegal {
                proposed: TurnType::Verifier
            }
        );
    }

    #[test]
    fn advisor_override_takes_precedence_over_gate() {
        let rules = vec![rule(
            TurnType::Review,
            ThreadAction::Advance,
            Gate::Objective(always_fail("nope")),
        )];
        let p = TurnProposal::self_report(TurnType::Review, ThreadAction::Advance, "done");
        let forced = TurnProposal::self_report(TurnType::General, ThreadAction::Exit, "drift");
        assert_eq!(
            resolve(&rules, &p, Some(&forced)),
            ResolvedTransition::Accepted {
                target: TurnType::General,
                action: ThreadAction::Exit
            }
        );
    }

    #[test]
    fn advisor_override_takes_precedence_over_self_report() {
        let rules = vec![rule(
            TurnType::Scout,
            ThreadAction::Advance,
            Gate::SelfReport,
        )];
        let p = TurnProposal::self_report(TurnType::Scout, ThreadAction::Advance, "done");
        let forced = TurnProposal::self_report(TurnType::General, ThreadAction::Exit, "drift");
        assert!(matches!(
            resolve(&rules, &p, Some(&forced)),
            ResolvedTransition::Accepted {
                target: TurnType::General,
                ..
            }
        ));
    }
}
