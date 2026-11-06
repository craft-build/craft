//! Task-boundary-aware cut policy: the pairing scan below is deliberately
//! local (animist F17 would export it), the boundary rule itself is craft's.

use std::collections::HashSet;

use crate::history::{Role, StoredMessage, ext};

/// Result of a task-boundary-aware cut: summarize `history[..tail_start]` and
/// keep `history[tail_start..]` verbatim.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cut {
    pub tail_start: usize,
}

fn is_prompt_user(stored: &StoredMessage) -> bool {
    ext::is_user(stored.message())
        && stored
            .message()
            .content
            .iter()
            .any(|b| matches!(b, crate::history::ContentBlock::Text { .. }))
}

fn tool_use_ids(stored: &StoredMessage) -> Vec<String> {
    ext::tool_calls(stored.message())
        .filter_map(|block| match block {
            crate::history::ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(stored: &StoredMessage) -> Vec<String> {
    ext::tool_results(stored.message())
        .filter_map(|block| match block {
            crate::history::ContentBlock::ToolResult { tool_use_id, .. } => {
                Some(tool_use_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn is_user(stored: &StoredMessage) -> bool {
    matches!(stored.message().role, Role::User)
}

fn is_assistant(stored: &StoredMessage) -> bool {
    matches!(stored.message().role, Role::Assistant)
}

/// Find completed tool-call cycle boundaries. A cycle ends at the message that
/// supplies the last matching tool result for an assistant's tool calls.
fn completed_cycle_ends(messages: &[StoredMessage]) -> Vec<usize> {
    let mut cycles = Vec::new();
    let mut current_assistant: Option<usize> = None;
    let mut pending: HashSet<String> = HashSet::new();
    for (i, stored) in messages.iter().enumerate() {
        if is_assistant(stored) {
            current_assistant = Some(i);
            pending.clear();
            for id in tool_use_ids(stored) {
                pending.insert(id);
            }
            continue;
        }
        if is_user(stored) {
            for id in tool_result_ids(stored) {
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
fn is_mid_flight(messages: &[StoredMessage], cut: usize) -> bool {
    let mut calls: HashSet<String> = HashSet::new();
    let mut results: HashSet<String> = HashSet::new();
    for stored in &messages[cut + 1..] {
        if is_prompt_user(stored) {
            break;
        }
        for id in tool_use_ids(stored) {
            calls.insert(id);
        }
        for id in tool_result_ids(stored) {
            results.insert(id);
        }
    }
    calls.iter().any(|id| !results.contains(id))
}

/// Task-boundary-aware cut: keep a tail starting at the last user prompt whose
/// response cycle is complete. Falls back to a mid-cycle boundary for
/// single-prompt agentic chains, then to compact-all.
pub(crate) fn find_cut(history: &[StoredMessage]) -> Option<Cut> {
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
    use crate::compaction::test_support::{stored_assistant_tool, stored_tool_result, stored_user};

    #[test]
    fn cut_keeps_tail_after_last_user_prompt() {
        let history = vec![
            stored_user("do task one"),
            stored_assistant_tool("t1", "bash"),
            stored_tool_result("t1"),
            stored_user("do task two"),
            stored_assistant_tool("t2", "bash"),
            stored_tool_result("t2"),
        ];
        let cut = find_cut(&history).unwrap();
        assert_eq!(cut.tail_start, 3);
        assert_eq!(ext::message_text(&history[cut.tail_start]), "do task two");
    }

    #[test]
    fn cut_pushes_back_when_turn_mid_flight() {
        let history = vec![
            stored_user("task one"),
            stored_assistant_tool("t1", "bash"),
            stored_tool_result("t1"),
            stored_user("task two"),
            stored_assistant_tool("t2", "bash"),
        ];
        let cut = find_cut(&history).unwrap();
        assert_eq!(cut.tail_start, 3);
    }

    #[test]
    fn cut_returns_none_for_short_history() {
        let history = vec![stored_user("hi"), stored_assistant_tool("t1", "bash")];
        assert!(find_cut(&history).is_none());
    }

    #[test]
    fn cut_mid_cycle_for_single_prompt_chain() {
        let mut history = vec![stored_user("do everything")];
        for i in 0..10 {
            history.push(stored_assistant_tool(&format!("t{i}"), "bash"));
            history.push(stored_tool_result(&format!("t{i}")));
        }
        let cut = find_cut(&history).unwrap();
        assert!(cut.tail_start > 0 && cut.tail_start < history.len());
    }
}
