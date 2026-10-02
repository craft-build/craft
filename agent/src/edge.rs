//! The provider edge: the only module that imports rig-core's message types.
//!
//! Everything crossing to or from a provider model goes through here: our
//! [`history::Message`] list becomes a rig [`CompletionRequest`], streamed rig
//! content becomes history messages. Keeping the mapping in one place is what
//! lets the rest of the crate stay free of `rig` types.

use std::collections::HashMap;

use rig_core::completion::message::{
    AssistantContent, DocumentSourceKind, Image as RigImage, ImageMediaType, Message as RigMessage,
    Reasoning as RigReasoning, Text as RigText, ToolCall as RigToolCall, ToolCallId, ToolFunction,
    ToolResult as RigToolResult, ToolResultContent as RigToolResultContent, UserContent,
};
use rig_core::completion::{CompletionRequest, ToolDefinition};
use rig_core::streaming::StreamedAssistantContent;

use crate::history;

/// Build the provider request for one model call.
#[allow(clippy::too_many_arguments)]
pub fn to_request(
    history: &[history::Message],
    tools: &[ToolDefinition],
    preamble: Option<&str>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
) -> CompletionRequest {
    CompletionRequest {
        model: None,
        preamble: preamble.filter(|text| !text.is_empty()).map(str::to_owned),
        chat_history: own_to_rig(history),
        documents: Vec::new(),
        tools: tools.to_vec(),
        temperature,
        max_tokens,
        tool_choice: None,
        additional_params: None,
        output_schema: None,
        record_telemetry_content: false,
    }
}

/// Convert our history into rig's replay format.
///
/// Images inside tool results are moved into a user message that follows the
/// tool-result message: OpenAI Chat Completions rejects image content in
/// tool results ("does not support images in tool results"), while both
/// families accept an image in user content.
pub fn own_to_rig(messages: &[history::Message]) -> Vec<RigMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        out.push(own_message_to_rig(message));
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
                                    Some(UserContent::Image(RigImage {
                                        data: DocumentSourceKind::Base64(image.data.clone()),
                                        media_type: Some(media_own_to_rig(image.media_type)),
                                        detail: None,
                                        additional_params: None,
                                    }))
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
                let mut content = vec![UserContent::Text(rig_text(
                    "Images returned by the tool calls above:",
                ))];
                content.extend(images);
                out.push(RigMessage::User { content });
            }
        }
    }
    out
}

pub fn own_message_to_rig(message: &history::Message) -> RigMessage {
    match message {
        history::Message::System { content } => RigMessage::system(content.clone()),
        history::Message::User { content } => RigMessage::User {
            content: content.iter().map(own_user_block_to_rig).collect(),
        },
        history::Message::Assistant { content } => RigMessage::Assistant {
            id: None,
            content: content
                .iter()
                .filter_map(own_assistant_block_to_rig)
                .collect(),
        },
    }
}

fn own_user_block_to_rig(block: &history::UserContent) -> UserContent {
    match block {
        history::UserContent::Text(text) => UserContent::Text(rig_text(&text.text)),
        history::UserContent::Image(image) => UserContent::Image(RigImage {
            data: DocumentSourceKind::Base64(image.data.clone()),
            media_type: Some(media_own_to_rig(image.media_type)),
            detail: None,
            additional_params: None,
        }),
        history::UserContent::ToolResult(result) => UserContent::ToolResult(RigToolResult {
            call: ToolCallId::new_or_mint(&result.call),
            provider: None,
            name: result.name.clone(),
            content: result
                .content
                .iter()
                .map(|item| match item {
                    history::ToolResultContent::Text(text) => {
                        RigToolResultContent::Text(rig_text(&text.text))
                    }
                    history::ToolResultContent::Json { value } => RigToolResultContent::Json {
                        value: value.clone(),
                    },
                    history::ToolResultContent::Image(image) => {
                        RigToolResultContent::Text(rig_text(&format!(
                            "[image {} returned; attached in the next message]",
                            image.caption
                        )))
                    }
                })
                .collect(),
        }),
    }
}

fn own_assistant_block_to_rig(block: &history::AssistantContent) -> Option<AssistantContent> {
    match block {
        history::AssistantContent::Text(text) => Some(AssistantContent::Text(rig_text(&text.text))),
        history::AssistantContent::Reasoning(reasoning) => {
            let content: Vec<_> = reasoning
                .content
                .iter()
                .map(own_reasoning_to_rig)
                // A part with neither text nor signature carries nothing any
                // provider can replay (Bedrock rejects the block outright);
                // dropping it recovers sessions persisted without signatures.
                .filter(|part| match part {
                    rig_core::message::ReasoningContent::Text { text, signature } => {
                        !text.is_empty() || signature.is_some()
                    }
                    _ => true,
                })
                .collect();
            (!content.is_empty())
                .then(|| AssistantContent::Reasoning(RigReasoning { id: None, content }))
        }
        history::AssistantContent::ToolCall(call) => {
            Some(AssistantContent::ToolCall(RigToolCall {
                id: ToolCallId::new_or_mint(&call.id),
                provider: None,
                function: ToolFunction {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                },
                signature: None,
                additional_params: None,
            }))
        }
    }
}

fn rig_text(text: &str) -> RigText {
    RigText {
        text: text.to_owned(),
        additional_params: None,
    }
}

fn own_reasoning_to_rig(
    item: &history::ReasoningContent,
) -> rig_core::completion::message::ReasoningContent {
    use rig_core::completion::message::ReasoningContent as Rig;
    match item {
        history::ReasoningContent::Text { text, signature } => Rig::Text {
            text: text.clone(),
            signature: signature.clone(),
        },
        history::ReasoningContent::Opaque(data) => Rig::Encrypted(data.clone()),
    }
}

fn rig_reasoning_to_own(
    item: &rig_core::completion::message::ReasoningContent,
) -> history::ReasoningContent {
    use rig_core::completion::message::ReasoningContent as Rig;
    match item {
        Rig::Text { text, signature } => history::ReasoningContent::Text {
            text: text.clone(),
            signature: signature.clone(),
        },
        Rig::Encrypted(data) | Rig::Summary(data) => {
            history::ReasoningContent::Opaque(data.clone())
        }
        Rig::Redacted { data } => history::ReasoningContent::Opaque(data.clone()),
    }
}

/// Assemble the assistant message for one completed model call from the
/// streamed content. `call_ids` remaps tool-call ids: the stream's complete
/// `ToolCall` events carry the run-stable `internal_call_id`, while the
/// aggregated choice keeps provider ids that may be missing; results must
/// correlate with whatever id we recorded at call time.
pub fn assistant_from_stream(
    choice: &[AssistantContent],
    call_ids: &HashMap<String, String>,
    streamed: &StreamedParts,
) -> history::Message {
    let _ = choice;
    let mut content = Vec::new();
    if !streamed.reasoning.content.is_empty() {
        content.push(history::AssistantContent::Reasoning(
            streamed.reasoning.clone(),
        ));
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
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
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
    reasoning_block_id: Option<String>,
    pub tool_calls: Vec<RigToolCall>,
}

/// Record one streamed assistant event into `parts`.
pub fn fold_streamed_event(parts: &mut StreamedParts, event: &StreamedAssistantContent) {
    match event {
        StreamedAssistantContent::Text(text) => parts.text.push_str(&text.text),
        StreamedAssistantContent::ReasoningDelta { id, reasoning, .. } => {
            if parts.reasoning_block_id.as_deref() != Some(id.as_str()) {
                parts.reasoning_block_id = Some(id.clone());
                parts
                    .reasoning
                    .content
                    .push(history::ReasoningContent::Text {
                        text: String::new(),
                        signature: None,
                    });
            }
            if let Some(history::ReasoningContent::Text { text, .. }) =
                parts.reasoning.content.last_mut()
            {
                text.push_str(reasoning);
            }
        }
        StreamedAssistantContent::Reasoning { id, reasoning, .. } => {
            // The complete block supersedes its own deltas (matched by
            // correlator) and carries what the deltas could not — the
            // signature, which the wire delivers out of band.
            let content: Vec<_> = reasoning.content.iter().map(rig_reasoning_to_own).collect();
            if parts.reasoning_block_id.as_deref() == Some(id.as_str())
                && parts.reasoning.content.pop().is_some()
            {
                parts.reasoning.content.extend(content);
            } else {
                parts.reasoning_block_id = Some(id.clone());
                parts.reasoning.content.extend(content);
            }
        }
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            parts.tool_calls.push(tool_call.clone());
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
                            name: result.name.clone(),
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
        RigMessage::Assistant { content, .. } => history::Message::Assistant {
            content: content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => {
                        Some(history::AssistantContent::Text(history::Text {
                            text: text.text.clone(),
                        }))
                    }
                    AssistantContent::Reasoning(reasoning) => {
                        Some(history::AssistantContent::Reasoning(history::Reasoning {
                            content: reasoning.content.iter().map(rig_reasoning_to_own).collect(),
                        }))
                    }
                    AssistantContent::ToolCall(call) => {
                        Some(history::AssistantContent::ToolCall(history::ToolCall {
                            id: call.id.to_string(),
                            function: history::ToolFunction {
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
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
        RigToolResultContent::Json { value, .. } => history::ToolResultContent::Json {
            value: value.clone(),
        },
        RigToolResultContent::Image(image) => {
            history::ToolResultContent::Image(history::ImageBlock {
                media_type: image
                    .media_type
                    .as_ref()
                    .map(|media| media_rig_to_own(media.clone()))
                    .unwrap_or(history::ImageMedia::Png),
                data: match &image.data {
                    rig_core::completion::message::DocumentSourceKind::Base64(data) => data.clone(),
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
                        }],
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

    #[test]
    fn to_request_sets_preamble_tools_and_sampling() {
        let tools = vec![ToolDefinition {
            name: "bash".into(),
            description: "run".into(),
            parameters: serde_json::json!({}),
        }];
        let request = to_request(&sample(), &tools, Some("be brief"), Some(0.5), Some(1024));
        assert_eq!(request.preamble.as_deref(), Some("be brief"));
        assert_eq!(request.chat_history.len(), 4);
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.max_tokens, Some(1024));
        assert!(matches!(request.chat_history[0], RigMessage::System { .. }));
    }

    #[test]
    fn empty_preamble_is_omitted() {
        let request = to_request(&sample(), &[], Some(""), None, None);
        assert_eq!(request.preamble, None);
    }

    #[test]
    fn assistant_from_stream_remaps_tool_ids() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(&mut parts, &StreamedAssistantContent::text("partial"));
        let call = rig_core::completion::message::ToolCall {
            id: ToolCallId::new_or_mint("provider-id"),
            provider: None,
            function: ToolFunction {
                name: "bash".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
            signature: None,
            additional_params: None,
        };
        fold_streamed_event(
            &mut parts,
            &StreamedAssistantContent::ToolCall {
                tool_call: call,
                internal_call_id: "internal-1".into(),
            },
        );
        let mut ids = HashMap::new();
        ids.insert("provider-id".to_string(), "internal-1".to_string());
        let message = assistant_from_stream(&[], &ids, &parts);
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
    fn reasoning_event(
        id: &str,
        content: Vec<rig_core::message::ReasoningContent>,
    ) -> StreamedAssistantContent {
        StreamedAssistantContent::Reasoning {
            reasoning: rig_core::message::Reasoning { id: None, content },
            id: id.to_owned(),
        }
    }

    fn reasoning_delta(id: &str, text: &str) -> StreamedAssistantContent {
        StreamedAssistantContent::ReasoningDelta {
            id: id.to_owned(),
            provider_id: None,
            reasoning: text.to_owned(),
        }
    }

    /// Regression for the Bedrock failure "reasoning conversion requires at
    /// least one text or summary block": an adaptive-thinking block is
    /// signature-only, and the signature used to be dropped when persisting
    /// the stream, so replay sent an empty unsigned block.
    #[test]
    fn signature_only_thinking_block_replays_with_its_signature() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                "b1",
                vec![rig_core::message::ReasoningContent::Text {
                    text: String::new(),
                    signature: Some("sig-1".into()),
                }],
            ),
        );
        let message = assistant_from_stream(&[], &HashMap::new(), &parts);
        let rig = own_to_rig(&[message, history::Message::user("next")]);
        let RigMessage::Assistant { content, .. } = &rig[0] else {
            panic!("assistant message");
        };
        assert!(matches!(
            content.first(),
            Some(AssistantContent::Reasoning(reasoning))
                if matches!(
                    reasoning.content.first(),
                    Some(rig_core::message::ReasoningContent::Text { text, signature })
                        if text.is_empty() && signature.as_deref() == Some("sig-1")
                )
        ));
    }

    #[test]
    fn signed_thinking_text_round_trips_its_signature() {
        let own = vec![history::Message::Assistant {
            content: vec![history::AssistantContent::Reasoning(history::Reasoning {
                content: vec![history::ReasoningContent::Text {
                    text: "deliberating".into(),
                    signature: Some("sig-2".into()),
                }],
            })],
        }];
        let rig = own_to_rig(&own);
        let RigMessage::Assistant { content, .. } = &rig[0] else {
            panic!("assistant message");
        };
        assert!(matches!(
            content.first(),
            Some(AssistantContent::Reasoning(reasoning))
                if matches!(
                    reasoning.content.first(),
                    Some(rig_core::message::ReasoningContent::Text { text, signature })
                        if text == "deliberating" && signature.as_deref() == Some("sig-2")
                )
        ));
    }

    /// A redacted sibling arrives as its own complete block after the
    /// plaintext one; it must land beside the text, not replace it.
    #[test]
    fn redacted_sibling_keeps_the_thinking_text() {
        let mut parts = StreamedParts::default();
        fold_streamed_event(&mut parts, &reasoning_delta("b1", "step one"));
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                "b1",
                vec![rig_core::message::ReasoningContent::Text {
                    text: "step one".into(),
                    signature: Some("sig-3".into()),
                }],
            ),
        );
        fold_streamed_event(
            &mut parts,
            &reasoning_event(
                "b2",
                vec![rig_core::message::ReasoningContent::Redacted {
                    data: "cmVkYWN0ZWQ=".into(),
                }],
            ),
        );
        let message = assistant_from_stream(&[], &HashMap::new(), &parts);
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
            }
        );
        assert_eq!(
            reasoning.content[1],
            history::ReasoningContent::Opaque("cmVkYWN0ZWQ=".into())
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
                    }],
                }),
                history::AssistantContent::text("answer"),
            ],
        }];
        let rig = own_to_rig(&own);
        let RigMessage::Assistant { content, .. } = &rig[0] else {
            panic!("assistant message");
        };
        assert_eq!(content.len(), 1);
        assert!(matches!(content[0], AssistantContent::Text(_)));
    }
}
