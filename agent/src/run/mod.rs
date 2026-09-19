//! The crate-owned multi-turn agent loop.
//!
//! Per turn: build the request through the provider [`crate::edge`], stream
//! the model call ([`stream`]), dispatch its tool calls ([`dispatch`]), append
//! the results, and continue while the model emits tool calls — bounded by
//! `max_turns`. History is committed to the caller on success; a failed run
//! commits nothing, while a cancelled run commits a sanitized partial
//! (dangling tool calls closed, cancel marker appended) so the next prompt
//! replays cleanly; a run that hits the turn budget
//! commits its sanitized partial history with an end marker so the next
//! prompt continues from where the budget ran out.

pub mod cancel;
pub mod dedup;
pub mod dispatch;
pub(crate) mod doom;
pub mod events;
pub mod guardrails;
mod nudge;
mod read_lifecycle;
mod recency;
mod retry;
mod stream;
mod task_set;

pub use dedup::{SharedDedupCache, ToolDedupCache, shared_cache};
pub use dispatch::{
    AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome, ToolDispatch,
};
pub use events::{Envelope, EventSender, EventStreamGuard, SessionEvents, event_stream};
pub use guardrails::{SharedGuardrails, shared_guardrails};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use retry::RetryCtx;
pub use stream::TurnOutput;

use std::collections::HashMap;
use std::sync::Arc;

use rig_core::completion::{CompletionModel, FinishReason};
use tokio::sync::watch;

use crate::compaction::CompactionEngine;
use crate::compression::{self, CompressionConfig};
use crate::config::{CompactionBuffer, CompactionConfig};
use crate::edge;
use crate::history::{self, Message};

/// Events emitted as the run progresses; consumed by the TUI and ACP
/// surfaces. Ported from the reference's `AgentEvent` taxonomy
/// (`craft-agent/src/types.rs`): variants whose backing subsystem is not yet
/// ported (retry ladder, stagnation tracker, auto-review plumbing, live tool
/// buffers) are forward substrate — defined but never emitted here.
#[allow(dead_code)] // forward substrate for C.2/C.8/auto-review/LiveToolBuf tasks
#[derive(Clone, Debug)]
pub enum Event {
    /// A streamed chunk of the assistant reply.
    TextDelta(String),
    /// A streamed chunk of the model's reasoning.
    ThinkingDelta(String),
    /// A tool call is queued but not yet running.
    ToolPending {
        id: String,
        name: String,
    },
    /// The model issued a tool call.
    ToolStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// `content` is the full accumulated output so far, not a delta.
    ToolOutput {
        id: String,
        content: String,
    },
    /// A tool call finished (ran, failed, or was skipped).
    ToolDone {
        id: String,
        name: String,
        arguments: serde_json::Value,
        result: history::ToolResult,
    },
    /// A wave of tool results was appended to the turn; `message` carries
    /// every result of the wave in call order.
    ToolResultsSubmitted {
        message: Message,
    },
    /// One model call completed: its usage report and the estimated size of
    /// the context the model just saw.
    TurnComplete {
        usage: history::Usage,
        context_size: u64,
    },
    /// The run ended. `context_window` is a `0` sentinel until window sizes
    /// reach the run seam (model-registry work).
    Done {
        usage: history::Usage,
        context_size: u64,
        context_window: u64,
        num_turns: u32,
        reason: DoneReason,
        /// What the run's turns were billed (H.6). `None` when no model in
        /// the run is priced, so callers show no cost instead of "$0.000".
        cost: Option<f64>,
        /// Per-model usage with each model's recorded (billed) cost, for the
        /// session ledger and `cost.jsonl`. Unpriced models carry `None`.
        by_model: HashMap<String, crate::usage::StoredTokenUsage>,
    },
    /// Human-readable, non-fatal status text.
    Info(String),
    /// The run failed; paired with a terminal `Done` carrying the reason.
    Error(String),
    /// A recoverable stream failure is being retried (attempt is 1-based).
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    AutoCompacting {
        context_size: u64,
        context_window: u64,
    },
    CompactionDone {
        context_size_before: u64,
        context_size_after: u64,
        context_window: u64,
    },
    StagnationDetected {
        similarity: f32,
    },
    AutoReviewStart {
        id: String,
        tool: String,
        scopes: Vec<String>,
    },
    AutoReviewDecision {
        id: String,
        tool: String,
        scopes: Vec<String>,
        verdict: String,
        risk: String,
        rationale: String,
    },
    /// The model returned an empty reply after tool calls and was nudged
    /// to continue.
    Nudge,
    /// Authentication failed (401) and the run paused for re-authentication
    /// (E.10); `attempt` is 1-based. The run resumes after the responder
    /// succeeds or fails with the message.
    AuthRequired {
        attempt: u32,
        message: String,
    },
    /// End-of-stream marker; emitted only by [`EventStreamGuard::drop`] and
    /// swallowed by [`SessionEvents::next`].
    StreamClosed,
}

/// Why a run ended, riding the terminal [`Event::Done`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoneReason {
    /// The model finished without pending tool calls.
    Stop,
    /// The turn budget ran out (partial history committed).
    MaxTurns,
    /// Every continuation of a truncated reply was spent.
    MaxTokens,
    Cancelled,
    Error,
    /// The doom-loop score reached the hard-stop threshold; the sanitized
    /// partial history (with an end marker) was committed.
    DoomStop,
}

impl From<&RunOutcome> for DoneReason {
    fn from(outcome: &RunOutcome) -> Self {
        match outcome {
            RunOutcome::Done { .. } => Self::Stop,
            RunOutcome::MaxTurns => Self::MaxTurns,
            RunOutcome::MaxTokens { .. } => Self::MaxTokens,
            RunOutcome::Cancelled => Self::Cancelled,
            RunOutcome::Failed(_) => Self::Error,
            RunOutcome::DoomStop => Self::DoomStop,
        }
    }
}

/// Cancellation shared between a surface and its run: set the flag, and the
/// run stops at the next stream/dispatch/turn boundary.
#[derive(Clone)]
pub struct CancelToken {
    rx: watch::Receiver<u64>,
    epoch: u64,
}

/// The setting half of a [`CancelToken`].
#[derive(Clone)]
pub struct CancelFlag {
    tx: watch::Sender<u64>,
}

/// Create a cancellation pair, initially not cancelled.
pub fn cancel_channel() -> (CancelFlag, CancelToken) {
    let (tx, rx) = watch::channel(0);
    (CancelFlag { tx }, CancelToken { rx, epoch: 0 })
}

impl CancelToken {
    pub fn cancelled(&self) -> bool {
        *self.rx.borrow() != self.epoch
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.rx.clone()
    }
}

impl CancelFlag {
    /// Cancellation is monotonic and epoch-scoped: `set(true)` bumps the
    /// generation and can never be overwritten by a concurrent re-arm,
    /// while `set(false)` is a no-op — a fresh turn starts clean by
    /// minting a new token at the current generation.
    pub fn set(&self, cancelled: bool) {
        if cancelled {
            self.tx.send_modify(|generation| *generation += 1);
        }
    }

    /// A token sharing this flag's state.
    pub fn token(&self) -> CancelToken {
        CancelToken {
            rx: self.tx.subscribe(),
            epoch: *self.tx.borrow(),
        }
    }
}

/// How many times a run recovers from a context-overflow stream error by
/// compacting and retrying before the error is surfaced (reference
/// `MAX_OVERFLOW_RECOVERIES`). The counter resets on every successful
/// stream, so a later overflow on another turn still gets its attempt.
pub(crate) const MAX_OVERFLOW_RECOVERIES: u32 = 1;

/// Auth errors tolerated per run before the reauth wait gives up and the
/// error surfaces (reference `MAX_REAUTH_ATTEMPTS`).
pub(crate) const MAX_REAUTH_ATTEMPTS: u32 = 2;

/// Floor for [`clamped_max_tokens`], never applied above the configured cap.
/// Servers like vLLM reject `prompt + max_tokens > window` even when the
/// prompt alone fits; the floor keeps the clamp from trading that overflow
/// for a too-small budget (reference `MIN_OUTPUT_TOKENS`).
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
        Some(window) => window as u64,
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

/// The session's compaction state shared with the run loop: the caller keeps
/// the `Arc` so its pre-turn compaction and the run's overflow recovery read
/// and write the same estimator/effectiveness state.
pub type SharedCompactionState = Arc<std::sync::Mutex<crate::compaction::CompactionState>>;

/// Everything the run loop needs to auto-compact and retry on a context
/// overflow: shared state, configured stages, and the model's window.
#[derive(Clone)]
pub struct CompactionCtx {
    pub state: SharedCompactionState,
    pub stages: Vec<CompactionConfig>,
    pub buffer: CompactionBuffer,
    pub context_length: Option<u32>,
}

/// Request-level settings for one run.
#[derive(Clone)]
pub struct RunParams {
    /// System instructions; `None` or empty omits the preamble.
    pub preamble: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    /// Bound on model calls per run. Interactive runs use [`RunParams::UNBOUNDED`].
    pub max_turns: usize,
    /// Per-turn volatile facts, appended to the last user message of each
    /// request (and never committed to history).
    pub recency: Option<Arc<dyn RecencySource>>,
    /// Tool-output pre-compression applied to the request view only;
    /// history and events always keep the raw results.
    pub compression: CompressionConfig,
    /// Bound on automatic continuations of truncated (`max_tokens`) replies.
    pub max_continuation_turns: usize,
    /// Compaction context for overflow recovery: when set, a context-overflow
    /// stream error triggers recalibration, forced compaction, and one retry;
    /// `None` surfaces the error as before.
    pub compaction: Option<CompactionCtx>,
    /// Retry machine inputs: key-rotation hook and fallback model chain.
    /// Defaults reproduce plain single-model behavior (C.2).
    pub retry: RetryCtx,
    /// Re-authentication hook (E.10): when set, a 401 stream error emits
    /// [`Event::AuthRequired`] and the run waits on this hook (up to
    /// [`MAX_REAUTH_ATTEMPTS`] times) instead of failing. A refreshed model
    /// returned by the hook replaces the stream's target for the rest of the
    /// run; `Ok(None)` retries the current one. `None` fails the run with
    /// the provider's auth error, like the reference's no-user-response
    /// path.
    pub reauth: Option<ReauthHook>,
    /// `provider/model` spec of the run's model, for usage & cost accounting
    /// (H.6). `None` disables pricing; usage counters still accumulate.
    pub model_spec: Option<std::sync::Arc<str>>,
    /// Price turns at the provider's fast/premium tier (2x rates where the
    /// price table defines one). Pricing is correct only if this matches the
    /// tier the provider actually billed, so surfaces must set it whenever
    /// they select fast mode.
    pub fast: bool,
}

/// What a surface calls when the model stream reports an auth error: block
/// until credentials are refreshed, returning the model rebuilt from them
/// (`Ok(None)` when the active model needs no swap), or report failure
/// (`Err`).
pub type ReauthFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<crate::providers::DynamicModel>, String>>
            + Send,
    >,
>;
pub type ReauthHook = std::sync::Arc<dyn Fn(u32) -> ReauthFuture + Send + Sync>;

impl std::fmt::Debug for RunParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunParams")
            .field("preamble", &self.preamble)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("max_turns", &self.max_turns)
            .field("recency", &self.recency.as_ref().map(|_| "<source>"))
            .field("compression", &self.compression)
            .field("max_continuation_turns", &self.max_continuation_turns)
            .field("compaction", &self.compaction.is_some())
            .field("retry", &self.retry)
            .field("reauth", &self.reauth.is_some())
            .field("model_spec", &self.model_spec)
            .finish()
    }
}

impl RunParams {
    pub const UNBOUNDED: usize = usize::MAX;

    /// Reference default (`DEFAULT_MAX_CONTINUATION_TURNS`).
    pub const DEFAULT_MAX_CONTINUATION_TURNS: usize = 3;

    pub fn new(preamble: Option<String>) -> Self {
        Self {
            preamble,
            fast: false,
            temperature: None,
            max_tokens: None,
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
            max_continuation_turns: Self::DEFAULT_MAX_CONTINUATION_TURNS,
            compaction: None,
            retry: RetryCtx::default(),
            reauth: None,
            model_spec: None,
        }
    }

    /// Enable in-run overflow recovery with this compaction context.
    pub fn with_compaction(mut self, compaction: CompactionCtx) -> Self {
        self.compaction = Some(compaction);
        self
    }
}

impl Default for RunParams {
    fn default() -> Self {
        Self {
            preamble: None,
            temperature: None,
            max_tokens: None,
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
            max_continuation_turns: Self::DEFAULT_MAX_CONTINUATION_TURNS,
            compaction: None,
            retry: RetryCtx::default(),
            reauth: None,
            model_spec: None,
            fast: false,
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// The model finished without pending tool calls. `reply` is the final
    /// assistant text; the turn's messages were committed to history.
    Done { reply: String },
    /// The turn budget ran out. The sanitized partial history (with an end
    /// marker) was committed; the next prompt continues from there.
    MaxTurns,
    /// Every continuation of a truncated (`max_tokens`) reply was spent and
    /// the model still stopped on the output limit. The turn's messages
    /// (including the truncated tail) were committed, like `Done`.
    MaxTokens { reply: String },
    /// The run was cancelled; the sanitized partial history (prompt, any
    /// partial assistant text, closed tool calls, cancel marker) was
    /// committed so the conversation can continue from the cut-off point.
    Cancelled,
    /// The run failed; history is not committed.
    Failed(String),
    /// The doom-loop score reached the hard-stop threshold; the run ends
    /// with its sanitized partial history committed, like `MaxTurns`.
    DoomStop,
}

/// Marker appended when a run is cut short, so the model knows the turn ended.
pub(crate) const END_MARKER: &str = "[The turn ended here; the run was cut short.]";

/// Run-wide accumulators for the terminal [`Event::Done`].
#[derive(Default)]
struct RunStats {
    usage: history::Usage,
    context_size: u64,
    turns: u32,
    /// Per-model ledger (H.6): turns are priced when they run and their cost
    /// recorded, so summing is the truth (see `usage::settle_session`).
    by_model: HashMap<String, crate::usage::StoredTokenUsage>,
}

/// The spec to bill: the model that actually answered the call. Retry-chain
/// fallbacks and reauth-refreshed models carry only a bare model id, so the
/// primary spec's provider prefixes it.
fn served_spec(
    primary: Option<&str>,
    served_fallback: Option<&str>,
    refreshed: Option<&crate::providers::DynamicModel>,
) -> Option<Arc<str>> {
    let primary = primary?;
    let label = served_fallback
        .or_else(|| refreshed.and_then(|m| m.label()))
        .unwrap_or_else(|| primary.rsplit_once('/').map_or(primary, |(_, m)| m));
    // A label with a slash is already a full spec (possibly cross-provider);
    // only a bare id borrows the primary's provider.
    let spec = if label.contains('/') {
        label.to_owned()
    } else {
        let provider = primary.split_once('/').map_or(primary, |(p, _)| p);
        format!("{provider}/{label}")
    };
    Some(spec.into())
}

impl RunStats {
    /// Fold one model call's usage into the ledger, pricing the turn against
    /// today's table. An unresolvable spec still counts its tokens, unpriced.
    fn add_usage(&mut self, usage: &history::Usage, spec: Option<&str>, fast: bool) {
        self.usage.add(*usage);
        let Some(spec) = spec else { return };
        let tokens = crate::usage::TokenUsage::from(usage);
        let cost = crate::usage::resolve_spec(spec).and_then(|m| m.billed_cost(&tokens, fast));
        *self.by_model.entry(spec.to_owned()).or_default() += tokens.billed(cost);
    }
}

/// Drive one multi-turn run. `history` is the caller-owned conversation; the
/// prompt is appended as the turn's first user message. `emit` receives every
/// event as it happens (it must not block).
pub async fn run<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    tools: &ToolDispatch,
    history: &mut Vec<Message>,
    prompt: &str,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> RunOutcome {
    let (outcome, mut stats) = run_inner(model, params, tools, history, prompt, cancel, emit).await;
    if let RunOutcome::Failed(message) = &outcome {
        emit(Event::Error(message.clone()));
    }
    // `context_window` is a 0 sentinel until window sizes reach this seam.
    // Cost comes from the per-model ledger: recorded turn costs win;
    // unpriced models settle to `None`, never a made-up "$0.000".
    let cost = if stats.by_model.is_empty() {
        None
    } else {
        crate::usage::settle_session(
            &crate::usage::TokenUsage::default(),
            &mut stats.by_model,
            params.model_spec.as_deref().unwrap_or(""),
            params.fast,
        )
    };
    emit(Event::Done {
        usage: stats.usage,
        context_size: stats.context_size,
        context_window: 0,
        num_turns: stats.turns,
        reason: DoneReason::from(&outcome),
        cost,
        by_model: stats.by_model,
    });
    // Clean (and cancelled) run ends close the capture session onto the
    // `/undo` stack; a failed run leaves it for the next attempt to merge
    // into, mirroring the reference's commit points.
    if !matches!(outcome, RunOutcome::Failed(_))
        && let Some(snapshots) = tools.snapshots()
    {
        snapshots.commit();
    }
    outcome
}

async fn run_inner<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    tools: &ToolDispatch,
    history: &mut Vec<Message>,
    prompt: &str,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> (RunOutcome, RunStats) {
    let definitions = tools.definitions();
    let mut turn = vec![Message::user(prompt)];
    let mut turns = 0;
    let mut stats = RunStats::default();
    // Nudge budget for this run; real progress (tool results) resets it.
    let mut nudges: u32 = 0;
    // Continuations spent on truncated (`max_tokens`) replies.
    let mut continuations: usize = 0;
    // Overflow recoveries spent in a row; reset by every successful stream.
    let mut overflow_recoveries: u32 = 0;
    // Reauth waits spent this run; reset by every successful stream
    // (reference resets `reauth_attempts` on every successful stream).
    let mut reauth_attempts: u32 = 0;
    // Model rebuilt by a successful reauth, used for the rest of the run.
    let mut refreshed_model: Option<crate::providers::DynamicModel> = None;
    // Run-wide transient-retry tally shared by every model call.
    let transient_budget = retry::TransientBudget::default();
    // Measured input tokens from the last completed stream; feeds the
    // output-token clamp a number better than the chars/4 estimate.
    let mut measured_prompt_tokens: u64 = 0;
    // Doom-loop tracker for this run; a trailing grace prompt from a
    // previous run must not replay as if the user asked for it.
    strip_trailing_grace_prompt(history);
    let mut doom = doom::DoomTracker::new();
    let mut recent = doom::RecentCalls::default();
    loop {
        if cancel.cancelled() {
            return (commit_cancelled(history, &mut turn), stats);
        }
        if doom.should_hard_stop() {
            return (
                commit_partial(history, &mut turn, RunOutcome::DoomStop),
                stats,
            );
        }
        if doom.should_grace() {
            doom.mark_grace_called();
            turn.push(Message::user(doom::GRACE_CALL_PROMPT));
            continue;
        }
        let mut full = history.clone();
        full.extend(turn.iter().cloned());
        // Volatile recency facts ride only this request; `full` is discarded.
        if let Some(source) = &params.recency
            && let Some(with_tail) = recency::recency_view(source, &full, turns as u32)
        {
            full = with_tail;
        }
        read_lifecycle::apply_to_request(&mut full, tools.compression_store());
        compress_request_view(&mut full, &params.compression);
        let prompt_tokens = crate::compaction::estimate_tokens(&full).max(measured_prompt_tokens);
        let window = params.compaction.as_ref().and_then(|c| c.context_length);
        let request = edge::to_request(
            &full,
            &definitions,
            params.preamble.as_deref(),
            params.temperature,
            clamped_max_tokens(window, prompt_tokens, params.max_tokens),
        );
        let (output, served_spec) = match match refreshed_model.as_ref() {
            Some(refreshed) => {
                retry::stream_with_retry(
                    refreshed,
                    &[],
                    params.retry.rotate.as_ref(),
                    &transient_budget,
                    &request,
                    cancel,
                    emit,
                )
                .await
            }
            None => {
                retry::stream_with_retry(
                    model,
                    &params.retry.fallbacks,
                    params.retry.rotate.as_ref(),
                    &transient_budget,
                    &request,
                    cancel,
                    emit,
                )
                .await
            }
        } {
            Ok((output, served_fallback)) => {
                // Bill the model that actually answered: a retry-chain
                // fallback or a reauth-refreshed model, not the primary.
                let served_spec = served_spec(
                    params.model_spec.as_deref(),
                    served_fallback,
                    refreshed_model.as_ref(),
                );
                (output, served_spec)
            }
            Err(stream::StreamFailure::Cancelled { streamed }) => {
                // Keep the partial reply the user already saw, so the next
                // prompt replays from what was on screen.
                if !streamed.is_empty() {
                    turn.push(Message::Assistant {
                        content: vec![history::AssistantContent::text(streamed)],
                    });
                }
                return (commit_cancelled(history, &mut turn), stats);
            }
            // The gauge is a chars/4 floor, so a prompt can overflow with the
            // thresholds unmet. Compaction is the only way out, so run it and
            // retry once; a second consecutive overflow means compaction did
            // not help and the error is the honest answer (reference
            // `TurnOutcome::Overflow`).
            Err(failure) if failure.is_overflow() => {
                let Some(message) = failure.message().map(str::to_owned) else {
                    unreachable!("is_overflow only matches Error");
                };
                if overflow_recoveries >= MAX_OVERFLOW_RECOVERIES {
                    return (RunOutcome::Failed(message), stats);
                }
                overflow_recoveries += 1;
                if !recover_from_overflow(params, model, history, &mut doom, emit).await {
                    return (RunOutcome::Failed(message), stats);
                }
                // The pre-compaction measurement no longer describes the
                // compacted context; drop it so the clamp trusts the
                // estimate until the next real usage report.
                measured_prompt_tokens = 0;
                continue;
            }
            // Auth failure: pause for re-authentication instead of failing
            // (E.10). Without a responder — or past the attempt budget — the
            // error is the honest answer, like the reference's no-rx path.
            Err(failure) if failure.is_auth() => {
                let Some(message) = failure.message().map(str::to_owned) else {
                    unreachable!("is_auth only matches Error");
                };
                let Some(reauth) = params.reauth.clone() else {
                    return (RunOutcome::Failed(message), stats);
                };
                if reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                    return (RunOutcome::Failed(message), stats);
                }
                reauth_attempts += 1;
                emit(Event::AuthRequired {
                    attempt: reauth_attempts,
                    message: message.clone(),
                });
                match tokio::select! {
                    biased;
                    _ = cancel.wait() => None,
                    r = reauth(reauth_attempts) => Some(r),
                } {
                    Some(Ok(refreshed)) => {
                        refreshed_model = refreshed.or(refreshed_model);
                        continue;
                    }
                    Some(Err(e)) => return (RunOutcome::Failed(e), stats),
                    None => return (commit_cancelled(history, &mut turn), stats),
                }
            }
            Err(failure) => {
                let message = failure
                    .message()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "stream cancelled".into());
                return (RunOutcome::Failed(message), stats);
            }
        };
        overflow_recoveries = 0;
        reauth_attempts = 0;
        // Track the last measured count (zero is the missing-report
        // sentinel, kept at the previous value); it naturally shrinks
        // again after compaction, unlike a running max.
        if output.usage.input_tokens > 0 {
            measured_prompt_tokens = output.usage.input_tokens;
        }
        stats.add_usage(&output.usage, served_spec.as_deref(), params.fast);
        stats.context_size = crate::compaction::estimate_tokens(&full);
        stats.turns = turns as u32 + 1;
        emit(Event::TurnComplete {
            usage: output.usage,
            context_size: stats.context_size,
        });
        let Message::Assistant { content } = &output.assistant else {
            return (
                RunOutcome::Failed("model produced a non-assistant message".into()),
                stats,
            );
        };
        let tool_calls: Vec<history::ToolCall> = content
            .iter()
            .filter_map(|block| match block {
                history::AssistantContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        turn.push(output.assistant);
        if tool_calls.is_empty() {
            let reply = turn.last().expect("assistant pushed").text();
            // A truncated reply continues: the model's cut-off message is
            // already in `turn`, so the next request resumes from it.
            let truncated = output.finish_reason == Some(FinishReason::Length);
            if let Some(outcome) = handle_terminal_reply(
                history,
                &mut turn,
                &full,
                &reply,
                truncated,
                &mut nudges,
                &mut turns,
                params,
                &mut continuations,
                emit,
            ) {
                return (outcome, stats);
            }
            continue;
        }
        let (stopped, batch) =
            dispatch_tool_calls(tools, &mut turn, tool_calls, &mut recent, cancel, emit).await;
        if let Some(outcome) = stopped {
            if matches!(outcome, RunOutcome::Cancelled) {
                return (commit_cancelled(history, &mut turn), stats);
            }
            return (outcome, stats);
        }
        for _ in 0..batch.doom_loops {
            doom.note_doom_loop();
        }
        for _ in 0..batch.errors {
            doom.note_tool_error();
        }
        for _ in 0..batch.successes {
            doom.note_tool_success();
        }
        turns += 1;
        nudges = 0;
        if turns >= params.max_turns {
            let outcome = commit_partial(history, &mut turn, RunOutcome::MaxTurns);
            return (outcome, stats);
        }
    }
}

/// Recalibrate and force a compaction after the request overflowed the
/// context window. Returns whether recovery is possible at all (a run without
/// a compaction context cannot recover). `history` is compacted in place; the
/// un-committed turn is left intact and re-appended by the retried request.
async fn recover_from_overflow<M: CompletionModel + Clone>(
    params: &RunParams,
    model: &M,
    history: &mut Vec<Message>,
    doom: &mut doom::DoomTracker,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> bool {
    let Some(ctx) = &params.compaction else {
        return false;
    };
    // `maybe_compact` awaits, so the engine runs on a clone; recalibration
    // and the write-back happen in short critical sections against the
    // live state so a concurrent session-side update is not clobbered.
    let Some(mut state) = ctx.state.lock().ok().map(|mut guard| guard.clone()) else {
        return false;
    };
    let estimated = crate::compaction::estimate_tokens(history);
    // A failed request reports no usage: `actual = 0` makes recalibration a
    // safe no-op. The window bound in the error text is never used as the
    // actual prompt size (it would over-inflate the multiplier).
    state.recalibrate(0, estimated);
    let window = ctx.context_length.map(u64::from).unwrap_or(0);
    let before = state.estimator.scale(estimated);
    emit(Event::AutoCompacting {
        context_size: before,
        context_window: window,
    });
    CompactionEngine::new(ctx.stages.clone())
        .with_buffer(ctx.buffer)
        .maybe_compact(&mut state, model, history, ctx.context_length)
        .await;
    let after = state
        .estimator
        .scale(crate::compaction::estimate_tokens(history));
    // Compaction that barely shrank the context is itself a doom signal;
    // one that paid off earns a decay.
    let savings = if before > 0 {
        1.0 - (after as f32 / before as f32)
    } else {
        0.0
    };
    if savings < doom::INEFFECTIVE_COMPACTION_THRESHOLD {
        doom.note_ineffective_compaction();
    } else {
        doom.note_effective_compaction();
    }
    emit(Event::CompactionDone {
        context_size_before: before,
        context_size_after: after,
        context_window: window,
    });
    if let Ok(mut guard) = ctx.state.lock() {
        guard.absorb_run(&state);
    }
    true
}

/// Commit a partial turn and end the run at its budget or the doom hard
/// stop: sanitized so dangling tool calls replay cleanly on the next request.
fn commit_partial(
    history: &mut Vec<Message>,
    turn: &mut Vec<Message>,
    outcome: RunOutcome,
) -> RunOutcome {
    sanitize_partial(turn);
    history.append(turn);
    outcome
}

/// Drop a trailing grace prompt left in committed history by a previous
/// run, so it does not replay as if the user asked for it (reference
/// `strip_trailing_grace_prompt`).
fn strip_trailing_grace_prompt(history: &mut Vec<Message>) {
    if let Some(Message::User { content }) = history.last()
        && content.len() == 1
        && let history::UserContent::Text(text) = &content[0]
        && text.text == doom::GRACE_CALL_PROMPT
    {
        history.pop();
    }
}

/// Marker appended when a run is cancelled by the user, so the model knows
/// where the turn stopped (reference `history.rs` `CANCEL_MARKER`).
pub(crate) const CANCEL_MARKER: &str = "[Cancelled by user]";

/// Commit the partial turn of a cancelled run: the prompt and whatever the
/// model produced are kept, dangling tool calls are closed with an error
/// result, and the cancel marker records the cut-off.
fn commit_cancelled(history: &mut Vec<Message>, turn: &mut Vec<Message>) -> RunOutcome {
    close_dangling_calls(turn, "skipped: cancelled by the user");
    turn.push(Message::user(CANCEL_MARKER));
    history.append(turn);
    RunOutcome::Cancelled
}

/// Handle an assistant turn with no tool calls. Continues truncated and
/// empty replies while their budgets allow; otherwise commits history and
/// returns the run outcome. `None` means "keep looping".
fn handle_terminal_reply(
    history: &mut Vec<Message>,
    turn: &mut Vec<Message>,
    full: &[Message],
    reply: &str,
    truncated: bool,
    nudges: &mut u32,
    turns: &mut usize,
    params: &RunParams,
    continuations: &mut usize,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Option<RunOutcome> {
    if truncated && *continuations < params.max_continuation_turns {
        *continuations += 1;
        *turns += 1;
        if *turns >= params.max_turns {
            return Some(commit_partial(history, turn, RunOutcome::MaxTurns));
        }
        return None;
    }
    // A truncated reply is never "empty-and-stalled": even a
    // zero-visible-text truncation keeps its cut-off message and
    // ends MaxTokens, so the nudge path below never swallows it.
    if reply.trim().is_empty() && !truncated {
        // The marker takes the silent reply's place in history.
        turn.pop();
        // `full` is the exact view the model just saw (its wire-only
        // rewrites are shape-preserving); the trailing marker+nudge
        // pairs this run already pushed are skipped by count.
        let nudge = *nudges < nudge::MAX_NUDGES
            && nudge::has_recent_tool_results(
                full,
                nudge::RECENT_TOOL_WINDOW,
                2 * *nudges as usize,
            );
        nudge::stall_turn(turn, nudge);
        if nudge {
            *nudges += 1;
            emit(Event::Nudge);
            *turns += 1;
            if *turns >= params.max_turns {
                return Some(commit_partial(history, turn, RunOutcome::MaxTurns));
            }
            return None;
        }
    }
    history.append(turn);
    Some(if truncated {
        RunOutcome::MaxTokens {
            reply: reply.to_owned(),
        }
    } else {
        RunOutcome::Done {
            reply: reply.to_owned(),
        }
    })
}

/// Tools that must never share a wave with another call: `batch` nests its
/// own parallel dispatch and `question` (not yet ported) blocks on the user.
fn is_never_parallel(name: &str) -> bool {
    matches!(name, "batch" | "question")
}

/// Execute the turn's tool calls, appending their results to `turn`.
/// Returns `Some(outcome)` when the run must stop (cancel or dispatch
/// failure); `None` means the loop continues.
///
/// Calls run concurrently in waves; a wave is joined and drained before the
/// next starts whenever two calls write the same path or a
/// never-parallel tool joins the batch. Results are committed in call
/// order; a panicking tool future becomes an error result instead of
/// unwinding the run.
async fn dispatch_tool_calls(
    tools: &ToolDispatch,
    turn: &mut Vec<Message>,
    calls: Vec<history::ToolCall>,
    recent: &mut doom::RecentCalls,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> (Option<RunOutcome>, doom::ToolBatchOutcome) {
    // Spawned tasks need owned state; the dispatch table is cheap to clone.
    let tools = Arc::new(tools.clone());
    let mut set = task_set::TaskSet::new();
    let mut wave: Vec<history::ToolCall> = Vec::new();
    let mut all_write_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut batch = doom::ToolBatchOutcome::default();

    for call in calls {
        if cancel.cancelled() {
            let outcome =
                commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await;
            return (outcome.or(Some(RunOutcome::Cancelled)), batch);
        }
        let name = call.function.name.clone();
        let arguments = call.function.arguments.clone();
        if recent.is_doom_loop(&name, &arguments) {
            // The call is blocked, not executed: emit the reference's error
            // result directly and clear the window so the warning does not
            // re-fire identically on the next retry.
            batch.doom_loops += 1;
            let result = history::ToolResult {
                call: call.id.clone(),
                name: name.clone(),
                content: vec![history::ToolResultContent::text(doom::DOOM_LOOP_MESSAGE)],
                is_error: true,
            };
            turn.push(Message::User {
                content: vec![history::UserContent::ToolResult(result.clone())],
            });
            emit(Event::ToolDone {
                id: call.id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                result,
            });
            recent.clear();
        } else {
            // A never-parallel tool or a repeat write path must not share a
            // wave with earlier calls, so the pending wave is flushed
            // *before* this call is spawned into a fresh one.
            let write_paths = dedup::extract_write_paths(&name, &call.function.arguments);
            let conflicts = is_never_parallel(&name)
                || write_paths
                    .iter()
                    .any(|path| all_write_paths.contains(path));
            if conflicts && !wave.is_empty() {
                if let Some(outcome) =
                    commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await
                {
                    return (Some(outcome), batch);
                }
                set = task_set::TaskSet::new();
                all_write_paths.clear();
            }
            all_write_paths.extend(write_paths);
            let executor = Arc::clone(&tools);
            wave.push(call.clone());
            set.spawn(async move { executor.execute(call).await });
        }
        recent.record(name, &arguments);
    }
    let stopped = commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await;
    (stopped, batch)
}

/// One finished wave entry: the call paired with its dispatch outcome, or
/// the panic/cancellation string when the tool task itself failed.
type WaveEntry = (
    history::ToolCall,
    Result<Result<DispatchOutcome, String>, String>,
);

/// Join a finished wave, pairing each spawn-order result with its call.
async fn join_wave(
    set: task_set::TaskSet<Result<DispatchOutcome, String>>,
    wave: std::vec::Drain<'_, history::ToolCall>,
) -> Vec<WaveEntry> {
    let ids: Vec<history::ToolCall> = wave.collect();
    set.join_all()
        .await
        .into_iter()
        .zip(ids)
        .map(|(outcome, call)| (call, outcome))
        .collect()
}

/// Commit one wave's results to `turn` in call order, emitting `ToolDone`
/// per call. `Some(outcome)` when the run must stop.
async fn commit_wave(
    results: Vec<WaveEntry>,
    turn: &mut Vec<Message>,
    batch: &mut doom::ToolBatchOutcome,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Option<RunOutcome> {
    let mut wave_results: Vec<history::ToolResult> = Vec::new();
    for (call, outcome) in results {
        let result = match outcome {
            Ok(Ok(DispatchOutcome::Ran(result))) | Ok(Ok(DispatchOutcome::Skipped(result))) => {
                result
            }
            Ok(Ok(DispatchOutcome::Stopped(_))) => return Some(RunOutcome::Cancelled),
            Ok(Err(unknown)) => return Some(RunOutcome::Failed(unknown)),
            // The task itself failed: a panic or cancellation inside the
            // tool future, reported as a per-call error result.
            Err(panic) => history::ToolResult {
                call: call.id.clone(),
                name: call.function.name.clone(),
                content: vec![history::ToolResultContent::text(format!(
                    "internal error: tool panicked: {panic}"
                ))],
                is_error: true,
            },
        };
        if result.is_error {
            batch.errors += 1;
        } else {
            batch.successes += 1;
        }
        turn.push(Message::User {
            content: vec![history::UserContent::ToolResult(result.clone())],
        });
        wave_results.push(result.clone());
        emit(Event::ToolDone {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
            result,
        });
    }
    if !wave_results.is_empty() {
        // One submission event per wave, carrying all of its results in
        // call order (reference semantics), independent of the per-call
        // messages committed to history above.
        emit(Event::ToolResultsSubmitted {
            message: Message::User {
                content: wave_results
                    .into_iter()
                    .map(history::UserContent::ToolResult)
                    .collect(),
            },
        });
    }
    None
}

/// Rewrite tool-result texts in the request copy through pre-compression.
/// Only the wire view is affected: `turn` and `history` keep raw results.
/// Request-time compression is unconditional (per the reference);
/// `protect_recent_tool_outputs` is a compaction-stage knob, not ours.
fn compress_request_view(full: &mut [Message], config: &CompressionConfig) {
    for message in full {
        let Message::User { content } = message else {
            continue;
        };
        for block in content {
            if let history::UserContent::ToolResult(result) = block {
                // Verbatim tools (e.g. `read`) return caller-selected content;
                // compressing it would drop lines the model explicitly asked for.
                if !compression::should_compress_tool(&result.name) {
                    continue;
                }
                for item in &mut result.content {
                    if let history::ToolResultContent::Text(text) = item {
                        text.text = compression::compress_for_llm(&text.text, config);
                    }
                }
            }
        }
    }
}

/// Close dangling tool calls and append the end marker, so the committed
/// partial history replays cleanly on the next request.
pub(crate) fn sanitize_partial(turn: &mut Vec<Message>) {
    close_dangling_calls(turn, "skipped: the turn ended before this call ran");
    turn.push(Message::user(END_MARKER));
}

/// Append error results for every tool call in the turn that never got an
/// answer, so the trailing assistant message is API-valid on replay.
fn close_dangling_calls(turn: &mut Vec<Message>, note: &str) {
    let mut dangling: Vec<history::ToolCall> = Vec::new();
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
    for message in turn.iter() {
        match message {
            Message::Assistant { content } => {
                for block in content {
                    if let history::AssistantContent::ToolCall(call) = block {
                        dangling.push(call.clone());
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let history::UserContent::ToolResult(result) = block {
                        answered.insert(result.call.clone());
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    let open: Vec<history::ToolCall> = dangling
        .into_iter()
        .filter(|call| !answered.contains(&call.id))
        .collect();
    if !open.is_empty() {
        let content = open
            .iter()
            .map(|call| {
                history::UserContent::ToolResult(history::ToolResult {
                    call: call.id.clone(),
                    name: call.function.name.clone(),
                    content: vec![history::ToolResultContent::text(note)],
                    is_error: true,
                })
            })
            .collect();
        turn.push(Message::User { content });
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod run_tests;
