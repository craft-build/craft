//! The provider edge: the only module that maps between craft's
//! [`history`] types and animist's. Everything crossing to or from a
//! provider model goes through here so the rest of the crate never names
//! a wire shape; animist's adapters own the actual protocol encoding and
//! replay validation.

use std::collections::HashMap;

use animist::history::{ContentBlock, ImageSource, Message, Role, ToolContent, ToolContents};
use animist::provider::Tool;

use crate::history;

/// Map craft's history onto animist's wire messages.
///
/// Adjacent user messages coalesce: history records a separate user
/// message per tool result (waves of parallel calls) and separate
/// end/cancel markers after them, while Anthropic Messages and Bedrock
/// Converse answer every tool use in the immediately following user
/// message. Image-bearing messages stay separate when tool-result images
/// relocate, because Chat Completions rejects images in a message that
/// also carries tool results.
///
/// `relocate_tool_images` moves images inside tool results into a user
/// message after the tool results, which Chat Completions targets need;
/// protocols that accept tool-result images carry them inline. An
/// assistant turn whose every part filters out (thinking with no payload)
/// is dropped rather than sent as an empty message providers reject.
pub fn to_animist(history: &[history::Message], relocate_tool_images: bool) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    for message in history {
        match message {
            history::Message::System { content } => {
                out.push(wire_message(
                    Role::System,
                    vec![ContentBlock::Text {
                        text: content.clone(),
                    }],
                ));
            }
            history::Message::User { content } => {
                let mut blocks = Vec::with_capacity(content.len());
                let mut relocated: Vec<ContentBlock> = Vec::new();
                for block in content {
                    match block {
                        history::UserContent::Text(text) => blocks.push(ContentBlock::Text {
                            text: text.text.clone(),
                        }),
                        history::UserContent::Image(image) => {
                            blocks.push(ContentBlock::Image {
                                source: image_source(image),
                            });
                        }
                        history::UserContent::ToolResult(result) => {
                            let mut contents = Vec::with_capacity(result.content.len());
                            for item in &result.content {
                                match item {
                                    history::ToolResultContent::Text(text) => {
                                        contents.push(ToolContent::Text {
                                            text: text.text.clone(),
                                        });
                                    }
                                    history::ToolResultContent::Json { value } => {
                                        contents.push(ToolContent::Text {
                                            text: serde_json::to_string_pretty(value)
                                                .unwrap_or_else(|_| value.to_string()),
                                        });
                                    }
                                    history::ToolResultContent::Image(image) => {
                                        if relocate_tool_images {
                                            // The placeholder keeps the
                                            // correlation visible where the
                                            // image cannot travel.
                                            contents.push(ToolContent::Text {
                                                text: format!(
                                                    "[image {} returned; attached in the next message]",
                                                    image.caption
                                                ),
                                            });
                                            relocated.push(ContentBlock::Image {
                                                source: image_source(image),
                                            });
                                        } else {
                                            contents.push(ToolContent::Image {
                                                source: image_source(image),
                                            });
                                        }
                                    }
                                }
                            }
                            blocks.push(ContentBlock::ToolResult {
                                tool_use_id: result.call.clone(),
                                name: Some(result.name.clone()),
                                content: ToolContents::from(contents),
                                is_error: result.is_error,
                                loaded_tools: Vec::new(),
                            });
                        }
                    }
                }
                push_user_message(&mut out, blocks);
                if !relocated.is_empty() {
                    let mut content = vec![ContentBlock::Text {
                        text: "Images returned by the tool calls above:".to_owned(),
                    }];
                    content.extend(relocated);
                    out.push(wire_message(Role::User, content));
                }
            }
            history::Message::Assistant { content } => {
                let mapped = assistant_to_animist(content);
                if !mapped.is_empty() {
                    out.push(wire_message(Role::Assistant, mapped));
                }
            }
        }
    }
    out
}

/// Append one user message's blocks, coalescing adjacent user messages
/// when neither side carries images (see [`to_animist`]).
fn push_user_message(out: &mut Vec<Message>, blocks: Vec<ContentBlock>) {
    let image_free = |blocks: &[ContentBlock]| {
        blocks
            .iter()
            .all(|block| !matches!(block, ContentBlock::Image { .. }))
    };
    match out.last_mut() {
        Some(Message {
            role: Role::User,
            content,
            ..
        }) if image_free(&blocks) && image_free(content) => {
            content.extend(blocks);
        }
        _ => out.push(wire_message(Role::User, blocks)),
    }
}

fn assistant_to_animist(content: &[history::AssistantContent]) -> Vec<ContentBlock> {
    let mut blocks = Vec::with_capacity(content.len());
    for block in content {
        match block {
            history::AssistantContent::Text(text) => blocks.push(ContentBlock::Text {
                text: text.text.clone(),
            }),
            history::AssistantContent::Reasoning(reasoning) => {
                for part in &reasoning.content {
                    match part {
                        history::ReasoningContent::Text {
                            text, signature, ..
                        } => {
                            blocks.push(ContentBlock::Thinking {
                                thinking: text.clone(),
                                signature: signature.clone(),
                            });
                        }
                        history::ReasoningContent::Redacted { data, .. } => {
                            blocks.push(ContentBlock::RedactedThinking { data: data.clone() });
                        }
                        history::ReasoningContent::Opaque(payload) if !payload.is_empty() => {
                            blocks.push(ContentBlock::Thinking {
                                thinking: payload.clone(),
                                signature: None,
                            });
                        }
                        history::ReasoningContent::Opaque(_) => {}
                    }
                }
            }
            history::AssistantContent::ToolCall(call) => {
                blocks.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    input: call.function.arguments.clone(),
                    thought_signature: None,
                });
            }
        }
    }
    blocks
}

/// Assemble the craft assistant message for one completed model call.
/// `call_ids` remaps provider tool-call ids to the run-stable internal
/// ids minted at call time, so results correlate with whatever id the
/// stream recorded.
pub fn assistant_from_animist(
    response: &Message,
    call_ids: &HashMap<String, String>,
) -> history::Message {
    let mut content: Vec<history::AssistantContent> = Vec::new();
    let mut reasoning_parts: Vec<history::ReasoningContent> = Vec::new();
    for block in &response.content {
        match block {
            ContentBlock::Text { text } => {
                flush_reasoning(&mut content, &mut reasoning_parts);
                content.push(history::AssistantContent::text(text.clone()));
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                reasoning_parts.push(history::ReasoningContent::Text {
                    text: thinking.clone(),
                    signature: signature.clone(),
                    native: None,
                });
            }
            ContentBlock::RedactedThinking { data } => {
                reasoning_parts.push(history::ReasoningContent::Redacted {
                    data: data.clone(),
                    native: None,
                });
            }
            ContentBlock::ToolUse {
                id, name, input, ..
            } => {
                flush_reasoning(&mut content, &mut reasoning_parts);
                content.push(history::AssistantContent::ToolCall(history::ToolCall {
                    id: call_ids.get(id).cloned().unwrap_or_else(|| id.clone()),
                    function: history::ToolFunction {
                        name: name.clone(),
                        arguments: input.clone(),
                    },
                }));
            }
            ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
        }
    }
    flush_reasoning(&mut content, &mut reasoning_parts);
    history::Message::Assistant { content }
}

/// Consecutive thinking blocks fold into one reasoning block, matching
/// the stored shape surfaces and compaction read.
fn flush_reasoning(
    content: &mut Vec<history::AssistantContent>,
    parts: &mut Vec<history::ReasoningContent>,
) {
    if !parts.is_empty() {
        content.push(history::AssistantContent::Reasoning(history::Reasoning {
            content: std::mem::take(parts),
            origin: None,
        }));
    }
}

fn image_source(image: &history::ImageBlock) -> ImageSource {
    ImageSource::Base64 {
        media_type: image.media_type.as_str().to_owned(),
        data: image.data.clone(),
    }
}

fn wire_message(role: Role, content: Vec<ContentBlock>) -> Message {
    Message {
        role,
        content,
        display_text: None,
        kind: animist::history::MessageKind::Turn,
    }
}

/// Replay validation for debug builds and tests: the mapped view must be
/// wire-legal (tool uses answered in order, signatures where required)
/// before any provider sees it.
pub fn assert_replay_valid(messages: &[Message], tools: &[Tool]) {
    if let Err(error) = animist::protocol::validate_replay(messages, tools) {
        debug_assert!(false, "mapped history violates replay rules: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{AssistantContent, Reasoning, ReasoningContent, ToolCall, ToolResult};

    fn sample() -> Vec<history::Message> {
        vec![
            history::Message::system("sys"),
            history::Message::user("hello"),
            history::Message::Assistant {
                content: vec![
                    AssistantContent::text("working"),
                    AssistantContent::Reasoning(Reasoning {
                        content: vec![ReasoningContent::Text {
                            text: "thinking".into(),
                            signature: Some("sig-1".into()),
                            native: None,
                        }],
                        origin: None,
                    }),
                    AssistantContent::ToolCall(ToolCall::new(
                        "t1",
                        "bash",
                        serde_json::json!({"command": "ls"}),
                    )),
                ],
            },
            history::Message::User {
                content: vec![history::UserContent::ToolResult(ToolResult::text(
                    "t1", "bash", "ok",
                ))],
            },
        ]
    }

    #[test]
    fn test_maps_roles_and_blocks_both_ways() {
        let mapped = to_animist(&sample(), false);
        assert_eq!(mapped[0].role, Role::System);
        assert_eq!(mapped[1].role, Role::User);
        let animist::history::ContentBlock::ToolUse {
            id, name, input, ..
        } = &mapped[2].content[2]
        else {
            panic!("tool use block");
        };
        assert_eq!(id, "t1");
        assert_eq!(name, "bash");
        assert_eq!(input["command"], "ls");
        // The tool result coalesces into the following user message with
        // its correlation id and name.
        let animist::history::ContentBlock::ToolResult {
            tool_use_id,
            name: result_name,
            ..
        } = &mapped[3].content[0]
        else {
            panic!("tool result block");
        };
        assert_eq!(tool_use_id, "t1");
        assert_eq!(result_name.as_deref(), Some("bash"));
    }

    #[test]
    fn test_adjacent_user_messages_coalesce() {
        let history = vec![
            history::Message::user("first"),
            history::Message::User {
                content: vec![history::UserContent::ToolResult(ToolResult::text(
                    "t1", "bash", "ok",
                ))],
            },
            history::Message::user("after"),
        ];
        let mapped = to_animist(&history, false);
        assert_eq!(mapped.len(), 1, "{mapped:?}");
        assert_eq!(mapped[0].content.len(), 3);
    }

    #[test]
    fn test_thinking_signature_and_redacted_round_trip() {
        let response = wire_message(
            Role::Assistant,
            vec![
                ContentBlock::Thinking {
                    thinking: "because".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "cmVk".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
                ContentBlock::ToolUse {
                    id: "p1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                    thought_signature: None,
                },
            ],
        );
        let mapped = assistant_from_animist(
            &response,
            &HashMap::from([("p1".to_owned(), "call-9".to_owned())]),
        );
        let history::Message::Assistant { content } = &mapped else {
            panic!("assistant message");
        };
        // One folded reasoning block with both parts, then text, then the
        // remapped tool call.
        assert_eq!(content.len(), 3, "{content:?}");
        let AssistantContent::Reasoning(reasoning) = &content[0] else {
            panic!("reasoning block");
        };
        assert_eq!(reasoning.content.len(), 2);
        assert_eq!(
            reasoning.content[0],
            ReasoningContent::Text {
                text: "because".into(),
                signature: Some("sig".into()),
                native: None,
            }
        );
        assert!(matches!(
            &reasoning.content[1],
            ReasoningContent::Redacted { data, .. } if data == "cmVk"
        ));
        let AssistantContent::ToolCall(call) = &content[2] else {
            panic!("tool call block");
        };
        assert_eq!(call.id, "call-9");
        // And the stored shape maps back onto the wire verbatim.
        let wire = to_animist(&[mapped], false);
        assert_eq!(wire[0].content.len(), 4);
    }

    #[test]
    fn test_tool_result_images_relocate_only_when_asked() {
        let history = vec![history::Message::User {
            content: vec![history::UserContent::ToolResult(ToolResult {
                call: "t1".into(),
                name: "view".into(),
                content: vec![history::ToolResultContent::Image(history::ImageBlock {
                    media_type: history::ImageMedia::Png,
                    data: "aGk=".into(),
                    caption: "[image]".into(),
                })],
                is_error: false,
            })],
        }];
        let inline = to_animist(&history, false);
        assert_eq!(inline.len(), 1);
        assert!(matches!(
            &inline[0].content[0],
            ContentBlock::ToolResult { content, .. }
                if matches!(content.0.as_slice(),
                    [ToolContent::Image { .. }])
        ));

        let relocated = to_animist(&history, true);
        assert_eq!(relocated.len(), 2, "{relocated:?}");
        assert!(matches!(
            &relocated[0].content[0],
            ContentBlock::ToolResult { content, .. }
                if matches!(content.0.as_slice(),
                    [ToolContent::Text { text }] if text.contains("[image"))
        ));
        assert!(matches!(
            &relocated[1].content[1],
            ContentBlock::Image { .. }
        ));
    }

    #[test]
    fn test_empty_assistant_turns_drop() {
        let history = vec![history::Message::Assistant {
            content: vec![AssistantContent::Reasoning(Reasoning {
                content: vec![ReasoningContent::Opaque(String::new())],
                origin: None,
            })],
        }];
        let mapped = to_animist(&history, false);
        assert!(mapped.is_empty());
    }

    #[test]
    fn test_mapped_history_passes_replay_validation() {
        let mapped = to_animist(&sample(), false);
        assert_replay_valid(&mapped, &[]);
    }
}
