//! Turn driver: the thin ring around `Agent::run` that no layer hook can
//! cover. It builds the input, runs the agent with its animist event
//! channel drained into the conversation sink, awaits the run after
//! cancellation, adds the terminal markers, and reports the folded usage.

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
    // 08: pre-turn compaction runs here, once the Compactor plans land.
    let agent = conversation.agent();
    let (events, event_stream) = flume::bounded::<AgentEvent>(RUN_EVENT_CHANNEL_CAPACITY);
    let options = RunOptions::builder()
        .cancellation(cancel)
        .events(events)
        .build();
    let run = agent.run(input, options);
    let forward = async {
        while let Ok(event) = event_stream.recv_async().await {
            sink.send(Event::Agent(event));
        }
    };
    let (report, _) = tokio::join!(run, forward);
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
    // One more flush after the terminal markers; `turn_end` has already run
    // inside the agent.
    conversation.flush().await;
    emit_done(&sink, conversation, &outcome, done.as_ref());
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
        RunStatus::Cancelled | RunStatus::Stopped => {
            // 05's only stop source is the approval gate's client cancel;
            // 07 decides whether a stop stays distinguishable.
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
) {
    let Some(report) = report else {
        // Error paths carry no settled report; emit the bare reason.
        sink.craft(CraftEvent::Done {
            usage: TokenUsage::default(),
            context_size: 0,
            context_window: conversation
                .model()
                .context_length()
                .map(u64::from)
                .unwrap_or(0),
            num_turns: 0,
            reason: DoneReason::Error,
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
            TurnOutcome::Failed(_) => DoneReason::Error,
        },
        cost,
        by_model,
    });
}
