//! Overflow-pruning helpers for the LLM compaction request (ported from
//! Craft's `compaction/strip.rs`, adapted to animist's history types).
//! Every function here exists so a summarization request that overflowed
//! the provider's context window can be cheaply shrunk and retried.

use animist::history::{ContentBlock, Message, Role, ToolContents};

use crate::history::ext;

use super::TOOL_RESULT_PLACEHOLDER;

pub(super) fn strip_thinking(messages: &mut [Message]) {
    for message in messages {
        if matches!(message.role, Role::Assistant) {
            message.content.retain(|block| !block.is_thinking());
        }
    }
}

/// Drops tool results whose call has no matching tool call anywhere in
/// `messages`, then drops messages left without content. Providers reject
/// orphaned results, so every truncation must be followed by this.
pub(super) fn remove_orphaned_tool_results(messages: &mut Vec<Message>) {
    let calls: std::collections::HashSet<String> = messages
        .iter()
        .filter(|message| matches!(message.role, Role::Assistant))
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    for message in messages.iter_mut() {
        if matches!(message.role, Role::User) {
            message.content.retain(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => calls.contains(tool_use_id),
                _ => true,
            });
        }
    }
    messages.retain(|message| match message.role {
        Role::System => true,
        _ => !message.content.is_empty(),
    });
}

fn result_len(content: &ToolContents) -> usize {
    ext::tool_content_text(content).len()
}

fn is_placeholder(content: &ToolContents) -> bool {
    content.0.len() == 1
        && matches!(&content.0[0],
            animist::history::ToolContent::Text { text } if text == TOOL_RESULT_PLACEHOLDER)
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
        if !matches!(message.role, Role::User) {
            continue;
        }
        for block in message.content.iter_mut().rev() {
            let ContentBlock::ToolResult {
                tool_use_id,
                name,
                content,
                ..
            } = block
            else {
                continue;
            };
            match budget.checked_sub(result_len(content)) {
                Some(rest) => budget = rest,
                None => {
                    collapsed |= !is_placeholder(content);
                    *block = ext::tool_result(
                        tool_use_id.clone(),
                        name.as_deref(),
                        TOOL_RESULT_PLACEHOLDER,
                        false,
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
        if !matches!(message.role, Role::User) {
            continue;
        }
        for (bi, block) in message.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = block
                && !is_placeholder(content)
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
        if let ContentBlock::ToolResult {
            tool_use_id,
            name,
            content,
            ..
        } = &messages[mi].content[bi]
            && !is_placeholder(content)
        {
            messages[mi].content[bi] = ext::tool_result(
                tool_use_id.clone(),
                name.as_deref(),
                TOOL_RESULT_PLACEHOLDER,
                false,
            );
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

    let removed_user = matches!(messages.remove(0).role, Role::User);
    if removed_user
        && messages.len() > 1
        && matches!(messages.first().map(|m| &m.role), Some(Role::Assistant))
    {
        messages.remove(0);
    }
    remove_orphaned_tool_results(messages);

    while messages.len() > 1 && matches!(messages.first().map(|m| &m.role), Some(Role::Assistant)) {
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
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "hmm".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: "hello".into(),
                    },
                ],
                display_text: None,
                kind: animist::history::MessageKind::Turn,
            },
            user("keep"),
        ];
        strip_thinking(&mut messages);
        assert_eq!(ext::text(&messages[0]), "hello");
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
        assert!(matches!(&messages[1].content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"));
    }

    #[test]
    fn collapse_tool_results_budgets_the_tail() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ext::tool_result("t1", Some("bash"), "old", false),
                ext::tool_result("t2", Some("bash"), "new", false),
                ContentBlock::Text {
                    text: "keep me".into(),
                },
            ],
            display_text: None,
            kind: animist::history::MessageKind::Turn,
        }];
        // Only "new" fits: the older result collapses.
        assert!(collapse_tool_results(&mut messages, 3));
        let texts: Vec<String> = messages[0]
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { content, .. } => ext::tool_content_text(content),
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts, vec![TOOL_RESULT_PLACEHOLDER, "new", "keep me"]);
        // A zero budget collapses what is left, and only then reports no
        // progress on a further pass.
        assert!(collapse_tool_results(&mut messages, 0));
        assert!(!collapse_tool_results(&mut messages, 0));
    }

    #[test]
    fn strip_by_ratio_removes_oldest_first() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ext::tool_result("t1", Some("bash"), "old1", false),
                ext::tool_result("t2", Some("bash"), "old2", false),
                ext::tool_result("t3", Some("bash"), "keep", false),
            ],
            display_text: None,
            kind: animist::history::MessageKind::Turn,
        }];
        assert_eq!(strip_tool_results_by_ratio(&mut messages, 0.5), 2);
        let texts: Vec<String> = messages[0]
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { content, .. } => ext::tool_content_text(content),
                ContentBlock::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts[0], TOOL_RESULT_PLACEHOLDER);
        assert_eq!(texts[1], TOOL_RESULT_PLACEHOLDER);
        assert_eq!(texts[2], "keep");
        // Placeholders are skipped, so a full sweep only drops the one left.
        assert_eq!(strip_tool_results_by_ratio(&mut messages, 1.0), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_user_and_assistant() {
        let mut messages = vec![user("first"), ext::assistant_text("reply"), user("second")];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert_eq!(ext::text(&messages[0]), "second");
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
        assert_eq!(ext::text(&messages[0]), "keep me");
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![user("only")];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
    }
}
