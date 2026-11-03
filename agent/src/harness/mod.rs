//! The conversation harness: one animist `Agent` per session behind a
//! long-lived [`Conversation`], driven turn by turn by
//! [`driver::run_turn`](driver::run_turn).
//!
//! Policy lives in the layer chain. Surfaces own a
//! conversation, subscribe to its event sink once, and never run a loop of
//! their own.

mod advisor;
pub mod approval;
pub mod cancel;
mod continuation;
pub mod conversation;
pub(crate) mod dedup;
pub(crate) mod doom;
pub mod driver;
pub mod events;
mod guardrail;
pub(crate) mod guardrails;
pub mod mode;
pub mod persist;
mod persist_layer;
mod policy;
pub(crate) mod read_lifecycle;
mod recovery;
pub mod replay;
pub(crate) mod request;
pub(crate) mod retry;
mod snapshot_layer;
#[cfg(test)]
mod tests;
pub(crate) mod view;

pub use approval::{Approval, ApprovalAsk, ApprovalResponder};
pub use cancel::{CancelFlag, CancelToken, cancel_channel};
pub use conversation::{Conversation, ConversationBuilder, MaxTurns, TurnSettings};
pub use driver::{TurnOutcome, compact_between_runs, run_turn};
pub use events::{CraftEvent, DoneReason, Event, EventSink, EventStream};
#[cfg(test)]
pub(crate) use guardrail::GuardrailLayer;
pub use mode::{AgentMode, PLAN_WRITE_RESTRICTED};
pub use persist::Persister;
pub use policy::PolicyLayerInputs;
pub use recovery::ReauthHook;
pub use replay::{ReplayItem, transcript};

use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
