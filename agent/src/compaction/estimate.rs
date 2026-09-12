//! Rough token estimation for conversation history (ported from Craft's
//! compaction estimator). Deliberately approximate; the engine's
//! effectiveness score keeps behavior safe when estimates drift.

use crate::history::{AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent};

const CHARS_PER_TOKEN: usize = 4;

fn text_len(text: &str) -> usize {
    // Characters, not bytes: CHARS_PER_TOKEN is calibrated on characters and
    // byte length would overestimate multibyte text ~3-4x.
    text.chars().count()
}

/// Estimate the prompt tokens for `messages`, image blocks weighted at
/// `TOKENS_PER_IMAGE`.
pub fn estimate_tokens(messages: &[Message]) -> u64 {
    let mut tokens: u64 = 0;
    for message in messages {
        match message {
            Message::System { content } => tokens += (text_len(content) / CHARS_PER_TOKEN) as u64,
            Message::User { content } => {
                tokens +=
                    (content.iter().map(user_content_chars).sum::<usize>() / CHARS_PER_TOKEN) as u64
            }
            Message::Assistant { content } => {
                tokens += (content.iter().map(assistant_content_chars).sum::<usize>()
                    / CHARS_PER_TOKEN) as u64
            }
        }
    }
    tokens
}

fn user_content_chars(block: &UserContent) -> usize {
    match block {
        UserContent::Text(text) => text_len(&text.text),
        UserContent::ToolResult(result) => result
            .content
            .iter()
            .map(|item| match item {
                ToolResultContent::Text(text) => text_len(&text.text),
                ToolResultContent::Json { value } => json_len(value) as usize,
            })
            .sum(),
    }
}

fn assistant_content_chars(block: &AssistantContent) -> usize {
    match block {
        AssistantContent::Text(text) => text_len(&text.text),
        AssistantContent::ToolCall(call) => {
            text_len(&call.function.name) + json_len(&call.function.arguments) as usize
        }
        AssistantContent::Reasoning(reasoning) => reasoning
            .content
            .iter()
            .map(|item| match item {
                ReasoningContent::Text { text } => text_len(text),
                ReasoningContent::Opaque(data) => text_len(data),
            })
            .sum(),
    }
}

fn json_len(value: &serde_json::Value) -> u64 {
    value.to_string().len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::text(text)],
        }
    }

    #[test]
    fn estimates_by_chars_per_token() {
        // 400 chars / 4 = 100 tokens.
        let messages = vec![user(&"x".repeat(400))];
        assert_eq!(estimate_tokens(&messages), 100);
    }

    #[test]
    fn zero_for_empty_history() {
        assert_eq!(estimate_tokens(&[]), 0);
    }
}
