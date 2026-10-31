//! The conversation harness: one animist `Agent` per session behind a
//! long-lived [`Conversation`], driven turn by turn by
//! [`driver::run_turn`](driver::run_turn).
//!
//! Policy lives in the layer chain (policy → guardrail → request →
//! snapshot; persistence and recovery join in later prompts). Surfaces own a
//! conversation, subscribe to its event sink once, and never run a loop of
//! their own.

pub mod conversation;
pub mod driver;
pub mod events;
mod guardrail;
pub mod persist;
mod persist_layer;
mod policy;
pub(crate) mod request;
mod snapshot_layer;
#[cfg(test)]
mod tests;

pub use conversation::{Conversation, ConversationBuilder, MaxTurns, TurnSettings};
pub use driver::{TurnOutcome, run_turn};
pub use events::{CraftEvent, DoneReason, Event, EventSink, EventStream};
pub use persist::Persister;
