//! Provider seam: the boundary between the TUI and an agent backend.
//!
//! The UI only ever sends [`Command`]s and renders [`AgentEvent`]s. A real
//! agent harness integrates by implementing [`Provider`]; [`mock::MockProvider`]
//! is the stand-in used for now.

pub mod mock;

use tokio::sync::mpsc;

/// Commands sent UI -> provider.
///
/// Payloads the mock doesn't consume yet are meant for real backends.
#[allow(dead_code)]
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
}

/// Agent lifecycle status, mirrors the prototype's STATUS_MAP.
#[allow(dead_code)] // `Failed` reserved for real backends
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
#[derive(Clone, Debug)]
pub enum AgentEvent {
    StatusChanged(Status),
    AssistantText(String),
    ToolCall(ToolCallData),
    PlanSet(Vec<PlanItem>),
    FilesSet(Vec<TouchedFile>),
    /// Human label for token usage, e.g. "44.8K (4%)".
    TokenUsage(String),
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
