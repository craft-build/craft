//! One iteration of the run loop: stream a model call (with overflow,
//! reauth, and cancel recovery) and finish the resulting turn.

use std::sync::Arc;

use rig_core::completion::{CompletionModel, CompletionRequest, FinishReason};

use crate::history::{self, Message};

use super::dispatch_tool_calls;
use super::doom;
use super::retry;
use super::served_spec;
use super::stream::{self, TurnOutput};
use super::{
    CancelToken, Event, MAX_OVERFLOW_RECOVERIES, MAX_REAUTH_ATTEMPTS, RunOutcome, RunParams,
    RunStats, ToolDispatch,
};

/// Outcome of one streamed model call: a produced turn, a recovered
/// failure (retry the loop with no turn produced), or a terminal outcome.
pub(super) enum Streamed {
    Turn(TurnOutput, Option<Arc<str>>),
    Retry,
    Stop(RunOutcome),
}

/// Outcome of finishing a produced turn: keep looping, or end the run.
pub(super) enum TurnEnd {
    Continue,
    Stop(RunOutcome),
}

/// Stream one model call for the current request, handling cancelled,
/// overflow, and auth failures exactly as the inline loop did.
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_turn<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
    history: &mut Vec<Message>,
    turn: &mut Vec<Message>,
    doom: &mut doom::DoomTracker,
    refreshed_model: &mut Option<crate::providers::DynamicModel>,
    overflow_recoveries: &mut u32,
    reauth_attempts: &mut u32,
    measured_prompt_tokens: &mut u64,
    transient_budget: &retry::TransientBudget,
    request: &CompletionRequest,
) -> Streamed {
    match match refreshed_model.as_ref() {
        Some(refreshed) => {
            retry::stream_with_retry(
                refreshed,
                &[],
                params.retry.rotate.as_ref(),
                transient_budget,
                request,
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
                transient_budget,
                request,
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
            Streamed::Turn(output, served_spec)
        }
        Err(stream::StreamFailure::Cancelled { streamed }) => {
            // Keep the partial reply the user already saw, so the next
            // prompt replays from what was on screen.
            if !streamed.is_empty() {
                turn.push(Message::Assistant {
                    content: vec![history::AssistantContent::text(streamed)],
                });
            }
            Streamed::Stop(super::commit_cancelled(history, turn))
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
            if *overflow_recoveries >= MAX_OVERFLOW_RECOVERIES {
                return Streamed::Stop(RunOutcome::Failed(message));
            }
            *overflow_recoveries += 1;
            if !super::recover_from_overflow(params, model, history, doom, emit).await {
                return Streamed::Stop(RunOutcome::Failed(message));
            }
            // The pre-compaction measurement no longer describes the
            // compacted context; drop it so the clamp trusts the
            // estimate until the next real usage report.
            *measured_prompt_tokens = 0;
            Streamed::Retry
        }
        // Auth failure: pause for re-authentication instead of failing
        // (E.10). Without a responder — or past the attempt budget — the
        // error is the honest answer, like the reference's no-rx path.
        Err(failure) if failure.is_auth() => {
            let Some(message) = failure.message().map(str::to_owned) else {
                unreachable!("is_auth only matches Error");
            };
            let Some(reauth) = params.reauth.clone() else {
                return Streamed::Stop(RunOutcome::Failed(message));
            };
            if *reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                return Streamed::Stop(RunOutcome::Failed(message));
            }
            *reauth_attempts += 1;
            emit(Event::AuthRequired {
                attempt: *reauth_attempts,
                message: message.clone(),
            });
            match tokio::select! {
                biased;
                _ = cancel.wait() => None,
                r = reauth(*reauth_attempts) => Some(r),
            } {
                Some(Ok(refreshed)) => {
                    *refreshed_model = refreshed.or(refreshed_model.take());
                    Streamed::Retry
                }
                Some(Err(e)) => Streamed::Stop(RunOutcome::Failed(e)),
                None => Streamed::Stop(super::commit_cancelled(history, turn)),
            }
        }
        Err(failure) => {
            let message = failure
                .message()
                .map(str::to_owned)
                .unwrap_or_else(|| "stream cancelled".into());
            Streamed::Stop(RunOutcome::Failed(message))
        }
    }
}

/// Finish a produced turn: record usage, handle a terminal (tool-less)
/// reply or dispatch the turn's tool calls, and apply doom bookkeeping.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_turn(
    params: &RunParams,
    tools: &ToolDispatch,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
    history: &mut Vec<Message>,
    turn: &mut Vec<Message>,
    full: &[Message],
    output: TurnOutput,
    served_spec: Option<Arc<str>>,
    stats: &mut RunStats,
    nudges: &mut u32,
    turns: &mut usize,
    continuations: &mut usize,
    doom: &mut doom::DoomTracker,
    recent: &mut doom::RecentCalls,
    overflow_recoveries: &mut u32,
    reauth_attempts: &mut u32,
    measured_prompt_tokens: &mut u64,
) -> TurnEnd {
    *overflow_recoveries = 0;
    *reauth_attempts = 0;
    // Track the last measured count (zero is the missing-report
    // sentinel, kept at the previous value); it naturally shrinks
    // again after compaction, unlike a running max.
    if output.usage.input_tokens > 0 {
        *measured_prompt_tokens = output.usage.input_tokens;
    }
    stats.add_usage(&output.usage, served_spec.as_deref(), params.fast);
    stats.context_size = crate::compaction::estimate_tokens(full);
    stats.turns = *turns as u32 + 1;
    emit(Event::TurnComplete {
        usage: output.usage,
        context_size: stats.context_size,
    });
    let Message::Assistant { content } = &output.assistant else {
        return TurnEnd::Stop(RunOutcome::Failed(
            "model produced a non-assistant message".into(),
        ));
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
        if let Some(outcome) = super::handle_terminal_reply(
            history,
            turn,
            full,
            &reply,
            truncated,
            nudges,
            turns,
            params,
            continuations,
            emit,
        ) {
            return TurnEnd::Stop(outcome);
        }
        return TurnEnd::Continue;
    }
    let (stopped, batch) = dispatch_tool_calls(tools, turn, tool_calls, recent, cancel, emit).await;
    if let Some(outcome) = stopped {
        if matches!(outcome, RunOutcome::Cancelled) {
            return TurnEnd::Stop(super::commit_cancelled(history, turn));
        }
        return TurnEnd::Stop(outcome);
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
    *turns += 1;
    *nudges = 0;
    if *turns >= params.max_turns {
        return TurnEnd::Stop(super::commit_partial(history, turn, RunOutcome::MaxTurns));
    }
    TurnEnd::Continue
}
