//! Crate-owned conversation history types.
//!
//! These are the domain types the whole crate speaks: the run driver appends
//! them, the TUI and ACP surfaces store them, and compaction rewrites them.
//! They are deliberately close to rig-core's message shapes so the provider
//! edge ([`crate::edge`]) can map between the two trivially, but no `rig`
//! type appears in a field. Everything is serializable for future JSONL
//! session persistence.

use serde::{Deserialize, Serialize};

/// One message in a conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    System { content: String },
    User { content: Vec<UserContent> },
    Assistant { content: Vec<AssistantContent> },
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: vec![UserContent::text(content)],
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![AssistantContent::text(content)],
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// Concatenated assistant text blocks, for surfaces that need the reply.
    pub fn text(&self) -> String {
        match self {
            Self::System { content } => content.clone(),
            Self::User { content } => content
                .iter()
                .filter_map(|block| match block {
                    UserContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Assistant { content } => content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::Text(text) => Some(text.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// A block of user-message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UserContent {
    Text(Text),
    ToolResult(ToolResult),
}

impl UserContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(Text { text: text.into() })
    }
}

/// A block of assistant-message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AssistantContent {
    Text(Text),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
}

impl AssistantContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(Text { text: text.into() })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Text {
    pub text: String,
}

/// A model-issued tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlation id: pairs the call with its [`ToolResult`].
    pub id: String,
    pub function: ToolFunction,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            function: ToolFunction {
                name: name.into(),
                arguments,
            },
        }
    }
}

/// The outcome of one tool invocation, sent back to the model as user content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the [`ToolCall`] this answers.
    pub call: String,
    /// Name of the invoked tool.
    pub name: String,
    pub content: Vec<ToolResultContent>,
    /// Whether the tool reported the call as failed.
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(call: impl Into<String>, name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            call: call.into(),
            name: name.into(),
            content: vec![ToolResultContent::Text(Text { text: text.into() })],
            is_error: false,
        }
    }
}

/// One block of tool-result content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolResultContent {
    Text(Text),
    Json { value: serde_json::Value },
}

impl ToolResultContent {
    /// Literal text content.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(Text { text: text.into() })
    }

    /// The model-visible text of this block.
    pub fn to_text(&self) -> String {
        match self {
            Self::Text(text) => text.text.clone(),
            Self::Json { value } => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
        }
    }
}

/// The model's reasoning ("thinking") attached to an assistant message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub content: Vec<ReasoningContent>,
}

/// One reasoning part. `Opaque` carries provider-opaque reasoning payloads
/// (encrypted/summary/redacted) that must be replayed verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReasoningContent {
    Text { text: String },
    Opaque(String),
}

/// Token usage reported by the provider for one model call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl Usage {
    /// Fold a later report into a running total.
    pub fn add(&mut self, other: Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_and_text_extraction() {
        assert_eq!(Message::system("sys").text(), "sys");
        assert_eq!(Message::user("hi").text(), "hi");
        let assistant = Message::Assistant {
            content: vec![
                AssistantContent::text("part one"),
                AssistantContent::text("part two"),
                AssistantContent::ToolCall(ToolCall::new("t1", "bash", serde_json::json!({}))),
            ],
        };
        assert_eq!(assistant.text(), "part one\npart two");
        assert!(!assistant.is_user());
        assert!(Message::user("x").is_user());
    }

    #[test]
    fn messages_serialize_to_json() {
        let history = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::Assistant {
                content: vec![AssistantContent::Reasoning(Reasoning {
                    content: vec![ReasoningContent::Text { text: "hmm".into() }],
                })],
            },
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult::text(
                    "t1", "bash", "ok",
                ))],
            },
        ];
        let json = serde_json::to_string(&history).unwrap();
        let back: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, history);
    }
}
