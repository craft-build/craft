//! The pre-animist craft message enums, serde-only, and the converter that
//! turns a legacy session's messages into animist [`StoredMessage`]s (03's
//! `edge.rs` mapping, in the history direction). Nothing in the crate
//! constructs these types; they exist so old session files keep loading.

use animist::history::{ContentBlock, MessageKind, Role, StoredMessage, ToolContent};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::history::marker;

/// Marker texts the old format spelled into plain user messages; the
/// converter recognizes them and tags the converted observation instead.
fn marker_of(text: &str) -> Option<&'static str> {
    if text == crate::run::CANCEL_MARKER {
        Some(marker::CANCEL)
    } else if text == crate::run::END_MARKER {
        Some(marker::END)
    } else if text == crate::run::doom::GRACE_CALL_PROMPT {
        Some(marker::GRACE)
    } else if text == crate::run::NUDGE_PROMPT {
        Some(marker::NUDGE)
    } else if text.starts_with("[Run failed: ") && text.ends_with(']') {
        Some(marker::FAILED)
    } else {
        None
    }
}

/// One message in a legacy conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    System { content: String },
    User { content: Vec<UserContent> },
    Assistant { content: Vec<AssistantContent> },
}

/// A block of legacy user-message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UserContent {
    Text(Text),
    ToolResult(ToolResult),
    Image(ImageBlock),
}

/// A block of legacy assistant-message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AssistantContent {
    Text(Text),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: ToolFunction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call: String,
    pub name: String,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolResultContent {
    Text(Text),
    Json { value: serde_json::Value },
    Image(ImageBlock),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub media_type: ImageMedia,
    pub data: String,
    pub caption: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageMedia {
    Png,
    Jpeg,
    Gif,
    Webp,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub content: Vec<ReasoningContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReasoningContent {
    Text {
        text: String,
        #[serde(default)]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<serde_json::Value>,
    },
    Redacted {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native: Option<serde_json::Value>,
    },
    Opaque(String),
}

/// Convert one legacy session's messages into stored messages. All messages
/// share `timestamp`: the legacy format stored no per-message time, so the
/// session's `created_at` stands in for every message — an approximation
/// that only mis-orders nothing (order is positional) and only mis-dates
/// messages written after the session started.
pub(super) fn to_stored(messages: &[Message], timestamp: Timestamp) -> Vec<StoredMessage> {
    messages
        .iter()
        .map(|message| convert(message, timestamp))
        .collect()
}

fn convert(message: &Message, timestamp: Timestamp) -> StoredMessage {
    match message {
        Message::System { content } => StoredMessage::builder()
            .timestamp(timestamp)
            .message(animist::history::Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: content.clone(),
                }],
                display_text: None,
                kind: MessageKind::Turn,
            })
            .build(),
        Message::User { content } => {
            let blocks: Vec<ContentBlock> = content.iter().map(user_block).collect();
            let mut stored = StoredMessage::builder()
                .timestamp(timestamp)
                .message(animist::history::Message {
                    role: Role::User,
                    content: blocks,
                    display_text: None,
                    kind: MessageKind::Turn,
                })
                .build();
            // A single-text user message may be an old marker spelled out in
            // prose; tag it so detection no longer depends on the text.
            if let [ContentBlock::Text { text }] = stored.message().content.as_slice()
                && let Some(tag) = marker_of(text)
            {
                stored
                    .tags_mut()
                    .insert(marker::TAG.to_owned(), tag.to_owned());
                stored.message_mut().kind = MessageKind::Observation;
            }
            stored
        }
        Message::Assistant { content } => StoredMessage::builder()
            .timestamp(timestamp)
            .message(animist::history::Message {
                role: Role::Assistant,
                content: content.iter().flat_map(assistant_blocks).collect(),
                display_text: None,
                kind: MessageKind::Turn,
            })
            .build(),
    }
}

fn user_block(block: &UserContent) -> ContentBlock {
    match block {
        UserContent::Text(Text { text }) => ContentBlock::Text { text: text.clone() },
        UserContent::Image(ImageBlock {
            media_type, data, ..
        }) => ContentBlock::Image {
            source: animist::history::ImageSource::Base64 {
                media_type: media_type.as_str().to_owned(),
                data: data.clone(),
            },
        },
        UserContent::ToolResult(ToolResult {
            call,
            name,
            content,
            is_error,
        }) => ContentBlock::ToolResult {
            tool_use_id: call.clone(),
            // The agent fills names for every result it writes; the
            // converter does the same, so converted sessions are
            // self-describing too.
            name: Some(name.clone()),
            content: content
                .iter()
                .map(result_content)
                .collect::<Vec<_>>()
                .into(),
            is_error: *is_error,
            loaded_tools: Vec::new(),
        },
    }
}

/// One legacy assistant block maps to zero or more animist blocks: a
/// reasoning block holds several parts, and empty opaque payloads drop, as
/// the request mapping always did.
fn assistant_blocks(block: &AssistantContent) -> Vec<ContentBlock> {
    match block {
        AssistantContent::Text(Text { text }) => vec![ContentBlock::Text { text: text.clone() }],
        AssistantContent::Reasoning(reasoning) => reasoning
            .content
            .iter()
            .filter_map(|part| match part {
                ReasoningContent::Text {
                    text, signature, ..
                } => Some(ContentBlock::Thinking {
                    thinking: text.clone(),
                    signature: signature.clone(),
                }),
                ReasoningContent::Redacted { data, .. } => {
                    Some(ContentBlock::RedactedThinking { data: data.clone() })
                }
                ReasoningContent::Opaque(payload) if !payload.is_empty() => {
                    Some(ContentBlock::Thinking {
                        thinking: payload.clone(),
                        signature: None,
                    })
                }
                ReasoningContent::Opaque(_) => None,
            })
            .collect(),
        AssistantContent::ToolCall(ToolCall {
            id,
            function: ToolFunction { name, arguments },
        }) => vec![ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: arguments.clone(),
            thought_signature: None,
        }],
    }
}

fn result_content(content: &ToolResultContent) -> ToolContent {
    match content {
        ToolResultContent::Text(Text { text }) => ToolContent::Text { text: text.clone() },
        // Legacy JSON results render to pretty text; tools that produce
        // JSON render it themselves (06).
        ToolResultContent::Json { value } => ToolContent::Text {
            text: serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        },
        ToolResultContent::Image(ImageBlock {
            media_type, data, ..
        }) => ToolContent::Image {
            source: animist::history::ImageSource::Base64 {
                media_type: media_type.as_str().to_owned(),
                data: data.clone(),
            },
        },
    }
}

/// The legacy `tool_outputs` payload as its animist block shape, so old raw
/// copies stay readable until 08 moves them into entry sources.
pub(super) fn tool_result_block(result: &ToolResult) -> ContentBlock {
    user_block(&UserContent::ToolResult(result.clone()))
}

impl ImageMedia {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> Timestamp {
        Timestamp::from_second(1_700_000_000).unwrap()
    }

    #[test]
    fn markers_are_recognized_and_tagged() {
        let messages = [
            Message::User {
                content: vec![UserContent::Text(Text {
                    text: crate::run::CANCEL_MARKER.into(),
                })],
            },
            Message::User {
                content: vec![UserContent::Text(Text {
                    text: "[Run failed: boom]".into(),
                })],
            },
            Message::User {
                content: vec![UserContent::Text(Text {
                    text: crate::run::doom::GRACE_CALL_PROMPT.into(),
                })],
            },
            Message::User {
                content: vec![UserContent::Text(Text {
                    text: "[Cancelled by user] and more".into(),
                })],
            },
        ];
        let stored = to_stored(&messages, ts());
        assert_eq!(marker::of(&stored[0]), Some(marker::CANCEL));
        assert_eq!(marker::of(&stored[1]), Some(marker::FAILED));
        assert_eq!(marker::of(&stored[2]), Some(marker::GRACE));
        assert_eq!(marker::of(&stored[3]), None, "not an exact marker text");
        assert_eq!(
            stored[0].message().kind,
            MessageKind::Observation,
            "a marker is never a user turn"
        );
        assert_eq!(stored[3].message().kind, MessageKind::Turn);
    }

    #[test]
    fn results_carry_names_and_json_renders_to_text() {
        let messages = [Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                call: "t1".into(),
                name: "grep".into(),
                content: vec![
                    ToolResultContent::Text(Text { text: "one".into() }),
                    ToolResultContent::Json {
                        value: serde_json::json!({"n": 1}),
                    },
                ],
                is_error: true,
            })],
        }];
        let stored = to_stored(&messages, ts());
        let ContentBlock::ToolResult {
            tool_use_id,
            name,
            content,
            is_error,
            ..
        } = &stored[0].message().content[0]
        else {
            panic!("tool result block");
        };
        assert_eq!(tool_use_id, "t1");
        assert_eq!(name.as_deref(), Some("grep"));
        assert!(*is_error);
        assert_eq!(
            crate::history::ext::tool_content_text(content),
            "one\n{\n  \"n\": 1\n}"
        );
    }
}
