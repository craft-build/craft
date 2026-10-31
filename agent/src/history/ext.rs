//! Craft-side helpers over animist's history types: the constructors and
//! readers the crate needs and animist does not provide.

use jiff::Timestamp;

use super::{
    ContentBlock, HistoryEntry, ImageSource, Message, MessageKind, Role, StoredMessage,
    ToolContent, ToolContents,
};

/// Image formats a provider message can carry, mirroring the subset the
/// `view_image` tool accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageMedia {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageMedia {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// Inline base64 image content for a known media type.
pub fn image_source(media: ImageMedia, data: impl Into<String>) -> ImageSource {
    ImageSource::Base64 {
        media_type: media.as_str().to_owned(),
        data: data.into(),
    }
}

/// Concatenated text blocks of a message, for surfaces that need the reply.
pub fn text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The stored message's text; see [`text`].
pub fn message_text(stored: &StoredMessage) -> String {
    text(stored.message())
}

/// Whether the message speaks with user authority (a turn or an
/// observation such as a tool result).
pub fn is_user(message: &Message) -> bool {
    matches!(message.role, Role::User)
}

pub fn system_text(text: impl Into<String>) -> Message {
    Message {
        role: Role::System,
        content: vec![ContentBlock::Text { text: text.into() }],
        display_text: None,
        kind: MessageKind::Turn,
    }
}

pub fn user_text(text: impl Into<String>) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
        display_text: None,
        kind: MessageKind::Turn,
    }
}

pub fn assistant_text(text: impl Into<String>) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
        display_text: None,
        kind: MessageKind::Turn,
    }
}

/// A user-role message the harness injected rather than the user authoring:
/// tool results, markers, prompts the driver feeds back to the model.
pub fn observation(content: Vec<ContentBlock>) -> Message {
    Message {
        role: Role::User,
        content,
        display_text: None,
        kind: MessageKind::Observation,
    }
}

/// A tool-result block. The agent fills `name` for every result it writes,
/// which keeps results self-describing; legacy results load with `None`.
pub fn tool_result(
    tool_use_id: impl Into<String>,
    name: Option<&str>,
    content: impl Into<ToolContents>,
    is_error: bool,
) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        name: name.map(str::to_owned),
        content: content.into(),
        is_error,
        loaded_tools: Vec::new(),
    }
}

/// The message's tool calls, in content order.
pub fn tool_calls(message: &Message) -> impl Iterator<Item = &ContentBlock> {
    message
        .content
        .iter()
        .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
}

/// The message's tool results, in content order.
pub fn tool_results(message: &Message) -> impl Iterator<Item = &ContentBlock> {
    message
        .content
        .iter()
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// The name of the tool a block refers to: the result's stored `name`, else
/// the matching call's name by id (legacy results load with `name: None`),
/// else the call's own name, else `"unknown"` for an orphaned result.
pub fn tool_name(block: &ContentBlock, history: &[Message]) -> String {
    match block {
        ContentBlock::ToolUse { name, .. } => name.clone(),
        ContentBlock::ToolResult {
            name: Some(name), ..
        } => name.clone(),
        ContentBlock::ToolResult {
            tool_use_id,
            name: None,
            ..
        } => history
            .iter()
            .flat_map(tool_calls)
            .find_map(|call| match call {
                ContentBlock::ToolUse { id, name, .. } if id == tool_use_id => Some(name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_owned()),
        _ => "unknown".to_owned(),
    }
}

/// The model-visible text of tool-result content: text blocks joined with
/// newlines, images as a stand-in marker.
pub fn tool_content_text(content: &ToolContents) -> String {
    content
        .0
        .iter()
        .map(|item| match item {
            ToolContent::Text { text } => text.clone(),
            ToolContent::Image { .. } => "[image]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A stored message stamped now.
pub fn stored(message: Message) -> StoredMessage {
    StoredMessage::builder()
        .timestamp(Timestamp::now())
        .message(message)
        .build()
}

/// A user-turn stored message stamped now.
pub fn stored_user_text(text: impl Into<String>) -> StoredMessage {
    stored(user_text(text))
}

/// A source-only active entry around a stored message.
pub fn entry(message: StoredMessage) -> HistoryEntry {
    HistoryEntry::builder().source(message).build()
}

/// Wrap stored messages as source-only entries, in order.
pub fn entries(messages: Vec<StoredMessage>) -> Vec<HistoryEntry> {
    messages.into_iter().map(entry).collect()
}

/// The latest stored message of each entry, in order. Inactive entries keep
/// their latest revision: nothing deactivates entries before 05.
pub fn stored_of_entries(entries: &[HistoryEntry]) -> Vec<StoredMessage> {
    entries.iter().map(HistoryEntry::latest).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::marker;

    fn call(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
            thought_signature: None,
        }
    }

    #[test]
    fn text_joins_text_blocks_only() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hidden".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "part one".into(),
                },
                ContentBlock::Text {
                    text: "part two".into(),
                },
            ],
            display_text: None,
            kind: MessageKind::Turn,
        };
        assert_eq!(text(&message), "part one\npart two");
    }

    #[test]
    fn tool_name_falls_back_to_the_call_then_unknown() {
        let call = call("t1", "bash");
        let assistant = Message {
            role: Role::Assistant,
            content: vec![call],
            display_text: None,
            kind: MessageKind::Turn,
        };
        let named = tool_result("t1", Some("bash"), "ok", false);
        assert_eq!(tool_name(&named, std::slice::from_ref(&assistant)), "bash");

        let legacy = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            name: None,
            content: ToolContents::from("ok"),
            is_error: false,
            loaded_tools: Vec::new(),
        };
        assert_eq!(tool_name(&legacy, &[assistant]), "bash");

        let orphan = ContentBlock::ToolResult {
            tool_use_id: "t404".into(),
            name: None,
            content: ToolContents::from("ok"),
            is_error: false,
            loaded_tools: Vec::new(),
        };
        assert_eq!(
            tool_name(&orphan, &[]),
            "unknown",
            "an orphaned legacy result has no name"
        );
    }

    #[test]
    fn observation_messages_are_user_role_not_turns() {
        let marker = marker::observation(marker::CANCEL, "[Cancelled by user]");
        assert!(is_user(marker.message()));
        assert_eq!(marker.message().kind, MessageKind::Observation);
        assert_eq!(message_text(&marker), "[Cancelled by user]");
    }

    #[test]
    fn entries_round_trip_through_stored_messages() {
        let messages = vec![stored_user_text("hello"), stored(assistant_text("hi"))];
        let wrapped = entries(messages);
        let back = stored_of_entries(&wrapped);
        assert_eq!(message_text(&back[0]), "hello");
        assert_eq!(message_text(&back[1]), "hi");
        assert!(back[0] != back[1]);
    }
}
