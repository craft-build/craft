//! Provider seam: the boundary between the TUI and an agent backend.
//!
//! The UI only ever sends [`Command`]s and renders [`AgentEvent`]s. The live
//! backend is [`live::CraftProvider`]; the scripted [`mock::MockProvider`] is
//! kept for seam and UI tests.

pub mod cards;
pub mod live;

#[cfg(test)]
pub mod mock;

use tokio::sync::mpsc;

use crate::run::AgentMode;

/// Commands sent UI -> provider.
#[derive(Clone, Debug)]
pub enum Command {
    /// User submitted a message in the composer, in the given mode.
    SendMessage(String, AgentMode),
    /// Bang-mode (`!` / `!!`): run a shell command directly, outside the
    /// model loop. `visible` runs enter history as an `I ran: …` user
    /// message; hidden runs never reach the model.
    Shell { command: String, visible: bool },
    /// User approved a pending diff (by tool-call id). `always` persists an
    /// allow rule to the project's `permissions.toml` instead of the session.
    Approve { id: String, always: bool },
    /// User rejected a pending diff (by tool-call id). `always` persists a
    /// deny rule to the project's `permissions.toml` instead of the session.
    Reject { id: String, always: bool },
    /// Answered the permission-prompt overlay (by tool-call id) with the
    /// full scope-negotiated decision (F.5).
    AnswerPermission {
        id: String,
        answer: crate::permissions::PermissionAnswer,
    },
    /// Esc: interrupt the running turn.
    Interrupt,
    /// Start over (new session): abort any turn and reset to the initial state.
    Reset,
    /// Conversation context was cleared by the user.
    Clear,
    /// Roll back the last committed snapshot session (`/undo`).
    Undo,
    /// Switch the provider/model used for subsequent turns.
    SelectModel { provider: String, model: String },
    /// Toggle LLM auto-review of permissions (`/auto-review`).
    ToggleAutoReview,
    /// Refresh the per-model usage snapshot shown by `/usage`.
    GetUsage,
    /// Fetch the provider-side usage quota shown by `/usage` (F.5); the
    /// answer streams back as [`AgentEvent::UsageQuota`].
    FetchUsage,
    /// Force-run every armed compaction stage on the current history
    /// (`/compact`), regardless of fill thresholds.
    Compact,
    /// Replace the session with a persisted one, by session id (`/sessions`).
    LoadSession { id: String },
}

/// Agent lifecycle status, mirrors the prototype's STATUS_MAP.
#[allow(dead_code)] // `WaitingApproval` is exercised by the test mock only
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Thinking,
    Running,
    WaitingApproval,
    Done,
    Failed,
}

/// Color tone for badges / file statuses.
#[allow(dead_code)] // full palette kept for real backends
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Danger,
    Info,
    Neutral,
}

/// Semantic kind of a rendered tool-output line (diff/grep/bash body lines).
#[allow(dead_code)] // `Cmd` reserved for real backends echoing commands
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Add,
    Del,
    Context,
    /// Hunk separator inside a diff body (renders as a dim `...`).
    Gap,
    Cmd,
    Muted,
    Success,
}

#[derive(Clone, Debug)]
pub struct ToolLine {
    pub kind: LineKind,
    pub text: String,
    /// Before-side line number for diff bodies (0 = no gutter, e.g. added
    /// lines and non-diff tools).
    pub nr: usize,
    /// Char ranges of word-level changes, from pairing removed/added lines.
    pub emph: Vec<(usize, usize)>,
}

impl ToolLine {
    pub fn new(kind: LineKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            nr: 0,
            emph: Vec::new(),
        }
    }
}

impl Default for ToolLine {
    fn default() -> Self {
        Self::new(LineKind::Context, String::new())
    }
}

#[derive(Clone, Debug)]
pub enum ToolKind {
    Read { path: String, summary: String },
    Grep { pattern: String, summary: String },
    Bash { cmd: String },
    Edit { path: String, summary: String },
}

impl ToolKind {
    /// Read/Grep/Edit blocks can be collapsed; Bash is always expanded.
    pub fn collapsible(&self) -> bool {
        matches!(
            self,
            ToolKind::Read { .. } | ToolKind::Grep { .. } | ToolKind::Edit { .. }
        )
    }

    /// Body-truncation hints for the card, ported from the reference's
    /// `RenderHintsRegistry`: how many body lines an unexpanded card shows
    /// and which end survives. Command-style outputs (Bash cards also cover
    /// generic tools) keep the tail — the interesting part of a log is its
    /// end; file-shaped tools keep the head.
    pub fn body_hints(&self) -> (usize, Keep) {
        const HEAD_CAP: usize = 40;
        const TAIL_CAP: usize = 30;
        match self {
            ToolKind::Bash { .. } => (TAIL_CAP, Keep::Tail),
            _ => (HEAD_CAP, Keep::Head),
        }
    }
}

/// Which end of a long tool output to keep when truncating its card body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    Head,
    Tail,
}

#[derive(Clone, Debug)]
pub struct ToolCallData {
    pub id: String,
    pub kind: ToolKind,
    pub lines: Vec<ToolLine>,
    /// The card awaits an approve/reject decision. Only backends that stage
    /// edits set this; the live agent applies filesystem tools inline.
    pub awaiting_approval: bool,
}

/// One selectable model on a configured provider, as shown in the model menu.
#[derive(Clone, Debug)]
pub struct ModelChoice {
    /// Owning provider name, sent back in [`Command::SelectModel`].
    pub provider: String,
    /// Model identifier, sent back in [`Command::SelectModel`].
    pub model: String,
    /// Display label (catalog name, falling back to the model id).
    pub label: String,
    /// Display label of the owning provider.
    pub provider_label: String,
}

#[derive(Clone, Debug)]
pub struct PlanItem {
    pub label: String,
    pub done: bool,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct TouchedFile {
    pub path: String,
    pub status: String,
    pub tone: Tone,
}

/// One per-model row of a usage/stats table: token totals and what they
/// were billed (`None` when the model is unpriced).
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRow {
    pub model: String,
    pub tokens: u64,
    pub cost: Option<f64>,
}

/// Lifecycle of the provider quota fetch backing `/usage` (F.5), mirroring
/// the reference `UsageFetchState` (Loading / Ready / Unsupported / Error).
/// `Idle` is the pre-first-fetch state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsageFetchState {
    Idle,
    Loading,
    Ready(crate::providers::ProviderUsage),
    /// The provider exposes no programmatic usage endpoint.
    Unsupported,
    Error(String),
}

/// Events streamed provider -> UI.
#[allow(dead_code)] // `PlanSet` has no real source yet; exercised by the test mock
#[derive(Clone, Debug)]
pub enum AgentEvent {
    StatusChanged(Status),
    /// A complete assistant message (notes, errors, one-shot replies).
    AssistantText(String),
    /// Streamed chunk of the assistant reply currently being written.
    AssistantDelta(String),
    /// Streamed chunk of the model's reasoning (thinking) text.
    ReasoningDelta(String),
    /// Closes the open streaming message; the next delta starts a new one.
    AssistantEnd,
    /// Tool cards are merged by id: emit once with empty lines when the call
    /// starts and again with the finished body when its result arrives.
    ToolCall(ToolCallData),
    PlanSet(Vec<PlanItem>),
    FilesSet(Vec<TouchedFile>),
    /// Human label for token usage, e.g. "44.8K/1M (4%)".
    TokenUsage(String),
    /// Model catalog and current selection; sent at startup and after a
    /// [`Command::SelectModel`] switch is confirmed.
    CatalogSet {
        models: Vec<ModelChoice>,
        current: usize,
    },
    /// Working directory and branch shown in the sidebar.
    SessionInfo {
        cwd: String,
        branch: String,
    },
    /// Per-model usage of the current session (`/usage`); pushed after each
    /// completed run and in answer to [`Command::GetUsage`].
    UsageSnapshot(Vec<UsageRow>),
    /// Provider-side quota answer for `/usage` (F.5). The last answer is
    /// kept on the App across modal close/reopen.
    UsageQuota(UsageFetchState),
    /// A tone-tagged system line (retry/auth/compaction/doom/guardrail
    /// status) rendered as scrollback text rather than a card. Notices are
    /// provider-facing only; they never enter the model-visible history.
    Notice {
        tone: Tone,
        text: String,
    },
    /// Auto-review status for the tool call with `id`. Merged by id onto the
    /// tool card but rendered as a line *under* it, so the card itself stays
    /// focused on the tool's own output. Emitted once while reviewing and
    /// again with the verdict, replacing the earlier line in place.
    AutoReview {
        id: String,
        tone: Tone,
        text: String,
    },
    /// The session was replaced by a persisted one (`/sessions`); carries
    /// the user/assistant text transcript for display rebuild.
    SessionLoaded {
        messages: Vec<LoadedMessage>,
    },
    /// A gated tool call is parked on the user's decision: opens the
    /// permission-prompt overlay (F.5). `files`/`commands` are display
    /// context for the form; `scopes` are the permission-engine scopes.
    PermissionRequest {
        id: String,
        tool: String,
        scopes: Vec<String>,
        files: Vec<String>,
        commands: Vec<String>,
    },
    /// The pending permission request resolved (answered, cancelled, or
    /// timed out); closes the overlay if the id matches.
    PermissionResolved {
        id: String,
    },
}

/// One displayable message of a loaded session's transcript.
#[derive(Clone, Debug)]
pub enum LoadedMessage {
    User(String),
    Assistant(String),
}

/// An agent backend. `start` consumes the provider and returns the two halves
/// of the UI <-> provider channel pair.
pub trait Provider {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    );
}
