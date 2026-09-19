//! Overflow recovery and terminal/partial-commit handling for the run loop.

use rig_core::completion::CompletionModel;

use crate::compaction::CompactionEngine;
use crate::history::{self, Message};

use super::doom;
use super::nudge;
use super::{Event, RunOutcome, RunParams};

/// Marker appended when a run is cut short, so the model knows the turn ended.
pub(crate) const END_MARKER: &str = "[The turn ended here; the run was cut short.]";

/// Recalibrate and force a compaction after the request overflowed the
/// context window. Returns whether recovery is possible at all (a run without
/// a compaction context cannot recover). `history` is compacted in place; the
/// un-committed turn is left intact and re-appended by the retried request.
pub(super) async fn recover_from_overflow<M: CompletionModel + Clone>(
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
pub(super) fn commit_partial(
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
pub(super) fn strip_trailing_grace_prompt(history: &mut Vec<Message>) {
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
pub(super) fn commit_cancelled(history: &mut Vec<Message>, turn: &mut Vec<Message>) -> RunOutcome {
    close_dangling_calls(turn, "skipped: cancelled by the user");
    turn.push(Message::user(CANCEL_MARKER));
    history.append(turn);
    RunOutcome::Cancelled
}

/// Handle an assistant turn with no tool calls. Continues truncated and
/// empty replies while their budgets allow; otherwise commits history and
/// returns the run outcome. `None` means "keep looping".
pub(super) fn handle_terminal_reply(
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

/// Close dangling tool calls and append the end marker, so the committed
/// partial history replays cleanly on the next request.
pub(super) fn sanitize_partial(turn: &mut Vec<Message>) {
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
