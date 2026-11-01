//! Turn driver: the thin ring around `Agent::run` that no layer hook can
//! cover. It builds the input, runs the agent with its animist event
//! channel drained into the conversation sink, awaits the run after
//! cancellation, adds the terminal markers, and reports the folded usage.

use std::sync::Arc;

use animist::agent::{AgentError, AgentEvent, Cancellation, RunOptions, RunReport, RunStatus};
use animist::history::{ContentBlock, History, Message, Role, StoredMessage};

use crate::history::{ext, marker};
use crate::usage::{StoredTokenUsage, TokenUsage};

use super::conversation::Conversation;
use super::events::{CraftEvent, DoneReason, Event, EventSink};

/// Marker text appended when a run is cut short by its step budget, so the
/// model knows the turn ended. Identity lives in the [`marker::END`] tag.
pub(crate) const END_MARKER: &str = "[The turn ended here; the run was cut short.]";

/// Marker text appended when a run is cancelled by the user. Identity lives
/// in the [`marker::CANCEL`] tag.
pub(crate) const CANCEL_MARKER: &str = "[Cancelled by user]";

/// Between-runs history edits (08): the durable alteration passes first,
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
    /// Generation hit the output token limit. No continuation until 09.
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

/// Drive one multi-turn run on the conversation's agent. Surfaces call this
/// once per prompt and await it to completion, including after cancelling.
pub async fn run_turn(
    conversation: &Conversation,
    prompt: &str,
    images: &[ContentBlock],
    cancel: Cancellation,
) -> TurnOutcome {
    let sink = conversation.sink();
    let input = input_message(prompt, images);
    let anchor_input = input.clone();
    // Between-runs history edits (08): alterations, then the engine's
    // stages, before the run builds its first request on top of them.
    compact_between_runs(conversation, false).await;
    let agent = conversation.agent();
    let (events, event_stream) = flume::bounded::<AgentEvent>(RUN_EVENT_CHANNEL_CAPACITY);
    let options = RunOptions::builder()
        .cancellation(cancel)
        .events(events)
        .build();
    let run = agent.run(input, options);
    // Report-less ends (a layer stop) still bill what the run spent, so the
    // forwarded events are tallied as a fallback usage view.
    let observed = std::sync::Arc::new(std::sync::Mutex::new((TokenUsage::default(), 0_u64)));
    let forward = async {
        let observed = Arc::clone(&observed);
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
    };
    let (report, _) = tokio::join!(run, forward);
    // The forward task exited with the run, so the tally is settled.
    let (observed_usage, observed_steps) = observed
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let (outcome, done) = match report {
        Ok(report) => (
            finish_report(&sink, &conversation.history(), &report).await,
            Some(report),
        ),
        Err(error) => (
            finish_error(&sink, &conversation.history(), error).await,
            None,
        ),
    };
    // Carry anchoring (Craft's `carry_from`): a turn that did not complete
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
    emit_done(
        &sink,
        conversation,
        &outcome,
        done.as_ref(),
        (observed_usage, observed_steps),
    );
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
            // other stop (a client cancelling inside an approval) keeps the
            // failed-run mapping 05 pinned.
            if reason.starts_with(crate::run::doom::DOOM_STOP_PREFIX) {
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
    observed: (TokenUsage, u64),
) {
    let reason = match outcome {
        TurnOutcome::Done { .. } => DoneReason::Stop,
        TurnOutcome::MaxTurns => DoneReason::MaxTurns,
        TurnOutcome::MaxTokens { .. } => DoneReason::MaxTokens,
        TurnOutcome::Cancelled => DoneReason::Cancelled,
        TurnOutcome::DoomStop => DoneReason::DoomStop,
        TurnOutcome::Failed(_) => DoneReason::Error,
    };
    let Some(report) = report else {
        // Error paths carry no settled report; the outcome still names the
        // reason (a doom stop is a controlled end, not an error) and the
        // tallied events carry what the run actually spent.
        let (usage, steps) = observed;
        sink.craft(CraftEvent::Done {
            usage,
            context_size: 0,
            context_window: conversation
                .model()
                .context_length()
                .map(u64::from)
                .unwrap_or(0),
            num_turns: steps.min(u64::from(u32::MAX)) as u32,
            reason,
            cost: None,
            by_model: Default::default(),
        });
        return;
    };
    let fast = conversation.settings().fast;
    let mut by_model: std::collections::HashMap<String, StoredTokenUsage> = report
        .stats
        .by_model
        .iter()
        .map(|((provider, model), usage)| {
            let spec = format!("{provider}/{model}");
            let cost = crate::usage::resolve_spec(&spec).and_then(|m| m.billed_cost(usage, fast));
            (spec, crate::usage::billed(usage, cost))
        })
        .collect();
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
        usage: report.stats.usage.clone(),
        context_size: report.stats.context_size,
        context_window: conversation
            .model()
            .context_length()
            .map(u64::from)
            .unwrap_or(0),
        num_turns: report.stats.steps.min(u64::from(u32::MAX)) as u32,
        reason: match outcome {
            TurnOutcome::Done { .. } => DoneReason::Stop,
            TurnOutcome::MaxTurns => DoneReason::MaxTurns,
            TurnOutcome::MaxTokens { .. } => DoneReason::MaxTokens,
            TurnOutcome::Cancelled => DoneReason::Cancelled,
            TurnOutcome::DoomStop => DoneReason::DoomStop,
            TurnOutcome::Failed(_) => DoneReason::Error,
        },
        cost,
        by_model,
    });
}
