use std::collections::HashSet;

use craft_providers::{ContentBlock, Message, Role};

/// Result of a task-boundary-aware cut: summarize `history[..tail_start]` and
/// keep `history[tail_start..]` verbatim.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cut {
    pub tail_start: usize,
}

fn is_prompt_user(msg: &Message) -> bool {
    matches!(msg.role, Role::User)
        && msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { .. }))
}

fn tool_use_ids(msg: &Message) -> Vec<String> {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(msg: &Message) -> Vec<String> {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

/// Find completed tool-call cycle boundaries. A cycle ends at the message that
/// supplies the last matching tool result for an assistant's tool calls.
fn completed_cycle_ends(messages: &[Message]) -> Vec<usize> {
    let mut cycles = Vec::new();
    let mut current_assistant: Option<usize> = None;
    let mut pending: HashSet<String> = HashSet::new();
    for (i, msg) in messages.iter().enumerate() {
        if matches!(msg.role, Role::Assistant) {
            current_assistant = Some(i);
            pending.clear();
            for id in tool_use_ids(msg) {
                pending.insert(id);
            }
            continue;
        }
        if matches!(msg.role, Role::User) {
            for id in tool_result_ids(msg) {
                pending.remove(&id);
            }
            if pending.is_empty() && current_assistant.is_some() {
                cycles.push(i);
                current_assistant = None;
            }
        }
    }
    cycles
}

/// Detect whether the turn starting at `cut` is mid-flight: the assistant
/// issued tool calls that have no matching tool results yet.
fn is_mid_flight(messages: &[Message], cut: usize) -> bool {
    let mut calls: HashSet<String> = HashSet::new();
    let mut results: HashSet<String> = HashSet::new();
    for msg in &messages[cut + 1..] {
        if matches!(msg.role, Role::User)
            && msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. }))
        {
            break;
        }
        for id in tool_use_ids(msg) {
            calls.insert(id);
        }
        for id in tool_result_ids(msg) {
            results.insert(id);
        }
    }
    calls.iter().any(|id| !results.contains(id))
}

/// Task-boundary-aware cut: keep a tail starting at the last user prompt whose
/// response cycle is complete. Falls back to a mid-cycle boundary for
/// single-prompt agentic chains, then to compact-all.
pub(crate) fn find_cut(history: &[Message]) -> Option<Cut> {
    if history.len() <= 2 {
        return None;
    }

    let last_user = history.iter().rposition(is_prompt_user);
    let mut cut = last_user.unwrap_or_default();

    if cut > 0 && is_mid_flight(history, cut) {
        if let Some(prev) = history[..cut].iter().rposition(is_prompt_user) {
            cut = prev;
        } else {
            cut = 0;
        }
    }

    if cut == 0 {
        let cycles = completed_cycle_ends(history);
        let target = history.len() / 2;
        if let Some(&best) = cycles
            .iter()
            .filter(|&&c| c > 0 && c < history.len() - 1)
            .min_by_key(|&&c| (c as isize - target as isize).unsigned_abs())
        {
            return Some(Cut {
                tail_start: best + 1,
            });
        }
        return Some(Cut {
            tail_start: history.len(),
        });
    }

    Some(Cut { tail_start: cut })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::user(text.into())
    }
    fn assistant_tool(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(id, "bash", serde_json::json!({}))],
            ..Default::default()
        }
    }
    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                images: vec![],
                is_error: false,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn cut_keeps_tail_after_last_user_prompt() {
        let history = vec![
            user("do task one"),
            assistant_tool("t1"),
            tool_result("t1"),
            user("do task two"),
            assistant_tool("t2"),
            tool_result("t2"),
        ];
        let cut = find_cut(&history).unwrap();
        assert_eq!(cut.tail_start, 3);
        assert!(matches!(history[cut.tail_start].role, Role::User));
        assert!(matches!(
            &history[cut.tail_start].content[0],
            ContentBlock::Text { text } if text == "do task two"
        ));
    }

    #[test]
    fn cut_pushes_back_when_turn_mid_flight() {
        let history = vec![
            user("task one"),
            assistant_tool("t1"),
            tool_result("t1"),
            user("task two"),
            assistant_tool("t2"),
        ];
        let cut = find_cut(&history).unwrap();
        assert_eq!(cut.tail_start, 3);
    }

    #[test]
    fn cut_returns_none_for_short_history() {
        let history = vec![user("hi"), assistant_tool("t1")];
        assert!(find_cut(&history).is_none());
    }

    #[test]
    fn cut_mid_cycle_for_single_prompt_chain() {
        let mut history = vec![user("do everything")];
        for i in 0..10 {
            history.push(assistant_tool(&format!("t{i}")));
            history.push(tool_result(&format!("t{i}")));
        }
        let cut = find_cut(&history).unwrap();
        assert!(cut.tail_start > 0 && cut.tail_start < history.len());
    }
}
