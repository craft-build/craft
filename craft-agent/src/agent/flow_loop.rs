//! Flow mode types: progress events, terminal outcome, the approval payload,
//! and the between-turn `FlowAdvisor` with forced-transition authority.
//!
//! Flow mode is Build mode with a mutable `turn_type` and a typed log. The
//! agent starts `General` and runs the normal `Agent::run_loop`
//! (`craft-agent/src/agent/run.rs`). At turn boundaries it drains the last
//! `shift` tool call from the just-completed turn, runs it through
//! `transitions::resolve` against the current type's declared `TransitionRule`
//! set (with the Advisor's forced transition as the override), and shifts,
//! blocks, or rejects. The typed log commits one distilled entry per turn.
//! The pipeline shape (Scout, Plan, chunks, Integrator, Verifier) emerges
//! from the model's shift choices, not from a driver.
//!
//! There is no orchestrator here. The host (CLI `craft flow`, TUI
//! `do_flow_run`, ACP `headless::spawn_interactive`) drives Flow mode through
//! `Agent::run` like Build/Plan. The terminal `AgentEvent::Done` stop reason
//! is translated to a `FlowOutcome` by the host. `FlowProgress` events are
//! emitted from inside `run_loop` (at shift / cancel / advisor boundaries)
//! and forwarded over the channel the host supplies via `AgentParams`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::advisor::AdvisorSeverity;
use super::threads::ThreadManager;
use super::turn_type::TurnType;
use super::typed_log::{EntryType, ThreadHistory, ThreadId};
use crate::cancel::CancelToken;
use craft_storage::flow::FlowStore;

pub const FLOW_APPROVE_ANSWER: &str = "approved";
/// Sentinel the host sends through the approval channel to cancel a Flow run
/// at the goal-approval gate (recognized by the TUI's `do_flow_run`). Distinct
/// from `FLOW_APPROVE_ANSWER` so a revised goal cannot be confused with cancel.
pub const FLOW_CANCEL_ANSWER: &str = "__flow_cancel__";

/// Goal-approval decision carried into a Flow resume by the host.
#[derive(Debug, Clone)]
pub enum ApprovalPayload {
    /// User accepted the proposed goal verbatim.
    Approved,
    /// User revised the goal; the variant carries the new goal text.
    Revised(String),
}

/// Live progress events emitted from inside `Agent::run_loop` at Flow-mode
/// turn boundaries, and forwarded to the host over the
/// `AgentParams::flow_progress_tx` channel. The host (TUI FlowPanel, ACP
/// forwarder) consumes these to reflect live thread / turn-type state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FlowProgress {
    /// A turn boundary entered `turn_type` in `thread_id`. Emitted by
    /// `Agent::advance_turn_type` on every accepted shift (the initial
    /// `General` entry is not emitted — the run starts there implicitly).
    TurnTypeEntered {
        thread_id: String,
        turn_type: TurnType,
    },
    /// A new child thread was spawned under `parent_id`. Emitted by the
    /// `task` tool's Flow integration (follow-on; not yet wired).
    ThreadSpawn {
        thread_id: String,
        parent_id: String,
        turn_type: TurnType,
    },
    /// A thread exited and control returned to `returning_to`.
    ThreadExit {
        thread_id: String,
        returning_to: String,
    },
    /// A chunk's status changed (queued / running / done). Emitted by the
    /// `task` tool's Flow integration (follow-on).
    Chunk {
        id: String,
        title: String,
        status: super::turn_type::ThreadStatus,
        stage: Option<TurnType>,
        depends_on: Vec<String>,
        order: usize,
    },
    /// The TPM turn produced a goal doc and is awaiting host approval. The
    /// host re-prompts; on resume the agent re-derives the next shift from the
    /// persisted goal (plan §7: the approval gate is an ordinary turn
    /// boundary that ends the run with `StopReason::AwaitingGoalApproval`).
    GoalReady { goal_doc: String },
    /// The Verifier turn certified the run as complete.
    Done { verdict: String },
    /// The Verifier turn flagged the run as needing human review.
    NeedsReview { report: String },
    /// A turn failed unrecoverably.
    Failed { stage: TurnType, reason: String },
    /// The run was cancelled at a turn boundary.
    Cancelled,
    /// The between-turn `FlowAdvisor` issued an addressed note (the
    /// addressed-note exception channel, design §7).
    AdvisorNote {
        thread_id: String,
        addressed_to: String,
        severity: AdvisorSeverity,
        message: String,
    },
}

/// Terminal outcome the host derives from the `AgentEvent::Done` stop reason
/// at the end of a Flow run. Translation lives in the CLI/TUI/ACP entry
/// points, not in the agent.
#[derive(Debug, Clone)]
pub enum FlowOutcome {
    /// The TPM turn produced a goal and the run paused for approval.
    AwaitingGoalApproval { goal_doc: String },
    /// The Verifier turn certified completion.
    Done { verification_report: String },
    /// A turn failed.
    Failed { stage: TurnType, reason: String },
    /// The Verifier flagged the result as needing human review.
    NeedsReview { verification_report: String },
    /// The run was cancelled.
    Cancelled,
}

/// The between-turn Advisor's forced-transition verdict. The forced target is
/// always `General` (plan §12, locked): a forced transition re-enters the
/// parent thread with the full typed-log picture so it can re-evaluate,
/// bypassing objective gates only because `General` has no objective-gated
/// edges.
#[derive(Debug, Clone)]
pub struct ForcedTransition {
    pub note: String,
    pub severity: AdvisorSeverity,
}

/// Between-turn tree watcher with override power, called from
/// `Agent::run_flow_advisor` at the turn boundary (right before
/// `transitions::resolve`). This is the expensive advisor that sees the full
/// `ThreadHistory` + `ThreadManager` tree; the cheaper per-turn delta reviewer
/// (`advisor::review`, called from `Agent::run_advisor`) is separate and
/// always-on. They coexist (plan §12).
pub trait FlowAdvisor: Send + Sync {
    fn review<'a>(
        &'a self,
        history: Arc<std::sync::Mutex<ThreadHistory>>,
        tree: ThreadManager,
        thread_id: ThreadId,
        turn_type: TurnType,
    ) -> Pin<Box<dyn Future<Output = Option<ForcedTransition>> + Send + 'a>>;
}

/// Production default: a no-op Advisor. Tests inject a stub that forces.
pub struct NoopFlowAdvisor;

impl FlowAdvisor for NoopFlowAdvisor {
    fn review<'a>(
        &'a self,
        _history: Arc<std::sync::Mutex<ThreadHistory>>,
        _tree: ThreadManager,
        _thread_id: ThreadId,
        _turn_type: TurnType,
    ) -> Pin<Box<dyn Future<Output = Option<ForcedTransition>> + Send + 'a>> {
        Box::pin(async move { None })
    }
}

/// Record an Advisor addressed note in `thread_id`'s typed log (the
/// addressed-note exception channel, design §7). Returns the formatted note
/// text so the caller can emit a `FlowProgress::AdvisorNote` with the same
/// string. Shared between `Agent::run_flow_advisor` (root) and any future
/// per-chunk advisor wiring.
pub(crate) fn record_advisor_note(
    history: &Arc<std::sync::Mutex<ThreadHistory>>,
    thread_id: &ThreadId,
    forced: &ForcedTransition,
) -> String {
    let note = format!("[advisor:{}] {}", forced.severity.as_str(), forced.note);
    history.lock().unwrap_or_else(|e| e.into_inner()).append(
        thread_id.clone(),
        EntryType::AdvisorNote,
        &note,
    );
    note
}

/// The Flow state a host attaches to `AgentParams` when running in Flow mode.
/// Built once per workstream (CLI `craft flow`, TUI `do_flow_run`, ACP
/// `headless::spawn_interactive`) and reused across resume prompts for the
/// same workstream. Holds the typed log, the thread-tree manager, the
/// between-turn advisor, and the channel `Agent::run_loop` emits
/// `FlowProgress` events onto.
///
/// Build/Plan runs do not construct one; they leave the `AgentParams` Flow
/// fields `None`.
pub struct FlowRunState {
    pub thread_history: Arc<std::sync::Mutex<ThreadHistory>>,
    pub thread_manager: Arc<std::sync::Mutex<ThreadManager>>,
    pub advisor: Arc<dyn FlowAdvisor + Send + Sync>,
    pub progress_tx: flume::Sender<FlowProgress>,
    pub cancel: CancelToken,
}

impl FlowRunState {
    /// Open the typed log for `workstream_id` (resuming if it already exists),
    /// create a root `ThreadManager`, install the no-op production advisor,
    /// and create the progress channel. The host owns the matching
    /// `progress_rx` and the cancel trigger (see [`FlowRunState::split`]).
    pub fn open(
        store: Arc<FlowStore>,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
    ) -> Self {
        let project_id = project_id.into();
        let workstream_id = workstream_id.into();
        let thread_history = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            store,
            project_id,
            workstream_id.clone(),
        )));
        let thread_manager = Arc::new(std::sync::Mutex::new(ThreadManager::new(
            workstream_id,
            TurnType::General,
        )));
        let (progress_tx, _progress_rx) = flume::unbounded::<FlowProgress>();
        let (_cancel_trigger, cancel) = CancelToken::new();
        Self {
            thread_history,
            thread_manager,
            advisor: Arc::new(NoopFlowAdvisor),
            progress_tx,
            cancel,
        }
    }

    /// Open the state and return it together with the matching progress
    /// receiver and cancel trigger, so the host can forward events and cancel
    /// the run. The receiver drains `FlowProgress` events emitted from inside
    /// `Agent::run_loop`.
    pub fn split(
        store: Arc<FlowStore>,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
    ) -> (
        Self,
        flume::Receiver<FlowProgress>,
        crate::cancel::CancelTrigger,
    ) {
        let project_id = project_id.into();
        let workstream_id = workstream_id.into();
        let thread_history = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            store,
            project_id,
            workstream_id.clone(),
        )));
        let thread_manager = Arc::new(std::sync::Mutex::new(ThreadManager::new(
            workstream_id,
            TurnType::General,
        )));
        let (progress_tx, progress_rx) = flume::unbounded::<FlowProgress>();
        let (cancel_trigger, cancel) = CancelToken::new();
        (
            Self {
                thread_history,
                thread_manager,
                advisor: Arc::new(NoopFlowAdvisor),
                progress_tx,
                cancel,
            },
            progress_rx,
            cancel_trigger,
        )
    }

    /// Replace the no-op advisor with a custom one (tests inject a stub).
    pub fn with_advisor(mut self, advisor: Arc<dyn FlowAdvisor + Send + Sync>) -> Self {
        self.advisor = advisor;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_constants_are_stable() {
        assert_eq!(FLOW_APPROVE_ANSWER, "approved");
        assert_eq!(FLOW_CANCEL_ANSWER, "__flow_cancel__");
    }

    #[test]
    fn flow_progress_round_trips_through_serde() {
        let p = FlowProgress::TurnTypeEntered {
            thread_id: "ws".into(),
            turn_type: TurnType::Scout,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: FlowProgress = serde_json::from_str(&json).unwrap();
        match back {
            FlowProgress::TurnTypeEntered {
                thread_id,
                turn_type,
            } => {
                assert_eq!(thread_id, "ws");
                assert_eq!(turn_type, TurnType::Scout);
            }
            other => panic!("unexpected variant {other:?}"),
        }
    }

    #[test]
    fn flow_progress_advisor_note_round_trips() {
        let p = FlowProgress::AdvisorNote {
            thread_id: "ws".into(),
            addressed_to: "ws".into(),
            severity: AdvisorSeverity::Blocker,
            message: "drift".into(),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: FlowProgress = serde_json::from_str(&json).unwrap();
        match back {
            FlowProgress::AdvisorNote {
                severity, message, ..
            } => {
                assert_eq!(severity, AdvisorSeverity::Blocker);
                assert_eq!(message, "drift");
            }
            other => panic!("unexpected variant {other:?}"),
        }
    }
}
