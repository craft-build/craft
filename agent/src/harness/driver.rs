//! Turn driver: the thin ring around `Agent::run` that no layer hook can
//! cover. It builds the input, runs the agent with its animist event
//! channel drained into the conversation sink, awaits the run after
//! cancellation, adds the terminal markers, and reports the folded usage.
//! The one re-run territory left here: the `ContextWindowExceeded`
//! stop reason (terminal by animist's rules, unreachable from `step_end`)
//! and the advisor's post-turn review — both as follow-up runs that carry
//! the turn's budgets over.

use std::sync::{Arc, Mutex};

use animist::agent::{AgentError, AgentEvent, Cancellation, RunOptions, RunReport, RunStatus};
use animist::history::{ContentBlock, History, Message, Role, StoredMessage};

use crate::history::{ext, marker};
use crate::usage::{StoredTokenUsage, TokenUsage};

use super::continuation::CONTINUATION_PROMPT;
use super::conversation::Conversation;
use super::events::{CraftEvent, DoneReason, Event, EventSink};
use super::recovery::RecoveryRunState;

/// Marker text appended when a run is cut short by its step budget, so the
/// model knows the turn ended. Identity lives in the [`marker::END`] tag.
pub(crate) const END_MARKER: &str = "[The turn ended here; the run was cut short.]";

/// Marker text appended when a run is cancelled by the user. Identity lives
/// in the [`marker::CANCEL`] tag.
pub(crate) const CANCEL_MARKER: &str = "[Cancelled by user]";

/// Between-runs history edits: the durable alteration passes first,
/// then the compaction engine's stages. The driver owns the sequencing —
/// this never runs concurrently with `Agent::run` — so a `Conflict` from a
/// pass means an unexpected writer won the race; each edit is retried once
/// with a fresh snapshot and never re-runs summarizer work implicitly.
/// `/compact` calls this with `force = true`; the pre-turn and post-turn
/// sites call it with `false`.
pub async fn compact_between_runs(conversation: &Conversation, force: bool) -> bool {
    let history = conversation.history();
    let (compression, root) = (conversation.compression_config(), conversation.write_root());
    if let Err(error) =
        crate::compaction::alter::run_passes(&history, &compression, root.as_deref()).await
    {
        tracing::warn!("alteration pass conflicted ({error}); retrying once");
        if let Err(error) =
            crate::compaction::alter::run_passes(&history, &compression, root.as_deref()).await
        {
            tracing::warn!("alteration pass conflicted twice; edits skipped: {error}");
        }
    }
    let engine = conversation.compaction_engine();
    let (state, model, sink) = (
        conversation.compaction(),
        conversation.model(),
        conversation.sink(),
    );
    if force {
        engine
            .force_compact(&state, &model, &history, model.context_length(), &sink)
            .await
    } else {
        engine
            .maybe_compact(&state, &model, &history, model.context_length(), &sink)
            .await
    }
}

/// Bounded per-run animist event channel; drained concurrently into the
/// sink so backpressure never stalls the run.
const RUN_EVENT_CHANNEL_CAPACITY: usize = 256;

/// How a turn ended.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// The model finished without pending tool calls; `reply` is the final
    /// assistant text.
    Done { reply: String },
    /// The step budget ran out; the partial history (with an end marker)
    /// committed, so the next prompt continues from there.
    MaxTurns,
    /// Generation hit the output token limit and every continuation of the
    /// truncated reply was spent; the truncated tail is committed like a
    /// completed reply.
    MaxTokens { reply: String },
    /// The run was cancelled; the interrupted attempt's validated prefix
    /// and the cancel marker committed.
    Cancelled,
    /// The doom-loop hard stop ended the run; partial history with an end
    /// marker committed, so the next prompt continues from there.
    DoomStop,
    /// The run failed; the committed messages stay (real workspace effects
    /// happened) and a `[Run failed]` observation records the failure.
    Failed(String),
}

/// The turn's first user message: the prompt text, with any composer image
/// attachments as trailing vision blocks.
fn input_message(prompt: &str, images: &[ContentBlock]) -> StoredMessage {
    let mut content = vec![ContentBlock::Text {
        text: prompt.to_owned(),
    }];
    content.extend(images.iter().cloned());
    ext::stored(Message {
        role: Role::User,
        content,
        display_text: None,
        kind: animist::history::MessageKind::Turn,
    })
}

/// One run of the series and everything the terminal `Done` needs, folded
/// across the initial run and every follow-up.
#[derive(Default)]
struct RunSeries {
    /// Summed settled statistics of the runs that produced reports.
    usage: TokenUsage,
    by_model: std::collections::HashMap<String, StoredTokenUsage>,
    steps: u64,
    context_size: u64,
    /// Attempts whose usage never arrived; logged on the terminal `Done`.
    unreported: u64,
    /// Report-less ends (a layer stop) still bill what the runs spent, so
    /// the forwarded events are tallied as a fallback usage view.
    observed: (TokenUsage, u64),
}

impl RunSeries {
    fn fold(&mut self, report: &RunReport) {
        let stats = &report.stats;
        self.usage.input += stats.usage.input;
        self.usage.output += stats.usage.output;
        self.usage.cache_creation += stats.usage.cache_creation;
        self.usage.cache_read += stats.usage.cache_read;
        for ((provider, model), usage) in &stats.by_model {
            let spec = format!("{provider}/{model}");
            let entry = self.by_model.entry(spec).or_default();
            entry.input += usage.input;
            entry.output += usage.output;
            entry.cache_creation += usage.cache_creation;
            entry.cache_read += usage.cache_read;
        }
        self.steps += stats.steps;
        self.context_size = stats.context_size;
        self.unreported += stats.unreported_incomplete_attempts;
    }
}

/// Mark the next run as a follow-up on every layer whose per-turn state
/// must carry over (the doom tracker, the recovery budgets, the
/// continuation budgets); each layer's `turn_start` consumes the mark.
fn mark_follow_up(conversation: &Conversation) {
    conversation.guardrails().mark_follow_up();
    lock_recovery(&conversation.recovery()).follow_up = true;
    conversation
        .continuation()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .follow_up = true;
}

/// Clear every layer's follow-up mark: a fresh user turn starts from clean
/// budgets. A cancelled turn can leave marks behind when its follow-up run
/// never reached `turn_start`; this makes the leak impossible.
fn clear_follow_up_marks(conversation: &Conversation) {
    if conversation.guardrails().take_follow_up() {
        tracing::debug!("cleared a stale guardrail follow-up mark");
    }
    lock_recovery(&conversation.recovery()).follow_up = false;
    conversation
        .continuation()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .follow_up = false;
}

/// Run the agent once on `input`, forwarding its events into the
/// conversation sink, and fold the result into `series`. Returns the
/// settled report, or the agent's error when the run failed before one
/// existed.
#[expect(
    clippy::result_large_err,
    reason = "AgentError is animist's public error type and cannot be boxed here"
)]
async fn execute_run(
    conversation: &Conversation,
    input: StoredMessage,
    cancel: Cancellation,
    series: &mut RunSeries,
) -> std::result::Result<RunReport, AgentError> {
    let sink = conversation.sink();
    let agent = conversation.agent();
    let (events, event_stream) = flume::bounded::<AgentEvent>(RUN_EVENT_CHANNEL_CAPACITY);
    let options = RunOptions::builder()
        .cancellation(cancel)
        .events(events)
        .build();
    let run = agent.run(input, options);
    let observed = Arc::new(Mutex::new((TokenUsage::default(), 0_u64)));
    let forward = {
        let sink = sink.clone();
        let observed = Arc::clone(&observed);
        async move {
            while let Ok(event) = event_stream.recv_async().await {
                if let AgentEvent::StepFinished { usage, .. } = &event {
                    let mut tally = observed.lock().unwrap_or_else(|poison| poison.into_inner());
                    tally.0.input += usage.input;
                    tally.0.output += usage.output;
                    tally.0.cache_read += usage.cache_read;
                    tally.0.cache_creation += usage.cache_creation;
                    tally.1 += 1;
                }
                sink.send(Event::Agent(event));
            }
        }
    };
    let (report, _) = tokio::join!(run, forward);
    // The forward task exited with the run, so the tally is settled.
    let (observed_usage, observed_steps) = observed
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    series.observed.0.input += observed_usage.input;
    series.observed.0.output += observed_usage.output;
    series.observed.0.cache_read += observed_usage.cache_read;
    series.observed.0.cache_creation += observed_usage.cache_creation;
    series.observed.1 += observed_steps;
    match report {
        Ok(report) => {
            series.fold(&report);
            Ok(report)
        }
        Err(error) => Err(error),
    }
}

/// Charge one overflow recovery against the shared budget; `false` means
/// the cap is spent and the overflow is the honest answer. `rearm` closes
/// the previous episode (the last series iteration was not itself a
/// recovered overflow), so a stop-reason overflow after an error-form
/// recovery plus real progress still gets its own compact-and-retry.
fn charge_overflow(recovery: &super::recovery::RecoveryHandle, rearm: bool) -> bool {
    lock_recovery(recovery).charge_overflow(rearm)
}

/// Lock the recovery state without exposing the poison dance at call sites.
fn lock_recovery(
    recovery: &super::recovery::RecoveryHandle,
) -> std::sync::MutexGuard<'_, RecoveryRunState> {
    recovery.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// The stop-reason overflow recovery: recalibrate from the provider's
/// measured prompt, force compaction, and hand back the continuation
/// observation the re-run continues from. `None` when the budget is spent
/// or no stage ran (the error is the honest answer, not a blind retry).
async fn recover_stop_reason_overflow(
    conversation: &Conversation,
    report: &RunReport,
    rearm: bool,
) -> Option<StoredMessage> {
    let recovery = conversation.recovery();
    if !charge_overflow(&recovery, rearm) {
        return None;
    }
    let model = lock_recovery(&recovery)
        .model_override
        .clone()
        .unwrap_or_else(|| conversation.model());
    let ran = super::recovery::recover_from_overflow(
        &conversation.compaction_engine(),
        &conversation.compaction(),
        &conversation.guardrails(),
        &model,
        &conversation.history(),
        &conversation.sink(),
        None,
        report.stats.context_size,
    )
    .await;
    ran.then(|| marker::observation(marker::CONTINUATION, CONTINUATION_PROMPT))
}

/// The advisor gate: one delta review after a terminal reply,
/// before the turn result escapes. Returns the follow-up input when the
/// run should continue.
async fn advisor_followup(
    conversation: &Conversation,
    cancel: &Cancellation,
    state: &mut crate::harness::advisor::AdvisorState,
    config: &crate::config::AdvisorConfig,
    continuations: u32,
) -> Option<StoredMessage> {
    conversation.sink().craft(CraftEvent::Info(
        crate::harness::advisor::ADVISOR_REVIEWING_INFO.to_owned(),
    ));
    // No cancel race: the deadline bounds the call and the token is
    // checked after.
    let model = lock_recovery(&conversation.recovery())
        .model_override
        .clone()
        .unwrap_or_else(|| conversation.model());
    let entries: Vec<StoredMessage> = conversation
        .history()
        .snapshot()
        .await
        .entries()
        .iter()
        .filter_map(|entry| entry.current())
        .cloned()
        .collect();
    let note = crate::harness::advisor::review(&model, state, &entries).await;
    if cancel.is_cancelled() {
        return None;
    }
    let note = note?;
    conversation.sink().craft(CraftEvent::AdvisorNote {
        severity: note.severity.as_str().to_owned(),
        message: note.message.clone(),
    });
    match crate::harness::advisor::advisor_turn_action(Some(note), config, continuations) {
        crate::harness::advisor::AdvisorTurnAction::Continue(note) => {
            conversation.sink().craft(CraftEvent::Info(
                crate::harness::advisor::advisor_continuation_info(
                    &note,
                    continuations + 1,
                    config.max_act_turns,
                ),
            ));
            Some(crate::harness::advisor::advisor_followup_message(&note))
        }
        crate::harness::advisor::AdvisorTurnAction::Stop => None,
    }
}

/// Drive one multi-turn turn on the conversation's agent. Surfaces call
/// this once per prompt and await it to completion, including after
/// cancelling.
pub async fn run_turn(
    conversation: &Conversation,
    prompt: &str,
    images: &[ContentBlock],
    cancel: Cancellation,
) -> TurnOutcome {
    let sink = conversation.sink();
    let input = input_message(prompt, images);
    let anchor_input = input.clone();
    // Between-runs history edits: alterations, then the engine's
    // stages, before the run builds its first request on top of them.
    compact_between_runs(conversation, false).await;
    // A fresh turn is never a follow-up: clear any mark a cancelled
    // previous turn left behind (a follow-up run that never reached
    // `turn_start` does not consume its own mark).
    clear_follow_up_marks(conversation);
    let mut series = RunSeries::default();
    // Advisor state: reviews only this turn's delta, so the cursor
    // starts at the current entries (the input lands after it).
    let advisor_config = conversation.advisor_config();
    let mut advisor_state = advisor_config
        .enabled
        .then(|| crate::harness::advisor::AdvisorState::with_dedup(advisor_config.dedup_size));
    if let Some(state) = advisor_state.as_mut() {
        state.last_reviewed = conversation.history().snapshot().await.entries().len();
    }
    let mut advisor_continuations = 0_u32;
    let mut next_input = input;
    // Whether the previous series iteration was itself a recovered
    // stop-reason overflow: consecutive ones are one episode, and the
    // second surfaces the error.
    let mut previous_was_overflow = false;
    let (outcome, done): (TurnOutcome, Option<RunReport>) = loop {
        let report = match execute_run(
            conversation,
            next_input.clone(),
            cancel.clone(),
            &mut series,
        )
        .await
        {
            Ok(report) => report,
            Err(error) => {
                break (
                    finish_error(&sink, &conversation.history(), error).await,
                    None,
                );
            }
        };
        // The one continuation that stays a driver re-run: animist makes
        // the `ContextWindowExceeded` stop reason terminal, and `step_end`
        // may not continue from it. Handle it exactly like the overflow
        // error — compact, then re-run with a continuation observation.
        if report.status == RunStatus::ContextWindowExceeded {
            // The episode closed unless the previous iteration was itself a
            // recovered overflow AND this run made no progress beyond its
            // own terminal step — a follow-up that ran real tool steps
            // between the overflows has earned a fresh compact-and-retry.
            let rearm = !previous_was_overflow || report.stats.steps > 1;
            match recover_stop_reason_overflow(conversation, &report, rearm).await {
                Some(continuation) => {
                    // A cancel landing during the forced compaction ends
                    // the turn here, before the mark: a follow-up whose
                    // `turn_start` never runs would leak its flags.
                    if cancel.is_cancelled() {
                        conversation
                            .history()
                            .add_stored(marker::observation(marker::CANCEL, CANCEL_MARKER))
                            .await;
                        break (TurnOutcome::Cancelled, None);
                    }
                    previous_was_overflow = true;
                    mark_follow_up(conversation);
                    next_input = continuation;
                    continue;
                }
                // The budget is spent or compaction declined: surface the
                // overflow instead of resubmitting the identical prompt.
                None => {
                    break (
                        failed(
                            &sink,
                            &conversation.history(),
                            "the request exceeded the context window",
                        )
                        .await,
                        None,
                    );
                }
            }
        }
        previous_was_overflow = false;
        let outcome = finish_report(&sink, &conversation.history(), &report).await;
        // The advisor reviews after a terminal Completed that was not
        // cancelled, and its follow-up message re-runs the agent.
        if let (true, TurnOutcome::Done { .. }) = (advisor_config.enabled, &outcome)
            && !cancel.is_cancelled()
            && let Some(state) = advisor_state.as_mut()
            && let Some(followup) = advisor_followup(
                conversation,
                &cancel,
                state,
                &advisor_config,
                advisor_continuations,
            )
            .await
        {
            if cancel.is_cancelled() {
                conversation
                    .history()
                    .add_stored(marker::observation(marker::CANCEL, CANCEL_MARKER))
                    .await;
                break (TurnOutcome::Cancelled, None);
            }
            advisor_continuations += 1;
            mark_follow_up(conversation);
            next_input = followup;
            continue;
        }
        break (outcome, Some(report));
    };
    // The turn's overflow episode is over whatever way it ended; a later
    // turn's overflow gets its own attempt.
    lock_recovery(&conversation.recovery()).overflow_recoveries = 0;
    // Carry anchoring: a turn that did not complete
    // leaves its input unanswered — everything from it on must stay out of
    // any summary until a later turn answers it. A completed turn answers
    // its input.
    match &outcome {
        TurnOutcome::Done { .. } => {
            conversation.compaction().lock().await.mark_answered();
        }
        _ => {
            let entries = conversation.history().snapshot().await;
            // The last match: re-sent identical prompt text must anchor at
            // the entry this run admitted, not an answered duplicate.
            if let Some(index) = entries
                .entries()
                .iter()
                .rposition(|entry| entry.verbatim().message() == anchor_input.message())
            {
                conversation.compaction().lock().await.protect_from(index);
            }
        }
    }
    // The other between-runs site: the finished turn's results are durable
    // alteration candidates, and the engine may need to run before the next
    // prompt. Both precede the flush so one checkpoint carries them.
    compact_between_runs(conversation, false).await;
    // One more flush after the terminal markers; `turn_end` has already run
    // inside the agent.
    conversation.flush().await;
    emit_done(&sink, conversation, &outcome, done.as_ref(), &series);
    outcome
}

/// The final assistant text of a completed run, when it produced one.
fn reply(report: &RunReport) -> String {
    report
        .response
        .as_ref()
        .map(ext::message_text)
        .unwrap_or_default()
}

/// Map a completed run's status onto its outcome, committing the terminal
/// markers `turn_end` cannot append.
async fn finish_report(sink: &EventSink, history: &History, report: &RunReport) -> TurnOutcome {
    match report.status {
        RunStatus::Completed => TurnOutcome::Done {
            reply: reply(report),
        },
        RunStatus::StepLimit => {
            history
                .add_stored(marker::observation(marker::END, END_MARKER))
                .await;
            TurnOutcome::MaxTurns
        }
        RunStatus::MaxTokens => TurnOutcome::MaxTokens {
            reply: reply(report),
        },
        RunStatus::Cancelled => {
            commit_cancelled(history, report).await;
            TurnOutcome::Cancelled
        }
        RunStatus::Stopped => {
            // A layer stop reaches the driver as `AgentError::Stopped` (see
            // `finish_error`), never as a settled report; this arm only
            // exists because the status is representable.
            commit_cancelled(history, report).await;
            TurnOutcome::Cancelled
        }
        RunStatus::Refused => failed(sink, history, "the provider refused the request").await,
        RunStatus::Paused => failed(sink, history, "the provider paused the turn").await,
        RunStatus::ContextWindowExceeded => {
            failed(sink, history, "the request exceeded the context window").await
        }
        RunStatus::Failed => failed(sink, history, "the run failed").await,
    }
}

/// Map a failed run onto its outcome. Errors raised before the input was
/// admitted leave history untouched; everything else keeps the committed
/// messages and records the failure.
async fn finish_error(sink: &EventSink, history: &History, error: AgentError) -> TurnOutcome {
    match &error {
        AgentError::AlreadyRunning { .. }
        | AgentError::ShutDown { .. }
        | AgentError::InvalidLimits { .. } => {
            return TurnOutcome::Failed(error.to_string());
        }
        AgentError::Stopped { reason, .. } => {
            // The guardrail layer's hard stop arrives as a tagged stop; the
            // run ends like a step budget does — partial history committed,
            // an end marker recording where, replayable from there. Any
            // other stop (a client cancelling inside an approval, a failed
            // reauth) keeps the failed-run mapping.
            if reason.starts_with(crate::harness::doom::DOOM_STOP_PREFIX) {
                history
                    .add_stored(marker::observation(marker::END, END_MARKER))
                    .await;
                return TurnOutcome::DoomStop;
            }
        }
        AgentError::HistoryChanged { messages, .. } => {
            tracing::warn!(
                count = messages.len(),
                "history changed mid-step; retained messages were not committed"
            );
        }
        _ => {}
    }
    failed(sink, history, &error.to_string()).await
}

/// Record a failed run and its observation.
async fn failed(sink: &EventSink, history: &History, message: &str) -> TurnOutcome {
    sink.craft(CraftEvent::Error(message.to_owned()));
    history
        .add_stored(marker::observation(
            marker::FAILED,
            format!("[Run failed: {message}]"),
        ))
        .await;
    TurnOutcome::Failed(message.to_owned())
}

/// Commit a cancelled run: the interrupted attempt's validated text/thinking
/// prefix (unexecuted tool calls dropped), then the cancel marker. The run
/// itself already committed explicit interrupted results for its calls.
async fn commit_cancelled(history: &History, report: &RunReport) {
    if let Some(attempt) = report.interrupted_attempts.last() {
        let prefix: Vec<ContentBlock> = attempt
            .partial
            .content
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    ContentBlock::Text { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                )
            })
            .cloned()
            .collect();
        if !prefix.is_empty() {
            let message = Message {
                role: Role::Assistant,
                content: prefix,
                display_text: None,
                kind: animist::history::MessageKind::Turn,
            };
            let mut view = history.current().await;
            view.push(message.clone());
            if animist::protocol::validate_replay(&view, &[]).is_ok() {
                history.add_stored(ext::stored(message)).await;
            } else {
                tracing::warn!(
                    "cancelled attempt's partial prefix failed replay validation; dropped"
                );
            }
        }
    }
    history
        .add_stored(marker::observation(marker::CANCEL, CANCEL_MARKER))
        .await;
}

/// Publish the terminal `Done` with the folded, priced usage.
fn emit_done(
    sink: &EventSink,
    conversation: &Conversation,
    outcome: &TurnOutcome,
    report: Option<&RunReport>,
    series: &RunSeries,
) {
    if series.unreported > 0 {
        // Unknown usage is not zero-cost inference; surfaces show the
        // settled ledger, and the gap is recorded here.
        tracing::info!(
            attempts = series.unreported,
            "interrupted attempts reported no usage; their spend is unknown"
        );
    }
    let reason = match outcome {
        TurnOutcome::Done { .. } => DoneReason::Stop,
        TurnOutcome::MaxTurns => DoneReason::MaxTurns,
        TurnOutcome::MaxTokens { .. } => DoneReason::MaxTokens,
        TurnOutcome::Cancelled => DoneReason::Cancelled,
        TurnOutcome::DoomStop => DoneReason::DoomStop,
        TurnOutcome::Failed(_) => DoneReason::Error,
    };
    // A fallback hop or a reauth swap may have served the runs; the
    // window shown is the serving model's.
    let context_window = serving_model(conversation)
        .context_length()
        .map(u64::from)
        .unwrap_or(0);
    let Some(_report) = report else {
        // Error paths carry no settled report; the outcome still names the
        // reason (a doom stop is a controlled end, not an error) and the
        // tallied events carry what the runs actually spent.
        let (usage, steps) = (series.observed.0.clone(), series.observed.1);
        sink.craft(CraftEvent::Done {
            usage,
            context_size: series.context_size,
            context_window,
            num_turns: steps.min(u64::from(u32::MAX)) as u32,
            reason,
            cost: None,
            by_model: Default::default(),
        });
        return;
    };
    let fast = conversation.settings().fast;
    let mut by_model = series.by_model.clone();
    let cost = if by_model.is_empty() {
        None
    } else {
        crate::usage::settle_session(
            &TokenUsage::default(),
            &mut by_model,
            &conversation.model_spec(),
            fast,
        )
    };
    sink.craft(CraftEvent::Done {
        usage: series.usage.clone(),
        context_size: series.context_size,
        context_window,
        num_turns: series.steps.min(u64::from(u32::MAX)) as u32,
        reason,
        cost,
        by_model,
    });
}

/// The model that served the turn: a recovery override while one is set,
/// else the conversation's model.
fn serving_model(conversation: &Conversation) -> crate::provider::ModelHandle {
    lock_recovery(&conversation.recovery())
        .model_override
        .clone()
        .unwrap_or_else(|| conversation.model())
}
