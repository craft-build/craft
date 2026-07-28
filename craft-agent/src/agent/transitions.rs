//! Transitions: local declarations and precedence.
//!
//! Each turn type declares its own small set of legal next types; the overall
//! shape of a run emerges from those local declarations rather than a global
//! dispatcher (design §2). Every transition is self-report: the turn decides
//! when it is done and what to hand off to. Review and QA run their own checks
//! (compile, tests, lint) via tools and report the result themselves — the host
//! never blocks a shift on a hard-wired command, since the right check depends
//! on the project's language and toolchain.
//!
//! Precedence (highest to lowest), resolved in [`resolve`]:
//! - Advisor-forced transition (Phase 3): overrides self-report; bypasses to
//!   `General` in a parent thread so it re-enters with the full picture.
//! - Turn's self-proposed transition: honored when no higher authority
//!   overrides.

#![allow(dead_code)]

use super::turn_type::{Gate, ThreadAction, TurnType};

/// The transition a turn proposed, parsed from its write (the model's stated
/// next step plus its self-assessment). Phase 3's Advisor can override it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnProposal {
    pub target: TurnType,
    pub action: ThreadAction,
    /// Free-text self-assessment carried for the record.
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
    /// The proposal is honored: this transition is legal.
    Accepted {
        target: TurnType,
        action: ThreadAction,
    },
    /// The proposed target is not in this type's declared transition set.
    Illegal { proposed: TurnType },
}

/// Resolve a turn's proposed next step against the declared transition rules
/// and precedence. The Advisor override is `None` in Phase 2 (Phase 3 supplies
/// it); when present and forced, it takes precedence over self-report (the
/// escape hatch). A forced transition targets `General` in a parent thread,
/// which re-enters with the full picture and re-evaluates.
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
    debug_assert!(
        matches!(rule.gate, Gate::SelfReport),
        "all transitions are self-report; objective gates were removed"
    );
    ResolvedTransition::Accepted {
        target: rule.target,
        action: rule.action,
    }
}

#[cfg(test)]
mod tests {
    use super::super::turn_type::{Gate, ThreadAction, TransitionRule, TurnType};
    use super::*;

    fn rule(target: TurnType, action: ThreadAction) -> TransitionRule {
        TransitionRule {
            target,
            action,
            gate: Gate::SelfReport,
        }
    }

    #[test]
    fn self_report_proposal_is_accepted() {
        let rules = vec![rule(TurnType::Scout, ThreadAction::Advance)];
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
    fn proposal_outside_transition_set_is_illegal() {
        let rules = vec![rule(TurnType::Scout, ThreadAction::Advance)];
        let p = TurnProposal::self_report(TurnType::Verifier, ThreadAction::Exit, "skip");
        assert_eq!(
            resolve(&rules, &p, None),
            ResolvedTransition::Illegal {
                proposed: TurnType::Verifier
            }
        );
    }

    #[test]
    fn advisor_override_takes_precedence_over_self_report() {
        let rules = vec![rule(TurnType::Scout, ThreadAction::Advance)];
        let p = TurnProposal::self_report(TurnType::Scout, ThreadAction::Advance, "done");
        let forced = TurnProposal::self_report(TurnType::General, ThreadAction::Exit, "drift");
        assert_eq!(
            resolve(&rules, &p, Some(&forced)),
            ResolvedTransition::Accepted {
                target: TurnType::General,
                action: ThreadAction::Exit
            }
        );
    }
}
