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

use crate::provider::ModelHandle;

use super::CancelToken;
use super::Event;
use super::stream::{ErrorKind, ProviderRequest, StreamFailure, TurnOutput, run_model_stream};

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
#[cfg(not(test))]
const RETRY_DELAY: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const MAX_DELAY: Duration = Duration::from_secs(8);
#[cfg(not(test))]
const ROTATION_DELAY: Duration = Duration::from_millis(500);
// Keep retry-machine behavior tests fast without changing the math they
// assert against (linear growth, cap, jitter band).
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

/// Sleep for `delay` unless cancellation wins first. A flag set before
/// this call cancels immediately; a dropped setting half does not.
async fn wait_cancellable(delay: Duration, cancel: &CancelToken) -> Result<(), StreamFailure> {
    if cancel.cancelled() {
        return Err(StreamFailure::Cancelled {
            streamed: String::new(),
        });
    }
    let mut cancel_rx = cancel.subscribe();
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        changed = cancel_rx.changed() => {
            if changed.is_ok() {
                Err(StreamFailure::Cancelled {
                    streamed: String::new(),
                })
            } else {
                Ok(())
            }
        }
    }
}

/// Rebudge the request's output cap for a fallback hop: never above the
/// hop's own `max_output` or what remains of its window, floored at
/// [`super::MIN_OUTPUT_TOKENS`] exactly like the primary clamp. `None`
/// limits everywhere leave the request untouched.
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
    super::clamped_max_tokens(hop.context_length(), prompt_tokens, configured)
}

/// Run one model stream under the retry machine. The primary model is tried
/// first; `fallbacks` are advanced to in order when key rotation fails or is
/// unavailable, and once the chain is spent the last hop keeps retrying.
/// Context overflow is not retried here — the run loop's recovery owns it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_with_retry(
    primary: &ModelHandle,
    fallbacks: &[ModelHandle],
    rotate: Option<&RotateKey>,
    budget: &TransientBudget,
    request: &ProviderRequest,
    prompt_tokens: u64,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Result<(TurnOutput, Option<String>), StreamFailure> {
    let mut retry = RetryState::new();
    let mut next_fallback = 0usize;
    let mut transient_failures = 0u32;
    let mut rotations = 0u32;
    loop {
        if cancel.cancelled() {
            return Err(StreamFailure::Cancelled {
                streamed: String::new(),
            });
        }
        // Fresh clone per attempt: the pristine `request` is never mutated,
        // so a later hop (or the primary) never inherits another model's
        // rebudgeted cap. Only fallback hops are rebudgeted; the primary
        // keeps the cap the run loop already clamped for it.
        let mut dispatch_request = request.clone();
        if next_fallback > 0 {
            let hop = &fallbacks[next_fallback - 1];
            if let Some(cap) = hop_max_tokens(
                request.options.max_output_tokens.map(u64::from),
                hop,
                prompt_tokens,
            ) {
                dispatch_request.options.max_output_tokens =
                    Some(cap.min(u64::from(u32::MAX)) as u32);
            }
        }
        let result = if next_fallback == 0 {
            run_model_stream(primary, dispatch_request.clone(), cancel, emit).await
        } else {
            run_model_stream(
                &fallbacks[next_fallback - 1],
                dispatch_request.clone(),
                cancel,
                emit,
            )
            .await
        };
        let failure = match result {
            Ok(output) => {
                let served = if next_fallback == 0 {
                    None
                } else {
                    Some(fallbacks[next_fallback - 1].model.id.clone())
                };
                return Ok((output, served));
            }
            Err(failure) => failure,
        };
        let StreamFailure::Error {
            kind,
            message,
            provider_retried,
        } = &failure
        else {
            // Cancellation carries the streamed text out to the caller.
            return Err(failure);
        };
        let kind = *kind;
        if !kind.is_retryable() {
            return Err(failure);
        }
        // The provider already spent its own retries on this failure
        // (Bedrock 429/503 backoff): same-model backoff must not stack on
        // it, so recovery advances straight to the fallback chain.
        let provider_retried = *provider_retried;
        transient_failures += 1;
        if kind != ErrorKind::Timeout
            && (transient_failures > MAX_TRANSIENT_RETRIES
                || budget.charge() > MAX_RUN_TRANSIENT_RETRIES)
        {
            return Err(failure);
        }
        let mut advanced = false;
        if kind.should_rotate_key() || provider_retried {
            // A provider-retried failure never rotates: the provider's own
            // retries exhausted the current credentials' patience, and the
            // remedy is a different model, not another key on the same one.
            if !provider_retried
                && rotations < MAX_ROTATIONS
                && let Some(rotate) = rotate
                && rotate()
            {
                rotations += 1;
                retry = RetryState::new();
                advanced = true;
                emit(Event::Retry {
                    attempt: rotations,
                    message: "rotated API key; retrying".to_owned(),
                    delay_ms: ROTATION_DELAY.as_millis() as u64,
                });
                // Even a successful rotation waits briefly: a hook that
                // returns true without changing the key must not hot-loop.
                wait_cancellable(ROTATION_DELAY, cancel).await?;
            } else {
                // Advance past hops whose window the prompt cannot fit
                // (bounded by the chain's length), then fail over to the
                // first fitting one. A fully skipped chain leaves
                // `advanced` unset, so the current model keeps its bounded
                // backoff instead of calling a hop that cannot fit.
                let mut cursor = next_fallback;
                while let Some(hop) = fallbacks.get(cursor) {
                    cursor += 1;
                    if let Some(window) = hop
                        .context_length()
                        .filter(|w| prompt_tokens >= u64::from(*w))
                    {
                        emit(Event::Retry {
                            attempt: 1,
                            message: format!(
                                "skipping {}: prompt (~{prompt_tokens} tokens) exceeds its context window ({window})",
                                hop.model.id.as_str(),
                            ),
                            delay_ms: 0,
                        });
                        continue;
                    }
                    next_fallback = cursor;
                    retry = RetryState::new();
                    advanced = true;
                    emit(Event::Retry {
                        attempt: 1,
                        message: format!(
                            "key rotation exhausted; failing over to {}",
                            hop.model.id.as_str()
                        ),
                        delay_ms: 0,
                    });
                    break;
                }
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
            // The cancelled-check inside `wait_cancellable` catches a flag
            // set earlier; a dropped setting half merely ends the race.
            wait_cancellable(delay, cancel).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Attempt, ScriptedProvider};

    /// A handle over a scripted provider carrying `id`'s metadata.
    fn handle(provider: &ScriptedProvider, id: &str) -> ModelHandle {
        ModelHandle::scripted(crate::testing::scripted_model(
            provider,
            crate::testing::model_info(id, 1_000_000, 65_536, false, false),
        ))
    }

    /// A request over one user message, with an optional output cap.
    fn request(max_output_tokens: Option<u64>) -> ProviderRequest {
        let mut options = animist::provider::RequestOptions::default();
        options.max_output_tokens = max_output_tokens.map(|n| n.min(u64::from(u32::MAX)) as u32);
        ProviderRequest {
            messages: crate::edge::to_animist(&[crate::history::Message::user("hi")], false),
            system: String::new(),
            tools: Vec::new(),
            options,
        }
    }

    fn rate_limit_turn() -> Attempt {
        crate::testing::http_error(429, "rate limit exceeded, retry after 30s")
    }

    async fn timeout_turn() -> Attempt {
        crate::testing::transport_timeout().await
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
        // The cap holds from attempt 4 on; the wait never exceeds it.
        for _ in 0..20 {
            let (_, delay) = retry.next_delay();
            assert!(delay <= MAX_DELAY, "delay {delay:?} exceeds cap");
            assert!(delay >= MAX_DELAY / 2);
        }
    }

    #[tokio::test]
    async fn retryable_error_is_retried_until_success() {
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(rate_limit_turn());
        provider.push(crate::testing::text_turn("recovered"));
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(
            &model,
            &[],
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.0.assistant.text(), "recovered");
        assert_eq!(output.1, None, "the primary model served");
        let retries = retry_events(&events.lock().unwrap());
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].0, 1, "attempt is 1-based");
        assert_eq!(retries[0].1, "Rate limited");
    }

    #[tokio::test]
    async fn fatal_error_surfaces_without_retry() {
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(crate::testing::http_error(400, "invalid api key"));
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(
            &model,
            &[],
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
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
        let provider = ScriptedProvider::new(Vec::new());
        for _ in 0..12 {
            provider.push(timeout_turn().await);
        }
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(
            &model,
            &[],
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
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
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(rate_limit_turn());
        provider.push(crate::testing::text_turn("ok"));
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let rotations = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rotations_for_hook = std::sync::Arc::clone(&rotations);
        let rotate: RotateKey = Arc::new(move || {
            rotations_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
        });
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(
            &model,
            &[],
            Some(&rotate),
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.0.assistant.text(), "ok");
        // Rotation reports itself and waits a brief guard delay.
        let retries = retry_events(&events.lock().unwrap());
        assert_eq!(retries.len(), 1);
        assert!(retries[0].1.contains("rotated API key"));
        assert_eq!(rotations.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rotation_exhaustion_advances_the_fallback_chain() {
        let primary_provider = ScriptedProvider::new(Vec::new());
        primary_provider.push(rate_limit_turn());
        primary_provider.push(crate::testing::text_turn("late"));
        let primary = handle(&primary_provider, "primary");
        let fallback_provider = ScriptedProvider::new(Vec::new());
        fallback_provider.push(crate::testing::text_turn("fallback"));
        let fallback = handle(&fallback_provider, "fallback-model");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let fallbacks = [fallback];
        let output = stream_with_retry(
            &primary,
            &fallbacks,
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.0.assistant.text(), "fallback");
        assert_eq!(
            output.1,
            Some("fallback-model".to_owned()),
            "a fallback served the call"
        );
        let retries = retry_events(&events.lock().unwrap());
        assert_eq!(retries.len(), 1);
        assert_eq!(retries[0].0, 1, "chain hop resets the attempt counter");
        assert!(retries[0].1.contains("failing over"));
        assert_eq!(retries[0].2, 0, "chain hop does not wait");
    }

    #[tokio::test]
    async fn empty_chain_keeps_retrying_the_primary() {
        // Two failures, then success — single-model behavior unchanged.
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(rate_limit_turn());
        provider.push(rate_limit_turn());
        provider.push(crate::testing::text_turn("ok"));
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(
            &model,
            &[],
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.0.assistant.text(), "ok");
        assert_eq!(retry_events(&events.lock().unwrap()).len(), 2);
    }

    #[tokio::test]
    async fn transient_errors_stop_after_the_cap() {
        let provider = ScriptedProvider::new(Vec::new());
        for _ in 0..20 {
            provider.push(rate_limit_turn());
        }
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(
            &model,
            &[],
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            failure,
            StreamFailure::Error {
                kind: ErrorKind::RateLimited,
                ..
            }
        ));
        assert_eq!(
            retry_events(&events.lock().unwrap()).len(),
            MAX_TRANSIENT_RETRIES as usize
        );
    }

    #[tokio::test]
    async fn already_cancelled_run_never_issues_a_model_call() {
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(crate::testing::text_turn("never"));
        let model = handle(&provider, "primary");
        let (flag, cancel) = crate::run::cancel_channel();
        flag.set(true);
        let failure = stream_with_retry(
            &model,
            &[],
            None,
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(failure, Err(StreamFailure::Cancelled { .. })));
        assert_eq!(provider.requests().len(), 0);
    }

    #[tokio::test]
    async fn rotation_cap_falls_through_to_backoff() {
        let provider = ScriptedProvider::new(Vec::new());
        for _ in 0..5 {
            provider.push(rate_limit_turn());
        }
        provider.push(crate::testing::text_turn("ok"));
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        // Every rotation "succeeds" without changing anything.
        let rotate: RotateKey = Arc::new(|| true);
        let events = std::sync::Mutex::new(Vec::new());
        let output = stream_with_retry(
            &model,
            &[],
            Some(&rotate),
            &TransientBudget::default(),
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.0.assistant.text(), "ok");
        let retries = retry_events(&events.lock().unwrap());
        assert_eq!(retries.len(), MAX_ROTATIONS as usize + 1);
        assert!(retries[MAX_ROTATIONS as usize].1.contains("Rate limited"));
    }

    #[tokio::test]
    async fn exhausted_run_budget_stops_the_next_call() {
        let budget = TransientBudget::default();
        while budget.charge() < MAX_RUN_TRANSIENT_RETRIES {}
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(rate_limit_turn());
        provider.push(crate::testing::text_turn("never"));
        let model = handle(&provider, "primary");
        let (_flag, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(
            &model,
            &[],
            None,
            &budget,
            &request(None),
            0,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            failure,
            StreamFailure::Error {
                kind: ErrorKind::RateLimited,
                ..
            }
        ));
        assert_eq!(provider.requests().len(), 1);
        assert!(retry_events(&events.lock().unwrap()).is_empty());
    }

    #[tokio::test]
    async fn cancel_during_backoff_carries_no_streamed_text() {
        // The stream never succeeds, so every attempt waits in backoff.
        let provider = ScriptedProvider::new(Vec::new());
        for _ in 0..50 {
            provider.push(rate_limit_turn());
        }
        let model = handle(&provider, "primary");
        let (flag, cancel) = crate::run::cancel_channel();
        let task = tokio::spawn(async move {
            stream_with_retry(
                &model,
                &[],
                None,
                &TransientBudget::default(),
                &request(None),
                0,
                &cancel,
                &|_| {},
            )
            .await
        });
        // Cancel within the first backoff window (10–20ms under test delays).
        tokio::time::sleep(Duration::from_millis(5)).await;
        flag.set(true);
        match task.await.unwrap().unwrap_err() {
            StreamFailure::Cancelled { streamed } => assert!(streamed.is_empty()),
            other => panic!("expected cancellation, got {other:?}"),
        }
    }

    // --- fallback-hop rebudgeting ---

    /// A fallback hop scripting one reply, with optional `max_output` and
    /// context window; the scripted provider records every request it
    /// serves.
    fn hop(
        id: &str,
        reply: &str,
        max_output: Option<u32>,
        context_window: Option<u32>,
    ) -> (ModelHandle, ScriptedProvider) {
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(crate::testing::text_turn(reply));
        let info = crate::testing::model_info(
            id,
            context_window.unwrap_or(1_000_000),
            max_output.unwrap_or(65_536),
            false,
            false,
        );
        (
            ModelHandle::scripted(crate::testing::scripted_model(&provider, info)),
            provider,
        )
    }

    /// A primary whose first attempt rate-limits, so the chain advances.
    fn rate_limited_primary() -> ModelHandle {
        let provider = ScriptedProvider::new(Vec::new());
        provider.push(rate_limit_turn());
        provider.push(crate::testing::text_turn("late"));
        handle(&provider, "primary")
    }

    #[tokio::test]
    async fn hop_never_receives_more_than_its_own_output_limit() {
        let primary = rate_limited_primary();
        let (fallback, requests) = hop("capped-hop", "fallback", Some(3_000), None);
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let chain = [fallback];
        let (output, served) = stream_with_retry(
            &primary,
            &chain,
            None,
            &TransientBudget::default(),
            &request(Some(8_192)),
            1,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "fallback");
        assert_eq!(served, Some("capped-hop".to_owned()));
        assert_eq!(
            requests.requests()[0].options.max_output_tokens,
            Some(3_000),
            "hop gets min(primary cap, hop max_output)"
        );
    }

    #[tokio::test]
    async fn hop_output_limit_fills_an_unset_request_cap() {
        let primary = rate_limited_primary();
        let (fallback, requests) = hop("capped-hop", "fallback", Some(3_000), None);
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let (output, _) = stream_with_retry(
            &primary,
            &[fallback],
            None,
            &TransientBudget::default(),
            &request(None), // no configured cap
            1,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "fallback");
        assert_eq!(
            requests.requests()[0].options.max_output_tokens,
            Some(3_000),
            "an unset cap is filled from the hop's own limit"
        );
    }

    #[tokio::test]
    async fn hop_window_clamps_the_output_budget() {
        let primary = rate_limited_primary();
        let (fallback, requests) = hop("window-hop", "fallback", None, Some(20_000));
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let (output, _) = stream_with_retry(
            &primary,
            &[fallback],
            None,
            &TransientBudget::default(),
            &request(Some(16_000)),
            10_000,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "fallback");
        assert_eq!(
            requests.requests()[0].options.max_output_tokens,
            Some(10_000),
            "cap shrinks to the remaining window"
        );
    }

    #[tokio::test]
    async fn hop_window_clamp_floors_at_min_output_tokens() {
        let primary = rate_limited_primary();
        let (fallback, requests) = hop("tight-hop", "fallback", None, Some(12_000));
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let (output, _) = stream_with_retry(
            &primary,
            &[fallback],
            None,
            &TransientBudget::default(),
            &request(Some(16_000)),
            11_000, // leaves 1000 — below the floor
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "fallback");
        assert_eq!(
            requests.requests()[0].options.max_output_tokens,
            Some(crate::run::MIN_OUTPUT_TOKENS as u32),
            "the clamp floors exactly like the primary's"
        );
    }

    #[tokio::test]
    async fn unfit_hop_is_skipped_without_being_called() {
        let primary_provider = ScriptedProvider::new(Vec::new());
        primary_provider.push(rate_limit_turn());
        let primary = handle(&primary_provider, "primary");
        let (unfit, unfit_requests) = hop("unfit-hop", "never", None, Some(8_000));
        let (fit, fit_requests) = hop("fit-hop", "fit", None, None);
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let chain = [unfit, fit];
        let (output, served) = stream_with_retry(
            &primary,
            &chain,
            None,
            &TransientBudget::default(),
            &request(None),
            9_000,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "fit");
        assert_eq!(served, Some("fit-hop".to_owned()));
        assert!(
            unfit_requests.requests().is_empty(),
            "the unfit hop is never called"
        );
        assert_eq!(fit_requests.requests().len(), 1);
        let guard = events.lock().unwrap();
        let messages: Vec<&str> = guard
            .iter()
            .filter_map(|e| match e {
                Event::Retry { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(messages.contains(
            &"skipping unfit-hop: prompt (~9000 tokens) exceeds its context window (8000)"
        ));
    }

    #[tokio::test]
    async fn all_unfit_hops_end_in_bounded_backoff_on_current_model() {
        let primary_provider = ScriptedProvider::new(Vec::new());
        for _ in 0..12 {
            primary_provider.push(rate_limit_turn());
        }
        let primary = handle(&primary_provider, "primary");
        let (small_a, a_requests) = hop("small-a", "a", None, Some(4_000));
        let (small_b, b_requests) = hop("small-b", "b", None, Some(5_000));
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let failure = stream_with_retry(
            &primary,
            &[small_a, small_b],
            None,
            &TransientBudget::default(),
            &request(None),
            9_000,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(failure, StreamFailure::Error { .. }),
            "bounded primary backoff, got {failure:?}"
        );
        assert!(a_requests.requests().is_empty());
        assert!(b_requests.requests().is_empty());
        let guard = events.lock().unwrap();
        let skips = guard
            .iter()
            .filter(
                |e| matches!(e, Event::Retry { message, .. } if message.starts_with("skipping ")),
            )
            .count();
        assert!(skips > 0, "skip events are emitted");
        assert!(
            guard.len() < 60,
            "event stream stays bounded, got {}",
            guard.len()
        );
    }

    #[tokio::test]
    async fn requests_are_rederived_from_the_pristine_request_per_attempt() {
        let primary_provider = ScriptedProvider::new(Vec::new());
        primary_provider.push(rate_limit_turn());
        let primary = handle(&primary_provider, "primary");
        // Hop A rate-limits its one attempt (with its own 2_000 cap), so the
        // chain advances to hop B under the pristine request cap.
        let (hop_a, a_requests) = {
            let provider = ScriptedProvider::new(Vec::new());
            provider.push(rate_limit_turn());
            let info = crate::testing::model_info("hop-a", 1_000_000, 2_000, false, false);
            (
                ModelHandle::scripted(crate::testing::scripted_model(&provider, info)),
                provider,
            )
        };
        let (hop_b, b_requests) = hop("hop-b", "b", None, None);
        let (_, cancel) = crate::run::cancel_channel();
        let events = std::sync::Mutex::new(Vec::new());
        let (output, _) = stream_with_retry(
            &primary,
            &[hop_a, hop_b],
            None,
            &TransientBudget::default(),
            &request(Some(8_192)),
            1,
            &cancel,
            &|e| events.lock().unwrap().push(e),
        )
        .await
        .unwrap();
        assert_eq!(output.assistant.text(), "b");
        assert_eq!(
            a_requests.requests()[0].options.max_output_tokens,
            Some(2_000)
        );
        assert_eq!(
            b_requests.requests()[0].options.max_output_tokens,
            Some(8_192),
            "hop B sees the original cap, not hop A's rebudget"
        );
    }
}
