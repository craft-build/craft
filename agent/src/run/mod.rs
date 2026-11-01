//! The run's supporting modules: request-view shaping, dedup, guardrails,
//! cancellation, the approval seam, and the mode taxonomy.
//!
//! The multi-turn loop itself moved to the animist `Agent` behind
//! `harness::Conversation` (docs/animist/05); what remains here are the
//! pieces every surface and layer still share. Recovery and overflow
//! handling return with their layers in 09.

pub mod approval;
pub mod cancel;
pub mod dedup;
// The doom tracker lives in the guardrail state from 07 on; the retry
// classification stays unwired until 09's recovery layer.
pub(crate) mod doom;
pub mod guardrails;
pub mod mode;
pub(crate) mod read_lifecycle;
pub mod recency;
#[allow(dead_code)]
mod retry;
pub mod view;

pub use approval::{Approval, ApprovalAsk, ApprovalResponder};
pub use cancel::{CancelFlag, CancelToken, cancel_channel};
pub use dedup::ToolDedupCache;
pub use guardrails::GuardrailState;
pub use mode::{AgentMode, PLAN_WRITE_RESTRICTED};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use retry::RetryCtx;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Floor for the output-token clamp, never applied above the configured
/// cap. Servers like vLLM reject `prompt + max_tokens > window` even when
/// the prompt alone fits; the floor keeps the clamp from trading that
/// overflow for a too-small budget (reference `MIN_OUTPUT_TOKENS`).
pub(crate) const MIN_OUTPUT_TOKENS: u64 = 4096;

/// Reduce the request's output cap to what remains of the context window.
/// `None` leaves the request alone (no window or no configured cap: the
/// provider picks its own). `prompt_tokens` should be the larger of the
/// chars/4 estimate and the last measured input count — the estimate is a
/// floor that skips the clamp on exactly the long sessions it exists for.
pub(crate) fn clamped_max_tokens(
    window: Option<u32>,
    prompt_tokens: u64,
    configured: Option<u64>,
) -> Option<u64> {
    let window = match window {
        Some(window) => u64::from(window),
        // No known window: leave the configured cap alone.
        None => return configured,
    };
    let cap = configured?;
    let remaining = window.saturating_sub(prompt_tokens);
    // The `min` keeps the floor from raising the cap over what was
    // configured, since the provider would reject a number it never offered.
    if cap > remaining {
        Some(remaining.max(MIN_OUTPUT_TOKENS).min(cap))
    } else {
        Some(cap)
    }
}

/// The session's compaction state shared with the request layer and the
/// compaction engine: the engine holds the lock across its stage awaits
/// (compaction runs between runs), so the handle is a tokio mutex and the
/// request layer's per-step updates take it briefly.
pub type SharedCompactionState = Arc<tokio::sync::Mutex<crate::compaction::CompactionState>>;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[cfg(test)]
// The module is `run_tests` (a sibling inline `mod tests` exists below);
// the split harness lives in `tests/mod.rs`.
#[path = "tests/mod.rs"]
mod run_tests;
