//! The seams surfaces and the harness share: the approval responder, the
//! cancel tokens, the advisor, and the mode taxonomy.

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
