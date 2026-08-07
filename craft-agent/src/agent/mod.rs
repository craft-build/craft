pub mod advisor;
mod cache;
mod compaction;
pub(crate) mod compression_store;
mod dedup;
mod doom;
mod escalation;
pub mod findings_store;
pub mod flow_index;
pub mod flow_loop;
mod format;
mod guardrails;
mod history;
mod inplace_edit;
mod instructions;
mod judge;
pub(crate) mod memory_extraction;
mod read_lifecycle;
pub(crate) mod recovery;
pub(crate) mod retrieve;
mod run;
mod snapshot;
mod streaming;
pub mod tool_dispatch;
mod transitions;
pub(crate) mod trust;
mod ttsr;
pub mod turn_type;
pub mod typed_log;
mod validation;
pub(crate) mod vcc;
pub(crate) mod vcc_recall;

pub(crate) mod threads;

mod embed_types;
pub use embed_types::EmbedRequest;

mod semantic;
pub use semantic::EmbeddingService;
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    semantic::cosine_similarity(a, b)
}

pub use compaction::compact;
pub use doom::{DoomTracker, SharedDoomTracker};
pub use findings_store::{FindingsStore, SharedFindingsStore, StoredFinding};
pub use flow_loop::{
    ApprovalPayload, FLOW_APPROVE_ANSWER, FLOW_CANCEL_ANSWER, FlowOutcome, FlowProgress,
    FlowRunState,
};
pub use history::{
    History, HistorySnapshot, SharedMessages, UNAVAILABLE_RESULT, close_dangling_tool_calls,
};
pub use instructions::{
    Instructions, LoadedInstructions, build_system_prompt, find_subdirectory_instructions,
    is_instruction_file, load_instruction_text, load_instructions,
};
pub use recovery::{
    RecoveryAction, RecoveryFailureKind, action_for, classify, classify_subagent_error,
};
pub use run::{
    Agent, AgentParams, AgentRunParams, estimate_message_tokens, resolve_compaction_model,
};
pub use turn_type::{ThreadStatus, TurnType};
