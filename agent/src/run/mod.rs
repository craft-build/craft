//! The seams surfaces and the harness share: the approval responder, the
//! cancel tokens, the advisor, and the mode taxonomy.
//!
//! The request-view, guardrail, dedup, read-lifecycle and retry logic the
//! layers own moved next to them under `harness/` (docs/animist/11); the
//! multi-turn loop itself lives in the animist `Agent` behind
//! `harness::Conversation` (docs/animist/05). The advisor is driven by the
//! harness driver (09).

pub mod advisor;
pub mod approval;
pub mod cancel;
pub mod mode;

pub use approval::{Approval, ApprovalAsk, ApprovalResponder};
pub use cancel::{CancelFlag, CancelToken, cancel_channel};
pub use mode::{AgentMode, PLAN_WRITE_RESTRICTED};

use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[cfg(test)]
// The module is `run_tests` (a sibling inline `mod tests` exists below);
// the split harness lives in `tests/mod.rs`.
#[path = "tests/mod.rs"]
mod run_tests;
