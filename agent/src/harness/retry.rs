//! The retry substrate: budget bounds, the rotation seam, and the linear
//! backoff state machine, driven by the recovery layer behind `Agent`
//! hooks.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::provider::ModelHandle;

/// Timeouts are retried more patiently than other transient errors, but not
/// forever.
pub(crate) const MAX_TIMEOUT_RETRIES: u32 = 10;
/// Run-scoped bound on non-timeout transient retries (rate-limit, server,
/// transport): the last hop otherwise retries forever, which keeps a
/// headless run alive against a permanently failing provider.
pub(crate) const MAX_TRANSIENT_RETRIES: u32 = 8;
/// Run-wide ceiling shared by every model call's retries: without it, an
/// unbounded run could spend a full per-call budget on each turn forever.
pub(crate) const MAX_RUN_TRANSIENT_RETRIES: u32 = 16;
/// Cap on key rotations per failure streak so a hook that keeps
/// "succeeding" without changing the active key cannot hot-loop the
/// provider.
pub(crate) const MAX_ROTATIONS: u32 = 4;
// Linear backoff band and rotation pacing; shrunk under cfg(test) so the
// state machine's tests stay fast. The recovery layer drives them.
#[cfg(not(test))]
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);
#[cfg(not(test))]
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(8);
#[cfg(not(test))]
pub(crate) const ROTATION_DELAY: Duration = Duration::from_millis(500);
#[cfg(test)]
pub(crate) const RETRY_DELAY: Duration = Duration::from_millis(20);
#[cfg(test)]
pub(crate) const MAX_DELAY: Duration = Duration::from_millis(80);
#[cfg(test)]
pub(crate) const ROTATION_DELAY: Duration = Duration::from_millis(5);

/// Consulted when a rate limit exhausts the current API key; returns whether
/// a different key became active. The provider layer owns key pools; this
/// seam only decides retry-vs-advance.
pub type RotateKey = Arc<dyn Fn() -> bool + Send + Sync>;

/// Retry inputs the conversation supplies: the rotation hook and the fallback
/// model chain. Defaults (no hook, empty chain) reproduce single-model
/// behavior.
#[derive(Clone, Default)]
pub struct RetryCtx {
    pub rotate: Option<RotateKey>,
    pub fallbacks: Vec<ModelHandle>,
}

impl std::fmt::Debug for RetryCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetryCtx")
            .field("rotate", &self.rotate.is_some())
            .field("fallbacks", &self.fallbacks.len())
            .finish()
    }
}

/// Linear backoff counter: delay grows by [`RETRY_DELAY`] per attempt up to
/// [`MAX_DELAY`], and the wait lands in the upper half of that band (half +
/// jitter).
#[derive(Default)]
pub(crate) struct RetryState {
    attempt: u32,
}

impl RetryState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn next_delay(&mut self) -> (u32, Duration) {
        self.attempt += 1;
        let delay = RETRY_DELAY.saturating_mul(self.attempt).min(MAX_DELAY);
        let half = delay / 2;
        let jitter = Duration::from_nanos(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 % half.as_millis() as u64)
                .unwrap_or(0),
        );
        (self.attempt, half + jitter)
    }
}

/// Rebudge the request's output cap for a fallback hop:
/// never above the hop's own `max_output` or what remains of its window,
/// floored at [`super::MIN_OUTPUT_TOKENS`] exactly like the primary clamp.
/// `None` limits everywhere leave the request untouched.
pub(crate) fn hop_max_tokens(
    request_max_tokens: Option<u64>,
    hop: &ModelHandle,
    prompt_tokens: u64,
) -> Option<u64> {
    let max_output = hop.max_output().map(u64::from);
    let configured = match (request_max_tokens, max_output) {
        (Some(request), Some(hop_cap)) => Some(request.min(hop_cap)),
        (request, hop_cap) => request.or(hop_cap),
    };
    crate::harness::request::clamped_max_tokens(hop.context_length(), prompt_tokens, configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_linear_capped_and_jittered() {
        let mut retry = RetryState::new();
        let (attempt, delay) = retry.next_delay();
        assert_eq!(attempt, 1);
        assert!(delay >= RETRY_DELAY / 2 && delay <= RETRY_DELAY);
        for expected in [2u32, 3, 4, 5] {
            let (attempt, _) = retry.next_delay();
            assert_eq!(attempt, expected);
        }
        // The cap holds from attempt 4 on; the wait never exceeds it.
        for _ in 0..20 {
            let (_, delay) = retry.next_delay();
            assert!(delay <= MAX_DELAY, "delay {delay:?} exceeds cap");
            assert!(delay >= MAX_DELAY / 2);
        }
    }
}
