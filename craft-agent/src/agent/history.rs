use std::sync::Arc;

use arc_swap::ArcSwap;
use craft_providers::{ContentBlock, Message, Role};

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

    #[test]
    fn estimate_tokens_counts_history() {
        let model =
            craft_providers::Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let history = History::new(vec![
            Message::user("hello world".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "hi there".into(),
                }],
                ..Default::default()
            },
        ]);
        let tokens = history.estimate_tokens(&model);
        assert!(
            tokens > 0,
            "should estimate some tokens for non-empty history"
        );
    }
}
