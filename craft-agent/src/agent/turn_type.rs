//! Turn types: the four-property behavior profile for one atomic turn.
//!
//! A [`TurnType`] is fully specified by what it reads, what it writes, what it
//! can do, and what can run after it (design §1). Phase 1 populates only
//! [`TurnType::General`]; the ten narrow variants exist as enum members but
//! [`TurnType::spec`] hands back a stub "not yet enabled in Flow mode" spec for
//! them. Phase 2 fills the narrow specs (read/write/tools/role/transitions)
//! from the migrated schemas and the `prompts/flow/*.md` templates.
//!
//! Build and Plan modes both resolve to [`TurnType::General`] and behave
//! exactly as before (design §0): General's turn is the existing
//! `Agent::turn` body, unchanged. Flow mode in Phase 1 also runs
//! General-in-root-thread; the pipeline is gone and returns in Phase 2 as one
//! shape the turn-typed loop happens to produce.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use craft_config::model_roles::ModelRole;

use super::typed_log::EntryType;
use crate::tools::ToolFilter;

pub mod schema;
pub mod templates;

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

    /// Resolve the four-property spec for this turn type. `gates` supplies the
    /// injectable objective gates (the `GateFn` seam, plan Risks: "gates are
    /// injectable; tests inject stubs"); production passes real cargo-backed
    /// gates, tests pass stubs. [`General`] ignores `gates` (it has no
    /// objective-gated edges).
    pub fn spec(self, gates: &GateSet) -> TurnTypeSpec {
        match self {
            TurnType::General => TurnTypeSpec {
                read: ReadPolicy::full(),
                write: WritePolicy::verbatim(),
                tools: ToolFilter::All,
                role: ModelRole::Default,
                // General is the entry point. It declares self-report edges to
                // the root-level pipeline stages so the model can shift into
                // them as the task demands (Scout is the canonical first
                // shift; the others support resume / direct entry). The
                // per-chunk types (Req/Execute/Review/Qa/Report) are not
                // reachable from General — they live under spawned child
                // threads (the `task` tool integration, plan §6).
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
                    schema: None,
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowScout,
                transitions: vec![TransitionRule {
                    target: TurnType::Tpm,
                    action: ThreadAction::Advance,
                    gate: Gate::SelfReport,
                }],
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
                    schema: Some(schema::goal_doc()),
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
                    schema: Some(schema::plan_doc()),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowPlan,
                transitions: vec![
                    TransitionRule {
                        target: TurnType::Req,
                        action: ThreadAction::Spawn,
                        gate: Gate::SelfReport,
                    },
                    TransitionRule {
                        target: TurnType::Integrator,
                        action: ThreadAction::Advance,
                        gate: Gate::SelfReport,
                    },
                ],
            },
            TurnType::Req => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![CoreRead {
                        entry: EntryType::Plan,
                        level: ThreadLevel::Parent,
                    }],
                    query_scope: vec![
                        QueryScope {
                            entry: EntryType::CodebaseContext,
                            level: ThreadLevel::Parent,
                        },
                        QueryScope {
                            entry: EntryType::ResearchNotes,
                            level: ThreadLevel::Parent,
                        },
                    ],
                },
                write: WritePolicy {
                    entry: EntryType::Requirement,
                    schema: Some(schema::requirement_doc()),
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowReq,
                transitions: vec![TransitionRule {
                    target: TurnType::Execute,
                    action: ThreadAction::Advance,
                    gate: Gate::SelfReport,
                }],
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
                    schema: None,
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
                        gate: Gate::Objective(Arc::clone(&gates.compile)),
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
                    schema: Some(schema::review_report()),
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
                    schema: Some(schema::qa_report()),
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
                        gate: Gate::Objective(Arc::clone(&gates.test)),
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
                    schema: None,
                },
                tools: ToolFilter::All,
                role: ModelRole::FlowQa,
                transitions: vec![TransitionRule {
                    target: TurnType::Report,
                    action: ThreadAction::Exit,
                    gate: Gate::SelfReport,
                }],
            },
            TurnType::Integrator => TurnTypeSpec {
                read: ReadPolicy {
                    core: vec![
                        CoreRead {
                            entry: EntryType::Report,
                            level: ThreadLevel::Parent,
                        },
                        CoreRead {
                            entry: EntryType::Plan,
                            level: ThreadLevel::Own,
                        },
                    ],
                    query_scope: Vec::new(),
                },
                write: WritePolicy {
                    entry: EntryType::IntegrationCheckpoint,
                    schema: Some(schema::integration_checkpoint()),
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
                        gate: Gate::Objective(Arc::clone(&gates.drift)),
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
                    schema: Some(schema::verification_report()),
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
                ],
            },
        }
    }
}

/// Injectable objective gates (plan Risks: "gates are injectable (the `GateFn`
/// seam), tests inject stubs"). Each gate is `Ok(())` to pass, `Err(reason)` to
/// block the self-proposed transition. Production fills these with
/// cargo-backed checks; tests inject stubs so the loop is fully exercisable
/// without a live toolchain.
#[derive(Clone)]
pub struct GateSet {
    /// Execute -> Review: the diff compiles. Default runs `cargo check`.
    pub compile: GateFn,
    /// QA -> Report: tests pass. Default runs `cargo nextest`/`cargo test`.
    pub test: GateFn,
    /// Integrator -> Verifier: no drift across merged chunks. Default runs the
    /// post-merge build/test gate (migrated from `craft_flow::default_post_merge_gate`).
    pub drift: GateFn,
}

impl GateSet {
    /// Production gates backed by cargo. Each fails closed on a missing
    /// toolchain.
    pub fn cargo() -> Self {
        Self {
            compile: Arc::new(|| {
                let out = std::process::Command::new("cargo")
                    .args(["check", "--all-features", "--workspace"])
                    .output()
                    .map_err(|e| format!("compile gate failed to run: {e}"))?;
                if out.status.success() {
                    Ok(())
                } else {
                    Err("compile gate failed: `cargo check` did not pass".to_string())
                }
            }),
            test: Arc::new(|| {
                let test = std::process::Command::new("cargo")
                    .args(["nextest", "run", "--all-features", "--workspace"])
                    .output()
                    .or_else(|_| {
                        std::process::Command::new("cargo")
                            .args(["test", "--all-features", "--workspace"])
                            .output()
                    })
                    .map_err(|e| format!("test gate failed to run: {e}"))?;
                if test.status.success() {
                    Ok(())
                } else {
                    Err("test gate failed: tests did not pass".to_string())
                }
            }),
            drift: Arc::new(super::transitions::default_post_merge_gate),
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
    /// Optional JSON Schema validating the committed entry; migrated from the
    /// former `craft_flow::schema` shapes in Phase 2.
    pub schema: Option<Value>,
}

impl WritePolicy {
    /// General writes itself back verbatim as a `GeneralTurn` entry.
    pub fn verbatim() -> Self {
        Self {
            entry: EntryType::GeneralTurn,
            schema: None,
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

/// Injectable objective gate (mirrors the former `PostMergeGateFn` seam in
/// `craft_flow::FlowParams`). `Ok(())` passes; `Err(reason)` blocks the
/// self-proposed transition (design §2).
pub type GateFn = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// Whether a transition's proposal is honored. Objective checks run where
/// they exist (Execute->Review's diff-compiles, QA->Report's tests-pass);
/// self-report is the fallback where nothing objective is available (design
/// §2, §10).
#[derive(Clone)]
pub enum Gate {
    Objective(GateFn),
    SelfReport,
}

/// One declared legal next step: a target type, a thread action, and the gate
/// that decides whether the turn's proposal is honored (design §2).
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

    fn stub_gates() -> GateSet {
        GateSet {
            compile: super::super::transitions::always_pass(),
            test: super::super::transitions::always_pass(),
            drift: super::super::transitions::always_pass(),
        }
    }

    #[test]
    fn general_spec_reads_full_and_writes_verbatim() {
        let spec = TurnType::General.spec(&stub_gates());
        assert!(spec.read.core.is_empty());
        assert!(spec.read.query_scope.is_empty());
        assert_eq!(spec.write.entry, EntryType::GeneralTurn);
        assert!(spec.write.schema.is_none());
        assert!(matches!(spec.tools, ToolFilter::All));
        assert_eq!(spec.role, ModelRole::Default);
    }

    #[test]
    fn narrow_specs_declare_their_transitions_and_writes() {
        let gates = stub_gates();
        assert_eq!(TurnType::Execute.spec(&gates).write.entry, EntryType::Diff);
        assert_eq!(TurnType::Qa.spec(&gates).write.entry, EntryType::QaReport);
        assert_eq!(TurnType::Report.spec(&gates).write.entry, EntryType::Report);
        assert_eq!(
            TurnType::Integrator.spec(&gates).write.entry,
            EntryType::IntegrationCheckpoint
        );
        assert_eq!(
            TurnType::Verifier.spec(&gates).write.entry,
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
                !t.spec(&gates).transitions.is_empty(),
                "{t:?} should declare transitions"
            );
        }
    }

    #[test]
    fn execute_to_review_uses_compile_gate() {
        let gates = GateSet {
            compile: super::super::transitions::always_fail("no compile"),
            test: super::super::transitions::always_pass(),
            drift: super::super::transitions::always_pass(),
        };
        let spec = TurnType::Execute.spec(&gates);
        let review_rule = spec
            .transitions
            .iter()
            .find(|r| r.target == TurnType::Review)
            .expect("Execute declares -> Review");
        match &review_rule.gate {
            Gate::Objective(g) => assert!(g().is_err(), "compile gate should fail"),
            Gate::SelfReport => panic!("Execute->Review must be objective"),
        }
    }

    #[test]
    fn qa_to_report_uses_test_gate() {
        let gates = GateSet {
            compile: super::super::transitions::always_pass(),
            test: super::super::transitions::always_fail("tests fail"),
            drift: super::super::transitions::always_pass(),
        };
        let spec = TurnType::Qa.spec(&gates);
        let report_rule = spec
            .transitions
            .iter()
            .find(|r| r.target == TurnType::Report)
            .expect("Qa declares -> Report");
        match &report_rule.gate {
            Gate::Objective(g) => assert!(g().is_err(), "test gate should fail"),
            Gate::SelfReport => panic!("Qa->Report must be objective"),
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
