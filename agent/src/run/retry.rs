//! The retry taxonomy: budget bounds, the rotation seam, and the linear
//! backoff state machine. The streaming retry driver went with the old run
//! loop; 09's recovery layer rewires this substrate behind `Agent` layers.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::provider::ModelHandle;

/// Timeouts are retried more patiently than other transient errors, but not
/// forever (reference `MAX_TIMEOUT_RETRIES`).
pub(crate) const MAX_TIMEOUT_RETRIES: u32 = 10;
/// Run-scoped bound on non-timeout transient retries (rate-limit, server,
/// transport): the last hop otherwise retries forever, which keeps a
/// headless run alive against a permanently failing provider.
pub(crate) const MAX_TRANSIENT_RETRIES: u32 = 8;
/// Run-wide ceiling shared by every model call's retry machine: without it,
/// an unbounded run could spend a full per-call budget on each turn forever.
pub(crate) const MAX_RUN_TRANSIENT_RETRIES: u32 = 16;

/// Run-scoped tally of transient retries, charged by every retryable
/// non-timeout failure across all of a run's `stream_with_retry` calls.
#[derive(Default, Clone)]
pub(crate) struct TransientBudget {
    spent: Arc<std::sync::atomic::AtomicU32>,
}

impl TransientBudget {
    fn charge(&self) -> u32 {
        self.spent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }
}
/// Cap on key rotations per run so a hook that keeps "succeeding" without
/// changing the active key cannot hot-loop the provider.
const MAX_ROTATIONS: u32 = 4;
// Linear backoff band and rotation pacing (reference `RETRY_DELAY`,
// `MAX_DELAY`, `ROTATION_DELAY`); shrunk under cfg(test) so the state
// machine's tests stay fast. 09's recovery layer drives them.
#[cfg(not(test))]
const RETRY_DELAY: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const MAX_DELAY: Duration = Duration::from_secs(8);
#[cfg(not(test))]
const ROTATION_DELAY: Duration = Duration::from_millis(500);
#[cfg(test)]
const RETRY_DELAY: Duration = Duration::from_millis(20);
#[cfg(test)]
const MAX_DELAY: Duration = Duration::from_millis(80);
#[cfg(test)]
const ROTATION_DELAY: Duration = Duration::from_millis(5);

/// Consulted when a rate limit exhausts the current API key; returns whether
/// a different key became active. The provider layer owns key pools (model
/// registry, Phase 5); this seam only decides retry-vs-advance.
pub type RotateKey = Arc<dyn Fn() -> bool + Send + Sync>;

/// Retry inputs the run loop supplies: the rotation hook and the fallback
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
/// jitter), matching the reference `RetryState::next_delay`.
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
