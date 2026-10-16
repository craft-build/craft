//! The provider edge: the only module that imports rig-core's message types.
//!
//! Everything crossing to or from a provider model goes through here: our
//! [`history::Message`] list becomes a rig [`CompletionRequest`], streamed rig
//! content becomes history messages. Keeping the mapping in one place is what
//! lets the rest of the crate stay free of `rig` types.

use std::collections::HashMap;

use rig_core::completion::message::{
    AssistantContent, AssistantMessage, CallId, DocumentSourceKind, ImageMediaType,
    Message as RigMessage, Origin, Reasoning as RigReasoning, ToolCall as RigToolCall, ToolName,
    ToolResultContent as RigToolResultContent, UserContent,
};
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::streaming::{Part, StreamEvent};

use crate::history;

/// A validated tool name, tolerating degenerate history records with empty
/// names: those replay under a stable placeholder rather than failing the
/// mapping.
pub(crate) fn tool_name(name: &str) -> ToolName {
    ToolName::new(name)
        .unwrap_or_else(|_| ToolName::new("unnamed").expect("placeholder tool name is non-empty"))
}

/// Pull a provider reasoning signature out of a block's native item, when
/// the provider preserved one. Anthropic writes a top-level `signature`;
/// Bedrock nests it in `reasoningContent.reasoningText.signature`.
fn reasoning_signature(reasoning: &RigReasoning) -> Option<String> {
    let item = reasoning.native.as_ref().map(|native| &native.item);
    item.and_then(|item| {
        item.get("signature")
            .or_else(|| item.pointer("/reasoningContent/reasoningText/signature"))
    })
    .and_then(serde_json::Value::as_str)
    .map(str::to_owned)
}

/// Build the provider request for one model call.
#[allow(clippy::too_many_arguments)]
pub fn to_request(
    history: &[history::Message],
    tools: &[ToolDefinition],
    preamble: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
) -> CompletionRequest {
    let mut request = CompletionRequest::from(own_to_rig(history))
        .tools(tools.to_vec())
        .temperature(temperature)
        .max_tokens(max_tokens)
        .accept_unknown_finish_reasons(true);
    if let Some(text) = preamble.filter(|text| !text.is_empty()) {
        request = request.preamble(text);
    }
    request
}

/// Convert our history into rig's replay format.
///
/// Images inside tool results are moved into a user message that follows the
/// tool-result message: OpenAI Chat Completions rejects image content in
/// tool results ("does not support images in tool results"), while both
/// families accept an image in user content.
pub fn own_to_rig(messages: &[history::Message]) -> Vec<RigMessage> {
    let mut out: Vec<RigMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        if let history::Message::User { content } = message {
            let blocks: Vec<UserContent> = content.iter().map(own_user_block_to_rig).collect();
            push_user_message(&mut out, blocks);
        } else {
            let mapped = own_message_to_rig(message);
            // An assistant turn whose every part filtered out (e.g. thinking
            // that carries no replayable payload) must not reach the wire as
            // an empty message providers reject; drop the turn instead.
            if !matches!(&mapped, RigMessage::Assistant(a) if a.content.is_empty()) {
                out.push(mapped);
            }
        }
        if let history::Message::User { content } = message {
            let images: Vec<UserContent> = content
                .iter()
                .filter_map(|block| match block {
                    history::UserContent::ToolResult(result) => {
                        let images: Vec<UserContent> = result
                            .content
                            .iter()
                            .filter_map(|item| match item {
                                history::ToolResultContent::Image(image) => {
                                    Some(UserContent::image_base64(
                                        image.data.clone(),
                                        Some(media_own_to_rig(image.media_type)),
                                        None,
                                    ))
                                }
                                _ => None,
                            })
                            .collect();
                        (!images.is_empty()).then_some(images)
                    }
                    _ => None,
                })
                .flatten()
                .collect();
            if !images.is_empty() {
                let mut content = vec![UserContent::text(
                    "Images returned by the tool calls above:",
                )];
                content.extend(images);
                out.push(RigMessage::User { content });
            }
        }
    }
    out
}

fn image_free(content: &[UserContent]) -> bool {
    content
        .iter()
        .all(|block| !matches!(block, UserContent::Image(_)))
}

/// Append one user message's blocks, coalescing adjacent user messages.
///
/// History records a separate user message per tool result (waves of
/// parallel calls) and separate end/cancel markers after them. Bedrock and
/// Anthropic reject that split on replay: every `tool_use` must be answered
/// by `toolResult` blocks in the immediately following user message
/// ("Expected toolResult blocks at messages.N.content"), so adjacent user
/// messages merge into one wire message. Image-bearing messages stay
/// separate: OpenAI Chat Completions rejects images in a message that also
/// carries tool results.
fn push_user_message(out: &mut Vec<RigMessage>, blocks: Vec<UserContent>) {
    match out.last_mut() {
        Some(RigMessage::User { content }) if image_free(&blocks) && image_free(content) => {
            content.extend(blocks);
        }
        _ => out.push(RigMessage::User { content: blocks }),
    }
}

pub fn own_message_to_rig(message: &history::Message) -> RigMessage {
    match message {
        history::Message::System { content } => RigMessage::system(content.clone()),
        history::Message::User { content } => RigMessage::User {
            content: content.iter().map(own_user_block_to_rig).collect(),
        },
        history::Message::Assistant { content } => {
            // Replay of provider-native reasoning items requires the turn's
            // origin to name the wire that produced it: rig's request
            // shaping keeps native items only for the target's own turns.
            let origin = content.iter().find_map(|block| match block {
                history::AssistantContent::Reasoning(reasoning) => {
                    origin_from_history(reasoning.origin.as_ref())
                }
                _ => None,
            });
            let message = AssistantMessage::new(
                content
                    .iter()
                    .flat_map(own_assistant_block_to_rig)
                    .collect(),
            );
            RigMessage::Assistant(match origin {
                Some(origin) => message.with_origin(origin),
                None => message,
            })
        }
    }
}

/// The stored turn origin, decoded back into rig's `Origin`.
fn origin_from_history(stored: Option<&serde_json::Value>) -> Option<Origin> {
    stored
        .and_then(|value| serde_json::from_value::<Origin>(value.clone()).ok())
        .filter(|origin| !origin.model.is_empty())
}

fn own_user_block_to_rig(block: &history::UserContent) -> UserContent {
    match block {
        history::UserContent::Text(text) => UserContent::text(&text.text),
        history::UserContent::Image(image) => UserContent::image_base64(
            image.data.clone(),
            Some(media_own_to_rig(image.media_type)),
            None,
        ),
        history::UserContent::ToolResult(result) => UserContent::tool_result(
            CallId::from_wire(&result.call),
            tool_name(&result.name),
            result
                .content
                .iter()
                .map(|item| match item {
                    history::ToolResultContent::Text(text) => {
                        RigToolResultContent::text(&text.text)
                    }
                    history::ToolResultContent::Json { value } => RigToolResultContent::Json {
                        value: value.clone(),
                    },
                    history::ToolResultContent::Image(image) => {
                        RigToolResultContent::text(format!(
                            "[image {} returned; attached in the next message]",
                            image.caption
                        ))
                    }
                })
                .collect(),
        ),
    }
}

fn own_assistant_block_to_rig(block: &history::AssistantContent) -> Vec<AssistantContent> {
    match block {
        history::AssistantContent::Text(text) => vec![AssistantContent::text(&text.text)],
        history::AssistantContent::Reasoning(reasoning) => reasoning
            .content
            .iter()
            .map(own_reasoning_to_rig)
            // A part with neither a payload nor a provider-native item
            // carries nothing any provider can replay (Bedrock rejects the
            // block outright); dropping it recovers sessions persisted
            // without signatures.
            .filter(|(reasoning, native)| {
                !reasoning.text.is_empty() || reasoning.redacted || native.is_some()
            })
            .map(|(reasoning, native)| {
                let block = AssistantContent::Reasoning(reasoning);
                // Re-attach the provider item so the wire replays it
                // verbatim (signature included) instead of demoting the
                // thinking to plain text.
                match native {
                    Some(item) => block.with_native(item),
                    None => block,
                }
            })
            .collect(),
        history::AssistantContent::ToolCall(call) => vec![AssistantContent::tool_call(
            &call.id,
            tool_name(&call.function.name),
            call.function.arguments.clone(),
        )],
    }
}

/// Each persisted reasoning part becomes its own streamed block, with its
/// provider-native item (when one was kept) for verbatim replay.
fn own_reasoning_to_rig(
    item: &history::ReasoningContent,
) -> (RigReasoning, Option<serde_json::Value>) {
    match item {
        history::ReasoningContent::Text { text, native, .. } => (
            RigReasoning {
                text: text.clone(),
                redacted: false,
                native: None,
            },
            native.clone(),
        ),
        history::ReasoningContent::Opaque(data) => (
            RigReasoning {
                text: data.clone(),
                redacted: false,
                native: None,
            },
            None,
        ),
        history::ReasoningContent::Redacted { data, native } => (
            RigReasoning {
                text: data.clone(),
                redacted: true,
                native: None,
            },
            native.clone(),
        ),
    }
}

fn rig_reasoning_block_to_own(reasoning: &RigReasoning) -> history::ReasoningContent {
    let signature = reasoning_signature(reasoning);
    // The provider-native item is the only replayable form of signed or
    // redacted thinking; keep it beside the canonical text. `native_item`
    // (not the raw `native`) so an edited block is not replayed stale.
    let native = AssistantContent::Reasoning(reasoning.clone())
        .native_item()
        .cloned();
    match () {
        _ if reasoning.redacted => history::ReasoningContent::Redacted {
            data: reasoning.text.clone(),
            native,
        },
        _ if reasoning.text.is_empty() && signature.is_none() => {
            history::ReasoningContent::Opaque(String::new())
        }
        _ => history::ReasoningContent::Text {
            text: reasoning.text.clone(),
            signature,
            native,
        },
    }
}

/// Assemble the assistant message for one completed model call from the
/// streamed content. `call_ids` remaps tool-call ids: the stream's complete
/// `ToolCall` events carry the run-stable `internal_call_id`, while the
/// aggregated choice keeps provider ids that may be missing; results must
/// correlate with whatever id we recorded at call time.
pub fn assistant_from_stream(
    choice: &[AssistantContent],
    origin: Option<&Origin>,
    call_ids: &HashMap<String, String>,
    streamed: &StreamedParts,
) -> history::Message {
    let _ = choice;
    let mut content = Vec::new();
    if !streamed.reasoning.content.is_empty() {
        let mut reasoning = streamed.reasoning.clone();
        // The turn's origin is what lets the next request replay the
        // reasoning's provider-native items on the same wire.
        reasoning.origin = origin.and_then(|origin| serde_json::to_value(origin).ok());
        content.push(history::AssistantContent::Reasoning(reasoning));
    }
    if !streamed.text.is_empty() {
        content.push(history::AssistantContent::text(streamed.text.clone()));
    }
    for call in &streamed.tool_calls {
        content.push(history::AssistantContent::ToolCall(history::ToolCall {
            id: call_ids
                .get(&call.id.to_string())
                .cloned()
                .unwrap_or_else(|| call.id.to_string()),
            function: history::ToolFunction {
                name: call.function.name.to_string(),
                arguments: call.function.arguments_value(),
            },
        }));
    }
    history::Message::Assistant { content }
}

/// Text and tool calls accumulated from a stream's public events.
#[derive(Default)]
pub struct StreamedParts {
    pub text: String,
    pub reasoning: history::Reasoning,
    /// Correlator of the reasoning block the deltas are accumulating into;
    /// a complete event under a different correlator is a sibling block
    /// (e.g. redacted beside plaintext) and must not clobber it.
    reasoning_block_id: Option<Part>,
    pub tool_calls: Vec<RigToolCall>,
}

/// Record one streamed assistant event into `parts`.
pub fn fold_streamed_event(parts: &mut StreamedParts, event: &StreamEvent) {
    match event {
        StreamEvent::Text { text, .. } => parts.text.push_str(text),
        StreamEvent::Reasoning { part, text } => {
            if parts.reasoning_block_id != Some(*part) {
                parts.reasoning_block_id = Some(*part);
                parts
                    .reasoning
                    .content
                    .push(history::ReasoningContent::Text {
                        text: String::new(),
                        signature: None,
                        native: None,
                    });
            }
            if let Some(history::ReasoningContent::Text { text: pending, .. }) =
                parts.reasoning.content.last_mut()
            {
                pending.push_str(text);
            }
        }
        StreamEvent::End {
            part,
            content: AssistantContent::Reasoning(reasoning),
        } => {
            // The complete block supersedes its own deltas (matched by part)
            // and carries what the deltas could not — the signature, which
            // the wire delivers out of band.
            let block = rig_reasoning_block_to_own(reasoning);
            if parts.reasoning_block_id == Some(*part) && parts.reasoning.content.pop().is_some() {
                parts.reasoning.content.push(block);
            } else {
                parts.reasoning_block_id = Some(*part);
                parts.reasoning.content.push(block);
            }
        }
        StreamEvent::End {
            content: AssistantContent::ToolCall(call),
            ..
        } => {
            parts.tool_calls.push(call.clone());
        }
        _ => {}
    }
}

/// Convert rig messages back to our history (used by round-trip tests; real
/// history never enters as rig messages).
pub fn rig_to_own(messages: &[RigMessage]) -> Vec<history::Message> {
    messages.iter().map(rig_message_to_own).collect()
}

fn rig_message_to_own(message: &RigMessage) -> history::Message {
    match message {
        RigMessage::System { content } => history::Message::system(content.clone()),
        RigMessage::User { content } => history::Message::User {
            content: content
                .iter()
                .filter_map(|block| match block {
                    UserContent::Text(text) => Some(history::UserContent::Text(history::Text {
                        text: text.text.clone(),
                    })),
                    UserContent::Image(image) => {
                        let data = match &image.data {
                            DocumentSourceKind::Base64(data) => data.clone(),
                            _ => String::new(),
                        };
                        Some(history::UserContent::Image(history::ImageBlock {
                            media_type: image
                                .media_type
                                .as_ref()
                                .map(|m| media_rig_to_own(m.clone()))
                                .unwrap_or(history::ImageMedia::Png),
                            data,
                            caption: "[image]".into(),
                        }))
                    }
                    UserContent::ToolResult(result) => {
                        Some(history::UserContent::ToolResult(history::ToolResult {
                            call: result.call.to_string(),
                            name: result.name.to_string(),
                            content: result
                                .content
                                .iter()
                                .map(rig_result_content_to_own)
                                .collect(),
                            is_error: false,
                        }))
                    }
                    _ => None,
                })
                .collect(),
        },
        RigMessage::Assistant(assistant) => history::Message::Assistant {
            content: assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => {
                        Some(history::AssistantContent::Text(history::Text {
                            text: text.text.clone(),
                        }))
                    }
                    AssistantContent::Reasoning(reasoning) => {
                        Some(history::AssistantContent::Reasoning(history::Reasoning {
                            content: vec![rig_reasoning_block_to_own(reasoning)],
                            origin: None,
                        }))
                    }
                    AssistantContent::ToolCall(call) => {
                        Some(history::AssistantContent::ToolCall(history::ToolCall {
                            id: call.id.to_string(),
                            function: history::ToolFunction {
                                name: call.function.name.to_string(),
                                arguments: call.function.arguments_value(),
                            },
                        }))
                    }
                    _ => None,
                })
                .collect(),
        },
    }
}

fn rig_result_content_to_own(item: &RigToolResultContent) -> history::ToolResultContent {
    match item {
        RigToolResultContent::Text(text) => history::ToolResultContent::Text(history::Text {
            text: text.text.clone(),
        }),
        RigToolResultContent::Json { value } => history::ToolResultContent::Json {
            value: value.clone(),
        },
        RigToolResultContent::Image(image) => {
            history::ToolResultContent::Image(history::ImageBlock {
                media_type: image
                    .media_type
                    .clone()
                    .map(media_rig_to_own)
                    .unwrap_or(history::ImageMedia::Png),
                data: match &image.data {
                    DocumentSourceKind::Base64(data) => data.clone(),
                    _ => String::new(),
                },
                caption: "[image]".into(),
            })
        }
    }
}

fn media_own_to_rig(media: history::ImageMedia) -> ImageMediaType {
    match media {
        history::ImageMedia::Png => ImageMediaType::PNG,
        history::ImageMedia::Jpeg => ImageMediaType::JPEG,
        history::ImageMedia::Gif => ImageMediaType::GIF,
        history::ImageMedia::Webp => ImageMediaType::WEBP,
    }
}

fn media_rig_to_own(media: ImageMediaType) -> history::ImageMedia {
    match media {
        ImageMediaType::PNG => history::ImageMedia::Png,
        ImageMediaType::JPEG => history::ImageMedia::Jpeg,
        ImageMediaType::GIF => history::ImageMedia::Gif,
        ImageMediaType::WEBP => history::ImageMedia::Webp,
        // Unsupported formats (HEIC/HEIF/SVG) cannot be replayed by our
        // tools; keep an inert placeholder.
        _ => history::ImageMedia::Png,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_bedrock::completion::Converse;
    use rig_core::completion::message::ToolFunction;
    use rig_core::wire::Wire;

    fn part(index: u32) -> Part {
        serde_json::from_value(serde_json::json!(index)).expect("part index")
    }

    fn sample() -> Vec<history::Message> {
        vec![
            history::Message::system("sys"),
            history::Message::user("hello"),
            history::Message::Assistant {
                content: vec![
                    history::AssistantContent::text("working"),
                    history::AssistantContent::Reasoning(history::Reasoning {
                        content: vec![history::ReasoningContent::Text {
                            text: "thinking".into(),
                            signature: None,
                            native: None,
                        }],
                        origin: None,
                    }),
                    history::AssistantContent::ToolCall(history::ToolCall::new(
                        "t1",
                        "bash",
                        serde_json::json!({"command": "ls"}),
                    )),
                ],
            },
            history::Message::User {
                content: vec![
                    history::UserContent::text("and this"),
                    history::UserContent::ToolResult(history::ToolResult {
                        call: "t1".into(),
                        name: "bash".into(),
                        content: vec![
                            history::ToolResultContent::Text(history::Text {
                                text: "output".into(),
                            }),
                            history::ToolResultContent::Json {
                                value: serde_json::json!({"lines": 2}),
                            },
                        ],
                        is_error: false,
                    }),
                ],
            },
        ]
    }

    fn reasoning_native(json: &str) -> rig_core::message::Native {
        let item: serde_json::Value = serde_json::from_str(json).expect("native item");
        rig_core::message::Native {
            fingerprint: rig_core::message::Fingerprint::of(&item),
            item,
        }
    }

    /// Attach a provider-native item the way a rig decoder does, so its
    /// fingerprint matches the canonical block and the item replays.
    fn reasoning_with_native(reasoning: RigReasoning, json: &str) -> RigReasoning {
        let item: serde_json::Value = serde_json::from_str(json).expect("native item");
        match AssistantContent::Reasoning(reasoning).with_native(item) {
            AssistantContent::Reasoning(reasoning) => reasoning,
            _ => unreachable!("with_native preserves the block kind"),
        }
    }

    fn reasoning_event(part_index: u32, reasoning: RigReasoning) -> StreamEvent {
        StreamEvent::End {
            part: part(part_index),
            content: AssistantContent::Reasoning(reasoning),
        }
    }

    fn reasoning_delta(part_index: u32, text: &str) -> StreamEvent {
        StreamEvent::Reasoning {
            part: part(part_index),
            text: text.to_owned(),
        }
    }

    #[test]
    fn image_tool_result_round_trips_data_and_media_type() {
        let own = vec![history::Message::User {
            content: vec![history::UserContent::ToolResult(history::ToolResult {
                call: "t1".into(),
                name: "view_image".into(),
                content: vec![history::ToolResultContent::Image(history::ImageBlock {
                    media_type: history::ImageMedia::Webp,
                    data: "aGVsbG8=".into(),
                    caption: "[image: shot.webp 1KB 32x32]".into(),
                })],
                is_error: false,
            })],
        }];
        let rig = own_to_rig(&own);
        // OpenAI-compatible requests cannot carry images in tool results;
        // the provider-bound replay replaces the block with a text note and
        // attaches the image in a following user message.
        let RigMessage::User { content } = &rig[0] else {
            panic!("expected tool-result user message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("expected tool result");
        };
        assert!(matches!(
            result.content.first(),
            Some(RigToolResultContent::Text(_))
        ));
        let RigMessage::User { content } = &rig[1] else {
            panic!("expected follow-up image user message, got {:?}", rig[1]);
        };
        assert!(matches!(content.first(), Some(UserContent::Text(_))));
        assert!(matches!(
            content.get(1),
            Some(UserContent::Image(image))
                if image.media_type == Some(ImageMediaType::WEBP)
                    && matches!(&image.data, DocumentSourceKind::Base64(data) if data == "aGVsbG8=")
        ));
        // And the image survives the back conversion as user vision input.
        let back = rig_to_own(&rig);
        match &back[1] {
            history::Message::User { content } => {
                let history::UserContent::Image(image) = &content[1] else {
                    panic!("expected image block, got {:?}", content[0]);
                };
                assert_eq!(image.media_type, history::ImageMedia::Webp);
                assert_eq!(image.data, "aGVsbG8=");
                // rig carries no caption field; the text stand-in is generic.
                assert_eq!(image.caption, "[image]");
            }
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn round_trips_every_block_kind() {
        let own = sample();
        let back = rig_to_own(&own_to_rig(&own));
        assert_eq!(back, own);
    }

    #[test]
    fn round_trip_is_stable_across_two_passes() {
        let own = sample();
        let once = rig_to_own(&own_to_rig(&own));
        let twice = rig_to_own(&own_to_rig(&once));
        assert_eq!(once, twice);
    }

    /// Regression for the Bedrock failure "Expected toolResult blocks at
    /// messages.N.content": parallel tool calls commit as one assistant
    /// message with a separate user message per result, so the wire copy
    /// must coalesce them — every tool_use answered in the immediately
    /// following user message.
    #[test]
    fn parallel_tool_results_replay_in_one_user_message() {
        let result = |id: &str| {
            history::UserContent::ToolResult(history::ToolResult {
                call: id.into(),
                name: "read".into(),
                content: vec![history::ToolResultContent::text("ok")],
                is_error: false,
            })
        };
        let own = vec![
            history::Message::Assistant {
                content: vec![
                    history::AssistantContent::ToolCall(history::ToolCall::new(
                        "t1",
                        "read",
                        serde_json::json!({"path": "a"}),
                    )),
                    history::AssistantContent::ToolCall(history::ToolCall::new(
                        "t2",
                        "read",
                        serde_json::json!({"path": "b"}),
                    )),
                ],
            },
            history::Message::User {
                content: vec![result("t1")],
            },
            history::Message::User {
                content: vec![result("t2")],
            },
        ];
        let rig = own_to_rig(&own);
        assert_eq!(rig.len(), 2, "per-result user messages must coalesce");
        let RigMessage::User { content } = &rig[1] else {
            panic!("expected user message");
        };
        let ids: Vec<String> = content
            .iter()
            .filter_map(|block| match block {
                UserContent::ToolResult(result) => Some(result.call.wire().to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids.iter().map(String::as_str).collect::<Vec<_>>(),
            ["t1", "t2"]
        );
    }

    /// Cancel/end markers trail their tool-result messages as separate user
    /// messages; they merge in so no tool_result-less user message directly
    /// follows a tool_use.
    #[test]
    fn trailing_marker_merges_into_tool_result_message() {
        let own = vec![
            history::Message::Assistant {
                content: vec![history::AssistantContent::ToolCall(history::ToolCall::new(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "ls"}),
                ))],
            },
            history::Message::User {
                content: vec![history::UserContent::ToolResult(history::ToolResult {
                    call: "t1".into(),
                    name: "bash".into(),
                    content: vec![history::ToolResultContent::text(
                        "skipped: cancelled by the user",
                    )],
                    is_error: true,
                })],
            },
            history::Message::user("[Cancelled by user]"),
        ];
        let rig = own_to_rig(&own);
        assert_eq!(rig.len(), 2);
        let RigMessage::User { content } = &rig[1] else {
            panic!("expected user message");
        };
        assert!(matches!(content[0], UserContent::ToolResult(_)));
        assert!(matches!(content[1], UserContent::Text(_)));
    }

    /// Image-bearing user messages never merge with tool-result messages,
    /// preserving the OpenAI split the follow-up message exists for.
    #[test]
    fn image_messages_stay_out_of_tool_result_messages() {
        let own = vec![
            history::Message::Assistant {
                content: vec![history::AssistantContent::ToolCall(history::ToolCall::new(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "ls"}),
                ))],
            },
            history::Message::User {
                content: vec![history::UserContent::ToolResult(history::ToolResult {
                    call: "t1".into(),
                    name: "bash".into(),
                    content: vec![history::ToolResultContent::text("done")],
                    is_error: false,
                })],
            },
            history::Message::User {
                content: vec![history::UserContent::Image(history::ImageBlock {
                    media_type: history::ImageMedia::Png,
                    data: "aGk=".into(),
                    caption: "[image]".into(),
                })],
            },
        ];
        let rig = own_to_rig(&own);
        assert_eq!(rig.len(), 3);
        let RigMessage::User { content } = &rig[1] else {
            panic!("expected tool-result user message");
        };
        assert!(matches!(content[0], UserContent::ToolResult(_)));
        let RigMessage::User { content } = &rig[2] else {
            panic!("expected image user message");
        };
        assert!(matches!(content[0], UserContent::Image(_)));
    }

    #[test]
    fn to_request_sets_preamble_tools_and_sampling() {
        let tools = vec![ToolDefinition {
            name: ToolName::new("bash").expect("non-empty name"),
            description: "run".into(),
            parameters: serde_json::json!({}),
        }];
        let request = to_request(&sample(), &tools, Some("be brief"), Some(0.5), Some(1024));
        assert_eq!(request.system_instructions(), Some("be brief"));
        // Sample history's own system message rides after the preamble.
        assert_eq!(request.chat_history.len(), 5);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.max_tokens, Some(1024));
        assert!(request.accept_unknown_finish_reasons);
        assert!(matches!(
            request.chat_history.first(),
            Some(RigMessage::System { content }) if content == "be brief"
        ));
    }

    #[test]
    fn empty_preamble_is_omitted() {
        let request = to_request(&sample(), &[], Some(""), None, None);
        assert_eq!(request.system_instructions(), Some("sys"));
        assert!(matches!(
            request.chat_history.first(),
            Some(RigMessage::System { content }) if content == "sys"
        ));
    }

    #[test]
    fn assistant_from_stream_remaps_tool_ids() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(
            &mut parts,
            &StreamEvent::Text {
                part: part(0),
                text: "partial".into(),
            },
        );
        let call = rig_core::completion::message::ToolCall::from_wire(
            "provider-id",
            ToolFunction::new(tool_name("bash"), serde_json::json!({"command": "ls"})),
        );
        fold_streamed_event(
            &mut parts,
            &StreamEvent::End {
                part: part(1),
                content: AssistantContent::ToolCall(call),
            },
        );
        let mut ids = HashMap::new();
        ids.insert("provider-id".to_string(), "internal-1".to_string());
        let message = assistant_from_stream(&[], None, &ids, &parts);
        let history::Message::Assistant { content } = &message else {
            panic!("assistant message");
        };
        assert_eq!(content[0], history::AssistantContent::text("partial"));
        let history::AssistantContent::ToolCall(call) = &content[1] else {
            panic!("tool call block");
        };
        assert_eq!(call.id, "internal-1");
        assert_eq!(call.function.name, "bash");
    }
    #[test]
    fn user_image_block_round_trips() {
        let own = history::Message::User {
            content: vec![
                history::UserContent::text("what is this?"),
                history::UserContent::Image(history::ImageBlock {
                    media_type: history::ImageMedia::Webp,
                    data: "d2Vi-cA==".into(),
                    caption: "[image]".into(),
                }),
            ],
        };
        let rig = own_message_to_rig(&own);
        let RigMessage::User { content } = &rig else {
            panic!("user message");
        };
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], UserContent::Text(_)));
        let UserContent::Image(image) = &content[1] else {
            panic!("image block");
        };
        assert_eq!(image.media_type, Some(ImageMediaType::WEBP));

        // And back: the replayed block keeps its payload.
        let back = rig_message_to_own(&rig);
        let history::Message::User { content } = back else {
            panic!("user message");
        };
        assert!(matches!(
            &content[1],
            history::UserContent::Image(b) if b.data == "d2Vi-cA=="
        ));
    }

    /// Regression for the Bedrock failure "reasoning conversion requires at
    /// least one text or summary block": an adaptive-thinking block is
    /// signature-only, and the signature must survive the stream fold into
    /// history. The canonical 0.44 wire form carries signatures only in
    /// provider-native items a generic mapping cannot mint, so the replay
    /// copy drops the empty block instead of sending thinking Bedrock
    /// rejects — exactly the failure this guards against.
    /// Regression for craft-build/craft#17: rig-bedrock 0.44 keeps the
    /// thinking signature nested in the native item
    /// (`reasoningContent.reasoningText.signature`), and the only replayable
    /// form of signed thinking is the native item itself. The decode must
    /// find the nested signature, keep the item, and the encode must
    /// re-attach both the item and the turn origin so the wire replays the
    /// block verbatim instead of demoting it to text.
    #[test]
    fn bedrock_signed_thinking_replays_through_its_native_item() {
        let native = r#"{"reasoningContent":{"reasoningText":{"text":"Thinking about it.","signature":"sig-bedrock"}}}"#;
        let mut parts = StreamedParts::default();
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                0,
                reasoning_with_native(
                    RigReasoning {
                        text: "Thinking about it.".into(),
                        redacted: false,
                        native: None,
                    },
                    native,
                ),
            ),
        );
        let origin = Origin::new(
            "bedrock.converse",
            "bedrock",
            "us.anthropic.claude-opus-5-5",
        );
        let message = assistant_from_stream(&[], Some(&origin), &HashMap::new(), &parts);
        let history::Message::Assistant { content } = &message else {
            panic!("assistant message");
        };
        let history::AssistantContent::Reasoning(reasoning) = &content[0] else {
            panic!("reasoning block");
        };
        // The nested Bedrock signature is extracted...
        match &reasoning.content[0] {
            history::ReasoningContent::Text {
                signature, native, ..
            } => {
                assert_eq!(signature, &Some("sig-bedrock".into()));
                assert!(native.is_some(), "the provider item must be kept");
            }
            other => panic!("expected text reasoning, got {other:?}"),
        }
        // ...and the turn origin rides the history for same-wire replay.
        assert!(reasoning.origin.is_some());

        let rig = own_to_rig(&[message]);
        let RigMessage::Assistant(assistant) = &rig[0] else {
            panic!("assistant message");
        };
        // The origin names the wire that produced the turn...
        assert_eq!(assistant.origin.as_ref(), Some(&origin));
        // ...and the native item is re-attached with a current fingerprint,
        // so rig's encoders replay the signed block verbatim.
        let item: serde_json::Value = serde_json::from_str(native).unwrap();
        assert_eq!(
            assistant
                .content
                .first()
                .and_then(|block| block.native_item()),
            Some(&item)
        );
    }

    /// The turn origin from the response reaches the replayed message even
    /// when the reasoning arrived as deltas only (no complete block).
    #[test]
    fn origin_rides_unsigned_reasoning_turns_too() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(&mut parts, &reasoning_delta(0, "hmm"));
        let origin = Origin::new(
            "bedrock.converse",
            "bedrock",
            "us.anthropic.claude-opus-5-5",
        );
        let message = assistant_from_stream(&[], Some(&origin), &HashMap::new(), &parts);
        let rig = own_to_rig(&[message]);
        let RigMessage::Assistant(assistant) = &rig[0] else {
            panic!("assistant message");
        };
        assert_eq!(assistant.origin.as_ref(), Some(&origin));
    }

    /// End-to-end against the real rig-bedrock encoder: a Bedrock turn whose
    /// signed thinking was captured with its native item and origin must go
    /// back over the wire as `reasoningContent` with the signature — not be
    /// demoted to a plain text block, which is what broke craft-build/craft#17.
    #[test]
    fn bedrock_wire_replays_signed_thinking_verbatim() {
        let model = "us.anthropic.claude-opus-5-5";
        let native = r#"{"reasoningContent":{"reasoningText":{"text":"Thinking about it.","signature":"sig-bedrock"}}}"#;
        let mut parts = StreamedParts::default();
        parts.text.push_str("Hello there.");
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                0,
                reasoning_with_native(
                    RigReasoning {
                        text: "Thinking about it.".into(),
                        redacted: false,
                        native: None,
                    },
                    native,
                ),
            ),
        );
        // The origin a Bedrock response carries: rig's provider descriptor
        // name, not craft's config spelling.
        let origin = Origin::new("bedrock.converse", "aws_bedrock", model);
        let turn = assistant_from_stream(&[], Some(&origin), &HashMap::new(), &parts);
        let request = to_request(
            &[turn, history::Message::user("next")],
            &[],
            None,
            None,
            None,
        );
        let encoded = Converse::new(model)
            .encode(request, rig_core::wire::Mode::Streaming)
            .expect("the request is representable");
        let body = serde_json::to_value(&encoded.body).expect("body is JSON");
        let messages = body
            .pointer("/messages")
            .and_then(|m| m.as_array())
            .expect("messages");
        // [assistant reasoning+text, user "next"]: the reasoning must be the
        // first block of the assistant message, still signed.
        let assistant = messages
            .iter()
            .find(|m| m.get("role") == Some(&serde_json::json!("assistant")))
            .expect("assistant message");
        let blocks = assistant
            .get("content")
            .and_then(|c| c.as_array())
            .expect("content");
        assert_eq!(
            blocks
                .first()
                .and_then(|b| b.pointer("/reasoningContent/reasoningText/signature")),
            Some(&serde_json::json!("sig-bedrock")),
            "signed thinking must replay as reasoningContent, not text: {blocks:?}"
        );
    }

    #[test]
    fn signature_only_thinking_block_replays_with_its_signature() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                0,
                RigReasoning {
                    text: String::new(),
                    redacted: false,
                    native: Some(reasoning_native(r#"{"signature": "sig-1"}"#)),
                },
            ),
        );
        let message = assistant_from_stream(&[], None, &HashMap::new(), &parts);
        let history::Message::Assistant { content } = &message else {
            panic!("assistant message");
        };
        // The signature persists into craft's own history form.
        assert_eq!(
            content.first(),
            Some(&history::AssistantContent::Reasoning(history::Reasoning {
                content: vec![history::ReasoningContent::Text {
                    text: String::new(),
                    signature: Some("sig-1".into()),
                    native: None,
                }],
                origin: None,
            }))
        );
        // ...and the wire copy omits the empty block rather than shipping
        // thinking no provider can answer back.
        let rig = own_to_rig(&[message, history::Message::user("next")]);
        assert!(matches!(rig.first(), Some(RigMessage::User { .. })));
    }

    #[test]
    fn signed_thinking_text_round_trips_its_signature() {
        let own = vec![history::Message::Assistant {
            content: vec![history::AssistantContent::Reasoning(history::Reasoning {
                content: vec![history::ReasoningContent::Text {
                    text: "deliberating".into(),
                    signature: Some("sig-2".into()),
                    native: None,
                }],
                origin: None,
            })],
        }];
        let rig = own_to_rig(&own);
        let RigMessage::Assistant(assistant) = &rig[0] else {
            panic!("assistant message");
        };
        assert!(matches!(
            assistant.content.first(),
            Some(AssistantContent::Reasoning(reasoning))
                if reasoning.text == "deliberating"
        ));
        // The signature lives in craft's stored copy; the wire copy carries
        // text reasoning and lets providers that need signatures replay it
        // through their own provenance channel.
        let back = rig_to_own(&rig);
        let history::Message::Assistant { content } = &back[0] else {
            panic!("assistant message");
        };
        let history::AssistantContent::Reasoning(reasoning) = &content[0] else {
            panic!("reasoning block");
        };
        assert_eq!(reasoning.content.len(), 1);
        match &reasoning.content[0] {
            history::ReasoningContent::Text {
                text, signature, ..
            } => {
                assert_eq!(text, "deliberating");
                // The canonical wire form of 0.44 carries signatures only in
                // provider-native items, which a generic mapping cannot mint.
                assert_eq!(signature, &None);
            }
            other => panic!("expected text reasoning, got {other:?}"),
        }
    }

    /// A redacted sibling arrives as its own complete block after the
    /// plaintext one; it must land beside the text, not replace it.
    #[test]
    fn redacted_sibling_keeps_the_thinking_text() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(&mut parts, &reasoning_delta(0, "step one"));
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                0,
                RigReasoning {
                    text: "step one".into(),
                    redacted: false,
                    native: Some(reasoning_native(r#"{"signature": "sig-3"}"#)),
                },
            ),
        );
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                1,
                RigReasoning {
                    text: "cmVkYWN0ZWQ=".into(),
                    redacted: true,
                    native: None,
                },
            ),
        );
        let message = assistant_from_stream(&[], None, &HashMap::new(), &parts);
        let history::Message::Assistant { content } = &message else {
            panic!("assistant message");
        };
        let history::AssistantContent::Reasoning(reasoning) = &content[0] else {
            panic!("reasoning block");
        };
        assert_eq!(reasoning.content.len(), 2);
        assert_eq!(
            reasoning.content[0],
            history::ReasoningContent::Text {
                text: "step one".into(),
                signature: Some("sig-3".into()),
                native: None,
            }
        );
        // Bedrock-native redacted: kept as its own kind so replay can decode
        // it back into a `redactedContent` blob instead of dropping it as
        // foreign ciphertext.
        assert_eq!(
            reasoning.content[1],
            history::ReasoningContent::Redacted {
                data: "cmVkYWN0ZWQ=".into(),
                native: None,
            }
        );
    }

    #[test]
    fn redacted_and_foreign_opaque_replay_distinctly() {
        let own = vec![history::Message::Assistant {
            content: vec![history::AssistantContent::Reasoning(history::Reasoning {
                content: vec![
                    history::ReasoningContent::Redacted {
                        data: "YmVkcm9jay1ibG9i".into(),
                        native: None,
                    },
                    history::ReasoningContent::Opaque("openai-encrypted".into()),
                ],
                origin: None,
            })],
        }];
        let rig = own_to_rig(&own);
        let RigMessage::Assistant(assistant) = &rig[0] else {
            panic!("assistant message");
        };
        // Each persisted part is its own streamed block: redacted carries the
        // payload under the redacted flag, opaque ciphertext replays as text.
        match &assistant.content[0] {
            AssistantContent::Reasoning(reasoning) => {
                assert!(reasoning.redacted && reasoning.text == "YmVkcm9jay1ibG9i");
            }
            other => panic!("expected redacted reasoning, got {other:?}"),
        }
        match &assistant.content[1] {
            AssistantContent::Reasoning(reasoning) => {
                assert!(!reasoning.redacted && reasoning.text == "openai-encrypted");
            }
            other => panic!("expected opaque reasoning, got {other:?}"),
        }
        // Inbound direction: a stream's redacted block persists as Redacted.
        let back = rig_to_own(&rig);
        let history::Message::Assistant { content } = &back[0] else {
            panic!("assistant message");
        };
        let history::AssistantContent::Reasoning(reasoning) = &content[0] else {
            panic!("reasoning block");
        };
        assert_eq!(
            reasoning.content[0],
            history::ReasoningContent::Redacted {
                data: "YmVkcm9jay1ibG9i".into(),
                native: None,
            }
        );
    }

    /// Sessions persisted by builds that dropped signatures contain empty
    /// unsigned thinking blocks; replay must omit them rather than fail.
    #[test]
    fn replay_drops_empty_unsigned_reasoning() {
        let own = vec![history::Message::Assistant {
            content: vec![
                history::AssistantContent::Reasoning(history::Reasoning {
                    content: vec![history::ReasoningContent::Text {
                        text: String::new(),
                        signature: None,
                        native: None,
                    }],
                    origin: None,
                }),
                history::AssistantContent::text("answer"),
            ],
        }];
        let rig = own_to_rig(&own);
        let RigMessage::Assistant(assistant) = &rig[0] else {
            panic!("assistant message");
        };
        assert_eq!(assistant.content.len(), 1);
        assert!(matches!(assistant.content[0], AssistantContent::Text(_)));
    }
}
