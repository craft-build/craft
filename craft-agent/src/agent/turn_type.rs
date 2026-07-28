//! Turn types: the four-property behavior profile for one atomic turn.
//!
//! A [`TurnType`] is fully specified by what it reads, what it writes, what it
//! can do, and what can run after it (design §1). [`TurnType::spec`] returns
//! the read policy, write policy (with JSON Schema), tool filter, model role,
//! and declared transitions for each variant.
//!
//! Build and Plan modes both resolve to [`TurnType::General`] and behave
//! exactly as before (design §0): General's turn is the existing `Agent::turn`
//! body, unchanged. Flow mode starts General-in-root-thread; the pipeline
//! shape emerges from the model's `shift` choices, applied at turn boundaries
//! through `transitions::resolve`.

use serde::{Deserialize, Serialize};

use craft_config::model_roles::ModelRole;

use super::typed_log::EntryType;
use crate::tools::ToolFilter;

/// The eleven turn types (design §8.1). `General` is the broad-read,
/// unrestricted-transition type; the rest are the narrow pipeline shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnType {
    General,
    Scout,
    Tpm,
    Plan,
    Req,
    Execute,
    Review,
    Qa,
    Report,
    Integrator,
    Verifier,
}

impl TurnType {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnType::General => "general",
            TurnType::Scout => "scout",
            TurnType::Tpm => "tpm",
            TurnType::Plan => "plan",
            TurnType::Req => "req",
            TurnType::Execute => "execute",
            TurnType::Review => "review",
            TurnType::Qa => "qa",
            TurnType::Report => "report",
            TurnType::Integrator => "integrator",
            TurnType::Verifier => "verifier",
        }
    }

    /// Parse from the serde (`rename_all = "lowercase"`) form. Unknown
    /// strings yield `None` so persisted state from newer versions does not
    /// panic on load. This replaces the former `craft_flow::Stage::parse` at
    /// the UI/ACP boundary.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "general" => TurnType::General,
            "scout" => TurnType::Scout,
            "tpm" => TurnType::Tpm,
            "plan" => TurnType::Plan,
            "req" => TurnType::Req,
            "execute" => TurnType::Execute,
            "review" => TurnType::Review,
            "qa" => TurnType::Qa,
            "report" => TurnType::Report,
            "integrator" => TurnType::Integrator,
            "verifier" => TurnType::Verifier,
            _ => return None,
        })
    }

    /// Resolve the four-property spec for this turn type. Transitions are
    /// all self-report: each turn type decides when it is done and what to
    /// hand off to. There are no host-side objective gates — a Review or QA
    /// turn runs its own checks (compile, tests, lint) via tools and reports
    /// the result itself, since the right check depends on the project's
    /// language and toolchain.
    pub fn spec(self) -> TurnTypeSpec {
        match self {
            TurnType::General => TurnTypeSpec {
                read: ReadPolicy::full(),
                write: WritePolicy::verbatim(),
                tools: ToolFilter::All,
                role: ModelRole::Default,
                // General is the entry point and the catch-all owner. It can
                // shift into any working type as the task demands. Root owner
                // and subtasks alike reach the working types from here; there
                // is no separate "per-chunk" reachability rule.
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Scout,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Tpm,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Plan,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Req,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Execute,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Review,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Qa,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Report,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Integrator,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Verifier,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Scout => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![CoreRead {
                        entry: EntryType::UserRequest,
                        level: ThreadLevel::Root,
                    }],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::CodebaseContext,
                    guidance: Some(
                        "A codebase map: the files, symbols, and conventions the \
                         request touches. Prose or bullet points.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowScout,
                // Scout hands off to tpm when a goal needs shaping, or returns
                // to general when the investigation answered the immediate
                // question. Either is a normal exit from scout; the model
                // picks based on what it found.
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Tpm,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Tpm => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::UserRequest,
                            level: ThreadLevel::Root,
                        },
                        CoreRead {
                            entry: EntryType::CodebaseContext,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::ResearchNotes,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::Goal,
                    guidance: Some(
                        "A goal doc in prose with three sections: `## Goal` \
                         (one-sentence restatement), `## Scope` (what is in \
                         and out), and `## Acceptance criteria` (a checklist \
                         the verifier will check against).",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowTpm,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Tpm,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Plan,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    // Tpm may also bow out: if the request is small enough
                    // that a goal doc is overkill, hand back to general.
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Plan => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Goal,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::CodebaseContext,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::Plan,
                    guidance: Some(
                        "A plan in prose: how you will meet the goal. Cover the \
                         approach, the steps in order, the files or areas each \
                         step touches, and any risks. The plan is for whoever \
                         does the work next (you in a later type, or subtasks \
                         you spawn with the `task` tool for parallel pieces).",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowPlan,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Execute,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Integrator,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Verifier,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Req => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![CoreRead {
                        entry: EntryType::Plan,
                        level: ThreadLevel::Own,
                    }],
                    query_scope: vec![
                        QueryScope {
                            entry: EntryType::CodebaseContext,
                            level: ThreadLevel::Own,
                        },
                        QueryScope {
                            entry: EntryType::ResearchNotes,
                            level: ThreadLevel::Own,
                        },
                    ],
                },
                write: WritePolicy {
                    entry: EntryType::Requirement,
                    guidance: Some(
                        "A precise spec for the piece of work you are about to \
                         do: what needs to be built or changed, the constraints, \
                         and how to tell it is done.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowReq,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Execute,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Execute => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Requirement,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::ReviewFindings,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::Diff,
                    guidance: Some(
                        "Make the code changes. The committed entry is a short \
                         prose summary of what you changed and why; the actual \
                         diff lives in the working tree.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowExecute,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Execute,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Review,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Qa,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Review => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Requirement,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::Diff,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::ReviewFindings,
                    guidance: Some(
                        "Review the implementation against the spec. Write an \
                         overall result (passed, failed, or needs review) and \
                         a list of findings. P0 or P1 findings mean the work \
                         should go back to execute.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowReview,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Execute,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Qa,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Qa => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Requirement,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::Diff,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::QaReport,
                    guidance: Some(
                        "Run a quality pass: builds and tests. Write an overall \
                         result (passed or failed), what you ran, and any \
                         failures. Tests must pass before you shift to report.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowQa,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Qa,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Report,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Execute,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Report => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::ReviewFindings,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::QaReport,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::Diff,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::Report,
                    guidance: Some(
                        "A short outcome summary: what was done and its final \
                         status. This is the wrap-up for a unit of work before \
                         control returns to the owner.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowQa,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Report,
                        action: ThreadAction::Exit,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Integrator => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Plan,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::Diff,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::IntegrationCheckpoint,
                    guidance: Some(
                        "An integration checkpoint in prose: confirm the merged \
                         work fits together (integrated or failed), note any \
                         conflicts, and how the pieces combine.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowIntegrator,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Plan,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Verifier,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Verifier => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Goal,
                            level: ThreadLevel::Own,
                        },
                        CoreRead {
                            entry: EntryType::IntegrationCheckpoint,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::VerificationReport,
                    guidance: Some(
                        "A verification report in prose: an overall verdict \
                         (ship or block), whether the goal was met, each \
                         acceptance criterion checked (met or unmet, with a \
                         short finding), and a summary.",
                    ),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowVerifier,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Plan,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Verifier,
                        action: ThreadAction::Exit,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::General,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
        }
    }
}

/// Which thread level a read resolves against (design §6). `Own` is the
/// current thread; `Parent` is the spawning thread; `Root` is the root thread
/// (the whole request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadLevel {
    Own,
    Parent,
    Root,
}

/// One handed-over core read: an [`EntryType`] resolved at a [`ThreadLevel`].
/// Core context is assembled directly into the turn's messages (design §4:
/// "don't make a turn earn its own obvious context through a query").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CoreRead {
    pub entry: EntryType,
    pub level: ThreadLevel,
}

/// One searchable scope: same shape as [`CoreRead`] but resolved through
/// `history.query` rather than handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueryScope {
    pub entry: EntryType,
    pub level: ThreadLevel,
}

/// What slice of history gets assembled into context this turn.
#[derive(Debug, Clone, Default)]
pub struct ReadPolicy {
    /// Pinned core context handed over directly.
    pub core: Vec<CoreRead>,
    /// Searchable scopes fed to `history.query` on demand.
    pub query_scope: Vec<QueryScope>,
}

impl ReadPolicy {
    /// General's read policy: no filter, the whole log (design §1, §4). Empty
    /// core/query_scope is the precise statement of "no filter at all," since
    /// General reads everything by declaration rather than by enumeration.
    pub fn full() -> Self {
        Self::default()
    }
}

/// What gets committed back on transition, and as what shape (design §1).
#[derive(Debug, Clone)]
pub struct WritePolicy {
    pub entry: EntryType,
    /// Short prose describing what the committed entry should contain. The
    /// stage brief surfaces this so the model knows what to write; the entry
    /// itself is the assistant's verbatim final text (prose/markdown), never a
    /// JSON blob.
    pub guidance: Option<&'static str>,
}

impl WritePolicy {
    /// General writes itself back verbatim as a `GeneralTurn` entry.
    pub fn verbatim() -> Self {
        Self {
            entry: EntryType::GeneralTurn,
            guidance: None,
        }
    }
}

/// Thread-level action a transition takes (design §5): stay, close and return
/// to parent, or open a new child.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadAction {
    Advance,
    Exit,
    Spawn,
}

/// A transition's proposal policy. All transitions are self-report: the
/// turn decides when it is done and what to hand off to. Review and QA run
/// their own checks (compile, tests, lint) via tools and report the result;
/// the host never blocks a shift on a hard-wired command, since the right
/// check depends on the project's language and toolchain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    SelfReport,
}

/// One declared legal next step: a target type and the thread action the
/// shift takes.
#[derive(Clone)]
pub struct TransitionRule {
    pub target: TurnType,
    pub action: ThreadAction,
    pub gate: Gate,
}

/// The four-property spec for one [`TurnType`] (design §1).
#[derive(Clone)]
pub struct TurnTypeSpec {
    pub read: ReadPolicy,
    pub write: WritePolicy,
    pub tools: ToolFilter,
    pub role: ModelRole,
    pub transitions: Vec<TransitionRule>,
}

impl TurnTypeSpec {}

/// Lifecycle status of a thread, surfaced to the UI/ACP. Replaces the former
/// `craft_flow::ChunkStatus` at the rendering boundary (design migration
/// table: "keep `ChunkStatus`-like glyphs in the UI as `ThreadStatus`").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    #[default]
    Queued,
    Running,
    NeedsReview,
    Blocked,
    Done,
}

impl ThreadStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            ThreadStatus::Queued => "·",
            ThreadStatus::Running => "▸",
            ThreadStatus::NeedsReview => "?",
            ThreadStatus::Blocked => "✗",
            ThreadStatus::Done => "✓",
        }
    }

    /// Parse from the serde (`rename_all = "snake_case"`) form; unknown
    /// strings yield `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "queued" => ThreadStatus::Queued,
            "running" => ThreadStatus::Running,
            "needs_review" => ThreadStatus::NeedsReview,
            "blocked" => ThreadStatus::Blocked,
            "done" => ThreadStatus::Done,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(TurnType::General ; "general")]
    #[test_case(TurnType::Scout ; "scout")]
    #[test_case(TurnType::Report ; "report")]
    #[test_case(TurnType::Verifier ; "verifier")]
    fn turn_type_round_trips(t: TurnType) {
        assert_eq!(TurnType::parse(t.as_str()), Some(t));
    }

    #[test]
    fn turn_type_parse_unknown_is_none() {
        assert!(TurnType::parse("nope").is_none());
        assert!(TurnType::parse("").is_none());
    }

    #[test]
    fn general_spec_reads_full_and_writes_verbatim() {
        let spec = TurnType::General.spec();
        assert!(spec.read.core.is_empty());
        assert!(spec.read.query_scope.is_empty());
        assert_eq!(spec.write.entry, EntryType::GeneralTurn);
        assert!(spec.write.guidance.is_none());
        assert!(matches!(spec.tools, ToolFilter::All));
        assert_eq!(spec.role, ModelRole::Default);
    }

    #[test]
    fn narrow_specs_declare_their_transitions_and_writes() {
        assert_eq!(TurnType::Execute.spec().write.entry, EntryType::Diff);
        assert_eq!(TurnType::Qa.spec().write.entry, EntryType::QaReport);
        assert_eq!(TurnType::Report.spec().write.entry, EntryType::Report);
        assert_eq!(
            TurnType::Integrator.spec().write.entry,
            EntryType::IntegrationCheckpoint
        );
        assert_eq!(
            TurnType::Verifier.spec().write.entry,
            EntryType::VerificationReport
        );
        // Each narrow type declares at least one transition.
        for t in [
            TurnType::Scout,
            TurnType::Tpm,
            TurnType::Plan,
            TurnType::Req,
            TurnType::Execute,
            TurnType::Review,
            TurnType::Qa,
            TurnType::Report,
            TurnType::Integrator,
            TurnType::Verifier,
        ] {
            assert!(
                !t.spec().transitions.is_empty(),
                "{t:?} should declare transitions"
            );
        }
    }

    #[test]
    fn all_transitions_are_self_report() {
        // No host-side objective gates: Review and QA run their own checks via
        // tools. Every declared transition is self-report.
        for t in [
            TurnType::General,
            TurnType::Scout,
            TurnType::Tpm,
            TurnType::Plan,
            TurnType::Req,
            TurnType::Execute,
            TurnType::Review,
            TurnType::Qa,
            TurnType::Report,
            TurnType::Integrator,
            TurnType::Verifier,
        ] {
            for rule in &t.spec().transitions {
                assert!(
                    matches!(rule.gate, Gate::SelfReport),
                    "{t:?} -> {} must be self-report",
                    rule.target.as_str()
                );
            }
        }
    }

    #[test_case(ThreadStatus::Queued, "·", "queued" ; "queued")]
    #[test_case(ThreadStatus::Running, "▸", "running" ; "running")]
    #[test_case(ThreadStatus::NeedsReview, "?", "needs_review" ; "needs_review")]
    #[test_case(ThreadStatus::Blocked, "✗", "blocked" ; "blocked")]
    #[test_case(ThreadStatus::Done, "✓", "done" ; "done")]
    fn thread_status_glyph(s: ThreadStatus, glyph: &str, name: &str) {
        assert_eq!(s.glyph(), glyph);
        assert_eq!(ThreadStatus::parse(name), Some(s));
    }

    #[test]
    fn thread_status_parse_unknown_is_none() {
        assert!(ThreadStatus::parse("nope").is_none());
    }
}
