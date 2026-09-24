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
mod overflow;
mod read_lifecycle;
mod recency;
mod retry;
mod stream;
mod task_set;
mod turns;

pub use dedup::{SharedDedupCache, ToolDedupCache, shared_cache};
pub(crate) use dispatch::dispatch_tool_calls;
pub use dispatch::{
    AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome, ToolDispatch,
};
pub use events::{Envelope, EventSender, EventStreamGuard, SessionEvents, event_stream};
pub use guardrails::{SharedGuardrails, shared_guardrails};
#[cfg(test)]
use overflow::{CANCEL_MARKER, END_MARKER};
use overflow::{
    commit_cancelled, commit_partial, handle_terminal_reply, recover_from_overflow,
    strip_trailing_grace_prompt,
};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use retry::RetryCtx;
pub use stream::TurnOutput;

use std::collections::HashMap;
use std::sync::Arc;

use rig_core::completion::CompletionModel;
use tokio::sync::watch;

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
    ToolPending { id: String, name: String },
    /// The model issued a tool call.
    ToolStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// `content` is the full accumulated output so far, not a delta.
    ToolOutput { id: String, content: String },
    /// A tool call finished (ran, failed, or was skipped).
    ToolDone {
        id: String,
        name: String,
        arguments: serde_json::Value,
        result: history::ToolResult,
    },
    /// A wave of tool results was appended to the turn; `message` carries
    /// every result of the wave in call order.
    ToolResultsSubmitted { message: Message },
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
    /// The doom-loop grace prompt was injected (fires exactly once per
    /// run, at the grace threshold). `similarity` is reference-taxonomy
    /// residue: here it carries the doom score normalized toward
    /// `HARD_STOP_THRESHOLD` (1.0 = about to hard-stop).
    StagnationDetected { similarity: f32 },
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
    AuthRequired { attempt: u32, message: String },
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
            // Fires exactly once per run (the grace flag above): the doom
            // score normalized toward the hard-stop threshold rides in
            // `similarity` (see the variant's doc comment).
            emit(Event::StagnationDetected {
                similarity: doom.score() as f32 / doom::HARD_STOP_THRESHOLD as f32,
            });
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
        match turns::stream_turn(
            model,
            params,
            cancel,
            emit,
            history,
            &mut turn,
            &mut doom,
            &mut refreshed_model,
            &mut overflow_recoveries,
            &mut reauth_attempts,
            &mut measured_prompt_tokens,
            &transient_budget,
            &request,
        )
        .await
        {
            turns::Streamed::Retry => continue,
            turns::Streamed::Stop(outcome) => return (outcome, stats),
            turns::Streamed::Turn(output, served_spec) => {
                match turns::finish_turn(
                    params,
                    tools,
                    cancel,
                    emit,
                    history,
                    &mut turn,
                    &full,
                    output,
                    served_spec,
                    &mut stats,
                    &mut nudges,
                    &mut turns,
                    &mut continuations,
                    &mut doom,
                    &mut recent,
                    &mut overflow_recoveries,
                    &mut reauth_attempts,
                    &mut measured_prompt_tokens,
                )
                .await
                {
                    turns::TurnEnd::Continue => continue,
                    turns::TurnEnd::Stop(outcome) => return (outcome, stats),
                }
            }
        }
    }
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

#[cfg(test)]
#[path = "tests.rs"]
mod run_tests;
