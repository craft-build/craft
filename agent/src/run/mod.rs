//! The run loop's supporting modules: request-view shaping, tool dispatch,
//! dedup, guardrails, cancellation, and the mode taxonomy.
//!
//! The multi-turn loop itself moved to the animist `Agent` behind
//! `harness::Conversation` (docs/animist/05); what remains here are the
//! pieces every surface and layer still share. Recovery, doom-loop, and
//! overflow handling return with their layers in 07–09.

pub mod cancel;
pub mod dedup;
pub mod dispatch;
// The doom tracker and the retry classification are the recovery layers'
// substrate, unwired until 07 (doom) and 09 (retry) land.
#[allow(dead_code)]
pub(crate) mod doom;
pub mod guardrails;
pub mod mode;
pub(crate) mod read_lifecycle;
pub mod recency;
#[allow(dead_code)]
mod retry;
pub mod view;

pub use cancel::{CancelFlag, CancelToken, cancel_channel};
pub use dedup::{SharedDedupCache, ToolDedupCache, shared_cache};
pub use dispatch::{
    AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome, ToolDispatch,
};
pub use guardrails::{SharedGuardrails, shared_guardrails};
pub use mode::{AgentMode, PLAN_WRITE_RESTRICTED};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use retry::RetryCtx;

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

/// The session's compaction state shared with the request layer: surfaces
/// and layers read and write the same estimator/effectiveness state (08
/// drives it).
pub type SharedCompactionState = Arc<std::sync::Mutex<crate::compaction::CompactionState>>;

#[cfg(test)]
// The module is `run_tests` (a sibling inline `mod tests` exists below);
// the split harness lives in `tests/mod.rs`.
#[path = "tests/mod.rs"]
mod run_tests;
