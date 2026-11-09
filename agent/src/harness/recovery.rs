//! Recovery layer: the retry taxonomy behind [`LayerImpl::recover`].
//! A failed inference attempt classifies through the taxonomy (HTTP
//! status first, then message matching), and today's policy applies:
//! rate limits rotate the key then fail over to the next fallback model,
//! server/transport/timeout failures back off linearly (raced against the
//! run's cancellation), auth failures pause for the surface's reauth hook,
//! and context overflow recalibrates the estimator and forces the
//! compaction engine before the one retry. Content-policy and fatal
//! errors propagate.
//!
//! The fallback hop and a reauth-swapped model live in the shared
//! [`RecoveryRunState`] so the request layer serves them; the driver
//! reads the same handle for its stop-reason overflow re-runs and to
//! carry budgets across follow-up runs.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use animist::agent::{Agent, AgentError, LayerImpl, Recovery, Result, RunContext};
use animist::history::StoredMessage;
use animist::provider::ProviderError;
use snafu::GenerateImplicitData;

use crate::compaction::CompactionEngine;
use crate::compaction::SharedCompactionState;
use crate::harness::doom;
use crate::harness::guardrails::GuardrailState;
use crate::harness::retry::{
    MAX_ROTATIONS, MAX_RUN_TRANSIENT_RETRIES, MAX_TIMEOUT_RETRIES, MAX_TRANSIENT_RETRIES,
    ROTATION_DELAY, RetryCtx, RetryState,
};
use crate::provider::ModelHandle;

use super::events::{CraftEvent, EventSink};

/// How many times a run recovers from a context-overflow stream error by
/// compacting and retrying before the error is surfaced. The counter resets
/// on every successful stream, so a later overflow on another turn still
/// gets its attempt.
pub(crate) const MAX_OVERFLOW_RECOVERIES: u32 = 1;

/// Auth errors tolerated before the reauth wait gives up and the error
/// surfaces. The counter resets after a successful attempt.
pub(crate) const MAX_REAUTH_ATTEMPTS: u32 = 2;

/// Rearm cycles covered by animist's run-wide retry budget beyond the
/// transient tally: reauth and overflow recoveries re-arm after every
/// successful stream, and a long turn may see several episodes.
const RETRY_BUDGET_REARM_CYCLES: u32 = 4;

/// The retry budget animist's loop enforces: the run-wide transient budget
/// plus headroom for rearm cycles of the reauth and overflow caps. Rotations
/// charge the transient budget themselves. A turn with more reauth/overflow
/// episodes than the headroom covers surfaces the error from the agent's
/// counter instead of the layer's caps.
pub(crate) fn retry_budget() -> u32 {
    MAX_RUN_TRANSIENT_RETRIES
        + RETRY_BUDGET_REARM_CYCLES * (MAX_REAUTH_ATTEMPTS + MAX_OVERFLOW_RECOVERIES)
}

// --- Reauth hook -----------------------------------------------------------------

/// What a surface supplies so a run can pause for re-authentication:
/// block until credentials are refreshed, returning the model rebuilt
/// from them (`Ok(None)` when the active model needs no swap — its
/// provider's credentials are reloaded in place), or report failure. The
/// TUI and ACP implement it.
#[async_trait]
pub trait ReauthHook: Send + Sync {
    async fn reauth(&self, attempt: u32) -> std::result::Result<Option<ModelHandle>, String>;
}

// --- Recovery taxonomy ------------------------------------------------------------

/// Recovery taxonomy for provider stream errors. Most failures classify
/// from the preserved HTTP status, when one survived, and otherwise fall
/// back to message matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// 429: retried with backoff; the first remedy is key rotation.
    RateLimited,
    /// 5xx / overloaded: retried with backoff.
    Server,
    /// Connection-level transport failure: retried with backoff.
    Transport,
    /// Timed out: retried with backoff, capped at
    /// [`MAX_TIMEOUT_RETRIES`](crate::harness::retry::MAX_TIMEOUT_RETRIES).
    Timeout,
    /// 401: authentication failed. Never retried here; the reauth wait
    /// owns it.
    Auth,
    /// Prompt exceeded the context window: never retried here; overflow
    /// recovery owns it.
    Overflow,
    /// Content policy / billing: abort immediately, retrying cannot help.
    Abort,
    /// Everything else: surface the failure.
    Fatal,
}

impl ErrorKind {
    /// 429 alone: rotation is only reachable inside the retryable branch
    /// (the transient arms of `recover`), so 401/403 never rotate.
    pub(crate) fn should_rotate_key(self) -> bool {
        self == Self::RateLimited
    }

    /// Human summary for [`CraftEvent::Retry`].
    pub(crate) fn retry_message(self, message: &str) -> String {
        match self {
            Self::RateLimited => "Rate limited".into(),
            Self::Server => "Provider server error".into(),
            Self::Transport => "Connection error".into(),
            Self::Timeout => "Stream timed out".into(),
            _ => message.to_owned(),
        }
    }
}

/// Classify a typed provider error: the preserved HTTP status first
/// (Bedrock errors through their source), credential failures as auth,
/// transport timeouts as timeouts, and everything else by message.
pub(crate) fn classify_error(error: &ProviderError) -> ErrorKind {
    match error {
        ProviderError::Http { status: 429, .. } => return ErrorKind::RateLimited,
        ProviderError::Http { status: 401, .. } => return ErrorKind::Auth,
        ProviderError::Http {
            status: 500..=599, ..
        } => return ErrorKind::Server,
        // 4xx bodies carry the real reason (overflow, content policy);
        // keep matching on the message instead of guessing Fatal.
        ProviderError::Http { body, .. } => {
            if crate::compaction::llm::is_context_overflow_body(body) {
                return ErrorKind::Overflow;
            }
        }
        ProviderError::Transport { source } if source.is_timeout() => return ErrorKind::Timeout,
        ProviderError::Transport { .. } => return ErrorKind::Transport,
        ProviderError::Credentials { .. }
        | ProviderError::CredentialEnvironment { .. }
        | ProviderError::EmptyApiKey { .. }
        | ProviderError::CredentialHeader { .. } => return ErrorKind::Auth,
        ProviderError::Bedrock { source } => return classify_bedrock(source),
        _ => {}
    }
    classify_string(&error.to_string())
}

/// Bedrock failures classify through the same taxonomy; 429/503 additionally
/// mark themselves as provider-retried (see [`provider_retried`]).
fn classify_bedrock(source: &animist::provider::bedrock::BedrockError) -> ErrorKind {
    use animist::provider::bedrock::BedrockError;
    let (status, body): (u16, &[u8]) = match source {
        BedrockError::Http { status, body, .. } => (*status, body.as_slice()),
        BedrockError::ErrorBodyTooLarge {
            status,
            body_prefix,
            ..
        } => (*status, body_prefix),
        BedrockError::Credentials { .. } | BedrockError::MissingCredentialsProvider { .. } => {
            return ErrorKind::Auth;
        }
        BedrockError::Transport { source } if source.is_timeout() => return ErrorKind::Timeout,
        BedrockError::Transport { .. } => return ErrorKind::Transport,
        other => return classify_string(&other.to_string()),
    };
    match status {
        429 => ErrorKind::RateLimited,
        401 => ErrorKind::Auth,
        500..=599 => ErrorKind::Server,
        _ => {
            let body = String::from_utf8_lossy(body);
            if crate::compaction::llm::is_context_overflow_body(&body) {
                ErrorKind::Overflow
            } else {
                classify_string(&body)
            }
        }
    }
}

/// Whether animist's provider already spent its own retries on this error:
/// Bedrock's 429/503 handling, and nothing else. Recovery skips the
/// same-model backoff entirely and fails over.
pub(crate) fn provider_retried(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Bedrock {
            source: animist::provider::bedrock::BedrockError::Http {
                status: 429 | 503,
                ..
            }
        }
    )
}

fn classify_string(message: &str) -> ErrorKind {
    // The single shared context-overflow classifier lives in compaction
    // (its superset also rejects oversized *output* caps, which stream
    // errors for prompt overflow cannot be mistaken for either).
    if crate::compaction::llm::is_context_overflow_body(message) {
        return ErrorKind::Overflow;
    }
    let message = message.to_ascii_lowercase();
    if ["content policy", "content_policy", "billing"]
        .iter()
        .any(|n| message.contains(n))
    {
        return ErrorKind::Abort;
    }
    if message.contains("timeout") || message.contains("timed out") {
        return ErrorKind::Timeout;
    }
    if message.contains("unauthorized") || message.contains("401") {
        return ErrorKind::Auth;
    }
    if message.contains("rate limit")
        || message.contains("rate_limit")
        || message.contains("429")
        || message.contains("too many requests")
    {
        return ErrorKind::RateLimited;
    }
    if message.contains("overloaded")
        || message.contains("503")
        || message.contains("server error")
        || ["500", "502", "504"].iter().any(|n| message.contains(n))
    {
        return ErrorKind::Server;
    }
    if [
        "connection",
        "network",
        "broken pipe",
        "connection reset",
        "error sending request",
    ]
    .iter()
    .any(|n| message.contains(n))
    {
        return ErrorKind::Transport;
    }
    ErrorKind::Fatal
}

// --- Overflow prompt-size parse ----------------------------------------------------

/// Best-effort prompt size parsed out of a provider overflow error body, to
/// feed estimator recalibration. Providers phrase these differently —
/// OpenAI-style "maximum context length is 8192 tokens. However, you
/// requested 9016 tokens.", Anthropic-style "prompt is too long: 213132
/// tokens > 200000 maximum". A number following a request-size keyword wins
/// outright; otherwise the largest token-adjacent number is the offending
/// size (the window bound is the smaller figure), and a lone candidate next
/// to window words is treated as the window bound, not the prompt — no safe
/// parse. `None` means "do not recalibrate".
pub(crate) fn parse_overflow_prompt_size(text: &str) -> Option<u64> {
    // All offsets are measured against the lowercased copy: keyword finds
    // and number spans must agree, and `to_lowercase` can change byte
    // lengths (e.g. 'İ' grows), so mixing the two panics on char
    // boundaries.
    let lower = text.to_lowercase();
    let numbers = numbers_in(&lower);
    // A request-size keyword ("you requested 9016 tokens", "you sent 9000")
    // names the offending prompt size directly: the first number after it.
    for keyword in ["requested", "you sent", "resulted in"] {
        if let Some(at) = lower.find(keyword) {
            let after = at + keyword.len();
            if let Some(&(value, _, _)) = numbers
                .iter()
                .find(|&&(_, start, _)| start >= after && start < after + 64)
            {
                return Some(value);
            }
        }
    }
    // "N tokens"-style candidates: a token mention within a few characters.
    let candidates: Vec<u64> = numbers
        .iter()
        .filter(|&&(_, _, end)| lower[end..lower.len().min(end + 12)].contains("token"))
        .map(|&(value, _, _)| value)
        .collect();
    match candidates.as_slice() {
        [] => None,
        [single] => {
            // One token-sized figure next to window words ("maximum context
            // length is 8192 tokens") is the window bound, not the prompt
            // size.
            let window_words = ["maximum", "context length", "context window", " window"];
            (!window_words.iter().any(|w| lower.contains(w))).then_some(*single)
        }
        // With several sizes on the table, the offending prompt is the
        // largest: overflow means it exceeded the window bound.
        many => many.iter().copied().max(),
    }
}

/// The longest server-mandated wait honored as a backoff floor: a provider
/// naming an hour parks the turn for this long at most (cancellation still
/// ends it immediately).
const MAX_RETRY_AFTER: u64 = 300;

/// The server-mandated wait in a rate-limit message, in seconds:
/// "retry after 30s", "Retry-After: 30", "retry-after 1m". The digits must
/// sit within a few characters of the keyword, so unrelated numbers later
/// in the message are not mistaken for the wait. `None` when the message
/// names no usable wait.
fn retry_after_seconds(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    let phrase = if lower.contains("retry-after") {
        "retry-after"
    } else if lower.contains("retry after") {
        "retry after"
    } else {
        return None;
    };
    let at = lower.find(phrase)?;
    // Only punctuation and spaces may sit between the phrase and its
    // number, so unrelated digits later in the message are not the wait.
    // The window's end is floored to a char boundary: provider bodies
    // carry multibyte text, and a mid-character slice would panic.
    let start = at + phrase.len();
    let mut end = (start + 12).min(lower.len());
    while end > start && !lower.is_char_boundary(end) {
        end -= 1;
    }
    let window = &lower[start..end];
    let lead: String = window.chars().take_while(|c| !c.is_ascii_digit()).collect();
    if !lead
        .chars()
        .all(|c| matches!(c, '-' | ':' | ' ' | '.' | '='))
    {
        return None;
    }
    let digits: String = window
        .chars()
        .skip(lead.chars().count())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let value: u64 = digits.parse().ok()?;
    let unit = window
        .chars()
        .skip(lead.chars().count() + digits.len())
        .find(|c| !c.is_whitespace())
        .unwrap_or('s');
    let seconds = if unit == 'm' {
        value.saturating_mul(60)
    } else {
        value
    };
    Some(seconds.min(MAX_RETRY_AFTER))
}

/// Scan `text` for integer literals (internal grouping commas stripped),
/// returning `(value, start, end)` per number.
fn numbers_in(text: &str) -> Vec<(u64, usize, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut value: u64 = 0;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b',') {
            if bytes[i] != b',' {
                value = value
                    .saturating_mul(10)
                    .saturating_add(u64::from(bytes[i] - b'0'));
            }
            i += 1;
        }
        out.push((value, start, i));
    }
    out
}

// --- Shared run state ---------------------------------------------------------------

/// Per-run recovery state, shared as a typed handle: the request layer
/// reads the model override, the driver asks it to charge the overflow
/// budget for stop-reason re-runs and marks follow-up runs. Guarded by a
/// `std` mutex; the layer never holds it across an `.await`.
pub(crate) struct RecoveryRunState {
    /// The model serving this run's requests: a reauth swap or an active
    /// fallback hop. `None` is the conversation's model.
    pub model_override: Option<ModelHandle>,
    /// Whether `model_override` is a fallback hop (and so carries its own
    /// request budgets) rather than a reauth swap of the same model.
    pub is_fallback_hop: bool,
    /// The fallback chain cursor: 0 is the primary, `i` the `(i-1)`-th
    /// fallback. Advances inside `recover`; resets at `turn_start`.
    fallback: usize,
    /// Consecutive transient failures without a successful stream between
    /// them.
    transient_failures: u32,
    /// Run-wide transient tally across every model call.
    transient_run: u32,
    /// Key rotations since the last successful stream.
    rotations: u32,
    /// Backoff machine for this failure streak.
    retry: RetryState,
    /// Consecutive reauth waits; resets after a successful attempt.
    reauth_attempts: u32,
    /// Consecutive overflow recoveries; resets after a successful stream.
    /// The driver charges the same budget for stop-reason re-runs.
    overflow_recoveries: u32,
    /// Whether the previous driver series iteration was itself a recovered
    /// stop-reason overflow: consecutive ones are one episode, and the
    /// second surfaces the error. Only the driver's stop-reason path sets
    /// it.
    previous_stop_reason_overflow: bool,
    /// Completed steps observed at the last `recover` call: a higher count
    /// on entry means a stream succeeded since, re-arming the budgets.
    last_seen_steps: u64,
    /// Set by the driver before a follow-up run (advisor and overflow
    /// re-runs): `turn_start` skips the reset and consumes the mark.
    pub(crate) follow_up: bool,
}

impl Default for RecoveryRunState {
    fn default() -> Self {
        Self {
            model_override: None,
            is_fallback_hop: false,
            fallback: 0,
            transient_failures: 0,
            transient_run: 0,
            rotations: 0,
            retry: RetryState::new(),
            reauth_attempts: 0,
            overflow_recoveries: 0,
            previous_stop_reason_overflow: false,
            last_seen_steps: 0,
            follow_up: false,
        }
    }
}

impl RecoveryRunState {
    /// A stream succeeded since the last failure the state saw: re-arm the
    /// per-streak counters. The run-wide transient tally is never
    /// re-armed.
    fn rearm_after_success(&mut self) {
        self.transient_failures = 0;
        self.rotations = 0;
        self.reauth_attempts = 0;
        self.overflow_recoveries = 0;
        self.retry = RetryState::new();
    }

    /// Charge one overflow recovery against the shared budget, re-arming
    /// first when the caller says the last episode closed (a successful
    /// stream followed the previous charge, or a new episode opened).
    fn charge_overflow(&mut self, rearm: bool) -> bool {
        if rearm {
            self.rearm_after_success();
        }
        if self.overflow_recoveries >= MAX_OVERFLOW_RECOVERIES {
            return false;
        }
        self.overflow_recoveries += 1;
        true
    }

    /// Charge the budget for a stop-reason overflow re-run, deriving the
    /// re-arm from the episode this state saw: the episode closed unless
    /// the previous series iteration was itself a recovered overflow AND
    /// this run made no progress beyond its own terminal step — a
    /// follow-up that ran real tool steps between the overflows has earned
    /// a fresh compact-and-retry. Records a successful charge as the
    /// episode's first iteration.
    pub(crate) fn try_charge_stop_reason_overflow(&mut self, steps: u64) -> bool {
        let rearm = !self.previous_stop_reason_overflow || steps > 1;
        let charged = self.charge_overflow(rearm);
        self.previous_stop_reason_overflow |= charged;
        charged
    }

    /// A driver series iteration settled without a stop-reason overflow:
    /// the next overflow opens a fresh episode.
    pub(crate) fn close_stop_reason_episode(&mut self) {
        self.previous_stop_reason_overflow = false;
    }

    /// The turn's overflow episode is over whatever way it ended; a later
    /// turn's overflow gets its own attempt.
    pub(crate) fn end_turn_overflow_episode(&mut self) {
        self.close_stop_reason_episode();
        self.overflow_recoveries = 0;
    }
}

/// The recovery state's typed handle.
pub(crate) type RecoveryHandle = Arc<Mutex<RecoveryRunState>>;

/// Recalibrate the estimator and force the compaction engine after a
/// context overflow — the shared body of the error-form recovery (inside
/// `recover`, with the provider's error body) and the driver's
/// stop-reason re-run (with the provider's measured prompt size).
/// Returns whether a stage ran: a declined compaction changed nothing, so
/// the retried request would overflow byte-identically and the error is
/// the honest answer.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_from_overflow(
    engine: &CompactionEngine,
    compaction: &SharedCompactionState,
    guardrails: &Arc<GuardrailState>,
    model: &ModelHandle,
    history: &animist::history::History,
    sink: &EventSink,
    error_text: Option<&str>,
    measured_prompt: u64,
) -> bool {
    // Include the request overhead (preamble + tool schemas) the request
    // layer published: the provider's actual prompt size counts it, so the
    // recalibration below must compare like with like. An error body naming
    // no size leaves the estimate alone — never recalibrate to a bogus
    // value like the window bound.
    let overhead = compaction.lock().await.request_overhead();
    let view: Vec<StoredMessage> = history
        .snapshot()
        .await
        .entries()
        .iter()
        .filter_map(|entry| entry.current())
        .cloned()
        .collect();
    let estimated = crate::compaction::estimate::estimate_tokens(&view).saturating_add(overhead);
    if let Some(actual) = error_text.and_then(parse_overflow_prompt_size) {
        compaction.lock().await.recalibrate(actual, estimated);
    } else if measured_prompt > 0 {
        compaction
            .lock()
            .await
            .recalibrate(measured_prompt, estimated);
    }
    // The engine runs between the failed attempt and its retry; the agent
    // re-takes its history snapshot on every attempt, so the compaction
    // cannot conflict with the pending exchange. The doom score below
    // compares like the engine's effectiveness score: the unfloored
    // estimate, after any recalibration.
    let before = compaction.lock().await.scaled_estimate_unfloored(&view);
    let ran = engine
        .force_compact(compaction, model, history, model.context_length(), sink)
        .await;
    // Compaction that barely shrank the context is itself a doom signal;
    // one that paid off earns a decay.
    let after_view: Vec<StoredMessage> = history
        .snapshot()
        .await
        .entries()
        .iter()
        .filter_map(|entry| entry.current())
        .cloned()
        .collect();
    let after = compaction
        .lock()
        .await
        .scaled_estimate_unfloored(&after_view);
    let savings = if before > 0 {
        1.0 - (after as f32 / before as f32)
    } else {
        0.0
    };
    guardrails.with(|inner| {
        if savings < doom::INEFFECTIVE_COMPACTION_THRESHOLD {
            inner.doom.note_ineffective_compaction();
        } else {
            inner.doom.note_effective_compaction();
        }
    });
    // The pre-compaction measurement no longer describes the compacted
    // context; drop it so the clamp trusts the estimate until the next
    // real usage report.
    compaction.lock().await.set_measured_prompt(0);
    ran
}

/// Lock the recovery state without exposing the poison dance at call sites.
fn lock(state: &RecoveryHandle) -> std::sync::MutexGuard<'_, RecoveryRunState> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

// --- The layer -----------------------------------------------------------------------

pub(crate) struct RecoveryLayer {
    /// Retry inputs: the rotation hook and the fallback model chain.
    retry: RetryCtx,
    /// The surface's reauth hook for the current model (TUI and ACP wire
    /// one); the conversation swaps it with the model.
    reauth: Arc<arc_swap::ArcSwapOption<Arc<dyn ReauthHook>>>,
    state: RecoveryHandle,
    compaction: SharedCompactionState,
    engine: CompactionEngine,
    guardrails: Arc<GuardrailState>,
    /// The conversation's model: overflow compaction summarizes with it.
    model: Arc<arc_swap::ArcSwap<ModelHandle>>,
    sink: EventSink,
}

impl RecoveryLayer {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        retry: RetryCtx,
        reauth: Arc<arc_swap::ArcSwapOption<Arc<dyn ReauthHook>>>,
        state: RecoveryHandle,
        compaction: SharedCompactionState,
        engine: CompactionEngine,
        guardrails: Arc<GuardrailState>,
        model: Arc<arc_swap::ArcSwap<ModelHandle>>,
        sink: EventSink,
    ) -> Self {
        Self {
            retry,
            reauth,
            state,
            compaction,
            engine,
            guardrails,
            model,
            sink,
        }
    }

    /// Advance the fallback chain past hops whose window the prompt cannot
    /// fit, then to the first fitting one. `None` when the chain is spent
    /// or holds nothing that fits; the current model keeps its bounded
    /// backoff instead of calling a hop that cannot fit.
    async fn advance_fallback(&self, prompt_tokens: u64, rotated: bool) -> Option<ModelHandle> {
        let mut cursor = lock(&self.state).fallback;
        let mut hop = None;
        while let Some(candidate) = self.retry.fallbacks.get(cursor) {
            cursor += 1;
            if let Some(window) = candidate
                .context_length()
                .filter(|w| prompt_tokens >= u64::from(*w))
            {
                self.sink.craft(CraftEvent::Retry {
                    attempt: 1,
                    message: format!(
                        "skipping {}: prompt (~{prompt_tokens} tokens) exceeds its context window ({window})",
                        candidate.model.id,
                    ),
                    delay_ms: 0,
                });
                continue;
            }
            let lead = if rotated {
                "key rotation exhausted; "
            } else {
                ""
            };
            self.sink.craft(CraftEvent::Retry {
                attempt: 1,
                message: format!("{lead}failing over to {}", candidate.model.id),
                delay_ms: 0,
            });
            hop = Some(candidate.clone());
            break;
        }
        let mut state = lock(&self.state);
        if hop.is_some() {
            state.fallback = cursor;
            state.model_override = hop.clone();
            state.is_fallback_hop = true;
            state.retry = RetryState::new();
        }
        hop
    }

    /// Recalibrate the estimator from the error body, force the compaction
    /// engine, and score the result into the doom tracker. Returns whether
    /// a stage ran (a declined compaction must not be retried blindly).
    async fn recover_overflow(&self, agent: &Agent, message: &str) -> bool {
        let model = Arc::unwrap_or_clone(self.model.load_full());
        recover_from_overflow(
            &self.engine,
            &self.compaction,
            &self.guardrails,
            &model,
            &agent.history(),
            &self.sink,
            Some(message),
            0,
        )
        .await
    }

    /// The transient-failure policy: rotation, fallback, then linear
    /// backoff, bounded per kind. Returns the recovery verdict.
    async fn recover_transient(
        &self,
        run: &RunContext,
        source: &ProviderError,
        kind: ErrorKind,
        prompt_tokens: u64,
    ) -> Recovery {
        let message = source.to_string();
        {
            let mut state = lock(&self.state);
            state.transient_failures += 1;
            state.transient_run += 1;
            if kind != ErrorKind::Timeout
                && (state.transient_failures > MAX_TRANSIENT_RETRIES
                    || state.transient_run > MAX_RUN_TRANSIENT_RETRIES)
            {
                return Recovery::Propagate;
            }
        }
        // Bedrock already retried this 429/503 internally: no same-model
        // backoff may stack on it — fail over, or surface when the chain
        // holds nothing.
        if provider_retried(source) {
            // No rotation attempt: animist already retried this failure.
            return match self.advance_fallback(prompt_tokens, false).await {
                Some(_) => Recovery::Retry,
                None => Recovery::Propagate,
            };
        }
        let mut advanced = false;
        if kind.should_rotate_key() {
            let rotate = self.retry.rotate.clone();
            let rotated = {
                let state = lock(&self.state);
                state.rotations < MAX_ROTATIONS && rotate.as_ref().is_some_and(|rotate| rotate())
            };
            if rotated {
                let attempt = {
                    let mut state = lock(&self.state);
                    state.rotations += 1;
                    state.retry = RetryState::new();
                    state.rotations
                };
                self.sink.craft(CraftEvent::Retry {
                    attempt,
                    message: "rotated API key; retrying".to_owned(),
                    delay_ms: ROTATION_DELAY.as_millis() as u64,
                });
                // Even a successful rotation waits briefly: a hook that
                // returns true without changing the key must not hot-loop.
                wait_cancellable(ROTATION_DELAY, run).await;
                advanced = true;
            } else {
                advanced = self
                    .advance_fallback(prompt_tokens, rotate.is_some())
                    .await
                    .is_some();
            }
        }
        if !advanced {
            let (attempt, delay) = lock(&self.state).retry.next_delay();
            if kind == ErrorKind::Timeout && attempt > MAX_TIMEOUT_RETRIES {
                return Recovery::Propagate;
            }
            // A rate limit that names its wait ("retry after 30s",
            // "Retry-After: 30") is honored as a floor on the backoff: the
            // server asked for patience and the linear band alone
            // could hammer it well before that.
            let delay = match kind {
                ErrorKind::RateLimited => {
                    let floor =
                        retry_after_seconds(&message).map_or(delay, std::time::Duration::from_secs);
                    delay.max(floor)
                }
                _ => delay,
            };
            self.sink.craft(CraftEvent::Retry {
                attempt,
                message: kind.retry_message(&message),
                delay_ms: delay.as_millis() as u64,
            });
            wait_cancellable(delay, run).await;
        }
        Recovery::Retry
    }

    /// The auth-failure policy: pause for the surface's reauth hook, up to
    /// [`MAX_REAUTH_ATTEMPTS`] consecutive waits.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    async fn recover_auth(&self, message: &str) -> Result<Recovery> {
        let attempt = {
            let mut state = lock(&self.state);
            if state.reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                return Ok(Recovery::Propagate);
            }
            state.reauth_attempts += 1;
            state.reauth_attempts
        };
        let Some(hook) = self.reauth.load_full() else {
            return Ok(Recovery::Propagate);
        };
        // Ahead of animist's own events, so the surface shows the wait
        // before the attempt is retried.
        self.sink.craft(CraftEvent::AuthRequired {
            attempt,
            message: message.to_owned(),
        });
        match hook.reauth(attempt).await {
            Ok(Some(model)) => {
                let mut state = lock(&self.state);
                state.model_override = Some(model);
                state.is_fallback_hop = false;
                Ok(Recovery::Retry)
            }
            Ok(None) => {
                // The hook refreshed credentials in place; reload them on
                // the serving model's provider before the retry.
                let handle = {
                    let state = lock(&self.state);
                    state
                        .model_override
                        .clone()
                        .unwrap_or_else(|| Arc::unwrap_or_clone(self.model.load_full()))
                };
                if let Err(error) = handle.model.provider.reload_auth().await {
                    tracing::warn!(%error, "credential reload after reauth failed");
                }
                Ok(Recovery::Retry)
            }
            Err(error) => Err(AgentError::Stopped {
                reason: format!("reauth wait failed: {error}"),
                backtrace: snafu::Backtrace::generate(),
            }),
        }
    }
}

/// Sleep for `delay` unless cancellation wins first. The agent races the
/// whole hook against cancellation as well; this keeps the wait itself
/// from outliving a cancel that lands mid-sleep.
async fn wait_cancellable(delay: std::time::Duration, run: &RunContext) {
    tokio::select! {
        () = tokio::time::sleep(delay) => {}
        () = run.cancellation.cancelled() => {}
    }
}

#[async_trait]
impl LayerImpl for RecoveryLayer {
    async fn turn_start(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        message: &animist::history::StoredMessage,
    ) -> Result<animist::history::StoredMessage> {
        let mut state = lock(&self.state);
        if state.follow_up {
            // A driver follow-up continues the same turn: budgets, the
            // fallback hop, and any reauth swap carry over. The new run's
            // step counter starts at zero.
            state.follow_up = false;
            state.last_seen_steps = 0;
        } else {
            *state = RecoveryRunState::default();
        }
        Ok(message.clone())
    }

    async fn recover(
        &self,
        agent: &Agent,
        run: &RunContext,
        error: &AgentError,
    ) -> Result<Recovery> {
        let AgentError::Inference { source, .. } = error else {
            return Ok(Recovery::Propagate);
        };
        let kind = classify_error(source);
        // A stream succeeded since the last failure: re-arm the budgets.
        let steps = run.stats.snapshot().await.steps;
        {
            let mut state = lock(&self.state);
            if steps > state.last_seen_steps {
                state.rearm_after_success();
            }
            state.last_seen_steps = steps;
        }
        match kind {
            ErrorKind::Overflow => {
                let charged = lock(&self.state).charge_overflow(false);
                if !charged || !self.recover_overflow(agent, &source.to_string()).await {
                    return Ok(Recovery::Propagate);
                }
                // Ahead of animist's `Retrying`, like every other kind.
                self.sink.craft(CraftEvent::Retry {
                    attempt: 1,
                    message: "Context overflow; compacted and retrying".to_owned(),
                    delay_ms: 0,
                });
                Ok(Recovery::Retry)
            }
            ErrorKind::Auth => self.recover_auth(&source.to_string()).await,
            ErrorKind::RateLimited
            | ErrorKind::Server
            | ErrorKind::Transport
            | ErrorKind::Timeout => {
                // The fallback chain's window checks need the prompt size:
                // the history view plus the overhead the request layer last
                // published.
                let overhead = self.compaction.lock().await.request_overhead();
                let view = agent.history().current().await;
                let prompt_tokens =
                    crate::compaction::estimate::estimate_messages(&view).saturating_add(overhead);
                Ok(self
                    .recover_transient(run, source, kind, prompt_tokens)
                    .await)
            }
            ErrorKind::Abort | ErrorKind::Fatal => Ok(Recovery::Propagate),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_error_kinds_from_messages() {
        let cases = [
            (
                "rate limit exceeded, retry after 30s",
                ErrorKind::RateLimited,
            ),
            ("HTTP 429", ErrorKind::RateLimited),
            ("Too many requests", ErrorKind::RateLimited),
            ("provider is overloaded", ErrorKind::Server),
            ("server error (503)", ErrorKind::Server),
            ("request timed out", ErrorKind::Timeout),
            ("operation timeout", ErrorKind::Timeout),
            (
                "error sending request: connection reset by peer",
                ErrorKind::Transport,
            ),
            ("network unreachable", ErrorKind::Transport),
            (
                "your credit balance is too low (billing hard limit)",
                ErrorKind::Abort,
            ),
            ("request rejected by content policy", ErrorKind::Abort),
            ("invalid api key", ErrorKind::Fatal),
            ("401 unauthorized: invalid credentials", ErrorKind::Auth),
            ("Unauthorized", ErrorKind::Auth),
        ];
        for (message, expected) in cases {
            assert_eq!(classify_string(message), expected, "message: {message}");
        }
    }

    #[test]
    fn only_rate_limits_rotate() {
        assert!(ErrorKind::RateLimited.should_rotate_key());
        for kind in [
            ErrorKind::Server,
            ErrorKind::Transport,
            ErrorKind::Timeout,
            ErrorKind::Overflow,
            ErrorKind::Abort,
            ErrorKind::Fatal,
            ErrorKind::Auth,
        ] {
            assert!(!kind.should_rotate_key(), "{kind:?}");
        }
    }

    #[test]
    fn classifies_typed_http_status() {
        for (status, body, expected) in [
            (429u16, "boom", ErrorKind::RateLimited),
            (500, "boom", ErrorKind::Server),
            (503, "boom", ErrorKind::Server),
            (401, "nope", ErrorKind::Auth),
            (
                400,
                "This model's maximum context length is 200000 tokens. However, your messages resulted in 210744 tokens.",
                ErrorKind::Overflow,
            ),
        ] {
            let error = ProviderError::Http {
                status,
                body: body.to_owned(),
                body_truncated: false,
                backtrace: snafu::Backtrace::generate(),
            };
            assert_eq!(classify_error(&error), expected, "status {status}");
        }
    }

    #[test]
    fn bedrock_status_errors_classify_and_mark_provider_retried() {
        use animist::provider::bedrock::BedrockError;
        for (status, retried) in [(429u16, true), (503, true), (500, false), (400, false)] {
            let error = ProviderError::Bedrock {
                source: BedrockError::Http {
                    status,
                    request_id: None,
                    error_type: None,
                    body: Vec::new(),
                    backtrace: snafu::Backtrace::generate(),
                },
            };
            assert_eq!(provider_retried(&error), retried, "status {status}");
            match status {
                429 => assert_eq!(classify_error(&error), ErrorKind::RateLimited),
                503 | 500 => assert_eq!(classify_error(&error), ErrorKind::Server),
                _ => {}
            }
        }
        // Credential failures are auth, whatever their transport shape.
        let error = ProviderError::Bedrock {
            source: BedrockError::MissingCredentialsProvider {
                backtrace: snafu::Backtrace::generate(),
            },
        };
        assert_eq!(classify_error(&error), ErrorKind::Auth);
        // A non-retried Bedrock body classifies through the same table.
        let overflow = ProviderError::Bedrock {
            source: BedrockError::Http {
                status: 400,
                request_id: None,
                error_type: None,
                body: b"Input is too long for requested model".to_vec(),
                backtrace: snafu::Backtrace::generate(),
            },
        };
        assert_eq!(classify_error(&overflow), ErrorKind::Overflow);
    }

    #[test]
    fn overflow_bodies_classify_as_overflow() {
        for message in [
            "Error: prompt is too long: 210744 tokens > 200000 maximum",
            "This model's maximum context length is 4096 tokens. However, you requested ...",
            "conversation exceeds the model context window",
        ] {
            assert_eq!(classify_string(message), ErrorKind::Overflow, "{message}");
        }
        // Output-cap complaints are not prompt overflow: retrying with a
        // shorter prompt cannot fix a `max_tokens` bid too high.
        assert_eq!(
            classify_string(
                "input length and `max_tokens` exceed context limit: 10922 + 8192 > 8192"
            ),
            ErrorKind::Fatal
        );
    }

    // --- overflow prompt-size parse ---

    #[test]
    fn parses_openai_style_requested_size() {
        assert_eq!(
            parse_overflow_prompt_size(
                "This model's maximum context length is 8192 tokens. However, you requested 10000 tokens."
            ),
            Some(10000),
            "the request-size keyword names the offending prompt, not the window"
        );
    }

    #[test]
    fn parses_resulted_in_style() {
        assert_eq!(
            parse_overflow_prompt_size(
                "maximum context length is 4096 tokens, however your messages resulted in 9016 tokens"
            ),
            Some(9016)
        );
    }

    #[test]
    fn comma_grouped_numbers_parse() {
        assert_eq!(
            parse_overflow_prompt_size("you requested 200,000 tokens"),
            Some(200_000)
        );
    }

    #[test]
    fn multiple_token_figures_take_the_largest() {
        assert_eq!(
            parse_overflow_prompt_size("prompt of 9000 tokens exceeds the limit of 8192 tokens"),
            Some(9000),
            "overflow means the prompt size exceeded the window bound"
        );
    }

    #[test]
    fn lone_window_bound_is_not_a_prompt_size() {
        assert_eq!(
            parse_overflow_prompt_size("exceeded the maximum context length of 4096 tokens"),
            None,
            "the window bound must never become the recalibration target"
        );
        assert_eq!(
            parse_overflow_prompt_size("prompt is too long: 213132 tokens > 200000 maximum"),
            None,
            "a single figure when window words are present is ambiguous: stay safe"
        );
    }

    #[test]
    fn lone_size_without_window_words_parses() {
        assert_eq!(
            parse_overflow_prompt_size("prompt is too long at 213132 tokens"),
            Some(213132)
        );
    }

    #[test]
    fn retry_after_seconds_parses_provider_phrasings() {
        assert_eq!(
            retry_after_seconds("rate limit exceeded, retry after 30s"),
            Some(30)
        );
        assert_eq!(retry_after_seconds("Retry-After: 30"), Some(30));
        assert_eq!(retry_after_seconds("please retry-after 2m"), Some(120));
        assert_eq!(retry_after_seconds("rate limited"), None);
        assert_eq!(retry_after_seconds(""), None);
        // Unrelated numbers are not the wait, and an unreasonable one is
        // capped at what a turn may actually pause for.
        assert_eq!(
            retry_after_seconds("retry after the cooldown; 503 in 30s"),
            None
        );
        assert_eq!(
            retry_after_seconds("retry after 3600"),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn no_numbers_means_no_recalibration() {
        assert_eq!(parse_overflow_prompt_size("context window"), None);
        assert_eq!(parse_overflow_prompt_size(""), None);
    }

    #[test]
    fn case_changing_chars_do_not_shift_number_offsets() {
        // 'İ' lowercases to a longer sequence: number offsets measured on
        // the original text would slice the lowercased copy mid-character
        // (panic) or miss the "tokens" mention.
        assert_eq!(
            parse_overflow_prompt_size("\u{130}stanbul prompt: 9016 tokens"),
            Some(9016)
        );
        assert_eq!(
            parse_overflow_prompt_size("\u{212a} request resulted in 7000 tokens"),
            Some(7000)
        );
    }

    // --- budget sizing and rearm semantics ---

    #[test]
    fn retry_budget_covers_the_transient_tally_plus_rearm_headroom() {
        assert_eq!(retry_budget(), 16 + 4 * (2 + 1));
    }

    #[test]
    fn success_between_failures_rearms_every_streak_budget() {
        let mut state = RecoveryRunState {
            transient_failures: MAX_TRANSIENT_RETRIES,
            transient_run: MAX_RUN_TRANSIENT_RETRIES,
            rotations: MAX_ROTATIONS,
            reauth_attempts: MAX_REAUTH_ATTEMPTS,
            overflow_recoveries: MAX_OVERFLOW_RECOVERIES,
            ..RecoveryRunState::default()
        };
        let _ = state.retry.next_delay();
        let _ = state.retry.next_delay();
        state.rearm_after_success();
        assert_eq!(state.transient_failures, 0);
        assert_eq!(state.rotations, 0);
        assert_eq!(state.reauth_attempts, 0);
        assert_eq!(state.overflow_recoveries, 0);
        // The backoff machine starts the next streak fresh, and the
        // run-wide transient tally is never re-armed.
        assert_eq!(state.retry.next_delay().0, 1);
        assert_eq!(state.transient_run, MAX_RUN_TRANSIENT_RETRIES);
    }

    #[test]
    fn stop_reason_charges_rearm_only_when_the_episode_closed() {
        let mut state = RecoveryRunState::default();
        assert!(
            state.try_charge_stop_reason_overflow(1),
            "first charge allowed"
        );
        assert!(
            !state.try_charge_stop_reason_overflow(1),
            "second consecutive charge denied"
        );
        assert!(
            state.try_charge_stop_reason_overflow(2),
            "real progress between overflows rearms the budget"
        );
        assert!(!state.try_charge_stop_reason_overflow(1));

        // A settled iteration closes the episode; the turn's end clears
        // both the episode flag and the charged budget.
        state.close_stop_reason_episode();
        assert!(state.try_charge_stop_reason_overflow(1));
        state.end_turn_overflow_episode();
        assert_eq!(state.overflow_recoveries, 0);
        assert!(state.try_charge_stop_reason_overflow(1));
    }
}
