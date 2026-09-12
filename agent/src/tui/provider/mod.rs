//! Provider seam: the boundary between the TUI and an agent backend.
//!
//! The UI only ever sends [`Command`]s and renders [`AgentEvent`]s. The live
//! backend is [`live::CraftProvider`]; the scripted [`mock::MockProvider`] is
//! kept for seam and UI tests.

pub mod live;

#[cfg(test)]
pub mod mock;

use tokio::sync::mpsc;

/// Commands sent UI -> provider.
#[derive(Clone, Debug)]
pub enum Command {
    /// User submitted a message in the composer.
    SendMessage(String),
    /// User approved a pending diff (by tool-call id).
    Approve(String),
    /// User rejected a pending diff (by tool-call id).
    Reject(String),
    /// Esc: interrupt the running turn.
    Interrupt,
    /// Start over (new session): abort any turn and reset to the initial state.
    Reset,
    /// Conversation context was cleared by the user.
    Clear,
    /// Switch the provider/model used for subsequent turns.
    SelectModel { provider: String, model: String },
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
    Cmd,
    Muted,
    Success,
}

#[derive(Clone, Debug)]
pub struct ToolLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Clone, Debug)]
pub enum ToolKind {
    Read { path: String, summary: String },
    Grep { pattern: String, summary: String },
    Bash { cmd: String },
    Edit { path: String },
}

impl ToolKind {
    /// Read/Grep blocks can be collapsed; Bash/Edit are always expanded.
    pub fn collapsible(&self) -> bool {
        matches!(self, ToolKind::Read { .. } | ToolKind::Grep { .. })
    }
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
    /// Human label for token usage, e.g. "44.8K (4%)".
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
