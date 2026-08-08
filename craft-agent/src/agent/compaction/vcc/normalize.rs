use std::collections::HashMap;

use craft_providers::{ContentBlock, Message, Role};
use serde_json::Value;

use super::util::sanitize;

/// A normalized, role-tagged view of a single content block from the message
/// stream. Mirrors pi-vcc's `NormalizedBlock`.
#[derive(Debug, Clone)]
pub(crate) enum NormalizedBlock {
    User {
        text: String,
        source_index: usize,
    },
    Assistant {
        text: String,
        source_index: usize,
    },
    ToolCall {
        name: String,
        args: Value,
        source_index: usize,
    },
    ToolResult {
        name: String,
        text: String,
        is_error: bool,
        source_index: usize,
    },
    Bash {
        command: String,
        output: String,
        exit_code: Option<i32>,
        source_index: usize,
    },
    #[expect(dead_code)]
    Thinking {
        text: String,
        redacted: bool,
        source_index: usize,
    },
}

impl NormalizedBlock {
    #[expect(dead_code)]
    pub(crate) fn source_index(&self) -> usize {
        match self {
            Self::User { source_index, .. }
            | Self::Assistant { source_index, .. }
            | Self::ToolCall { source_index, .. }
            | Self::ToolResult { source_index, .. }
            | Self::Bash { source_index, .. }
            | Self::Thinking { source_index, .. } => *source_index,
        }
    }

    #[expect(dead_code)]
    pub(crate) fn kind_str(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Assistant { .. } => "assistant",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Bash { .. } => "bash",
            Self::Thinking { .. } => "thinking",
        }
    }
}

fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize Craft messages into [`NormalizedBlock`]s.
///
/// Bash tool calls are folded into [`NormalizedBlock::Bash`] blocks pairing the
/// command (from the tool-use input) with the output/exit code (from the paired
/// tool result). Their tool result is not emitted separately.
pub(crate) fn normalize(messages: &[Message]) -> Vec<NormalizedBlock> {
    let mut name_by_id: HashMap<&str, &str> = HashMap::new();
    let mut result_by_id: HashMap<&str, (&str, bool)> = HashMap::new();
    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::ToolUse { id, name, .. } => {
                    name_by_id.insert(id.as_str(), name.as_str());
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    ..
                } => {
                    result_by_id.insert(tool_use_id.as_str(), (content.as_str(), *is_error));
                }
                _ => {}
            }
        }
    }

    let bash_ids: HashMap<&str, &str> = name_by_id
        .iter()
        .filter(|(_, n)| n.eq_ignore_ascii_case("bash"))
        .map(|(k, v)| (*k, *v))
        .collect();

    let mut out = Vec::new();
    for (msg_index, msg) in messages.iter().enumerate() {
        match msg.role {
            Role::User => {
                let text = sanitize(&text_of(&msg.content));
                if !text.is_empty() {
                    out.push(NormalizedBlock::User {
                        text,
                        source_index: msg_index,
                    });
                }
                for block in &msg.content {
                    if let ContentBlock::Image { source } = block {
                        out.push(NormalizedBlock::User {
                            text: format!("[image: {}]", source.media_type.as_mime()),
                            source_index: msg_index,
                        });
                    }
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = block
                    {
                        if bash_ids.contains_key(tool_use_id.as_str()) {
                            continue;
                        }
                        let name = name_by_id
                            .get(tool_use_id.as_str())
                            .copied()
                            .unwrap_or("unknown")
                            .to_string();
                        out.push(NormalizedBlock::ToolResult {
                            name,
                            text: sanitize(content),
                            is_error: *is_error,
                            source_index: msg_index,
                        });
                    }
                }
            }
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            let t = sanitize(text);
                            if !t.is_empty() {
                                out.push(NormalizedBlock::Assistant {
                                    text: t,
                                    source_index: msg_index,
                                });
                            }
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            let t = sanitize(thinking);
                            if !t.is_empty() {
                                out.push(NormalizedBlock::Thinking {
                                    text: t,
                                    redacted: false,
                                    source_index: msg_index,
                                });
                            }
                        }
                        ContentBlock::RedactedThinking { data } => {
                            out.push(NormalizedBlock::Thinking {
                                text: data.clone(),
                                redacted: true,
                                source_index: msg_index,
                            });
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            if name.eq_ignore_ascii_case("bash") {
                                let command = input
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| input.get("description").and_then(|v| v.as_str()))
                                    .unwrap_or("")
                                    .to_string();
                                let (output, is_error) = result_by_id
                                    .get(id.as_str())
                                    .map(|(c, e)| (c.to_string(), *e))
                                    .unwrap_or_default();
                                let exit_code = if is_error { Some(1) } else { Some(0) };
                                out.push(NormalizedBlock::Bash {
                                    command,
                                    output,
                                    exit_code,
                                    source_index: msg_index,
                                });
                            } else {
                                out.push(NormalizedBlock::ToolCall {
                                    name: name.clone(),
                                    args: input.clone(),
                                    source_index: msg_index,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_user_and_assistant_text() {
        let msgs = vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "hi there".into(),
                }],
                ..Default::default()
            },
        ];
        let blocks = normalize(&msgs);
        assert!(matches!(blocks[0], NormalizedBlock::User { ref text, .. } if text == "hello"));
        assert!(
            matches!(blocks[1], NormalizedBlock::Assistant { ref text, .. } if text == "hi there")
        );
    }

    #[test]
    fn normalize_folds_bash_into_bash_block() {
        let msgs = vec![
            Message::user("run it".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "git commit -m 'fix'"}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "[main abc123] fix".into(),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let blocks = normalize(&msgs);
        assert!(
            blocks
                .iter()
                .any(|b| matches!(b, NormalizedBlock::Bash { .. }))
        );
        assert!(
            !blocks
                .iter()
                .any(|b| matches!(b, NormalizedBlock::ToolResult { .. }))
        );
    }
}
