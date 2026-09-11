//! LLM-powered compaction: one summarization call replaces the conversation
//! head; the recent tail is preserved verbatim (ported from Craft's
//! `compaction/llm.rs`, simplified to craft-acp's caller-owned history).

use rig::completion::message::{AssistantContent, Text, UserContent};
use rig::completion::{CompletionModel, Message};
use snafu::ResultExt;

use super::estimate::estimate_tokens;
use super::vcc::find_cut;
use crate::error::{Result, SummarizeSnafu};

/// Marker identifying an LLM compaction summary in history. Distinct from the
/// VCC summary prefix so each strategy recognizes only its own summaries.
pub(crate) const LLM_SUMMARY_PREFIX: &str = "Summary of the conversation so far:";

const COMPACT_SYSTEM: &str = "\
You are a summarizer for a coding agent's conversation history. Produce a \
dense handoff summary that lets the conversation continue without losing \
important context. Preserve:
- The user's original request and any restated goals or requirements
- Decisions made and their rationale
- Files examined or modified, and what was learned from each
- Tool calls and their key results, especially errors and fixes
- Anything still in progress or explicitly deferred
Omit pleasantries, redundant tool output, and dead-end exploration that led \
nowhere. Write in terse bullet points. Output only the summary.";

const COMPACT_USER_PROMPT: &str = "What did we do so far?";

/// LLM compaction of `history`. The head is summarized (via `model`, with a
/// static fallback if the call fails) and the tail is kept verbatim. Returns
/// whether the compacted history fits within `token_limit`.
pub async fn llm_compact<M: CompletionModel + Clone>(
    model: &M,
    history: &mut Vec<Message>,
    token_limit: u64,
) -> Result<bool> {
    if history.len() <= 2 {
        return Ok(false);
    }
    let live = history.clone();
    let tail_start = find_cut(&live).map_or(0, |cut| cut.tail_start.min(live.len()));
    let head = &live[..tail_start];
    if head.is_empty() {
        return Ok(false);
    }

    let summary = match summarize(model, head).await {
        Ok(text) if !text.trim().is_empty() => text,
        _ => build_static_summary(head),
    };

    let mut new_history = Vec::with_capacity(1 + (live.len() - tail_start));
    new_history.push(summary_message(summary));
    new_history.extend(live.into_iter().skip(tail_start));
    *history = new_history;
    Ok(estimate_tokens(history) <= token_limit)
}

/// Extract text from a history message for the summarization request.
fn message_text(message: &Message) -> Option<String> {
    match message {
        Message::System { content } => Some(content.clone()),
        Message::User { content } => {
            let parts: Vec<_> = content
                .iter()
                .map(|block| match block {
                    UserContent::Text(text) => text.text.clone(),
                    UserContent::ToolResult(result) => {
                        let output = result
                            .content
                            .iter()
                            .map(|item| match item {
                                rig::completion::message::ToolResultContent::Text(text) => {
                                    text.text.clone()
                                }
                                rig::completion::message::ToolResultContent::Image(_) => {
                                    "[image]".into()
                                }
                                rig::completion::message::ToolResultContent::Json { value } => {
                                    value.to_string()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("tool result: {output}")
                    }
                    _ => "[non-text content]".into(),
                })
                .collect();
            Some(parts.join("\n"))
        }
        Message::Assistant { content, .. } => {
            let parts: Vec<_> = content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(text) => text.text.clone(),
                    AssistantContent::ToolCall(call) => format!(
                        "tool call: {} {}",
                        call.function.name, call.function.arguments
                    ),
                    AssistantContent::Reasoning(_) => String::new(),
                    AssistantContent::Image(_) => "[image]".into(),
                })
                .collect();
            Some(parts.join("\n"))
        }
    }
}

async fn summarize<M: CompletionModel + Clone>(model: &M, head: &[Message]) -> Result<String> {
    let mut messages: Vec<Message> = Vec::with_capacity(head.len());
    for message in head {
        let Some(text) = message_text(message) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        messages.push(Message::User {
            content: vec![UserContent::Text(Text {
                text,
                additional_params: None,
            })],
        });
    }
    if messages.is_empty() {
        return Ok(String::new());
    }
    let response = model
        .completion_request(COMPACT_USER_PROMPT)
        .preamble(COMPACT_SYSTEM.to_string())
        .messages(messages)
        .send()
        .await
        .context(SummarizeSnafu)?;
    let text: String = response
        .choice
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(text)
}

fn summary_message(summary: String) -> Message {
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::Text(Text {
            text: format!("{LLM_SUMMARY_PREFIX}\n\n{summary}"),
            additional_params: None,
        })],
    }
}

/// Deterministic fallback when the summarization call fails: a terse digest
/// of user prompts and assistant text in the head.
pub(crate) fn build_static_summary(head: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in head {
        let role = match message {
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
            Message::System { .. } => continue,
        };
        let Some(text) = message_text(message) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let preview: String = trimmed.chars().take(200).collect();
        lines.push(format!("- {role}: {preview}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::test_utils::{MockCompletionModel, MockTurn};

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::Text(Text {
                text: text.into(),
                additional_params: None,
            })],
        }
    }

    fn history() -> Vec<Message> {
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(user(&format!(
                "task {i}: please do the thing carefully {i}"
            )));
            messages.push(Message::Assistant {
                id: None,
                content: vec![AssistantContent::Text(Text {
                    text: format!("working on task {i}"),
                    additional_params: None,
                })],
            });
        }
        messages
    }

    #[tokio::test]
    async fn summarizes_head_and_keeps_tail() {
        let model = MockCompletionModel::new([MockTurn::text("condensed summary")]);
        let mut messages = history();
        let under = llm_compact(&model, &mut messages, u64::MAX).await.unwrap();
        assert!(under);
        assert!(matches!(&messages[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains(LLM_SUMMARY_PREFIX)
                    && t.text.contains("condensed summary"))));
        assert!(messages.len() > 1, "tail must be preserved");
        assert_eq!(model.requests().len(), 1);
        let request = &model.requests()[0];
        // Rig renders the preamble as the leading system message.
        assert!(
            request
                .chat_history
                .iter()
                .any(|m| matches!(m, rig::completion::Message::System { content } if content == COMPACT_SYSTEM))
        );
    }

    #[tokio::test]
    async fn falls_back_to_static_summary_on_empty_response() {
        let model = MockCompletionModel::new([MockTurn::text("")]);
        let mut messages = history();
        let under = llm_compact(&model, &mut messages, u64::MAX).await.unwrap();
        assert!(under);
        assert!(matches!(&messages[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains("task 0"))));
    }

    #[test]
    fn static_summary_lists_roles() {
        let summary = build_static_summary(&history());
        assert!(summary.contains("- user: task 0"));
        assert!(summary.contains("- assistant: working on task"));
    }
}
