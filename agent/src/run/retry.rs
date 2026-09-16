//! Streaming retry state machine: bounded backoff for transient provider
//! failures, API-key rotation, and model-chain fallback. Ported from the
//! reference's `craft-agent/src/agent/streaming.rs` (`stream_with_retry`) and
//! `craft-providers/src/retry.rs` (`RetryState`); the reference's
//! loop-resident logic is re-expressed as a wrapper around this repo's
//! [`super::stream::run_model_stream`].
//!
//! Recovery policy per error kind (see [`super::stream::ErrorKind`]):
//! rate-limited errors first try key rotation (resetting the backoff), then
//! the model fallback chain, then plain backoff; server/transport errors
//! back off; timeouts back off capped at [`MAX_TIMEOUT_RETRIES`]; overflow,
//! content-policy, and fatal errors pass straight through to the caller.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rig_core::completion::{CompletionModel, CompletionRequest};

use super::CancelToken;
use super::Event;
use super::stream::{ErrorKind, StreamFailure, TurnOutput, run_model_stream};

/// Timeouts are retried more patiently than other transient errors, but not
/// forever (reference `MAX_TIMEOUT_RETRIES`).
pub(crate) const MAX_TIMEOUT_RETRIES: u32 = 10;
#[cfg(not(test))]
const RETRY_DELAY: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const MAX_DELAY: Duration = Duration::from_secs(8);
// Keep retry-machine behavior tests fast without changing the math they
// assert against (linear growth, cap, jitter band).
#[cfg(test)]
const RETRY_DELAY: Duration = Duration::from_millis(20);
#[cfg(test)]
const MAX_DELAY: Duration = Duration::from_millis(80);

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
    pub fallbacks: Vec<crate::providers::DynamicModel>,
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

/// Run one model stream under the retry machine. The primary model is tried
/// first; `fallbacks` are advanced to in order when key rotation fails or is
/// unavailable, and once the chain is spent the last hop keeps retrying.
/// Context overflow is not retried here — the run loop's recovery owns it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry<M>(
    primary: &M,
    fallbacks: &[crate::providers::DynamicModel],
    rotate: Option<&RotateKey>,
    request: &CompletionRequest,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Result<TurnOutput, StreamFailure>
where
    M: CompletionModel + Clone,
{
    let mut retry = RetryState::new();
    let mut next_fallback = 0usize;
    loop {
        let result = if next_fallback == 0 {
            run_model_stream(primary, request.clone(), cancel, emit).await
        } else {
            run_model_stream(&fallbacks[next_fallback - 1], request.clone(), cancel, emit).await
        };
        let failure = match result {
            Ok(output) => return Ok(output),
            Err(failure) => failure,
        };
        let StreamFailure::Error { kind, message } = &failure else {
            // Cancellation carries the streamed text out to the caller.
            return Err(failure);
        };
        let kind = *kind;
        if !kind.is_retryable() {
            return Err(failure);
        }
        let mut advanced = false;
        if kind.should_rotate_key() {
            if let Some(rotate) = rotate
                && rotate()
            {
                retry = RetryState::new();
                advanced = true;
            } else if let Some(hop) = fallbacks.get(next_fallback) {
                next_fallback += 1;
                retry = RetryState::new();
                advanced = true;
                emit(Event::Retry {
                    attempt: 1,
                    message: format!(
                        "key rotation exhausted; failing over to {}",
                        hop.label().unwrap_or("fallback model")
                    ),
                    delay_ms: 0,
                });
            }
        }
        if !advanced {
            let (attempt, delay) = retry.next_delay();
            if kind == ErrorKind::Timeout && attempt > MAX_TIMEOUT_RETRIES {
                return Err(failure);
            }
            emit(Event::Retry {
                attempt,
                message: kind.retry_message(message),
                delay_ms: delay.as_millis() as u64,
            });
            let mut cancel_rx = cancel.subscribe();
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                changed = cancel_rx.changed() => {
                    // A dropped flag disables the branch; only a real bump cancels.
                    if changed.is_ok() {
                        return Err(StreamFailure::Cancelled {
                            streamed: String::new(),
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    fn request() -> CompletionRequest {
        crate::edge::to_request(&[], &[], None, None, None)
    }

    fn rate_limit_turn() -> Vec<MockStreamEvent> {
        vec![MockStreamEvent::Error(
            rig_core::test_utils::MockError::provider("rate limit exceeded, retry after 30s"),
        )]
    }

    fn timeout_turn() -> Vec<MockStreamEvent> {
        vec![MockStreamEvent::Error(
            rig_core::test_utils::MockError::provider("request timed out"),
        )]
    }

    fn done_turn(text: &str) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]
    }

    fn retry_events(events: &[Event]) -> Vec<(u32, String, u64)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Retry {
                    attempt,
                    message,
                    delay_ms,
                } => Some((*attempt, message.clone(), *delay_ms)),
                _ => None,
            })
            .collect()
    }

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
        // The 8s cap holds from attempt 4 on; the wait never exceeds it.
        for _ in 0..20 {
            let (_, delay) = retry.next_delay();
            assert!(delay <= MAX_DELAY, "delay {delay:?} exceeds cap");
            assert!(delay >= MAX_DELAY / 2);
        }
    }

    #[tokio::test]
    async fn retryable_error_is_retried_until_success() {
        let model =
            MockCompletionModel::from_stream_turns(vec![rate_limit_turn(), done_turn("recovered")]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(&model, &[], None, &request(), &cancel, &|e| {
            events.lock().unwrap().push(e)
        })
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "recovered");
        let retries = retry_events(&events.lock().unwrap());
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].0, 1, "attempt is 1-based");
        assert_eq!(retries[0].1, "Rate limited");
    }

    #[tokio::test]
    async fn fatal_error_surfaces_without_retry() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![MockStreamEvent::Error(
            rig_core::test_utils::MockError::provider("invalid api key"),
        )]]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(&model, &[], None, &request(), &cancel, &|e| {
            events.lock().unwrap().push(e)
        })
        .await
        .unwrap_err();
        assert!(matches!(
            failure,
            StreamFailure::Error {
                kind: ErrorKind::Fatal,
                ..
            }
        ));
        assert!(retry_events(&events.lock().unwrap()).is_empty());
    }

    #[tokio::test]
    async fn timeout_errors_stop_after_the_cap() {
        let turns: Vec<Vec<MockStreamEvent>> = std::iter::repeat_n(timeout_turn(), 12).collect();
        let model = MockCompletionModel::from_stream_turns(turns);
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(&model, &[], None, &request(), &cancel, &|e| {
            events.lock().unwrap().push(e)
        })
        .await
        .unwrap_err();
        assert!(matches!(
            failure,
            StreamFailure::Error {
                kind: ErrorKind::Timeout,
                ..
            }
        ));
        // 10 retries spent (12 attempts total: 1 initial + 10 retries + the
        // 12th surfaces the error), never an 11th.
        assert_eq!(retry_events(&events.lock().unwrap()).len(), 10);
    }

    #[tokio::test]
    async fn successful_rotation_resets_the_backoff() {
        let model =
            MockCompletionModel::from_stream_turns(vec![rate_limit_turn(), done_turn("ok")]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let rotations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rotations_for_hook = std::sync::Arc::clone(&rotations);
        let rotate: RotateKey = Arc::new(move || {
            rotations_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        });
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(&model, &[], Some(&rotate), &request(), &cancel, &|e| {
            events.lock().unwrap().push(e)
        })
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "ok");
        // Rotation advances without a backoff event.
        assert!(retry_events(&events.lock().unwrap()).is_empty());
        assert_eq!(rotations.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rotation_exhaustion_advances_the_fallback_chain() {
        let primary =
            MockCompletionModel::from_stream_turns(vec![rate_limit_turn(), done_turn("late")]);
        // Fallback succeeds on its first attempt.
        let fallback = crate::providers::DynamicModel::wrap(
            Some("fallback-model"),
            MockCompletionModel::from_stream_turns(vec![done_turn("fallback")]),
        );
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(&primary, &[fallback], None, &request(), &cancel, &|e| {
            events.lock().unwrap().push(e)
        })
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "fallback");
        let retries = retry_events(&events.lock().unwrap());
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].0, 1, "chain hop resets the attempt counter");
        assert!(retries[0].1.contains("failing over"));
        assert_eq!(retries[0].2, 0, "chain hop does not wait");
    }

    #[tokio::test]
    async fn empty_chain_keeps_retrying_the_primary() {
        // Two failures, then success — single-model behavior unchanged.
        let model = MockCompletionModel::from_stream_turns(vec![
            rate_limit_turn(),
            rate_limit_turn(),
            done_turn("ok"),
        ]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(&model, &[], None, &request(), &cancel, &|e| {
            events.lock().unwrap().push(e)
        })
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "ok");
        assert_eq!(retry_events(&events.lock().unwrap()).len(), 2);
    }

    #[tokio::test]
    async fn cancel_during_backoff_carries_no_streamed_text() {
        // The stream never succeeds, so every attempt waits in backoff.
        let turns: Vec<Vec<MockStreamEvent>> = std::iter::repeat_n(rate_limit_turn(), 50).collect();
        let model = MockCompletionModel::from_stream_turns(turns);
        let (flag, cancel) = crate::run::cancel_channel();
        let task = tokio::spawn(async move {
            stream_with_retry(&model, &[], None, &request(), &cancel, &|_| {}).await
        });
        // Cancel within the first backoff window (10–20ms under test delays).
        tokio::time::sleep(Duration::from_millis(5)).await;
        flag.set(true);
        match task.await.unwrap().unwrap_err() {
            StreamFailure::Cancelled { streamed } => assert!(streamed.is_empty()),
            other => panic!("expected cancellation, got {other:?}"),
        }
    }
}
