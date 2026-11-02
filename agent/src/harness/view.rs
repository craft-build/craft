//! Request-view shaping: pure `Vec<Message>` → `Vec<Message>` functions
//! applied where the provider call is made. Stored history is never
//! mutated here; every rewrite exists because a protocol rejects the shape
//! animist's adapters pass through.

use animist::history::{ContentBlock, Message, Role, ToolContent};

use crate::compression::{self, CompressionConfig};
#[cfg(test)]
use crate::history::StoredMessage;
use crate::history::{self};

/// The stored history as wire messages, unshaped: the request rewrites
/// (read lifecycle, compression) apply to this view before protocol
/// shaping, mirroring the order the provider edge always used. The
/// request layer maps history itself now; only the test-facing
/// [`request_messages`] convenience still reads it.
#[cfg(test)]
pub(crate) fn plain_messages(history: &[StoredMessage]) -> Vec<Message> {
    history
        .iter()
        .map(StoredMessage::message)
        .cloned()
        .collect()
}

/// The wire view of a stored history: the messages themselves, shaped for
/// the target protocol (`relocate_tool_images` for Chat Completions
/// targets). Test-facing convenience over [`shape_messages`].
#[cfg(test)]
pub(crate) fn request_messages(
    history: &[StoredMessage],
    relocate_tool_images: bool,
) -> Vec<Message> {
    shape_messages(plain_messages(history), relocate_tool_images)
}

/// Shape wire messages for the target protocol: image relocation first,
/// then user-message coalescing, with content-less assistant turns dropped
/// (providers reject an assistant message with no blocks). Callers that
/// already hold plain messages (side completions) call this directly.
pub(crate) fn shape_messages(
    mut messages: Vec<Message>,
    relocate_tool_images: bool,
) -> Vec<Message> {
    if relocate_tool_images {
        messages = relocate_tool_result_images(messages);
    }
    messages
        .retain(|message| !matches!(message.role, Role::Assistant) || !message.content.is_empty());
    coalesce_user_messages(messages)
}

/// Adjacent user messages merge: history records a separate user message
/// per tool result (waves of parallel calls) and separate observations
/// after them, while Anthropic Messages and Bedrock Converse answer every
/// tool use of a step in the immediately following user message, and the
/// animist adapters map each message one to one. Image-bearing messages
/// stay separate, because Chat Completions rejects images in a message
/// that also carries tool results.
pub(crate) fn coalesce_user_messages(mut messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages.drain(..) {
        // Both sides must be user messages; folding an assistant message
        // into a user one is a replay violation.
        let merges = matches!(message.role, Role::User)
            && matches!(
                out.last(),
                Some(Message {
                    role: Role::User,
                    ..
                })
            )
            && out.last().is_some_and(|last| image_free(&last.content))
            && image_free(&message.content);
        if merges && let Some(Message { content, .. }) = out.last_mut() {
            content.extend(message.content);
        } else {
            out.push(message);
        }
    }
    out
}

fn image_free(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .all(|block| !matches!(block, ContentBlock::Image { .. }))
}

/// Move images inside tool results into a user message after the results,
/// which Chat Completions targets need; protocols that accept tool-result
/// images carry them inline. The placeholder keeps the correlation visible
/// where the image cannot travel.
pub(crate) fn relocate_tool_result_images(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for message in messages {
        let mut relocated: Vec<ContentBlock> = Vec::new();
        let mut content = message.content;
        for block in content.iter_mut() {
            if let ContentBlock::ToolResult {
                content: result, ..
            } = block
            {
                let mut kept: Vec<ToolContent> = Vec::with_capacity(result.0.len());
                for item in result.0.drain(..) {
                    match item {
                        ToolContent::Image { source } => {
                            kept.push(ToolContent::Text {
                                text: "[image returned; attached in the next message]".to_owned(),
                            });
                            relocated.push(ContentBlock::Image { source });
                        }
                        other => kept.push(other),
                    }
                }
                *result = kept.into();
            }
        }
        out.push(Message {
            role: message.role,
            content,
            display_text: message.display_text,
            kind: message.kind,
        });
        if !relocated.is_empty() {
            let mut blocks = vec![ContentBlock::Text {
                text: "Images returned by the tool calls above:".to_owned(),
            }];
            blocks.extend(relocated);
            out.push(history::ext::observation(blocks));
        }
    }
    out
}

/// Rewrite tool-result texts in the request copy through pre-compression.
/// Only the wire view is affected: `turn` and `history` keep raw results.
/// Request-time compression is unconditional;
/// `protect_recent_tool_outputs` is a compaction-stage knob, not ours.
///
/// This is the in-run half of the durable compression alteration:
/// results before `from` (where the run started in the view) already
/// arrived compressed from history, so only the suffix is rewritten. The
/// compressed bytes are the same either way — the pass is deterministic —
/// but the prefix keeps its durable revision untouched.
#[cfg(test)]
pub(crate) fn compress_request_view(full: &mut [Message], config: &CompressionConfig) {
    compress_request_view_from(full, config, 0)
}

pub(crate) fn compress_request_view_from(
    full: &mut [Message],
    config: &CompressionConfig,
    from: usize,
) {
    let names_by_id: std::collections::HashMap<String, String> = full
        .iter()
        .flat_map(history::ext::tool_calls)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
            _ => None,
        })
        .collect();
    for (index, message) in full.iter_mut().enumerate() {
        if index < from || !history::ext::is_user(message) {
            continue;
        }
        for block in &mut message.content {
            let ContentBlock::ToolResult {
                tool_use_id,
                name,
                content,
                ..
            } = block
            else {
                continue;
            };
            // A result written without its name (pre-name metadata) borrows
            // the call's; either way the verbatim-tool check needs one.
            let name = name
                .as_deref()
                .or_else(|| names_by_id.get(tool_use_id).map(String::as_str));
            let Some(name) = name else {
                continue;
            };
            // Verbatim tools (e.g. `read`) return caller-selected content;
            // compressing it would drop lines the model explicitly asked for.
            if !compression::should_compress_tool(name) {
                continue;
            }
            for item in &mut content.0 {
                if let ToolContent::Text { text } = item {
                    *text = compression::compress_for_llm(text, config);
                }
            }
        }
    }
}

/// Replay validation for debug builds and tests: the request view must be
/// wire-legal (tool uses answered in order, signatures where required)
/// before any provider sees it.
#[cfg(test)]
use animist::provider::Tool;

#[cfg(test)]
pub fn assert_replay_valid(messages: &[Message], tools: &[Tool]) {
    if let Err(error) = animist::protocol::validate_replay(messages, tools) {
        debug_assert!(false, "request view violates replay rules: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ext;
    use animist::history::{MessageKind, ToolContents};

    fn user_text(text: &str) -> Message {
        ext::user_text(text)
    }

    fn tool_result(id: &str, name: &str) -> ContentBlock {
        ext::tool_result(id, Some(name), "ok", false)
    }

    #[test]
    fn empty_assistant_turns_drop() {
        let messages = vec![
            user_text("first"),
            Message {
                role: Role::Assistant,
                content: Vec::new(),
                display_text: None,
                kind: MessageKind::Turn,
            },
            ext::assistant_text("kept"),
        ];
        let shaped = shape_messages(messages, false);
        assert_eq!(shaped.len(), 2);
        assert_eq!(ext::text(&shaped[1]), "kept");
    }

    #[test]
    fn adjacent_user_messages_coalesce() {
        let messages = vec![
            user_text("first"),
            history::ext::observation(vec![tool_result("t1", "bash")]),
            user_text("after"),
        ];
        let coalesced = coalesce_user_messages(messages);
        assert_eq!(coalesced.len(), 1, "{coalesced:?}");
        assert_eq!(coalesced[0].content.len(), 3);
    }

    #[test]
    fn image_bearing_user_messages_stay_separate() {
        let image = ContentBlock::Image {
            source: history::ext::image_source(history::ext::ImageMedia::Png, "aGk="),
        };
        let messages = vec![
            history::ext::observation(vec![tool_result("t1", "view")]),
            history::ext::observation(vec![image]),
        ];
        let coalesced = coalesce_user_messages(messages);
        assert_eq!(coalesced.len(), 2, "images must not merge into results");
    }

    #[test]
    fn tool_result_images_relocate_only_when_asked() {
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            name: Some("view".into()),
            content: ToolContents::from(vec![ToolContent::Image {
                source: history::ext::image_source(history::ext::ImageMedia::Png, "aGk="),
            }]),
            is_error: false,
            loaded_tools: Vec::new(),
        };
        let with_image = history::ext::observation(vec![result]);
        let inline = request_messages(&[ext::stored(with_image.clone())], false);
        assert_eq!(inline.len(), 1);
        assert!(matches!(
            &inline[0].content[0],
            ContentBlock::ToolResult { content, .. }
                if matches!(content.0.as_slice(), [ToolContent::Image { .. }])
        ));

        let relocated = request_messages(&[ext::stored(with_image)], true);
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
    fn request_view_passes_replay_validation() {
        let history = vec![
            ext::stored(ext::user_text("hello")),
            ext::stored(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    thought_signature: None,
                }],
                display_text: None,
                kind: MessageKind::Turn,
            }),
            ext::stored(history::ext::observation(vec![tool_result("t1", "bash")])),
        ];
        let view = request_messages(&history, false);
        assert_replay_valid(&view, &[]);
    }
}
