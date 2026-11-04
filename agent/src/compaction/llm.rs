//! LLM-powered compaction: one summarization call replaces the conversation
//! head; the recent tail is preserved verbatim. The head is `Remove`d and
//! the summary `Insert`ed before the first kept entry, and the original
//! entries stay in the snapshot as inactive sources.
//!
//! The summary prompt embeds the top relevance-scored topics, and an
//! overflow retry ladder prunes
//! the request (collapse tool results → drop the oldest round → strip tool
//! results by progressive ratio) before giving up and falling back to a
//! static summary. The ladder works on the summarizer's *request copy* and
//! never touches the plan.

use animist::history::{CompactionEdit, CompactionPlan, HistorySnapshot, StoredMessage};
use async_trait::async_trait;
use snafu::ResultExt;

use crate::history::{Message, Role, ext};
use crate::prompt::{COMPACTION_TARGETED_USER, COMPACTION_USER, PromptFormat};
use crate::provider::ModelHandle;

use super::strip::{
    collapse_tool_results, remove_orphaned_tool_results, strip_thinking,
    strip_tool_results_by_ratio, truncate_oldest_round,
};
use super::vcc::find_cut;
use crate::error::{Result, SummarizeSnafu};

/// Marker identifying an LLM compaction summary in history. Distinct from
/// the VCC summary preamble so each strategy recognizes only its own
/// summaries.
pub(crate) const LLM_SUMMARY_PREFIX: &str = "Summary of the conversation so far:";

/// Tag marking a summary this stage generated.
pub(crate) const COMPACTION_TAG: &str = "craft.compaction";
pub(crate) const COMPACTION_LLM: &str = "llm";

const TARGETED_TOPICS_COUNT: usize = 10;
const TARGETED_MIN_SCORE: f32 = 0.5;
const MAX_OVERFLOW_RETRIES: usize = 3;

/// `(message index, relevance score)` pairs, best first, from the semantic
/// module. Callers without scores pass `None` and the plain summary prompt
/// is used.
pub type RelevanceScores<'a> = &'a [(usize, f32)];

/// The active entries of a snapshot, as `(snapshot index, stored message)`
/// pairs — the coordinate system every plan edit indexes.
pub(crate) type Active<'a> = Vec<(usize, &'a StoredMessage)>;

pub(crate) fn active_entries(snapshot: &HistorySnapshot) -> Active<'_> {
    snapshot
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.current().map(|stored| (index, stored)))
        .collect()
}

/// LLM compaction of the snapshot: the head is summarized (via `model`, with
/// a static fallback if the call fails) and the tail is kept verbatim.
/// Entries from `summarize_end` (an index into `active`, the first protected
/// entry) are held out of the summary and kept as-is, so input no turn has
/// answered yet survives compaction verbatim.
/// Returns `None` when the stage declines (nothing compactable ahead of the
/// protected tail); errors from the summarizer never abort the plan — the
/// static fallback covers them.
pub(crate) async fn build_llm_plan(
    model: &ModelHandle,
    snapshot: &HistorySnapshot,
    summarize_end: usize,
    relevance_scores: Option<RelevanceScores<'_>>,
    prompt_format: PromptFormat,
) -> Option<CompactionPlan> {
    let active = active_entries(snapshot);
    if active.len() <= 2 {
        return None;
    }
    let summarize_end = summarize_end.min(active.len());
    // The boundary finder sees the whole active view; the protection then
    // clamps it.
    let tail_start = find_cut(&active_messages(&active))
        .map_or(0, |cut| cut.tail_start.min(active.len()))
        .min(summarize_end);
    if tail_start == 0 {
        return None;
    }
    let head: Vec<&StoredMessage> = active[..tail_start].iter().map(|(_, m)| *m).collect();

    let summary = match summarize(model, &head, relevance_scores, prompt_format).await {
        Ok(text) if !text.trim().is_empty() => text,
        _ => build_static_summary(&head),
    };

    // Compact-all cuts keep nothing: the summary appends at the snapshot
    // length instead of inserting before a kept entry.
    let before = active
        .get(tail_start)
        .map(|(index, _)| *index)
        .unwrap_or(snapshot.entries().len());
    let mut edits: Vec<CompactionEdit> = active[..tail_start]
        .iter()
        .map(|(index, _)| CompactionEdit::Remove { index: *index })
        .collect();
    edits.push(CompactionEdit::Insert {
        before,
        message: summary_message(summary),
    });
    Some(CompactionPlan::builder().edits(edits).build())
}

/// The active view as a flat message list, for the boundary finder.
fn active_messages(active: &[(usize, &StoredMessage)]) -> Vec<StoredMessage> {
    active.iter().map(|(_, stored)| (*stored).clone()).collect()
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
            return ext::user_text(prompt);
        }
    }
    ext::user_text(COMPACTION_USER.to_string())
}

/// Extract text from a history message for the static fallback summary.
fn message_text(stored: &StoredMessage) -> Option<String> {
    let message = stored.message();
    match message.role {
        Role::System => Some(ext::text(message)),
        Role::User => {
            let parts: Vec<_> = message
                .content
                .iter()
                .map(|block| match block {
                    crate::history::ContentBlock::Text { text } => text.clone(),
                    crate::history::ContentBlock::Image { .. } => "[image]".to_owned(),
                    crate::history::ContentBlock::ToolResult { content, .. } => {
                        format!("tool result: {}", ext::tool_content_text(content))
                    }
                    _ => String::new(),
                })
                .collect();
            Some(parts.join("\n"))
        }
        Role::Assistant => {
            let parts: Vec<_> = message
                .content
                .iter()
                .map(|block| match block {
                    crate::history::ContentBlock::Text { text } => text.clone(),
                    crate::history::ContentBlock::ToolUse { name, input, .. } => {
                        format!("tool call: {name} {input}")
                    }
                    _ => String::new(),
                })
                .collect();
            Some(parts.join("\n"))
        }
    }
}

/// Provider request-field names that signal an oversized *output* cap, not
/// an oversized prompt: an overflow retry ladder cannot fix those.
const OUTPUT_CAP_FIELDS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

/// The single shared context-overflow classifier: compaction's retry
/// ladder and the recovery layer's classification both consult it.
/// Detects context-window overflow in a provider error string across
/// providers:
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
        || m.contains("context_length_exceeded")
        // Generic provider phrasing with no explicit "exceeds": any
        // "context window" mention counts as overflow.
        || m.contains("context window");
    is_scope && is_overflow
}

/// Maps a provider error onto the overflow classification: a typed HTTP
/// body first, then whatever text the error carries.
fn is_context_overflow(error: &crate::error::Error) -> bool {
    let message = match error {
        crate::error::Error::ProviderClient { source } => source.to_string(),
        other => other.to_string(),
    };
    is_context_overflow_body(&message)
}

/// Summarize `head`, retrying overflow-failed requests on a progressively
/// pruned copy of it: first the tool-result tail is collapsed and the oldest
/// round dropped, then tool results are stripped by ratio, and only when the
/// ladder is exhausted does the caller's static fallback apply.
#[expect(
    clippy::result_large_err,
    reason = "the crate error type is deliberately unboxed"
)]
async fn summarize(
    model: &ModelHandle,
    head: &[&StoredMessage],
    relevance_scores: Option<RelevanceScores<'_>>,
    prompt_format: PromptFormat,
) -> Result<String> {
    let system = prompt_format.render(&crate::prompt::compaction());
    let mut request_messages: Vec<Message> =
        head.iter().map(|stored| stored.message().clone()).collect();
    strip_thinking(&mut request_messages);
    remove_orphaned_tool_results(&mut request_messages);
    collapse_tool_results(&mut request_messages, super::RECENT_TOOL_RESULT_BUDGET);
    request_messages.push(build_compaction_user_message(relevance_scores));

    let mut overflow_retries = 0;
    let mut removal_step = 0;

    loop {
        let response = match crate::provider::complete_text(model, &request_messages, &system).await
        {
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
        return Ok(response);
    }
}

fn summary_message(summary: String) -> StoredMessage {
    let mut stored = ext::stored(ext::assistant_text(format!(
        "{LLM_SUMMARY_PREFIX}\n\n{summary}"
    )));
    stored
        .tags_mut()
        .insert(COMPACTION_TAG.to_owned(), COMPACTION_LLM.to_owned());
    stored
}

/// Deterministic fallback when the summarization call fails: a terse digest
/// of user prompts and assistant text in the head.
pub(crate) fn build_static_summary(head: &[&StoredMessage]) -> String {
    let mut lines = Vec::new();
    for stored in head {
        let message = stored.message();
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => continue,
        };
        let Some(text) = message_text(stored) else {
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

/// Commits a precomputed plan through `History::compact`, so the commit runs
/// under the generation check like any other compaction. The plan's indices
/// name the snapshot they were computed from; the only writer that can
/// interleave between plan and commit is an append (the driver owns
/// between-runs sequencing), which never shifts existing indices.
pub(crate) struct PlanCompactor {
    plan: CompactionPlan,
}

impl PlanCompactor {
    pub(crate) fn new(plan: CompactionPlan) -> Self {
        Self { plan }
    }
}

#[async_trait]
impl animist::history::Compactor for PlanCompactor {
    async fn call(&self, _history: &HistorySnapshot) -> Result<CompactionPlan, animist::Error> {
        Ok(self.plan.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support::{
        stored_assistant_text, stored_assistant_tool, stored_tool_result_of, stored_user,
    };
    use crate::history::ext;

    const OVERFLOW: &str = "prompt is too long";

    fn user(text: &str) -> StoredMessage {
        stored_user(text)
    }

    fn history() -> Vec<StoredMessage> {
        let mut messages = Vec::new();
        for i in 0..6 {
            messages.push(user(&format!(
                "task {i}: please do the thing carefully {i}"
            )));
            messages.push(stored_assistant_text(&format!("working on task {i}")));
        }
        messages
    }

    /// History with tool rounds, so the overflow ladder has tool results to
    /// prune.
    fn tool_history() -> Vec<StoredMessage> {
        let mut messages = vec![user("request")];
        for i in 0..4 {
            messages.push(stored_assistant_tool(&format!("t{i}"), "bash"));
            messages.push(stored_tool_result_of(&format!("t{i}"), &"x".repeat(500)));
        }
        messages.push(user("prompt"));
        messages
    }

    /// A snapshot over seeded entries.
    async fn snapshot(messages: &[StoredMessage]) -> animist::history::HistorySnapshot {
        animist::history::History::from_entries(ext::entries(messages.to_vec()))
            .snapshot()
            .await
    }

    /// A model over the scripted replies, and its request recorder.
    fn script(
        turns: Vec<crate::testing::Attempt>,
    ) -> (
        crate::provider::ModelHandle,
        crate::testing::ScriptedProvider,
    ) {
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        for turn in turns {
            provider.push(turn);
        }
        (
            crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
                &provider,
                crate::testing::model_info("summary", 1_000_000, 8_192, false, false),
            )),
            provider,
        )
    }

    fn overflow_turn() -> crate::testing::Attempt {
        crate::testing::http_error(400, OVERFLOW)
    }

    /// Model-visible text of one user message (text blocks plus tool
    /// result text), for asserting what the summarizer was sent.
    fn message_text(message: &Message) -> String {
        if !crate::history::ext::is_user(message) {
            return crate::history::ext::text(message);
        }
        message
            .content
            .iter()
            .map(|block| match block {
                crate::history::ContentBlock::Text { text } => text.clone(),
                crate::history::ContentBlock::ToolResult { content, .. } => {
                    crate::history::ext::tool_content_text(content)
                }
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn request_texts(provider: &crate::testing::ScriptedProvider) -> Vec<String> {
        provider
            .requests()
            .iter()
            .map(|request| {
                request
                    .messages
                    .iter()
                    .map(message_text)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect()
    }

    #[tokio::test]
    async fn plan_removes_head_and_inserts_tagged_summary_before_kept_tail() {
        let (model, provider) = script(vec![crate::testing::text_turn("condensed summary")]);
        let messages = history();
        let snap = snapshot(&messages).await;
        let plan = build_llm_plan(&model, &snap, messages.len(), None, PromptFormat::Markdown)
            .await
            .expect("plan");
        let removes = plan
            .edits
            .iter()
            .filter(|edit| matches!(edit, CompactionEdit::Remove { .. }))
            .count();
        let insert = plan
            .edits
            .iter()
            .find_map(|edit| match edit {
                CompactionEdit::Insert { before, message } => Some((*before, message.clone())),
                _ => None,
            })
            .expect("one summary insert");
        // The cut keeps a tail: only the head is removed.
        assert!(removes > 0 && removes < messages.len());
        // The summary lands before the first kept entry, which the active
        // view's message at that snapshot index names.
        assert_eq!(messages[insert.0].message().role, Role::User);
        assert_eq!(
            ext::message_text(&insert.1),
            "Summary of the conversation so far:\n\ncondensed summary"
        );
        assert_eq!(
            insert.1.tags().get(COMPACTION_TAG).map(String::as_str),
            Some(COMPACTION_LLM)
        );
        assert_eq!(provider.requests().len(), 1);
        let request = &provider.requests()[0];
        // The preamble travels as the request's preamble field.
        assert_eq!(
            request.system,
            PromptFormat::Markdown.render(&crate::prompt::compaction()),
        );
        assert_eq!(
            request.messages.last().map(ext::text).as_deref(),
            Some(COMPACTION_USER)
        );
    }

    #[tokio::test]
    async fn carry_protection_clamps_the_summarized_head() {
        let (model, provider) = script(vec![crate::testing::text_turn("summary")]);
        let messages = history();
        let snap = snapshot(&messages).await;
        // Protect everything from message 10 on: the head is messages 0..10.
        let plan = build_llm_plan(&model, &snap, 10, None, PromptFormat::Markdown)
            .await
            .expect("plan");
        for edit in &plan.edits {
            match edit {
                CompactionEdit::Remove { index } => assert!(*index < 10),
                CompactionEdit::Insert { before, .. } => assert!(*before <= 10),
                CompactionEdit::Replace { .. } => panic!("the LLM stage never replaces"),
            }
        }
        // The summarizer never saw the carried messages.
        let last = request_texts(&provider)[0].clone();
        assert!(
            !last.contains("task 5"),
            "carried input must not be summarized"
        );
    }

    #[tokio::test]
    async fn fully_protected_history_declines() {
        let (model, provider) = script(vec![crate::testing::text_turn("never")]);
        let messages = history();
        let snap = snapshot(&messages).await;
        assert!(
            build_llm_plan(&model, &snap, 0, None, PromptFormat::Markdown)
                .await
                .is_none(),
            "nothing compactable ahead of the protection"
        );
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn short_history_declines() {
        let (model, _provider) = script(vec![crate::testing::text_turn("x")]);
        let messages = vec![user("hi"), user("there")];
        let snap = snapshot(&messages).await;
        assert!(
            build_llm_plan(&model, &snap, messages.len(), None, PromptFormat::Markdown)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn falls_back_to_static_summary_on_empty_response() {
        let (model, _provider) = script(vec![crate::testing::text_turn("")]);
        let messages = history();
        let snap = snapshot(&messages).await;
        let plan = build_llm_plan(&model, &snap, messages.len(), None, PromptFormat::Markdown)
            .await
            .expect("plan");
        let insert = plan.edits.iter().find_map(|edit| match edit {
            CompactionEdit::Insert { message, .. } => Some(message.clone()),
            _ => None,
        });
        assert!(ext::message_text(&insert.unwrap()).contains("task 0"));
    }

    #[test]
    fn static_summary_lists_roles() {
        let history = history();
        let stored: Vec<&StoredMessage> = history.iter().collect();
        let summary = build_static_summary(&stored);
        assert!(summary.contains("- user: task 0"));
        assert!(summary.contains("- assistant: working on task"));
    }

    #[tokio::test]
    async fn targeted_prompt_embeds_top_topics() {
        let (model, provider) = script(vec![crate::testing::text_turn("summary")]);
        let messages = history();
        let snap = snapshot(&messages).await;
        let scores: Vec<(usize, f32)> = (0..15).map(|i| (i, 0.9)).collect();
        build_llm_plan(
            &model,
            &snap,
            messages.len(),
            Some(&scores),
            PromptFormat::Markdown,
        )
        .await
        .expect("plan");
        let last = request_texts(&provider)[0].clone();
        assert!(last.contains("msg#0 (relevance: 0.90)"));
        assert!(last.contains("msg#9 (relevance: 0.90)"), "top ten kept");
        assert!(!last.contains("msg#10"), "beyond ten dropped");
        assert!(last.contains(COMPACTION_TARGETED_USER.lines().next().unwrap()));
    }

    #[tokio::test]
    async fn low_scores_fall_back_to_plain_prompt() {
        let (model, provider) = script(vec![crate::testing::text_turn("summary")]);
        let messages = history();
        let snap = snapshot(&messages).await;
        let scores = vec![(0usize, 0.4f32), (1, 0.5)];
        build_llm_plan(
            &model,
            &snap,
            messages.len(),
            Some(&scores),
            PromptFormat::Markdown,
        )
        .await
        .expect("plan");
        let last = request_texts(&provider)[0].clone();
        assert!(last.contains(COMPACTION_USER));
        assert!(!last.contains("relevance"));
    }

    #[tokio::test]
    async fn overflow_retries_collapse_then_drop_round() {
        let (model, provider) = script(vec![
            overflow_turn(),
            overflow_turn(),
            crate::testing::text_turn("made it"),
        ]);
        let messages = tool_history();
        let snap = snapshot(&messages).await;
        let plan = build_llm_plan(&model, &snap, messages.len(), None, PromptFormat::Markdown)
            .await
            .expect("plan");

        let requests = request_texts(&provider);
        assert_eq!(requests.len(), 3, "collapse, truncate, then success");
        // First attempt carries the real tool results.
        assert!(requests[0].contains("xxxx"));
        // Attempt 2 collapsed every result to the placeholder.
        assert!(requests[1].contains(crate::compaction::TOOL_RESULT_PLACEHOLDER));
        assert!(!requests[1].contains("xxxx"));
        // Attempt 3 dropped the oldest round entirely.
        assert!(!requests[2].contains("tool result: [tool result]"));
        assert!(
            ext::message_text(
                &plan
                    .edits
                    .iter()
                    .find_map(|edit| match edit {
                        CompactionEdit::Insert { message, .. } => Some(message.clone()),
                        _ => None,
                    })
                    .unwrap()
            )
            .contains("made it")
        );
    }

    #[tokio::test]
    async fn ladder_exhaustion_falls_back_to_static_summary() {
        let turns = std::iter::repeat_with(overflow_turn)
            .take(3 + super::super::PROGRESSIVE_TOOL_REMOVAL_RATIOS.len() + 1)
            .collect::<Vec<_>>();
        let (model, provider) = script(turns);
        let messages = tool_history();
        let snap = snapshot(&messages).await;
        let plan = build_llm_plan(&model, &snap, messages.len(), None, PromptFormat::Markdown)
            .await
            .expect("plan");
        // One collapse + one truncate (which drains the whole tool middle,
        // leaving a single message), then 4 ratio no-ops, then the failure.
        assert_eq!(provider.requests().len(), 7);
        let summary = plan
            .edits
            .iter()
            .find_map(|edit| match edit {
                CompactionEdit::Insert { message, .. } => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        assert!(
            ext::message_text(&summary).contains("user"),
            "static fallback summarizes the head"
        );
    }

    #[tokio::test]
    async fn non_overflow_error_falls_back_without_retry() {
        let (model, provider) = script(vec![crate::testing::http_error(429, "rate limited")]);
        let messages = tool_history();
        let snap = snapshot(&messages).await;
        let plan = build_llm_plan(&model, &snap, messages.len(), None, PromptFormat::Markdown)
            .await
            .expect("plan");
        assert_eq!(provider.requests().len(), 1, "only errors are not retried");
        let summary = plan
            .edits
            .iter()
            .find_map(|edit| match edit {
                CompactionEdit::Insert { message, .. } => Some(message.clone()),
                _ => None,
            })
            .unwrap();
        assert!(ext::message_text(&summary).contains("user"));
    }

    /// Provider overflow messages (Anthropic, OpenAI, Ollama, Mistral,
    /// Bedrock shapes).
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

    /// Table-driven test over the shared classifier, mined from both former
    /// implementations (the deleted pre-animist stream classifier and this
    /// file's provider table). Where the two disagreed, this superset's semantics
    /// win — notably output-cap complaints (`max_tokens`) are never prompt
    /// overflow, and a bare "maximum tokens per request" tier message is.
    #[test]
    fn shared_overflow_classifier_covers_both_call_sites() {
        let cases: &[(&str, bool)] = &[
            // compaction provider table (Anthropic / OpenAI / Ollama /
            // Mistral / Bedrock shapes)
            ("prompt is too long", true),
            (
                "This model's maximum context length is 8192 tokens. However, you requested 9850 tokens",
                true,
            ),
            ("context length exceeded", true),
            (
                "Prompt contains 321774 tokens and 0 draft tokens, too large for model with 262144 maximum context length",
                true,
            ),
            ("Input is too long for requested model", true),
            // former stream-classifier positives
            (
                "Error: prompt is too long: 210744 tokens > 200000 maximum",
                true,
            ),
            (
                "This model's maximum context length is 4096 tokens. However, you requested ...",
                true,
            ),
            (
                "The input token count (524389) exceeds the maximum number of tokens allowed (1048576)",
                true,
            ),
            ("conversation exceeds the model context window", true),
            // negatives from both sites
            ("rate limit exceeded, retry after 30s", false),
            ("invalid api key", false),
            ("connection closed before response", false),
            ("rate limited", false),
            // dis-ambiguators: output caps are not prompt overflow
            ("Invalid 'max_tokens': integer above maximum value", false),
            (
                "input length and `max_tokens` exceed context limit: 10922 + 8192 > 8192",
                false,
            ),
            // tier-cap phrasing reads as the overflow it is
            ("maximum tokens per request is 128000 for this tier", true),
        ];
        for (message, expected) in cases {
            assert_eq!(
                is_context_overflow_body(message),
                *expected,
                "classification mismatch: {message}"
            );
        }
    }

    #[test]
    fn overflow_classifier_reads_provider_error_bodies() {
        let overflow = crate::error::client_error(animist::provider::ProviderError::Http {
            status: 400,
            body: OVERFLOW.to_owned(),
            body_truncated: false,
            backtrace: snafu::GenerateImplicitData::generate(),
        });
        assert!(is_context_overflow(&overflow));
        let other = crate::error::client_error(animist::provider::ProviderError::Http {
            status: 400,
            body: "decode error".to_owned(),
            body_truncated: false,
            backtrace: snafu::GenerateImplicitData::generate(),
        });
        assert!(!is_context_overflow(&other));
    }
}
