use std::sync::Arc;

use arc_swap::ArcSwap;
use craft_providers::{ContentBlock, Message, Role};
use tracing::warn;

const CANCEL_MARKER: &str = "[Cancelled by user]";
const UNAVAILABLE_RESULT: &str = "[Tool result not available]";

pub type SharedMessages = Arc<ArcSwap<Vec<Message>>>;

pub struct History {
    messages: Vec<Message>,
    mirror: Option<SharedMessages>,
}

impl History {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            messages,
            mirror: None,
        }
    }

    pub fn restored(mut messages: Vec<Message>) -> Self {
        sanitize_restored(&mut messages);
        Self {
            messages,
            mirror: None,
        }
    }

    pub fn with_mirror(mut self, mirror: SharedMessages) -> Self {
        self.mirror = Some(mirror);
        self.publish();
        self
    }

    pub fn as_slice(&self) -> &[Message] {
        &self.messages
    }

    pub fn as_mut_slice(&mut self) -> &mut [Message] {
        &mut self.messages
    }

    pub fn push(&mut self, msg: Message) {
        self.edit(|msgs| msgs.push(msg));
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn has_recent_tool_results(&self, depth: usize) -> bool {
        let msgs = self.as_slice();
        let start = msgs.len().saturating_sub(depth);
        msgs[start..].iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
    }

    pub fn replace(&mut self, messages: Vec<Message>) {
        self.edit(|msgs| *msgs = messages);
    }

    pub fn truncate(&mut self, len: usize) {
        self.edit(|msgs| msgs.truncate(len));
    }

    pub fn into_vec(self) -> Vec<Message> {
        self.messages
    }

    pub fn edit(&mut self, f: impl FnOnce(&mut Vec<Message>)) {
        f(&mut self.messages);
        self.publish();
    }

    fn publish(&self) {
        if let Some(mirror) = &self.mirror {
            let mut snapshot = self.messages.clone();
            close_dangling_tool_calls(&mut snapshot, UNAVAILABLE_RESULT);
            mirror.store(Arc::new(snapshot));
        }
    }

    pub fn select_view(&self, indices: &[usize], total_len: usize) -> Vec<Message> {
        let mut result = Vec::new();
        let mut last_included: Option<usize> = None;

        for &idx in indices {
            if let Some(prev) = last_included
                && idx > prev + 1
            {
                let gap_count = idx - prev - 1;
                result.push(Message::user(format!(
                    "[{gap_count} earlier messages omitted — use retrieve tool if needed]"
                )));
            }
            if let Some(msg) = self.messages.get(idx) {
                result.push(msg.clone());
            }
            last_included = Some(idx);
        }

        if let Some(last_idx) = last_included
            && total_len > last_idx + 1
        {
            let remaining = total_len - last_idx - 1;
            result.push(Message::user(format!(
                "[{remaining} later messages omitted — use retrieve tool if needed]"
            )));
        }

        result
    }

    pub fn message_token_estimate(&self, model: &craft_providers::Model, idx: usize) -> u32 {
        let mut total = 0u32;
        if let Some(msg) = self.messages.get(idx) {
            for block in &msg.content {
                let text = match block {
                    ContentBlock::Text { text } => text.as_str(),
                    ContentBlock::ToolResult { content, .. } => content.as_str(),
                    ContentBlock::ToolUse { name, input, .. } => {
                        total += model.estimate_tokens(name);
                        &serde_json::to_string(input).unwrap_or_default()
                    }
                    _ => continue,
                };
                total += model.estimate_tokens(text);
            }
        }
        total
    }

    /// Estimate total tokens for all messages using character-based heuristic.
    pub fn estimate_tokens(&self, model: &craft_providers::Model) -> u32 {
        use craft_providers::ContentBlock;
        let mut total = 0u32;
        for msg in &self.messages {
            for block in &msg.content {
                let text = match block {
                    ContentBlock::Text { text } => text.as_str(),
                    ContentBlock::ToolResult { content, .. } => content.as_str(),
                    ContentBlock::ToolUse { name, input, .. } => {
                        total += model.estimate_tokens(name);
                        &serde_json::to_string(input).unwrap_or_default()
                    }
                    _ => continue,
                };
                total += model.estimate_tokens(text);
            }
        }
        total
    }
}

pub(super) fn remove_orphaned_tool_results(messages: &mut Vec<Message>) -> bool {
    let mut changed = false;
    let mut i = 0;
    while i < messages.len() {
        if !matches!(messages[i].role, Role::User) {
            i += 1;
            continue;
        }

        let valid_ids: Vec<String> = if i > 0 && matches!(messages[i - 1].role, Role::Assistant) {
            messages[i - 1]
                .tool_uses()
                .map(|(id, _, _)| id.to_owned())
                .collect()
        } else {
            Vec::new()
        };

        let content_len = messages[i].content.len();
        let (mut had_results, mut kept_results) = (false, false);
        messages[i].content.retain(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => {
                had_results = true;
                let keep = valid_ids.iter().any(|id| id == tool_use_id);
                kept_results |= keep;
                keep
            }
            _ => true,
        });
        if had_results && !kept_results {
            messages[i]
                .content
                .retain(|b| !matches!(b, ContentBlock::Image { .. }));
        }
        changed |= messages[i].content.len() != content_len;

        if messages[i].content.is_empty() {
            messages.remove(i);
            changed = true;
        } else {
            i += 1;
        }
    }

    changed
}

/// Restored sessions can have orphaned tool_results or unclosed tool_uses
/// (e.g. the process was killed mid-turn). The API returns 400 if it sees those.
fn sanitize_restored(messages: &mut Vec<Message>) {
    let len_before = messages.len();
    let mut changed = remove_orphaned_tool_results(messages);
    close_dangling_tool_calls(messages, UNAVAILABLE_RESULT);
    changed |= messages.len() != len_before;

    if changed {
        warn!(
            before = len_before,
            after = messages.len(),
            "sanitized restored history"
        );
    }
}

fn close_dangling_tool_calls(messages: &mut Vec<Message>, note: &str) {
    let Some(last) = messages.last() else { return };
    if !matches!(last.role, Role::Assistant) || !last.has_tool_calls() {
        return;
    }
    let error_results: Vec<ContentBlock> = last
        .tool_uses()
        .map(|(id, _, _)| ContentBlock::ToolResult {
            tool_use_id: id.to_owned(),
            content: note.to_owned(),
            images: vec![],
            is_error: true,
        })
        .collect();
    messages.push(Message {
        role: Role::User,
        content: error_results,
        display_text: Some(String::new()),
    });
}

pub(crate) fn sanitize_cancelled_history(history: &mut History, rollback_len: usize) {
    if history.len() <= rollback_len {
        return;
    }
    history.edit(|msgs| {
        close_dangling_tool_calls(msgs, CANCEL_MARKER);
        msgs.push(Message::synthetic(CANCEL_MARKER.into()));
    });
}

#[cfg(test)]
mod tests {
    use craft_providers::{ContentBlock, Message, Role};
    use serde_json;
    use test_case::test_case;

    use super::*;

    fn make_tool_use_msg(ids: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn make_tool_result_msg(ids: &[&str]) -> Message {
        Message {
            role: Role::User,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: "ok".into(),
                    images: vec![],
                    is_error: false,
                })
                .collect(),
            display_text: Some(String::new()),
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        }
    }

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(matches!(&last.content[0], ContentBlock::Text { text } if text == CANCEL_MARKER));
    }

    #[test_case(
        vec![Message::user("old".into())],
        1,
        1,
        false
        ; "no_new_messages_is_noop"
    )]
    #[test_case(
        vec![Message::user("hello".into())],
        0,
        2,
        true
        ; "user_only_appends_marker"
    )]
    #[test_case(
        vec![
            Message::user("hello".into()),
            Message { role: Role::Assistant, content: vec![ContentBlock::Text { text: "hi".into() }], ..Default::default() },
        ],
        0,
        3,
        true
        ; "complete_turn_appends_marker"
    )]
    fn sanitize_cancelled_history_cases(
        messages: Vec<Message>,
        rollback_len: usize,
        expected_len: usize,
        expect_cancel_marker: bool,
    ) {
        let mut history = History::new(messages);
        sanitize_cancelled_history(&mut history, rollback_len);
        assert_eq!(history.len(), expected_len);
        if expect_cancel_marker {
            assert_ends_with_cancel_marker(&history);
        }
    }

    #[test]
    fn sanitize_dangling_tool_use_adds_error_results() {
        let mut history = History::new(vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "let me check".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "/tmp"}),
                    },
                    ContentBlock::ToolUse {
                        id: "t2".into(),
                        name: "glob".into(),
                        input: serde_json::json!({"pattern": "*.rs"}),
                    },
                ],
                ..Default::default()
            },
        ]);
        sanitize_cancelled_history(&mut history, 0);

        let tool_result_msg = &history.as_slice()[2];
        let error_ids: Vec<&str> = tool_result_msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error: true,
                    ..
                } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(error_ids, ["t1", "t2"]);
        assert_ends_with_cancel_marker(&history);
    }

    #[test_case(
        vec![make_tool_result_msg(&["t1"])],
        0
        ; "orphan_at_start_removed"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            text_msg(Role::Assistant, "done"),
            make_tool_result_msg(&["orphan1", "orphan2"]),
        ],
        2
        ; "orphans_after_non_tool_assistant_removed"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1", "t2"]),
            make_tool_result_msg(&["t1", "t2"]),
        ],
        3
        ; "valid_pairing_preserved"
    )]
    #[test_case(
        vec![Message::user("go".into()), make_tool_use_msg(&["t1"])],
        3
        ; "dangling_tool_use_closed_with_synthetic_result"
    )]
    fn sanitize_restored_cases(messages: Vec<Message>, expected_len: usize) {
        let history = History::restored(messages);
        assert_eq!(history.len(), expected_len);
    }

    #[test]
    fn sanitize_restored_partial_orphan_keeps_matched_ids() {
        let history = History::restored(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            make_tool_result_msg(&["t1", "t2"]),
        ]);
        let results: Vec<&str> = history.as_slice()[2]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ["t1"]);
    }

    #[test]
    fn sanitize_restored_drops_image_when_all_results_orphaned() {
        let image_block = ContentBlock::Image {
            source: craft_providers::ImageSource::new(
                craft_providers::ImageMediaType::Png,
                std::sync::Arc::from("aGVsbG8="),
            ),
        };
        let mut orphaned = make_tool_result_msg(&["orphan"]);
        orphaned.content.push(image_block.clone());
        let history = History::restored(vec![Message::user("go".into()), orphaned]);
        assert_eq!(history.len(), 1);

        // Chat-pasted image (no tool results) is untouched.
        let history = History::restored(vec![Message {
            role: Role::User,
            content: vec![image_block],
            ..Default::default()
        }]);
        assert_eq!(history.len(), 1);
        assert!(matches!(
            history.as_slice()[0].content[0],
            ContentBlock::Image { .. }
        ));
    }

    #[test]
    fn sanitize_restored_keeps_image_when_any_result_survives() {
        let mut msg = make_tool_result_msg(&["t1", "orphan"]);
        msg.content.push(ContentBlock::Image {
            source: craft_providers::ImageSource::new(
                craft_providers::ImageMediaType::Png,
                std::sync::Arc::from("aGVsbG8="),
            ),
        });
        let history = History::restored(vec![
            Message::user("go".into()),
            make_tool_use_msg(&["t1"]),
            msg,
        ]);
        let content = &history.as_slice()[2].content;
        assert!(matches!(
            &content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"
        ));
        assert!(matches!(content[1], ContentBlock::Image { .. }));
    }

    #[test]
    fn remove_orphaned_tool_results_reports_content_change() {
        let mut result = make_tool_result_msg(&["orphan"]);
        result.content.push(ContentBlock::Text {
            text: "keep me".into(),
        });
        let mut messages = vec![result];

        assert!(remove_orphaned_tool_results(&mut messages));
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0].content[..],
            [ContentBlock::Text { text }] if text == "keep me"
        ));
        assert!(!remove_orphaned_tool_results(&mut messages));
    }

    #[test_case(
        vec![Message::user("go".into())],
        0
        ; "no_tool_results"
    )]
    #[test_case(
        vec![
            Message::user("go".into()),
            make_tool_result_msg(&["t1"]),
        ],
        1
        ; "recent_tool_result"
    )]
    #[test_case(
        vec![
            Message::user("old1".into()),
            Message::user("old2".into()),
            Message::user("old3".into()),
            Message::user("old4".into()),
            Message::user("old5".into()),
            make_tool_result_msg(&["t1"]),
        ],
        1
        ; "at_depth_boundary"
    )]
    fn has_recent_tool_results(messages: Vec<Message>, depth: usize) {
        let history = History::new(messages);
        let result = if depth == 0 {
            history.has_recent_tool_results(0)
        } else {
            history.has_recent_tool_results(depth)
        };
        assert_eq!(result, depth > 0);
    }
}
