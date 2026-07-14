//! Async agent loop with tools.

pub mod agent;
pub mod cancel;
pub mod child_guard;
pub use child_guard::ChildGuard;
pub mod headless;
pub mod mcp;
pub use mcp::config::{McpConfigError, McpConfigErrors, McpServerInfo, McpServerStatus};
pub use mcp::protocol::PromptRole;
pub use mcp::{McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSnapshot, McpSnapshotReader};
pub(crate) mod task_set;
pub use agent::EmbedRequest;
pub use agent::EmbeddingService;
pub use agent::{
    Agent, AgentParams, AgentRunParams, DoomTracker, FindingsStore, History, Instructions,
    LoadedInstructions, RecoveryAction, RecoveryFailureKind, SharedDoomTracker,
    SharedFindingsStore, SharedMessages, StoredFinding, action_for, classify,
    classify_subagent_error, find_subdirectory_instructions, is_instruction_file,
};
pub use cancel::{CancelMap, CancelToken, CancelTrigger};
pub use craft_config::{AgentConfig, PermissionsConfig, ToolOutputLines};
pub mod checks;
pub mod command;
pub mod compression;
pub mod diff;
pub mod discovery;
pub mod permissions;
pub mod prompt;
pub mod recipe;
pub mod styleguide;
pub mod template;
pub mod tools;
pub use tools::ToolFilter;
pub mod types;
pub mod wiki;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use craft_providers::AgentError;
use craft_providers::Message;
pub use craft_providers::{ImageMediaType, ImageSource, ThinkingConfig};
pub use types::{
    AgentEvent, BatchProgressEvent, BatchToolEntry, BatchToolStatus, BufferSnapshot, Envelope,
    EventSender, Finding, GrepFileEntry, GrepLine, GrepMatchGroup, InstructionBlock,
    NO_FILES_FOUND, Priority, SharedBuf, SnapshotLine, SnapshotSpan, SpanStyle, SubagentInfo,
    ToolDoneEvent, ToolInput, ToolOutput, ToolStartEvent, TurnCompleteEvent,
};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum AgentMode {
    #[default]
    Build,
    Plan(PathBuf),
    /// Flow mode carries the active workstream id.
    Flow(String),
}

impl AgentMode {
    pub fn plan_path(&self) -> Option<&Path> {
        match self {
            Self::Plan(p) => Some(p),
            Self::Build | Self::Flow(_) => None,
        }
    }

    pub fn flow_workstream(&self) -> Option<&str> {
        match self {
            Self::Flow(id) => Some(id),
            Self::Build | Self::Plan(_) => None,
        }
    }
}

pub enum ExtractedCommand {
    Interrupt(AgentInput, u64),
    Compact(u64),
    Undo(u64),
}

pub trait InterruptSource: Send + Sync {
    fn poll(&self) -> Option<ExtractedCommand>;
}

pub mod hooks;
pub use hooks::{HookDecision, HookFuture, Hooks, ToolUseEvent};

#[derive(Clone)]
pub struct McpPromptRef {
    pub qualified_name: String,
    pub arguments: HashMap<String, String>,
}

#[derive(Default)]
pub struct AgentInput {
    pub message: String,
    pub mode: AgentMode,
    pub images: Vec<ImageSource>,
    pub preamble: Vec<Message>,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    pub prompt: Option<Box<McpPromptRef>>,
    pub goal: Option<String>,
    pub goal_criteria: Vec<String>,
    /// Flow mode only: resume a previously-failed run for this workstream
    /// instead of starting fresh. Ignored outside Flow mode.
    pub flow_resume: bool,
}
