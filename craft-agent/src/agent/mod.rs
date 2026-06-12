mod cache;
mod compaction;
pub(crate) mod compression_store;
mod dedup;
mod doom;
mod escalation;
pub mod findings_store;
mod guardrails;
mod history;
mod instructions;
mod read_lifecycle;
pub(crate) mod retrieve;
mod run;
mod snapshot;
mod streaming;
pub mod tool_dispatch;
pub(crate) mod trust;
mod validation;

mod embed_types;
pub use embed_types::EmbedRequest;

#[cfg(feature = "onnx")]
mod semantic;
#[cfg(feature = "onnx")]
pub use semantic::EmbeddingService;
#[cfg(feature = "onnx")]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    semantic::cosine_similarity(a, b)
}

pub use compaction::compact;
pub use doom::{DoomTracker, SharedDoomTracker};
pub use findings_store::{FindingsStore, SharedFindingsStore, StoredFinding};
pub use history::History;
pub(crate) use instructions::is_instruction_file;
pub use instructions::{
    Instructions, LoadedInstructions, build_system_prompt, find_subdirectory_instructions,
    load_instruction_text, load_instructions,
};
pub use run::{Agent, AgentParams, AgentRunParams, RunOutcome};
