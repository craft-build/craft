//! CraftProvider: the TUI's real backend, driving the shared run loop the
//! ACP server uses.
//!
//! Turn semantics mirror `acp::run_turn`: a per-turn provider rebuild, the
//! configured compaction stages ahead of the model call, cancellation through
//! the run loop's `CancelToken`, and history committed only on a successful
//! run. Edit-family tools are gated behind the UI's approve/reject seam via
//! the dispatch `BeforeExecute` hook: no workspace mutation runs without an
//! explicit user decision.

mod approval;
mod bootstrap;
mod commands;
mod mcp_request;
mod question;
mod resume;
mod turn;
mod usage_recorder;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, mpsc};

use crate::config::Config;
use crate::permissions::PermissionManager;
use crate::providers::CatalogModel;
use crate::run;
use crate::tools::Workspace;

use bootstrap::catalog_choices;
use usage_recorder::UsageLedger;

use super::cards::Files;
use super::{AgentEvent, ModelChoice};
// Re-exported for descendant modules (bootstrap/commands/resume were split
// out of the old live.rs and import these through `super::`).
#[allow(unused_imports)]
pub(super) use super::{Command, LoadedMessage, Provider, Status, Tone, UsageFetchState, cards};

/// Render an error and its sources as one client-facing message.
pub(super) fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

/// Session shared between the command loop and the (single) running turn.
#[derive(Default)]
struct SessionState {
    history: Vec<crate::history::Message>,
    /// Shared with the run loop for in-run overflow recovery.
    compaction: crate::run::SharedCompactionState,
    /// Session-wide tool dedup cache, shared by the dispatcher and cleared
    /// by the compaction engine.
    dedup: crate::run::SharedDedupCache,
    /// Session-wide guardrail counters, shared by the dispatcher and reset
    /// by the compaction engine.
    guardrails: crate::run::SharedGuardrails,
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pending_approval: Option<(
        String,
        tokio::sync::oneshot::Sender<crate::permissions::PermissionAnswer>,
    )>,
    /// Parked `question` tool call, by question-request id (A.5).
    pending_question: Option<(
        String,
        tokio::sync::oneshot::Sender<crate::tools::QuestionAnswer>,
    )>,
    /// Per-model usage totals and the cost ledger they feed.
    usage: UsageLedger,
    /// Persisted session (history + usage), the same store headless uses.
    /// `None` when the state dir is unavailable: the run is not persisted.
    store: Option<crate::headless::SessionStore>,
}

impl SessionState {
    /// Link the compaction state to this session's dedup cache so a
    /// compaction run clears it. Persistence is opt-in via
    /// [`Self::with_store`] so tests never touch the state dir.
    fn linked() -> Self {
        let dedup = crate::run::shared_cache();
        let guardrails = crate::run::shared_guardrails();
        Self {
            compaction: crate::runtime::new_compaction_state(dedup.clone(), guardrails.clone()),
            dedup,
            guardrails,
            usage: UsageLedger::open(),
            ..Self::default()
        }
    }

    /// Bind a freshly minted persisted session; its id also names this
    /// session's `cost.jsonl` records. `dir` is the resolved state dir;
    /// `None` disables persistence.
    fn with_store(
        mut self,
        dir: Option<&crate::storage::StateDir>,
        cwd: &str,
        model_spec: &str,
    ) -> Self {
        let session_ref = crate::id::SessionRef::generate();
        self.usage = self.usage.with_session_id(session_ref.id().to_string());
        self.store = dir.and_then(|dir| {
            crate::headless::SessionStore::open_in(dir.clone(), session_ref, cwd, model_spec).ok()
        });
        self
    }
}

/// Immutable-by-loop shared state for the command loop's handlers: the
/// session, its connections, and the pieces every arm reaches for. Loop
/// locals that mutate per command (`current_turn`, `selection`) stay in
/// the loop and are passed as `&mut`.
struct LoopCtx {
    state: Arc<Mutex<SessionState>>,
    files: Files,
    cancel_flag: run::CancelFlag,
    /// Per-tool-call subagent cancellation (task 96): shared with every
    /// turn's task-tool launcher; `Command::CancelSubagent` fires here.
    subagent_cancels: Arc<run::cancel::CancelMap<String>>,
    /// Bang-mode bookkeeping (task 96): run ids, in-flight cancel
    /// triggers, and visible-run results waiting for the next turn.
    shell: Arc<std::sync::Mutex<crate::tui::shell::ShellState>>,
    permissions: Arc<PermissionManager>,
    config: Arc<Config>,
    workspace: Workspace,
    instructions_text: String,
    catalogs: BTreeMap<String, Vec<CatalogModel>>,
    snapshots: crate::snapshot::SnapshotManager,
    state_dir: Option<crate::storage::StateDir>,
    cwd: String,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    /// Signals the loop when a running turn settles, so messages queued
    /// behind it drain even with no further user command.
    wake: Arc<Notify>,
}

impl LoopCtx {
    /// The current "provider/model" spec, for pricing and the session
    /// header.
    fn model_spec(selection: &Selection) -> String {
        format!("{}/{}", selection.provider, selection.model)
    }

    /// Flat model menu rows across all usable providers, with the
    /// current selection's index.
    fn catalog_choices(&self, selection: &Selection) -> (Vec<ModelChoice>, usize) {
        catalog_choices(&self.catalogs, selection)
    }
}

#[derive(Clone)]
struct Selection {
    provider: String,
    model: String,
    context_length: Option<u32>,
}

/// The live TUI backend: configured providers, discovered model catalogs, and
/// a workspace-scoped agent at `cwd`.
pub struct CraftProvider {
    config: Arc<Config>,
    workspace: Workspace,
    /// Instruction files (AGENTS.md and friends) discovered at startup;
    /// appended to the system prompt and shared with tool injection.
    instructions: crate::instructions::Instructions,
    /// Permission rule engine: persistent `permissions.bml` rules, session
    /// grants, and per-tool defaults; consulted by the approval gate.
    permissions: Arc<PermissionManager>,
    /// Per-provider discovered models, config key order.
    catalogs: BTreeMap<String, Vec<CatalogModel>>,
    /// Discovery problems surfaced to the user as notes at startup.
    notes: Vec<String>,
    selection: Selection,
    cwd_label: String,
    branch: String,
    /// F.3 resume-latest-by-cwd at startup (`craft --continue`).
    resume_latest: bool,
    /// Resume this specific session id at startup (`craft -s/--session`,
    /// G.1); takes precedence over `resume_latest`.
    resume_session: Option<String>,
    /// MCP client handle (B.11): installed on the workspace before the
    /// first turn and cloned into the app for the `/mcp` screen.
    mcp: Option<crate::mcp::McpHandle>,
    /// Receiver for events the MCP manager emits through `McpEvents`
    /// (log-notification notices); drained by `start`'s provider loop.
    mcp_evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
}

impl CraftProvider {
    /// Resume this directory's most recent session when the command loop
    /// starts (F.3 resume-latest-by-cwd, `craft --continue`).
    pub fn with_resume_latest(mut self, yes: bool) -> Self {
        self.resume_latest = yes;
        self
    }

    /// Resume a specific session id at startup (`craft -s/--session`, G.1).
    /// Takes precedence over [`Self::with_resume_latest`].
    pub fn with_session(mut self, id: Option<String>) -> Self {
        self.resume_session = id;
        self
    }

    /// Prepend first-run notes (G.6 auto-detected providers) to the startup
    /// notes so they surface before discovery problems.
    pub fn with_startup_notes(mut self, mut notes: Vec<String>) -> Self {
        notes.append(&mut self.notes);
        self.notes = notes;
        self
    }

    /// Apply G.1 permission flags before the session's first turn: `--yolo`
    /// bypasses every check, `-A/--auto-review` starts auto-review on.
    pub fn with_permission_flags(self, yolo: bool, auto_review: bool) -> Self {
        self.permissions.set_yolo(yolo);
        self.permissions.set_auto_review(auto_review);
        self
    }
}
