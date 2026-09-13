//! Overflow-pruning helpers for the LLM compaction request (ported from
//! Craft's `compaction/strip.rs`, adapted to this repo's history types).
//! Every function here exists so a summarization request that overflowed
//! the provider's context window can be cheaply shrunk and retried.

use crate::history::{AssistantContent, Message, ToolResult, UserContent};

use super::TOOL_RESULT_PLACEHOLDER;

pub(super) fn strip_thinking(messages: &mut [Message]) {
    for message in messages {
        if let Message::Assistant { content } = message {
            content.retain(|block| !matches!(block, AssistantContent::Reasoning(_)));
        }
    }
}

/// Drops tool results whose call has no matching tool call anywhere in
/// `messages`, then drops messages left without content. Providers reject
/// orphaned results, so every truncation must be followed by this.
pub(super) fn remove_orphaned_tool_results(messages: &mut Vec<Message>) {
    let calls: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|message| match message {
            Message::Assistant { content } => content.iter(),
            _ => [].iter(),
        })
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call.id.clone()),
            _ => None,
        })
        .collect();
    for message in messages.iter_mut() {
        if let Message::User { content } = message {
            content.retain(|block| match block {
                UserContent::ToolResult(result) => calls.contains(&result.call),
                UserContent::Text(_) => true,
            });
        }
    }
    messages.retain(|message| match message {
        Message::User { content } => !content.is_empty(),
        Message::Assistant { content } => !content.is_empty(),
        Message::System { .. } => true,
    });
}

fn result_len(result: &ToolResult) -> usize {
    result
        .content
        .iter()
        .map(|block| block.to_text().len())
        .sum()
}

fn is_placeholder(result: &ToolResult) -> bool {
    result.content.len() == 1 && result.content[0].to_text() == TOOL_RESULT_PLACEHOLDER
}

/// Walks newest first and collapses every tool result that does not fit in
/// what is left of `budget` (measured in content chars). One oversized result
/// is collapsed on its own and leaves the budget to the older ones, which is
/// the whole point: charging it would spend the tail on a block that is no
/// longer there. Returns whether anything actually shrank, so a caller
/// retrying on overflow can tell progress from a no-op.
pub(super) fn collapse_tool_results(messages: &mut [Message], mut budget: usize) -> bool {
    let mut collapsed = false;
    for message in messages.iter_mut().rev() {
        let Message::User { content } = message else {
            continue;
        };
        for block in content.iter_mut().rev() {
            let UserContent::ToolResult(result) = block else {
                continue;
            };
            match budget.checked_sub(result_len(result)) {
                Some(rest) => budget = rest,
                None => {
                    collapsed |= !is_placeholder(result);
                    *result = ToolResult::text(
                        result.call.clone(),
                        &result.name,
                        TOOL_RESULT_PLACEHOLDER,
                    );
                }
            }
        }
    }
    collapsed
}

/// Collapses the oldest `ratio` share of non-placeholder tool results to the
/// placeholder. Returns how many were collapsed.
pub(super) fn strip_tool_results_by_ratio(messages: &mut [Message], ratio: f32) -> usize {
    let mut indices: Vec<(usize, usize)> = Vec::new();
    for (mi, message) in messages.iter().enumerate() {
        let Message::User { content } = message else {
            continue;
        };
        for (bi, block) in content.iter().enumerate() {
            if let UserContent::ToolResult(result) = block
                && !is_placeholder(result)
            {
                indices.push((mi, bi));
            }
        }
    }
    let total = indices.len();
    if total == 0 {
        return 0;
    }
    let target = (total as f32 * ratio).ceil() as usize;
    let mut dropped = 0;
    for (mi, bi) in indices.into_iter().take(target) {
        if let Message::User { content } = &mut messages[mi]
            && let UserContent::ToolResult(result) = &mut content[bi]
            && !is_placeholder(result)
        {
            *result = ToolResult::text(result.call.clone(), &result.name, TOOL_RESULT_PLACEHOLDER);
            dropped += 1;
        }
    }
    dropped
}

/// Removes the oldest exchange: the first message, its assistant reply if the
/// removed message was user input, and any assistant leaders left exposed,
/// with orphaned tool results swept after every removal.
pub(super) fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let removed_user = matches!(messages.remove(0), Message::User { .. });
    if removed_user
        && messages.len() > 1
        && matches!(messages.first(), Some(Message::Assistant { .. }))
    {
        messages.remove(0);
    }
    remove_orphaned_tool_results(messages);

    while messages.len() > 1 && matches!(messages.first(), Some(Message::Assistant { .. })) {
        messages.remove(0);
        remove_orphaned_tool_results(messages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support::{assistant_tool, tool_result_of, user};

    #[test]
    fn strip_thinking_removes_reasoning_blocks() {
        let mut messages = vec![
            Message::Assistant {
                content: vec![
                    AssistantContent::Reasoning(Default::default()),
                    AssistantContent::text("hello"),
                ],
            },
            user("keep"),
        ];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].text(), "hello");
    }

    #[test]
    fn orphaned_tool_results_are_dropped() {
        let mut messages = vec![
            assistant_tool("t1", "bash"),
            tool_result_of("t1", "kept"),
            tool_result_of("t2", "orphan"),
            user("after"),
        ];
        remove_orphaned_tool_results(&mut messages);
        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[1], Message::User { content }
            if matches!(&content[0], UserContent::ToolResult(r) if r.call == "t1")));
    }

    #[test]
    fn collapse_tool_results_budgets_the_tail() {
        let mut messages = vec![Message::User {
            content: vec![
                UserContent::ToolResult(crate::history::ToolResult::text("t1", "bash", "old")),
                UserContent::ToolResult(crate::history::ToolResult::text("t2", "bash", "new")),
                UserContent::text("keep me"),
            ],
        }];
        // Only "new" fits: the older result collapses.
        assert!(collapse_tool_results(&mut messages, 3));
        let texts: Vec<String> = match &messages[0] {
            Message::User { content } => content
                .iter()
                .map(|block| match block {
                    UserContent::ToolResult(r) => r.content[0].to_text(),
                    UserContent::Text(t) => t.text.clone(),
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(texts, vec![TOOL_RESULT_PLACEHOLDER, "new", "keep me"]);
        // A zero budget collapses what is left, and only then reports no
        // progress on a further pass.
        assert!(collapse_tool_results(&mut messages, 0));
        assert!(!collapse_tool_results(&mut messages, 0));
    }

    #[test]
    fn strip_by_ratio_removes_oldest_first() {
        let mut messages = vec![Message::User {
            content: vec![
                UserContent::ToolResult(crate::history::ToolResult::text("t1", "bash", "old1")),
                UserContent::ToolResult(crate::history::ToolResult::text("t2", "bash", "old2")),
                UserContent::ToolResult(crate::history::ToolResult::text("t3", "bash", "keep")),
            ],
        }];
        assert_eq!(strip_tool_results_by_ratio(&mut messages, 0.5), 2);
        let texts: Vec<String> = match &messages[0] {
            Message::User { content } => content
                .iter()
                .map(|block| match block {
                    UserContent::ToolResult(r) => r.content[0].to_text(),
                    UserContent::Text(t) => t.text.clone(),
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(texts[0], TOOL_RESULT_PLACEHOLDER);
        assert_eq!(texts[1], TOOL_RESULT_PLACEHOLDER);
        assert_eq!(texts[2], "keep");
        // Placeholders are skipped, so a full sweep only drops the one left.
        assert_eq!(strip_tool_results_by_ratio(&mut messages, 1.0), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_user_and_assistant() {
        let mut messages = vec![user("first"), Message::assistant("reply"), user("second")];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "second");
    }

    #[test]
    fn truncate_oldest_round_removes_tool_pair_and_orphans() {
        let mut messages = vec![
            user("request"),
            assistant_tool("t1", "bash"),
            tool_result_of("t1", "output"),
            user("keep me"),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "keep me");
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![user("only")];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
    }
}
