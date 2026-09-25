mod brief;
mod cut;
mod extract;
mod filter;
mod format;
mod merge;
mod normalize;
mod sections;
mod util;

use crate::history::{AssistantContent, Message};

pub(crate) use cut::find_cut;
use filter::filter_noise;
use format::{format_summary, wrap_long_lines};
use merge::{HANDOFF_PREAMBLE, merge_previous, strip_recall_note};
use normalize::normalize;
use sections::build_sections;

/// A VCC compaction summary and the index at which the unsummarized tail begins.
#[derive(Debug, Clone)]
pub struct VccSummary {
    pub summary: String,
    pub tail_start: usize,
}

/// Deterministically build a structured, no-LLM summary of `messages`.
///
/// `prev_summary` (a prior VCC summary, with its preamble) is merged so that
/// sticky sections accumulate across compactions while volatile sections are
/// replaced. The returned `tail_start` is 0 for compact-all.
pub fn compact(messages: &[Message], prev_summary: Option<&str>) -> VccSummary {
    let tail_start = match cut::find_cut(messages) {
        Some(c) => c.tail_start,
        None => 0,
    };
    let head = &messages[..tail_start];
    let blocks = filter_noise(normalize(head));
    let data = build_sections(&blocks);
    let fresh = format_summary(&data);

    let body = match prev_summary {
        Some(prev) => {
            let stripped = strip_recall_note(prev);
            if stripped.is_empty() {
                fresh
            } else {
                merge_previous(&stripped, &fresh)
            }
        }
        None => fresh,
    };

    let summary = if body.is_empty() {
        String::new()
    } else {
        wrap_long_lines(&format!("{HANDOFF_PREAMBLE}\n\n{body}"))
    };

    VccSummary {
        summary,
        tail_start,
    }
}

const VCC_SUMMARY_PREFIX: &str = "This summary captures";

/// Whether `msg` is a VCC summary handoff message.
pub fn is_vcc_summary(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Assistant { content } if content.iter().any(
            |b| matches!(b, AssistantContent::Text(t) if t.text.starts_with(VCC_SUMMARY_PREFIX))
        )
    )
}

fn summary_text(msg: &Message) -> Option<&str> {
    match msg {
        Message::Assistant { content } => content.iter().find_map(|b| match b {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

/// No-LLM VCC compaction: summarize the head, keep the tail.
///
/// If the first message is itself a VCC summary, it is treated as the previous
/// summary and compacting starts at the live history after it. The head of the
/// history is replaced with a single assistant summary message and the tail is
/// kept verbatim. The last `carry_len` messages are held out of the summary
/// and re-appended verbatim, so input no turn has answered yet survives
/// compaction verbatim (same carry protection as the LLM stage). Returns
/// whether the compacted history is under `token_limit` (per
/// `estimate_tokens`); `false` signals the caller should fall back to a
/// heavier compactor.
pub fn vcc_compact(
    messages: &mut Vec<Message>,
    token_limit: u64,
    estimate_tokens: fn(&[Message]) -> u64,
    carry_len: usize,
) -> bool {
    let (prev, live_start) = match messages.first() {
        Some(m) if is_vcc_summary(m) => (summary_text(m), 1),
        _ => (None, 0),
    };
    let live: Vec<Message> = messages[live_start..].to_vec();
    let carry_len = carry_len.min(live.len());
    if live.len() <= 2 {
        return false;
    }
    // Summarization stops where the protected (unanswered) input begins.
    let summarize_end = live.len().saturating_sub(carry_len);
    let VccSummary {
        summary,
        tail_start,
    } = compact(&live[..summarize_end], prev);
    if summary.is_empty() {
        return false;
    }
    let tail_start = tail_start.min(summarize_end);
    let tail_msgs = live.len() - tail_start;
    let mut new_history: Vec<Message> = Vec::with_capacity(1 + tail_msgs);
    new_history.push(Message::assistant(summary));
    new_history.extend(live.into_iter().skip(tail_start));
    *messages = new_history;
    estimate_tokens(messages) <= token_limit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support::{assistant_tool_args, tool_result_of, user};
    use crate::history::UserContent;

    /// History with a second text prompt mid-way, so the first compaction
    /// leaves a multi-message tail for the second (merging) compaction.
    fn two_task_history() -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..8 {
            msgs.push(user(&format!("do task {i} with a fairly long instruction")));
            msgs.push(assistant_tool_args(
                &format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo step{i}")}),
            ));
            msgs.push(tool_result_of(&format!("t{i}"), &format!("output {i}")));
        }
        msgs.push(user("second task begins here"));
        for i in 8..16 {
            msgs.push(user(&format!(
                "do subtask {i} with a fairly long instruction"
            )));
            msgs.push(assistant_tool_args(
                &format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo step{i}")}),
            ));
            msgs.push(tool_result_of(&format!("t{i}"), &format!("output {i}")));
        }
        // No trailing user prompt: the tail must be the last complete round
        // so a second (merging) compaction has live messages to work on.
        msgs
    }

    fn sample_history() -> Vec<Message> {
        vec![
            user("Implement the login feature"),
            assistant_tool_args("t1", "bash", serde_json::json!({"command": "ls"})),
            tool_result_of("t1", "done"),
            user("Now add tests for it"),
            assistant_tool_args("t2", "bash", serde_json::json!({"command": "ls"})),
            tool_result_of("t1", "done"),
            user("Run the test suite"),
            assistant_tool_args("t3", "bash", serde_json::json!({"command": "ls"})),
            tool_result_of("t1", "done"),
        ]
    }

    #[test]
    fn compact_produces_nonempty_summary_with_tail() {
        let history = sample_history();
        let result = compact(&history, None);
        assert!(!result.summary.is_empty());
        assert!(result.summary.contains("[Session Goal]"));
        assert!(result.summary.starts_with("This summary captures"));
        assert!(result.tail_start > 0);
    }

    #[test]
    fn compact_is_deterministic() {
        let history = sample_history();
        let a = compact(&history, None);
        let b = compact(&history, None);
        assert_eq!(a.summary, b.summary);
        assert_eq!(a.tail_start, b.tail_start);
    }

    #[test]
    fn compact_merges_previous_summary() {
        let history = sample_history();
        let first = compact(&history, None);
        let second = compact(&history, Some(&first.summary));
        assert!(second.summary.contains("[Session Goal]"));
        assert!(second.summary.len() >= first.summary.len());
    }

    #[test]
    fn compact_handles_empty_history() {
        let result = compact(&[], None);
        assert!(result.summary.is_empty());
        assert_eq!(result.tail_start, 0);
    }

    #[test]
    fn compact_preserves_tail_verbatim() {
        let history = sample_history();
        let result = compact(&history, None);
        let tail = &history[result.tail_start..];
        assert!(!tail.is_empty());
        // The tail always starts at a user prompt and is kept as-is by the glue.
        assert!(matches!(&history[result.tail_start], Message::User { .. }));
    }

    fn char_estimate(messages: &[Message]) -> u64 {
        messages
            .iter()
            .map(|m| match m {
                Message::System { content } => content.chars().count() as u64,
                Message::User { content } => content
                    .iter()
                    .map(|b| match b {
                        UserContent::Text(t) => t.text.chars().count() as u64,
                        UserContent::ToolResult(r) => {
                            r.content.iter().map(|c| c.to_text().len() as u64).sum()
                        }
                    })
                    .sum::<u64>(),
                Message::Assistant { content } => content
                    .iter()
                    .map(|b| match b {
                        AssistantContent::Text(t) => t.text.chars().count() as u64,
                        AssistantContent::ToolCall(c) => {
                            c.function.arguments.to_string().len() as u64
                        }
                        _ => 0,
                    })
                    .sum::<u64>(),
            })
            .sum()
    }

    fn long_history() -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..16 {
            msgs.push(user(&format!("do task {i} with a fairly long instruction")));
            msgs.push(assistant_tool_args(
                &format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo step{i}")}),
            ));
            msgs.push(tool_result_of(&format!("t{i}"), &format!("output {i}")));
        }
        msgs.push(user("final user message"));
        msgs
    }

    #[test]
    fn vcc_compact_under_limit_keeps_tail() {
        let mut messages = long_history();
        let original_tail_user = "final user message";
        let under = vcc_compact(&mut messages, u64::MAX, char_estimate, 0);
        assert!(under);
        assert!(is_vcc_summary(&messages[0]));
        assert!(messages.len() > 1, "tail must be preserved");
        // The final user message must survive verbatim in the tail.
        assert!(messages.iter().any(
            |m| matches!(m, Message::User { content } if content.iter().any(
                |b| matches!(b, UserContent::Text(t) if t.text == original_tail_user)
            ))
        ));
        assert!(messages.len() < 20);
    }

    #[test]
    fn vcc_compact_over_limit_returns_false_but_still_compacts() {
        let mut messages = long_history();
        let tiny_limit = 1;
        let under = vcc_compact(&mut messages, tiny_limit, char_estimate, 0);
        assert!(!under, "tiny token limit should remain over the limit");
        assert!(is_vcc_summary(&messages[0]));
    }

    #[test]
    fn vcc_compact_merges_existing_summary_message() {
        let mut first = two_task_history();
        assert!(vcc_compact(&mut first, u64::MAX, char_estimate, 0));
        assert!(first.len() > 3, "first compaction must leave a real tail");
        let before = summary_text(&first[0]).unwrap_or_default().to_string();
        assert!(vcc_compact(&mut first, u64::MAX, char_estimate, 0));
        assert!(!first.is_empty());
        assert!(is_vcc_summary(&first[0]));
        // Merging must keep the previous summary's handoff content around.
        let merged = summary_text(&first[0]).unwrap_or_default();
        assert!(merged.contains("This summary captures"));
        assert!(merged.len() >= before.len() / 2);
    }

    #[test]
    fn vcc_compact_returns_false_for_short_history() {
        let mut messages = vec![user("hi"), user("there")];
        assert!(!vcc_compact(&mut messages, u64::MAX, char_estimate, 0));
        assert_eq!(messages.len(), 2);
    }
}
