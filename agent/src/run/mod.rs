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

pub mod advisor;
pub mod cancel;
pub mod dedup;
pub mod dispatch;
pub(crate) mod doom;
pub mod events;
pub mod guardrails;
pub mod mode;
mod nudge;
mod overflow;
mod read_lifecycle;
mod recency;
mod retry;
mod stats;
mod stream;
mod task_set;
mod turns;
mod view;

pub use cancel::{CancelFlag, CancelToken, cancel_channel};
pub use dedup::{SharedDedupCache, ToolDedupCache, shared_cache};
pub(crate) use dispatch::dispatch_tool_calls;
pub use dispatch::{
    AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome, ToolDispatch,
};
pub use events::{
    DoneReason, Envelope, Event, EventSender, EventStreamGuard, SessionEvents, event_stream,
};
pub use guardrails::{SharedGuardrails, shared_guardrails};
pub use mode::{AgentMode, PLAN_WRITE_RESTRICTED};
#[cfg(test)]
use overflow::{CANCEL_MARKER, END_MARKER};
use overflow::{
    commit_cancelled, commit_partial, handle_terminal_reply, recover_from_overflow,
    sanitize_partial, strip_trailing_grace_prompt,
};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use retry::RetryCtx;
use stats::{RunStats, served_spec};
pub use stream::TurnOutput;
use view::compress_request_view;

use std::sync::Arc;

use rig_core::completion::CompletionModel;

use crate::compression::CompressionConfig;
use crate::config::{CompactionBuffer, CompactionConfig};
use crate::edge;
use crate::history::{ImageBlock, Message};
// Module names for the test tree: `run/tests/*` qualifies `history::` and
// `compression::` through this scope.
#[cfg(test)]
use crate::compression;
#[cfg(test)]
use crate::history;

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
    pub thinking: crate::thinking::ThinkingConfig,
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
    /// Post-turn advisor (C.12). Disabled by default; when enabled, a
    /// terminal reply triggers one no-tools delta review whose note may
    /// continue the run (see [`advisor::should_act`]).
    pub advisor: crate::config::AdvisorConfig,
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
            .field("thinking", &self.thinking)
            .field("max_turns", &self.max_turns)
            .field("recency", &self.recency.as_ref().map(|_| "<source>"))
            .field("compression", &self.compression)
            .field("max_continuation_turns", &self.max_continuation_turns)
            .field("compaction", &self.compaction.is_some())
            .field("retry", &self.retry)
            .field("reauth", &self.reauth.is_some())
            .field("model_spec", &self.model_spec)
            .field("advisor", &self.advisor)
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
            temperature: None,
            max_tokens: None,
            thinking: Default::default(),
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
            max_continuation_turns: Self::DEFAULT_MAX_CONTINUATION_TURNS,
            compaction: None,
            retry: RetryCtx::default(),
            reauth: None,
            model_spec: None,
            fast: false,
            advisor: crate::config::AdvisorConfig::default(),
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
            thinking: Default::default(),
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
            max_continuation_turns: Self::DEFAULT_MAX_CONTINUATION_TURNS,
            compaction: None,
            retry: RetryCtx::default(),
            reauth: None,
            model_spec: None,
            fast: false,
            advisor: crate::config::AdvisorConfig::default(),
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
    run_with_images(model, params, tools, history, prompt, &[], cancel, emit).await
}

/// [`run`] with image attachments staged on the prompt message (F.6:
/// composer path picks and clipboard pastes).
// Clippy: the loop seam is deliberately flat (model, params, tools,
// history, prompt, attachments, cancel, emit) rather than a builder.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_images<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    tools: &ToolDispatch,
    history: &mut Vec<Message>,
    prompt: &str,
    images: &[crate::history::ImageBlock],
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> RunOutcome {
    let (outcome, mut stats) =
        run_inner(model, params, tools, history, prompt, images, cancel, emit).await;
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

#[allow(clippy::too_many_arguments)]
async fn run_inner<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    tools: &ToolDispatch,
    history: &mut Vec<Message>,
    prompt: &str,
    images: &[crate::history::ImageBlock],
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> (RunOutcome, RunStats) {
    // A trailing grace prompt from a previous run must not replay as if
    // the user asked for it, and a failed run must not keep the overflow
    // recovery's in-place compaction: keep a pristine snapshot and restore
    // it on failure. The failed turn itself is still committed, though —
    // tool calls that already ran have real workspace effects, and
    // dropping them desyncs both the model and any resumed session from
    // what actually happened.
    let pristine = history.clone();
    let carry_anchor: Option<Option<usize>> = params
        .compaction
        .as_ref()
        .map(|ctx| ctx.state.lock().ok().and_then(|guard| guard.carry_from()));
    strip_trailing_grace_prompt(history);
    let (outcome, stats, mut leftover) =
        run_loop(model, params, tools, history, prompt, images, cancel, emit).await;
    if let RunOutcome::Failed(message) = &outcome {
        *history = pristine;
        sanitize_partial(&mut leftover);
        leftover.push(Message::user(format!("[Run failed: {message}]")));
        history.append(&mut leftover);
        if let (Some(ctx), Some(anchor)) = (&params.compaction, carry_anchor)
            && let Ok(mut guard) = ctx.state.lock()
        {
            match anchor {
                Some(index) => guard.protect_from(index),
                None => guard.mark_answered(),
            }
        }
    }
    (outcome, stats)
}

/// The turn's first user message: the prompt text, with any composer image
/// attachments as trailing vision blocks (F.6).
fn prompt_message(prompt: &str, images: &[ImageBlock]) -> Message {
    if images.is_empty() {
        return Message::user(prompt);
    }
    let mut content = vec![crate::history::UserContent::text(prompt)];
    content.extend(
        images
            .iter()
            .map(|image| crate::history::UserContent::Image(image.clone())),
    );
    Message::User { content }
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    tools: &ToolDispatch,
    history: &mut Vec<Message>,
    prompt: &str,
    images: &[crate::history::ImageBlock],
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> (RunOutcome, RunStats, Vec<Message>) {
    let definitions = tools.definitions();
    // The prompt message carries any composer image attachments; later
    // grace/nudge prompts are plain text.
    let mut turn = vec![prompt_message(prompt, images)];
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
    // Doom-loop tracker for this run.
    let mut doom = doom::DoomTracker::new();
    let mut recent = doom::RecentCalls::default();
    // Advisor state (C.12): reviews only this run's messages, so the cursor
    // starts after the pre-existing history.
    let mut advisor_state = params
        .advisor
        .enabled
        .then(|| advisor::AdvisorState::with_dedup(params.advisor.dedup_size));
    if let Some(state) = advisor_state.as_mut() {
        state.last_reviewed = history.len();
    }
    let mut advisor_continuations: u32 = 0;
    loop {
        if cancel.cancelled() {
            return (commit_cancelled(history, &mut turn), stats, turn);
        }
        if doom.should_hard_stop() {
            return (
                commit_partial(history, &mut turn, RunOutcome::DoomStop),
                stats,
                turn,
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
        let write_root = tools
            .write_root()
            .map(|root| root.to_string_lossy().into_owned());
        read_lifecycle::apply_to_request(
            &mut full,
            tools.compression_store(),
            write_root.as_deref(),
        );
        compress_request_view(&mut full, &params.compression);
        let prompt_tokens = crate::compaction::estimate_tokens(&full).max(measured_prompt_tokens);
        let window = params.compaction.as_ref().and_then(|c| c.context_length);
        let mut request = edge::to_request(
            &full,
            &definitions,
            params.preamble.as_deref(),
            params.temperature,
            clamped_max_tokens(window, prompt_tokens, params.max_tokens),
        );
        crate::thinking::attach(&mut request, params.thinking);
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
            turns::Streamed::Stop(outcome) => return (outcome, stats, turn),
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
                    turns::TurnEnd::Stop(outcome) => {
                        // Advisor gate (C.12): one delta review after a
                        // terminal reply, before the run result escapes.
                        if let (true, Some(state)) =
                            (params.advisor.enabled, advisor_state.as_mut())
                            && matches!(outcome, RunOutcome::Done { .. })
                            && !cancel.cancelled()
                        {
                            emit(Event::Info(advisor::ADVISOR_REVIEWING_INFO.into()));
                            // No cancel race: the token's `wait`/`race` also
                            // resolve when the flag half is dropped, which
                            // would silently skip every review; the deadline
                            // bounds the call and the token is checked after
                            // (same deviation as the auto-reviewer).
                            let note = match refreshed_model.as_ref() {
                                Some(m) => advisor::review(m, state, history).await,
                                None => advisor::review(model, state, history).await,
                            };
                            if cancel.cancelled() {
                                return (commit_cancelled(history, &mut turn), stats, turn);
                            }
                            if let Some(note) = note {
                                emit(Event::AdvisorNote {
                                    severity: note.severity.as_str().to_string(),
                                    message: note.message.clone(),
                                });
                                if let advisor::AdvisorTurnAction::Continue(note) =
                                    advisor::advisor_turn_action(
                                        Some(note),
                                        &params.advisor,
                                        advisor_continuations,
                                    )
                                {
                                    advisor_continuations += 1;
                                    emit(Event::Info(advisor::advisor_continuation_info(
                                        &note,
                                        advisor_continuations,
                                        params.advisor.max_act_turns,
                                    )));
                                    turn.push(advisor::advisor_followup_message(&note));
                                    continue;
                                }
                            }
                        }
                        return (outcome, stats, turn);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
// The module is `run_tests` (a sibling inline `mod tests` exists below);
// the split harness lives in `tests/mod.rs`.
#[path = "tests/mod.rs"]
mod run_tests;

#[cfg(test)]
mod tests {
    use super::prompt_message;
    use crate::history::{ImageBlock, ImageMedia, Message, UserContent};

    #[test]
    fn prompt_message_is_plain_text_without_attachments() {
        let msg = prompt_message("hello", &[]);
        assert_eq!(msg.text(), "hello");
    }

    #[test]
    fn prompt_message_carries_image_attachments() {
        let images = vec![ImageBlock {
            media_type: ImageMedia::Png,
            data: "aGk=".into(),
            caption: "[image]".into(),
        }];
        let msg = prompt_message("look", &images);
        let UserContent::Image(block) = &msg_user_content(&msg)[1] else {
            panic!("image attachment");
        };
        assert_eq!(block.data, "aGk=");
    }

    fn msg_user_content(msg: &Message) -> &Vec<UserContent> {
        match msg {
            Message::User { content } => content,
            _ => panic!("user message"),
        }
    }
}
