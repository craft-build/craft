//! Stalled-turn nudging: a model reply with no text (and no tool calls)
//! would otherwise end the turn with nothing to show for the tool results
//! the model just received. Ported from the reference `run/turn.rs`
//! (`recover_stalled_turn`): push an empty marker for the silent reply, then
//! a synthetic nudge prompt — but only while a tool result sits in the
//! recent window and the nudge budget lasts. The nudge budget is tracked by
//! the run loop (reset on real progress) rather than mined from history, so
//! a user message that happens to equal the prompt text is never
//! misclassified as padding.

use crate::history::{AssistantContent, Message, UserContent};

/// A model that stalls once often stalls again on the retry, so it gets
/// plenty of chances before the turn ends empty handed.
pub(crate) const MAX_NUDGES: u32 = 20;

/// Counted over real (non-marker) messages.
pub(crate) const RECENT_TOOL_WINDOW: usize = 5;

pub(crate) const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";

/// Stands in for an assistant turn with no text, which providers reject as
/// the trailing message. Never a real response: readers mining history for
/// model text must skip it.
pub(crate) const EMPTY_RESPONSE_MARKER: &str = "(empty)";

/// Shape matters: an assistant whose sole text block is the marker. A model
/// that literally outputs `(empty)` plus anything else is a real reply.
fn is_empty_marker(m: &Message) -> bool {
    matches!(m, Message::Assistant { content }
        if matches!(&content[..], [AssistantContent::Text(t)] if t.text == EMPTY_RESPONSE_MARKER))
}

/// Whether any of the last `depth` real messages carries a tool result.
/// `synthetic_tail` counts marker+nudge pairs this run already appended;
/// they are skipped so repeated nudges cannot push the results out of the
/// window.
pub(crate) fn has_recent_tool_results(
    messages: &[Message],
    depth: usize,
    synthetic_tail: usize,
) -> bool {
    let real = &messages[..messages.len() - synthetic_tail];
    real.iter()
        .rev()
        .filter(|m| !is_empty_marker(m))
        .take(depth)
        .any(|m| {
            matches!(m, Message::User { content }
                if content.iter().any(|b| matches!(b, UserContent::ToolResult(_))))
        })
}

/// The turn came back without text or tool calls, so the empty marker takes
/// the reply's place in history. When `nudge`, a synthetic user prompt
/// follows it and the run continues.
pub(crate) fn stall_turn(turn: &mut Vec<Message>, nudge: bool) {
    turn.push(Message::Assistant {
        content: vec![AssistantContent::text(EMPTY_RESPONSE_MARKER)],
    });
    if nudge {
        turn.push(Message::user(NUDGE_PROMPT));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ToolResult;

    fn marker() -> Message {
        Message::Assistant {
            content: vec![AssistantContent::text(EMPTY_RESPONSE_MARKER)],
        }
    }

    fn nudge() -> Message {
        Message::user(NUDGE_PROMPT)
    }

    fn tool_result() -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(ToolResult::text(
                "t1", "read", "content",
            ))],
        }
    }

    #[test]
    fn has_recent_tool_results_respects_depth_and_synthetic_tail() {
        let real = vec![tool_result(), Message::user("now what")];
        assert!(has_recent_tool_results(&real, 2, 0));
        assert!(!has_recent_tool_results(&real, 1, 0));
        // Markers never consume window depth.
        let marked = vec![tool_result(), marker(), Message::user("go")];
        assert!(has_recent_tool_results(&marked, 2, 0));
        // The synthetic streak is skipped entirely.
        let streak = vec![
            tool_result(),
            Message::user("what next"),
            marker(),
            nudge(),
            marker(),
            nudge(),
        ];
        assert!(has_recent_tool_results(&streak, 2, 4));
        assert!(!has_recent_tool_results(&streak, 1, 4));
    }

    #[test]
    fn verbatim_user_prompt_is_a_real_message() {
        // A user typing the nudge prompt (or the marker) verbatim consumes
        // window depth like any real message.
        let verbatim = vec![tool_result(), Message::user(NUDGE_PROMPT)];
        assert!(has_recent_tool_results(&verbatim, 2, 0));
        assert!(!has_recent_tool_results(&verbatim, 1, 0));
        assert_eq!(
            has_recent_tool_results(&vec![Message::user(EMPTY_RESPONSE_MARKER)], 1, 0),
            false
        );
    }

    #[test]
    fn stall_turn_pushes_marker_then_nudge_prompt() {
        let mut turn = Vec::new();
        stall_turn(&mut turn, true);
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0].text(), EMPTY_RESPONSE_MARKER);
        assert_eq!(turn[1].text(), NUDGE_PROMPT);
    }

    #[test]
    fn stall_turn_without_nudge_pushes_marker_only() {
        let mut turn = Vec::new();
        stall_turn(&mut turn, false);
        assert_eq!(turn.len(), 1);
    }
}
