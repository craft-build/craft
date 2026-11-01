//! The conversation harness: one animist `Agent` per session behind a
//! long-lived [`Conversation`], driven turn by turn by
//! [`driver::run_turn`](driver::run_turn).
//!
//! Policy lives in the layer chain (policy → guardrail → request →
//! snapshot; persistence and recovery join in later prompts). Surfaces own a
//! conversation, subscribe to its event sink once, and never run a loop of
//! their own.

mod continuation;
pub mod conversation;
pub mod driver;
pub mod events;
mod guardrail;
pub mod persist;
mod persist_layer;
mod policy;
mod recovery;
pub(crate) mod request;
mod snapshot_layer;
#[cfg(test)]
mod tests;

pub use conversation::{Conversation, ConversationBuilder, MaxTurns, TurnSettings};
pub use driver::{TurnOutcome, compact_between_runs, run_turn};
pub use events::{CraftEvent, DoneReason, Event, EventSink, EventStream};
#[cfg(test)]
pub(crate) use guardrail::GuardrailLayer;
pub use persist::Persister;
pub use policy::PolicyLayerInputs;
pub use recovery::ReauthHook;
