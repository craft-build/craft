use std::collections::HashMap;

use serde_json::Value;

use crate::history::{AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent};

use super::util::sanitize;

/// A normalized, role-tagged view of a single content block from the message
/// stream.
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

fn text_of_user(content: &[UserContent]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize history messages into [`NormalizedBlock`]s.
///
/// Bash tool calls are folded into [`NormalizedBlock::Bash`] blocks pairing the
/// command (from the tool-call arguments) with the output/exit code (from the
/// paired tool result). Their tool result is not emitted separately. Error
/// flags come from the recorded tool result.
pub(crate) fn normalize(messages: &[Message]) -> Vec<NormalizedBlock> {
    let mut name_by_id: HashMap<String, String> = HashMap::new();
    let mut result_by_id: HashMap<String, (String, bool)> = HashMap::new();
    for msg in messages {
        match msg {
            Message::Assistant { content } => {
                for block in content {
                    if let AssistantContent::ToolCall(call) = block {
                        name_by_id.insert(call.id.clone(), call.function.name.clone());
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let UserContent::ToolResult(result) = block {
                        result_by_id.insert(
                            result.call.clone(),
                            (tool_result_text(&result.content), result.is_error),
                        );
                    }
                }
            }
            Message::System { .. } => {}
        }
    }

    let bash_ids: HashMap<String, String> = name_by_id
        .iter()
        .filter(|(_, n)| n.eq_ignore_ascii_case("bash"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let mut out = Vec::new();
    for (msg_index, msg) in messages.iter().enumerate() {
        match msg {
            Message::User { content } => {
                let text = sanitize(&text_of_user(content));
                if !text.is_empty() {
                    out.push(NormalizedBlock::User {
                        text,
                        source_index: msg_index,
                    });
                }
                for block in content {
                    if let UserContent::ToolResult(result) = block {
                        let tool_use_id = result.call.clone();
                        if bash_ids.contains_key(&tool_use_id) {
                            continue;
                        }
                        let name = name_by_id
                            .get(&tool_use_id)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string());
                        let (text, is_error) =
                            result_by_id.get(&tool_use_id).cloned().unwrap_or_default();
                        out.push(NormalizedBlock::ToolResult {
                            name,
                            text: sanitize(&text),
                            is_error,
                            source_index: msg_index,
                        });
                    }
                }
            }
            Message::Assistant { content } => {
                for block in content {
                    match block {
                        AssistantContent::Text(t) => {
                            let text = sanitize(&t.text);
                            if !text.is_empty() {
                                out.push(NormalizedBlock::Assistant {
                                    text,
                                    source_index: msg_index,
                                });
                            }
                        }
                        AssistantContent::Reasoning(reasoning) => {
                            let text: String = reasoning
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ReasoningContent::Text { text } => Some(text.as_str()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            let t = sanitize(&text);
                            if !t.is_empty() {
                                out.push(NormalizedBlock::Thinking {
                                    text: t,
                                    redacted: false,
                                    source_index: msg_index,
                                });
                            }
                        }
                        AssistantContent::ToolCall(call) => {
                            let name = &call.function.name;
                            let input = &call.function.arguments;
                            let id = call.id.clone();
                            if name.eq_ignore_ascii_case("bash") {
                                let command = input
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .or_else(|| input.get("description").and_then(|v| v.as_str()))
                                    .unwrap_or("")
                                    .to_string();
                                let (output, is_error) =
                                    result_by_id.get(&id).cloned().unwrap_or_default();
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
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support::{assistant_tool_args, tool_result_of, user};

    #[test]
    fn normalize_user_and_assistant_text() {
        let msgs = vec![
            user("hello"),
            crate::compaction::test_support::assistant_text("hi there"),
        ];
        let blocks = normalize(&msgs);
        assert!(matches!(&blocks[0], NormalizedBlock::User { text, .. } if text == "hello"));
        assert!(
            matches!(&blocks[1], NormalizedBlock::Assistant { text, .. } if text == "hi there")
        );
    }

    #[test]
    fn normalize_folds_bash_into_bash_block() {
        let msgs = vec![
            user("run it"),
            assistant_tool_args(
                "t1",
                "bash",
                serde_json::json!({"command": "git commit -m 'fix'"}),
            ),
            tool_result_of("t1", "[main abc123] fix"),
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
