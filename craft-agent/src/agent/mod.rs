mod cache;
mod compaction;
pub(crate) mod compression_store;
mod dedup;
mod escalation;
pub mod findings_store;
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

pub use compaction::compact;
pub use findings_store::{FindingsStore, SharedFindingsStore, StoredFinding};
pub use history::History;
pub(crate) use instructions::is_instruction_file;
pub use instructions::{
    Instructions, LoadedInstructions, build_system_prompt, find_subdirectory_instructions,
    load_instruction_text, load_instructions,
};
pub use run::{Agent, AgentParams, AgentRunParams, RunOutcome};
