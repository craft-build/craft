//! LLM-powered compaction: one summarization call replaces the conversation
//! head; the recent tail is preserved verbatim (ported from Craft's
//! `compaction/llm.rs`, simplified to craft-acp's caller-owned history).
//!
//! D.8 adds the two Craft refinements: a targeted summary prompt embedding
//! the top relevance-scored topics, and an overflow retry ladder that prunes
//! the request (collapse tool results → drop the oldest round → strip tool
//! results by progressive ratio) before giving up and falling back to a
//! static summary.

use rig_core::completion::{CompletionError, CompletionModel};
use snafu::ResultExt;

use crate::edge;
use crate::history::{AssistantContent, Message, ToolResultContent, UserContent};
use crate::prompt::{COMPACTION_SYSTEM, COMPACTION_TARGETED_USER, COMPACTION_USER};

use super::estimate::estimate_tokens;
use super::strip::{
    collapse_tool_results, remove_orphaned_tool_results, strip_thinking,
    strip_tool_results_by_ratio, truncate_oldest_round,
};
use super::vcc::find_cut;
use crate::error::{Result, SummarizeSnafu};

/// Marker identifying an LLM compaction summary in history. Distinct from the
/// VCC summary prefix so each strategy recognizes only its own summaries.
pub(crate) const LLM_SUMMARY_PREFIX: &str = "Summary of the conversation so far:";

const TARGETED_TOPICS_COUNT: usize = 10;
const TARGETED_MIN_SCORE: f32 = 0.5;
const MAX_OVERFLOW_RETRIES: usize = 3;

/// `(message index, relevance score)` pairs, best first, from the semantic
/// module. Until that module is ported every caller passes `None` and the
/// plain summary prompt is used.
pub type RelevanceScores<'a> = &'a [(usize, f32)];

/// LLM compaction of `history`. The head is summarized (via `model`, with a
/// static fallback if the call fails) and the tail is kept verbatim. The last
/// `carry_len` messages are held out of the summary and re-appended after it,
/// so input no turn has answered yet survives compaction verbatim (Craft's
/// carry-protection). Returns whether the compacted history fits within
/// `token_limit`.
pub async fn llm_compact<M: CompletionModel + Clone>(
    model: &M,
    history: &mut Vec<Message>,
    token_limit: u64,
    carry_len: usize,
    relevance_scores: Option<RelevanceScores<'_>>,
) -> Result<bool> {
    if history.len() <= 2 {
        return Ok(false);
    }
    let live = history.clone();
    // Summarization stops where the protected (unanswered) input begins.
    let summarize_end = live.len().saturating_sub(carry_len);
    let tail_start = find_cut(&live)
        .map_or(0, |cut| cut.tail_start.min(live.len()))
        .min(summarize_end);
    let head = &live[..tail_start];
    if head.is_empty() {
        return Ok(false);
    }

    let summary = match summarize(model, head, relevance_scores).await {
        Ok(text) if !text.trim().is_empty() => text,
        _ => build_static_summary(head),
    };

    let mut new_history = Vec::with_capacity(1 + (live.len() - tail_start));
    new_history.push(summary_message(summary));
    new_history.extend(live.into_iter().skip(tail_start));
    *history = new_history;
    Ok(estimate_tokens(history) <= token_limit)
}

/// The targeted prompt names the messages the semantic module scored most
/// relevant; with nothing above the bar the plain prompt is used.
fn build_compaction_user_message(relevance_scores: Option<RelevanceScores<'_>>) -> Message {
    if let Some(scores) = relevance_scores {
        let top_topics: Vec<String> = scores
            .iter()
            .take(TARGETED_TOPICS_COUNT)
            .filter(|(_, score)| *score > TARGETED_MIN_SCORE)
            .map(|(idx, score)| format!("msg#{idx} (relevance: {score:.2})"))
            .collect();
        if !top_topics.is_empty() {
            let prompt = COMPACTION_TARGETED_USER
                .replace("{topics}", &top_topics.join(", "))
                .replace("{intent_summary}", "see most recent messages");
            return Message::user(prompt);
        }
    }
    Message::user(COMPACTION_USER.to_string())
}

/// Extract text from a history message for the static fallback summary.
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
                            .map(ToolResultContent::to_text)
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("tool result: {output}")
                    }
                })
                .collect();
            Some(parts.join("\n"))
        }
        Message::Assistant { content } => {
            let parts: Vec<_> = content
                .iter()
                .map(|block| match block {
                    AssistantContent::Text(text) => text.text.clone(),
                    AssistantContent::ToolCall(call) => format!(
                        "tool call: {} {}",
                        call.function.name, call.function.arguments
                    ),
                    AssistantContent::Reasoning(_) => String::new(),
                })
                .collect();
            Some(parts.join("\n"))
        }
    }
}

/// Provider request-field names that signal an oversized *output* cap, not an
/// oversized prompt: an overflow retry ladder cannot fix those.
const OUTPUT_CAP_FIELDS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

/// Detects context-window overflow in a provider error string across
/// providers (ported from Craft's `craft-providers::error`):
/// - Anthropic:  413 "prompt is too long"
/// - OpenAI:     400 "maximum context length is X tokens"
/// - Gemini:     400 "input token count exceeds" / "too many tokens"
/// - Ollama:     400 "context length exceeded"
/// - llama.cpp:  400 "exceeds the available context size"
/// - Bedrock:    400 "Input is too long for requested model"
/// - DeepSeek / Mistral / OpenRouter: "maximum context length" variants
pub(crate) fn is_context_overflow_body(message: &str) -> bool {
    let m = message.to_lowercase();
    if OUTPUT_CAP_FIELDS.iter().any(|field| m.contains(field)) {
        return false;
    }
    let is_scope =
        m.contains("context") || m.contains("token") || m.contains("prompt") || m.contains("input");
    let is_overflow = m.contains("exceeds")
        || m.contains("exceeded")
        || m.contains("too long")
        || m.contains("too many")
        || m.contains("maximum")
        || m.contains("context_length_exceeded");
    is_scope && is_overflow
}

/// Maps a rig completion error onto the overflow classification. Rig
/// flattens provider failures into string payloads, so the sniff runs on
/// whatever text the variant carries.
fn is_context_overflow(error: &CompletionError) -> bool {
    match error {
        CompletionError::ProviderError(message) | CompletionError::ResponseError(message) => {
            is_context_overflow_body(message)
        }
        CompletionError::RequestError(err) => is_context_overflow_body(&err.to_string()),
        CompletionError::ProviderResponse(err) => is_context_overflow_body(&err.to_string()),
        _ => false,
    }
}

/// Summarize `head`, retrying overflow-failed requests on a progressively
/// pruned copy of it: first the tool-result tail is collapsed and the oldest
/// round dropped, then tool results are stripped by ratio, and only when the
/// ladder is exhausted does the caller's static fallback apply.
async fn summarize<M: CompletionModel + Clone>(
    model: &M,
    head: &[Message],
    relevance_scores: Option<RelevanceScores<'_>>,
) -> Result<String> {
    let mut request_messages = head.to_vec();
    strip_thinking(&mut request_messages);
    remove_orphaned_tool_results(&mut request_messages);
    collapse_tool_results(&mut request_messages, super::RECENT_TOOL_RESULT_BUDGET);
    request_messages.push(build_compaction_user_message(relevance_scores));

    let mut overflow_retries = 0;
    let mut removal_step = 0;

    loop {
        let request = edge::to_request(&request_messages, &[], Some(COMPACTION_SYSTEM), None, None);
        let response = match model.completion(request).await {
            Ok(response) => response,
            Err(e) if is_context_overflow(&e) => {
                // Truncation eats from the front, so it can never shrink an
                // oversized result sitting in the tail. Collapse that first,
                // and once there is nothing left to collapse, drop the
                // oldest round rather than resend the same request.
                if overflow_retries < MAX_OVERFLOW_RETRIES && request_messages.len() > 1 {
                    overflow_retries += 1;
                    if !collapse_tool_results(&mut request_messages, 0) {
                        truncate_oldest_round(&mut request_messages);
                    }
                    continue;
                }
                if removal_step < super::PROGRESSIVE_TOOL_REMOVAL_RATIOS.len() {
                    let ratio = super::PROGRESSIVE_TOOL_REMOVAL_RATIOS[removal_step];
                    removal_step += 1;
                    strip_tool_results_by_ratio(&mut request_messages, ratio);
                    continue;
                }
                return Err(e).context(SummarizeSnafu);
            }
            Err(e) => return Err(e).context(SummarizeSnafu),
        };
        let text: String = response
            .choice
            .iter()
            .filter_map(|block| match block {
                rig_core::completion::message::AssistantContent::Text(text) => {
                    Some(text.text.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(text);
    }
}

fn summary_message(summary: String) -> Message {
    Message::Assistant {
        content: vec![AssistantContent::text(format!(
            "{LLM_SUMMARY_PREFIX}\n\n{summary}"
        ))],
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
    use crate::compaction::test_support::{assistant_tool, tool_result_of};
    use rig_core::completion::message::{
        Message as RigMessage, ToolResultContent as RigToolResultContent,
        UserContent as RigUserContent,
    };
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    const OVERFLOW: &str = "prompt is too long";

    fn user(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::text(text)],
        }
    }

    fn history() -> Vec<Message> {
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(user(&format!(
                "task {i}: please do the thing carefully {i}"
            )));
            messages.push(Message::assistant(format!("working on task {i}")));
        }
        messages
    }

    /// History with tool rounds, so the overflow ladder has tool results to
    /// prune.
    fn tool_history() -> Vec<Message> {
        let mut messages = vec![user("request")];
        for i in 0..4 {
            messages.push(assistant_tool(&format!("t{i}"), "bash"));
            messages.push(tool_result_of(&format!("t{i}"), &"x".repeat(500)));
        }
        messages.push(user("prompt"));
        messages
    }

    /// Model-visible text of one rig user message (text blocks plus tool
    /// result text), for asserting what the summarizer was sent.
    fn rig_message_text(message: &RigMessage) -> String {
        match message {
            RigMessage::User { content } => content
                .iter()
                .map(|block| match block {
                    RigUserContent::Text(text) => text.text.clone(),
                    RigUserContent::ToolResult(result) => result
                        .content
                        .iter()
                        .map(|item| match item {
                            RigToolResultContent::Text(text) => text.text.clone(),
                            _ => String::new(),
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        }
    }

    fn request_texts(model: &MockCompletionModel) -> Vec<String> {
        model
            .requests()
            .iter()
            .map(|request| {
                request
                    .chat_history
                    .iter()
                    .map(rig_message_text)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect()
    }

    #[tokio::test]
    async fn summarizes_head_and_keeps_tail() {
        let model = MockCompletionModel::new([MockTurn::text("condensed summary")]);
        let mut messages = history();
        let under = llm_compact(&model, &mut messages, u64::MAX, 0, None)
            .await
            .unwrap();
        assert!(under);
        assert!(matches!(&messages[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains(LLM_SUMMARY_PREFIX)
                    && t.text.contains("condensed summary"))));
        assert!(messages.len() > 1, "tail must be preserved");
        assert_eq!(model.requests().len(), 1);
        let request = &model.requests()[0];
        // The preamble travels as the request's preamble field.
        assert_eq!(request.preamble.as_deref(), Some(COMPACTION_SYSTEM));
        assert!(matches!(
            request.chat_history.last(),
            Some(rig_core::completion::message::Message::User { content })
                if matches!(&content[0], rig_core::completion::message::UserContent::Text(t)
                    if t.text == COMPACTION_USER)
        ));
    }

    #[tokio::test]
    async fn falls_back_to_static_summary_on_empty_response() {
        let model = MockCompletionModel::new([MockTurn::text("")]);
        let mut messages = history();
        let under = llm_compact(&model, &mut messages, u64::MAX, 0, None)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn targeted_prompt_embeds_top_topics() {
        let model = MockCompletionModel::new([MockTurn::text("summary")]);
        let mut messages = history();
        let scores: Vec<(usize, f32)> = (0..15).map(|i| (i, 0.9)).collect();
        llm_compact(&model, &mut messages, u64::MAX, 0, Some(&scores))
            .await
            .unwrap();
        let last = request_texts(&model)[0].clone();
        assert!(last.contains("msg#0 (relevance: 0.90)"));
        assert!(last.contains("msg#9 (relevance: 0.90)"), "top ten kept");
        assert!(!last.contains("msg#10"), "beyond ten dropped");
        assert!(last.contains(COMPACTION_TARGETED_USER.lines().next().unwrap()));
    }

    #[tokio::test]
    async fn low_scores_fall_back_to_plain_prompt() {
        let model = MockCompletionModel::new([MockTurn::text("summary")]);
        let mut messages = history();
        let scores = vec![(0usize, 0.4f32), (1, 0.5)];
        llm_compact(&model, &mut messages, u64::MAX, 0, Some(&scores))
            .await
            .unwrap();
        let last = request_texts(&model)[0].clone();
        assert!(last.contains(COMPACTION_USER));
        assert!(!last.contains("relevance"));
    }

    #[tokio::test]
    async fn overflow_retries_collapse_then_drop_round() {
        let model = MockCompletionModel::new([
            MockTurn::error(OVERFLOW),
            MockTurn::error(OVERFLOW),
            MockTurn::text("made it"),
        ]);
        let mut messages = tool_history();
        llm_compact(&model, &mut messages, u64::MAX, 0, None)
            .await
            .unwrap();

        let requests = request_texts(&model);
        assert_eq!(requests.len(), 3, "collapse, truncate, then success");
        // First attempt carries the real tool results.
        assert!(requests[0].contains("xxxx"));
        // Attempt 2 collapsed every result to the placeholder.
        assert!(requests[1].contains(crate::compaction::TOOL_RESULT_PLACEHOLDER));
        assert!(!requests[1].contains("xxxx"));
        // Attempt 3 dropped the oldest round entirely.
        assert!(!requests[2].contains("tool result: [tool result]"));
        assert!(messages[0].text().contains("made it"));
    }

    #[tokio::test]
    async fn ladder_exhaustion_falls_back_to_static_summary() {
        let turns = std::iter::repeat_with(|| MockTurn::error(OVERFLOW))
            .take(3 + super::super::PROGRESSIVE_TOOL_REMOVAL_RATIOS.len() + 1)
            .collect::<Vec<_>>();
        let model = MockCompletionModel::new(turns);
        let mut messages = tool_history();
        llm_compact(&model, &mut messages, u64::MAX, 0, None)
            .await
            .unwrap();
        // One collapse + one truncate (which drains the whole tool middle,
        // leaving a single message), then 4 ratio no-ops, then the failure.
        assert_eq!(model.requests().len(), 7);
        assert!(
            matches!(&messages[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains("user"))),
            "static fallback summarizes the head"
        );
    }

    #[tokio::test]
    async fn non_overflow_error_falls_back_without_retry() {
        let model = MockCompletionModel::new([MockTurn::error("rate limited")]);
        let mut messages = tool_history();
        llm_compact(&model, &mut messages, u64::MAX, 0, None)
            .await
            .unwrap();
        assert_eq!(model.requests().len(), 1, "only errors are not retried");
        assert!(matches!(&messages[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains("user"))));
    }

    /// Provider overflow messages ported verbatim from Craft's error-classifier
    /// test table (Anthropic, OpenAI, Ollama, Mistral, Bedrock shapes).
    #[test]
    fn overflow_classifier_matches_provider_messages() {
        for message in [
            "prompt is too long",
            "This model's maximum context length is 8192 tokens. However, you requested 9850 tokens",
            "context length exceeded",
            "Prompt contains 321774 tokens and 0 draft tokens, too large for model with 262144 maximum context length",
            "Input is too long for requested model",
        ] {
            assert!(is_context_overflow_body(message), "{message:?}");
        }
    }

    #[test]
    fn overflow_classifier_rejects_other_messages() {
        // An oversized output cap must not read as prompt overflow: no amount
        // of summarization fixes a cap guessed too high.
        for message in [
            "Invalid 'max_tokens': integer above maximum value",
            "rate limited",
        ] {
            assert!(!is_context_overflow_body(message), "{message:?}");
        }
    }

    #[test]
    fn overflow_classifier_reads_rig_error_variants() {
        assert!(is_context_overflow(&CompletionError::ProviderError(
            OVERFLOW.into()
        )));
        assert!(!is_context_overflow(&CompletionError::JsonError(
            serde_json::from_str::<()>("x").unwrap_err()
        )));
    }
}
