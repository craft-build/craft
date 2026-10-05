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
/// context window. Returns whether recovery happened: a run without a
/// compaction context cannot recover, and a compaction that declines to run
/// left the prompt untouched, so the caller must surface the overflow error
/// instead of blindly retrying the identical oversized prompt. `history` is
/// compacted in place; the un-committed turn is left intact and re-appended
/// by the retried request.
pub(super) async fn recover_from_overflow<M: CompletionModel + Clone>(
    params: &RunParams,
    model: &M,
    history: &mut Vec<Message>,
    doom: &mut doom::DoomTracker,
    error_text: Option<&str>,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> bool {
    let Some(ctx) = &params.compaction else {
        return false;
    };
    // Include the request overhead (preamble + tool schemas) the run loop
    // published on the shared state: the provider's actual prompt size
    // counts it, so the recalibration below must compare like with like.
    let request_overhead = ctx
        .state
        .lock()
        .map(|guard| guard.request_overhead())
        .unwrap_or(0);
    let estimated = crate::compaction::estimate_tokens(history).saturating_add(request_overhead);
    // A failed request reports no usage, but overflow error bodies commonly
    // name the real prompt size ("...you requested 8192 tokens"). Calibrate
    // the estimator from it against the LIVE state: `absorb_run` below only
    // merges disarm/carry flags, so recalibrating the clone would be lost.
    // No parse ⇒ leave the estimate alone (never recalibrate to a bogus
    // value like the window bound).
    if let Some(actual) = error_text.and_then(parse_overflow_prompt_size)
        && let Ok(mut guard) = ctx.state.lock()
    {
        guard.recalibrate(actual, estimated);
    }
    // `force_compact` awaits, so the engine runs on a clone; the write-back
    // happens in a short critical section against the live state so a
    // concurrent session-side update is not clobbered.
    let Some(mut state) = ctx.state.lock().ok().map(|guard| guard.clone()) else {
        return false;
    };
    let window = ctx.context_length.map(u64::from).unwrap_or(0);
    let before = state.estimator.scale(estimated);
    emit(Event::AutoCompacting {
        context_size: before,
        context_window: window,
    });
    // Overflow recovery forces every armed stage: the provider already
    // proved the prompt does not fit, so waiting for fill thresholds would
    // just retry the identical oversized request.
    let ran = CompactionEngine::new(ctx.stages.clone())
        .with_buffer(ctx.buffer)
        .force_compact(&mut state, model, history, ctx.context_length)
        .await;
    let after = state.estimator.scale(
        crate::compaction::estimate_tokens(history).saturating_add(state.request_overhead()),
    );
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
    // A declined compaction changed nothing, so the retried request would
    // overflow byte-identically; tell the caller to fail with the provider's
    // error instead of looping.
    ran
}

/// Best-effort prompt size parsed out of a provider overflow error body, to
/// feed estimator recalibration. Providers phrase these differently —
/// OpenAI-style "maximum context length is 8192 tokens. However, you
/// requested 9016 tokens.", Anthropic-style "prompt is too long: 213132
/// tokens > 200000 maximum". A number following a request-size keyword wins
/// outright; otherwise the largest token-adjacent number is the offending
/// size (the window bound is the smaller figure), and a lone candidate next
/// to window words is treated as the window bound, not the prompt — no safe
/// parse. `None` means "do not recalibrate".
pub(super) fn parse_overflow_prompt_size(text: &str) -> Option<u64> {
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
            // length is 8192 tokens") is the window bound — the exact value
            // the old code refused to recalibrate to — not the prompt size.
            let window_words = ["maximum", "context length", "context window", " window"];
            (!window_words.iter().any(|w| lower.contains(w))).then_some(*single)
        }
        // With several sizes on the table, the offending prompt is the
        // largest: overflow means it exceeded the window bound.
        many => many.iter().copied().max(),
    }
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
#[allow(clippy::too_many_arguments)]
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
