//! CraftProvider: the TUI's real backend, driving the shared run loop the
//! ACP server uses.
//!
//! The session owns one long-lived [`Conversation`] built at loop start
//! (and rebuilt on `/sessions` load): a session-long forwarder task maps
//! its event stream into TUI [`AgentEvent`]s, and each turn drives
//! `harness::run_turn` on it. Edit-family tools are gated behind the UI's
//! approve/reject seam via the policy layer's approval hook: no workspace
//! mutation runs without an explicit user decision.

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

use arc_swap::ArcSwap;
use tokio::sync::{Mutex, Notify, mpsc, oneshot};

use crate::config::Config;
use crate::harness::Conversation;
use crate::permissions::{PermissionAnswer, PermissionManager};
use crate::harness;
use crate::tools::{QuestionAnswer, Workspace};
use animist::agent::Cancellation;
use animist::provider::ModelInfo;

use bootstrap::catalog_choices;
use turn::RunSignals;
use usage_recorder::UsageLedger;

use super::cards::Files;
use super::{AgentEvent, ModelChoice};
// Re-exported for descendant modules, which import these through `super::`.
#[allow(unused_imports)]
pub(super) use super::{Command, LoadedMessage, Provider, Status, Tone, UsageFetchState, cards};

/// Render an error and its sources as one client-facing message.
pub(super) fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

/// Tool-call parking slots shared by the approval/question gates and the
/// command loop that answers them, kept separate from the session state so
/// the gates can be built with a conversation before the state exists.
#[derive(Default)]
pub(super) struct ParkSlots {
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pub(super) pending_approval: Option<(String, oneshot::Sender<PermissionAnswer>)>,
    /// Parked `question` tool call, by question-request id.
    pub(super) pending_question: Option<(String, oneshot::Sender<QuestionAnswer>)>,
}

/// The gates' handle on the parking slots.
pub(super) type SharedPark = Arc<Mutex<ParkSlots>>;

/// Session shared between the command loop and the (single) running turn.
/// The conversation (history, model, tools, layers, persister) lives here;
/// everything else the surfaces need is reached through it.
struct SessionState {
    thinking: crate::thinking::ThinkingConfig,
    conversation: Conversation,
    /// Turn-level sync with the session forwarder: the streamed-reply
    /// flags and the terminal-Done generation each run settles on.
    signals: RunSignals,
    /// Per-model usage totals and the cost ledger they feed.
    usage: UsageLedger,
}

#[cfg(test)]
impl SessionState {
    /// A scripted-conversation session for tests that exercise the loop
    /// seams without a provider: no persister, empty history, no ledger.
    pub(super) fn for_tests() -> Self {
        let workspace = Workspace::new(std::env::temp_dir()).expect("workspace");
        let (sink, _stream) = crate::harness::EventSink::new();
        let info = crate::testing::model_info_with("main", Some(128_000), None);
        let model = crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &crate::testing::ScriptedProvider::new(vec![info.clone()]),
            info,
        ));
        let conversation = crate::harness::ConversationBuilder::new(
            &crate::config::Config::parse("").expect("default config"),
            &std::env::temp_dir().display().to_string(),
            "",
            model,
            workspace
                .registry(&animist::history::History::new())
                .expect("registry"),
            sink,
        )
        .build()
        .expect("conversation builds");
        Self {
            thinking: crate::thinking::ThinkingConfig::default(),
            conversation,
            signals: RunSignals::new(),
            usage: UsageLedger::default(),
        }
    }
}

/// The loop-lifetime pieces every session (re)build and every turn needs:
/// config, the clean workspace, the seams (gates' parking slots, the
/// per-turn cancellation cell), and the UI channels. Immutable once the
/// command loop starts.
#[derive(Clone)]
pub(super) struct SessionKit {
    pub(super) config: Arc<Config>,
    pub(super) workspace: Workspace,
    pub(super) instructions_text: String,
    pub(super) permissions: Arc<PermissionManager>,
    /// The session's per-turn cancellation cell: each turn stores its
    /// cancellation here, and the gates/tools linked to the cell observe
    /// whatever turn is current. `Command::Interrupt` cancels through it.
    pub(super) cancel_cell: Arc<ArcSwap<Cancellation>>,
    /// Per-tool-call subagent cancellation, shared with every
    /// session's task-tool launcher; `Command::CancelSubagent` fires here.
    pub(super) subagent_cancels: Arc<harness::cancel::CancelMap<String>>,
    /// The gates' parking slots; answers land here from the command loop.
    pub(super) park: SharedPark,
    pub(super) evt_tx: mpsc::UnboundedSender<AgentEvent>,
    pub(super) files: Files,
    /// The current model selection, shared with the forwarder (context
    /// labels) and every turn (model resolution).
    pub(super) selection: Arc<ArcSwap<Selection>>,
    pub(super) cwd: String,
    pub(super) state_dir: Option<crate::storage::StateDir>,
}

/// Immutable-by-loop shared state for the command loop's handlers: the
/// session, its connections, and the pieces every arm reaches for. Loop
/// locals that mutate per command (`current_turn`, `selection`) stay in
/// the loop and are passed as `&mut`.
struct LoopCtx {
    state: Arc<Mutex<SessionState>>,
    kit: SessionKit,
    /// Bang-mode bookkeeping: run ids, in-flight cancel
    /// triggers, and visible-run results waiting for the next turn.
    shell: Arc<std::sync::Mutex<crate::tui::shell::ShellState>>,
    catalogs: BTreeMap<String, Vec<ModelInfo>>,
    catalog: Arc<animist::catalog::Catalog>,
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

    /// Flat model menu rows across all usable providers, with the current
    /// selection's index.
    fn catalog_choices(&self, selection: &Selection) -> (Vec<ModelChoice>, usize) {
        catalog_choices(&self.kit.config, &self.catalog, &self.catalogs, selection)
    }
}

#[derive(Clone)]
pub(super) struct Selection {
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
    catalogs: BTreeMap<String, Vec<ModelInfo>>,
    /// The session's models.dev catalog: display names, vision claims,
    /// and any later model selection all read it.
    catalog: Arc<animist::catalog::Catalog>,
    /// Discovery problems surfaced to the user as notes at startup.
    notes: Vec<String>,
    selection: Selection,
    cwd_label: String,
    branch: String,
    /// Resume-latest-by-cwd at startup (`craft --continue`).
    resume_latest: bool,
    /// Resume this specific session id at startup (`craft -s/--session`);
    /// takes precedence over `resume_latest`.
    resume_session: Option<String>,
    /// MCP client handle: installed on the workspace before the
    /// first turn and cloned into the app for the `/mcp` screen.
    mcp: Option<crate::mcp::McpHandle>,
    /// Receiver for events the MCP manager emits through `McpEvents`
    /// (log-notification notices); drained by `start`'s provider loop.
    mcp_evt_rx: mpsc::UnboundedReceiver<AgentEvent>,
    /// Whether the session discovers custom slash commands (`--no-commands`
    /// clears it); read by the TUI through the `Provider` trait.
    custom_commands: bool,
    /// Whether the session starts in plan mode (`--mode plan`,
    /// `--permission-mode plan`); read by the TUI through the `Provider`
    /// trait.
    starts_in_plan_mode: bool,
}

impl CraftProvider {
    /// Resume this directory's most recent session when the command loop
    /// starts (`craft --continue`).
    pub fn with_resume_latest(mut self, yes: bool) -> Self {
        self.resume_latest = yes;
        self
    }

    /// Resume a specific session id at startup (`craft -s/--session`).
    /// Takes precedence over [`Self::with_resume_latest`].
    pub fn with_session(mut self, id: Option<String>) -> Self {
        self.resume_session = id;
        self
    }

    /// Prepend first-run notes (auto-detected providers) to the startup
    /// notes so they surface before discovery problems.
    pub fn with_startup_notes(mut self, mut notes: Vec<String>) -> Self {
        notes.append(&mut self.notes);
        self.notes = notes;
        self
    }

    /// Apply permission flags before the session's first turn: `--yolo`
    /// bypasses every check, `-A/--auto-review` starts auto-review on.
    /// `--yolo` also disables the command sandbox.
    pub fn with_permission_flags(self, yolo: bool, auto_review: bool) -> Self {
        self.permissions.set_yolo(yolo);
        self.permissions.set_auto_review(auto_review);
        if yolo {
            self.workspace
                .set_sandbox_policy(crate::sandbox::SandboxPolicy::off());
        }
        self
    }

    /// Install the CLI tool policy (`--allowed-tools` preapprovals,
    /// `--disallowed-tools` denies) before the session's first turn. The
    /// rules are process-lifetime: they never persist into the session or
    /// `permissions.bml`.
    pub fn with_cli_tool_rules(self, rules: Vec<crate::permissions::PermissionRule>) -> Self {
        self.permissions.add_cli_rules(rules);
        self
    }

    /// Toggle custom slash-command discovery (`--no-commands`); default on.
    pub fn with_custom_commands(mut self, yes: bool) -> Self {
        self.custom_commands = yes;
        self
    }

    /// Set the session's initial mode from the CLI (`--mode`, or
    /// `--permission-mode plan`); plan starts the TUI in plan mode.
    pub fn with_initial_mode(mut self, mode: crate::cli::CliMode) -> Self {
        self.starts_in_plan_mode = matches!(mode, crate::cli::CliMode::Plan);
        self
    }
}
