//! The provider edge: the only module that imports rig-core's message types.
//!
//! Everything crossing to or from a provider model goes through here: our
//! [`history::Message`] list becomes a rig [`CompletionRequest`], streamed rig
//! content becomes history messages. Keeping the mapping in one place is what
//! lets the rest of the crate stay free of `rig` types.

use std::collections::HashMap;

use rig_core::completion::message::{
    AssistantContent, ImageMediaType, Message as RigMessage, Reasoning as RigReasoning,
    Text as RigText, ToolCall as RigToolCall, ToolCallId, ToolFunction,
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
pub fn own_to_rig(messages: &[history::Message]) -> Vec<RigMessage> {
    messages.iter().map(own_message_to_rig).collect()
}

pub fn own_message_to_rig(message: &history::Message) -> RigMessage {
    match message {
        history::Message::System { content } => RigMessage::system(content.clone()),
        history::Message::User { content } => RigMessage::User {
            content: content.iter().map(own_user_block_to_rig).collect(),
        },
        history::Message::Assistant { content } => RigMessage::Assistant {
            id: None,
            content: content.iter().map(own_assistant_block_to_rig).collect(),
        },
    }
}

fn own_user_block_to_rig(block: &history::UserContent) -> UserContent {
    match block {
        history::UserContent::Text(text) => UserContent::Text(rig_text(&text.text)),
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
                    history::ToolResultContent::Image(image) => RigToolResultContent::image_base64(
                        image.data.clone(),
                        Some(media_own_to_rig(image.media_type)),
                        None,
                    ),
                })
                .collect(),
        }),
    }
}

fn own_assistant_block_to_rig(block: &history::AssistantContent) -> AssistantContent {
    match block {
        history::AssistantContent::Text(text) => AssistantContent::Text(rig_text(&text.text)),
        history::AssistantContent::Reasoning(reasoning) => {
            AssistantContent::Reasoning(RigReasoning {
                id: None,
                content: reasoning.content.iter().map(own_reasoning_to_rig).collect(),
            })
        }
        history::AssistantContent::ToolCall(call) => AssistantContent::ToolCall(RigToolCall {
            id: ToolCallId::new_or_mint(&call.id),
            provider: None,
            function: ToolFunction {
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            },
            signature: None,
            additional_params: None,
        }),
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
        history::ReasoningContent::Text { text } => Rig::Text {
            text: text.clone(),
            signature: None,
        },
        history::ReasoningContent::Opaque(data) => Rig::Encrypted(data.clone()),
    }
}

fn rig_reasoning_to_own(
    item: &rig_core::completion::message::ReasoningContent,
) -> history::ReasoningContent {
    use rig_core::completion::message::ReasoningContent as Rig;
    match item {
        Rig::Text { text, .. } => history::ReasoningContent::Text { text: text.clone() },
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
    pub tool_calls: Vec<RigToolCall>,
}

/// Record one streamed assistant event into `parts`.
pub fn fold_streamed_event(parts: &mut StreamedParts, event: &StreamedAssistantContent) {
    match event {
        StreamedAssistantContent::Text(text) => parts.text.push_str(&text.text),
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            if let Some(history::ReasoningContent::Text { text }) =
                parts.reasoning.content.last_mut()
            {
                text.push_str(reasoning);
            } else {
                parts
                    .reasoning
                    .content
                    .push(history::ReasoningContent::Text {
                        text: reasoning.clone(),
                    });
            }
        }
        StreamedAssistantContent::Reasoning { reasoning, .. } => {
            // The complete block supersedes its deltas.
            parts.reasoning = history::Reasoning {
                content: reasoning.content.iter().map(rig_reasoning_to_own).collect(),
            };
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
        let back = rig_to_own(&rig);
        match &back[0] {
            history::Message::User { content } => {
                let history::UserContent::ToolResult(result) = &content[0] else {
                    panic!("expected tool result");
                };
                let history::ToolResultContent::Image(image) = &result.content[0] else {
                    panic!("expected image block, got {:?}", result.content[0]);
                };
                assert_eq!(image.media_type, history::ImageMedia::Webp);
                assert_eq!(image.data, "aGVsbG8=");
                // rig carries no caption field; the text stand-in is generic.
                assert_eq!(image.caption, "[image]");
            }
            _ => panic!("expected user message"),
        }
        // The provider-bound replay keeps the image block as real vision input.
        let RigMessage::User { content } = &rig[0] else {
            panic!("expected user message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("expected tool result");
        };
        assert!(matches!(
            result.content.first(),
            Some(RigToolResultContent::Image(image))
                if image.media_type == Some(ImageMediaType::WEBP)
        ));
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
}
