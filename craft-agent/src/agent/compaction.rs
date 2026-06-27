use std::collections::{HashMap, HashSet};
use std::env;

use craft_providers::{ContentBlock, Message, Model, RequestOptions, Role, TokenUsage};
use tracing::info;

use super::history::History;
use super::streaming::stream_with_retry;
use crate::cancel::CancelToken;
use crate::compression::{self, CompressionConfig as AgentCompressionConfig};
use crate::{AgentError, AgentEvent, EventSender, TurnCompleteEvent};

pub(super) const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If you learned important project context during this session, consider saving it to memory before it's lost.";
const IMAGE_PLACEHOLDER: &str = "[image]";
const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";
const KEEP_LAST_TOOL_RESULTS: usize = 3;
const SUMMARY_MARKER_PREFIX: &str = "[Summary: ";
/// Fraction of remaining tool responses to drop (oldest first) on each LLM
/// compaction overflow retry, after round truncation is exhausted.
const PROGRESSIVE_TOOL_REMOVAL_RATIOS: &[f32] = &[0.10, 0.20, 0.50, 1.00];

const AGGRESSIVE_CODE_RATE: f32 = 0.15;
const AGGRESSIVE_MAX_LOG_LINES: usize = 20;
const AGGRESSIVE_MAX_DIFF_LINES: usize = 40;
const AGGRESSIVE_MAX_SEARCH_FILES: usize = 10;
const AGGRESSIVE_MAX_JSON_ITEMS: usize = 8;
const MIN_TOOL_RESULT_CHARS: usize = 300;
const LOW_RELEVANCE_THRESHOLD: f32 = 0.3;
const HIGH_RELEVANCE_THRESHOLD: f32 = 0.7;
#[cfg(feature = "onnx")]
const TARGETED_TOPICS_COUNT: usize = 10;
#[cfg(feature = "onnx")]
const TARGETED_MIN_SCORE: f32 = 0.5;
const VERY_OLD_MULTIPLIER: usize = 3;
const SUMMARY_PREVIEW_CHARS: usize = 80;
const SUFFICIENT_REDUCTION_RATIO: f32 = 0.15;
const ERROR_SNIPPET_CHARS: usize = 200;
const COMPACT_USER_PROMPT: &str = "What did we do so far?";

fn build_compaction_user_message(relevance_scores: Option<&[(usize, f32)]>) -> Message {
    #[cfg(feature = "onnx")]
    if let Some(scores) = relevance_scores {
        let top_topics: Vec<String> = scores
            .iter()
            .take(TARGETED_TOPICS_COUNT)
            .filter(|(_, score)| *score > TARGETED_MIN_SCORE)
            .map(|(idx, score)| format!("msg#{idx} (relevance: {score:.2})"))
            .collect();
        if !top_topics.is_empty() {
            let topics_str = top_topics.join(", ");
            let prompt = crate::prompt::COMPACTION_TARGETED_USER
                .replace("{topics}", &topics_str)
                .replace("{intent_summary}", "see most recent messages");
            return Message::user(prompt);
        }
    }
    #[allow(unused_variables)]
    let _ = relevance_scores;
    Message::user(COMPACT_USER_PROMPT.to_string())
}

fn aggressive_config() -> AgentCompressionConfig {
    AgentCompressionConfig {
        enabled: true,
        code_compression_rate: AGGRESSIVE_CODE_RATE,
        max_log_lines: AGGRESSIVE_MAX_LOG_LINES,
        max_search_files: AGGRESSIVE_MAX_SEARCH_FILES,
        max_matches_per_file: 3,
        max_diff_lines: AGGRESSIVE_MAX_DIFF_LINES,
        max_json_items: AGGRESSIVE_MAX_JSON_ITEMS,
        json_first_keep: 2,
        json_last_keep: 2,
        protect_recent_tool_outputs: 0,
        semantic_enabled: false,
    }
}

pub(super) async fn compact_history(
    provider: &dyn craft_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    cancel: &CancelToken,
    relevance_scores: Option<&[(usize, f32)]>,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();

    #[cfg(feature = "onnx")]
    let lifecycle_removed = super::read_lifecycle::run_lifecycle(history, None, None).await;
    #[cfg(not(feature = "onnx"))]
    let lifecycle_removed = super::read_lifecycle::run_lifecycle(history, None).await;
    if lifecycle_removed > 0 {
        info!(
            chars_removed = lifecycle_removed,
            "read lifecycle applied before compaction"
        );
    }

    let mut compaction_history: Vec<Message> = history.as_slice().to_vec();
    strip_images(&mut compaction_history);
    strip_thinking(&mut compaction_history);
    strip_old_tool_results(&mut compaction_history);
    compaction_history.push(build_compaction_user_message(relevance_scores));

    let empty_tools = serde_json::json!([]);
    const MAX_OVERFLOW_RETRIES: usize = 3;
    let mut overflow_retries = 0;
    let mut removal_step = 0;

    let response = loop {
        match stream_with_retry(
            provider,
            model,
            &compaction_history,
            crate::prompt::COMPACTION_SYSTEM,
            &empty_tools,
            event_tx,
            cancel,
            RequestOptions::default(),
            None,
        )
        .await
        {
            Ok(r) => break r,
            Err(e) if e.is_overflow() => {
                if overflow_retries < MAX_OVERFLOW_RETRIES && compaction_history.len() > 1 {
                    overflow_retries += 1;
                    truncate_oldest_round(&mut compaction_history);
                    info!(
                        attempt = overflow_retries,
                        "truncated oldest round for compaction overflow"
                    );
                    continue;
                }
                if removal_step < PROGRESSIVE_TOOL_REMOVAL_RATIOS.len() {
                    let ratio = PROGRESSIVE_TOOL_REMOVAL_RATIOS[removal_step];
                    removal_step += 1;
                    let dropped = strip_tool_results_by_ratio(&mut compaction_history, ratio);
                    info!(
                        removal_pct = format!("{:.0}%", ratio * 100.0),
                        dropped, "progressively removed tool responses for compaction overflow"
                    );
                    continue;
                }
                info!(error = %e, "LLM compaction failed, using static fallback");
                return Ok(static_fallback(history));
            }
            Err(e) => {
                info!(error = %e, "LLM compaction failed, using static fallback");
                return Ok(static_fallback(history));
            }
        }
    };

    event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        context_size: Some(response.usage.context_tokens()),
    })))?;

    let new_history = vec![Message::user(COMPACT_USER_PROMPT.into()), response.message];
    history.replace(new_history);
    info!(
        model = %model.id,
        duration_ms = compact_start.elapsed().as_millis() as u64,
        "compaction completed"
    );

    Ok(response.usage)
}

pub async fn compact(
    provider: &dyn craft_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
) -> Result<(), AgentError> {
    let cancel = CancelToken::none();
    let usage = compact_history(provider, model, history, event_tx, &cancel, None).await?;

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        stop_reason: None,
    })?;

    Ok(())
}

pub(super) struct CompactContext<'a> {
    pub usage: &'a TokenUsage,
    pub model: &'a Model,
    pub compaction_buffer: u32,
    pub cache_tracker: Option<&'a super::cache::PrefixCacheTracker>,
    pub compression_store: Option<&'a super::compression_store::SharedCompressionStore>,
    pub relevance_scores: Option<&'a [(usize, f32)]>,
    #[cfg(feature = "onnx")]
    pub scorer: Option<&'a super::semantic::RelevanceScorer>,
}

/// Attempt progressive compaction: compress old tool outputs in-place without
/// LLM summarization. Returns total characters removed.
///
/// Passes:
/// 1. Read lifecycle — replace stale/superseded reads
/// 2. Compress old tool results — aggressive compression on results past `protect_recent`
/// 3. Summarize very old tool results — replace with compact markers
pub(super) async fn progressive_compact(
    history: &mut History,
    protect_recent: usize,
    ctx: &CompactContext<'_>,
) -> usize {
    let total_before: usize = history
        .as_slice()
        .iter()
        .flat_map(|m| {
            m.content.iter().map(|b| match b {
                ContentBlock::Text { text } | ContentBlock::ToolResult { content: text, .. } => {
                    text.len()
                }
                _ => 0,
            })
        })
        .sum();

    // Pass 1: read lifecycle
    #[cfg(feature = "onnx")]
    let mut removed =
        super::read_lifecycle::run_lifecycle(history, ctx.scorer, ctx.compression_store).await;
    #[cfg(not(feature = "onnx"))]
    let mut removed = super::read_lifecycle::run_lifecycle(history, ctx.compression_store).await;

    let tool_result_indices: Vec<usize> = history
        .as_slice()
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
        .map(|(i, _)| i)
        .collect();

    let recent_cutoff = tool_result_indices.len().saturating_sub(protect_recent);
    let recent_msg_indices: HashSet<usize> = tool_result_indices
        .into_iter()
        .skip(recent_cutoff)
        .collect();

    // Semantic overlap detection — find old tool results that semantically
    // duplicate a newer result and mark them for aggressive compression.
    #[cfg(feature = "onnx")]
    let overlap_indices: HashSet<usize> = {
        let mut set = HashSet::new();
        if let Some(scorer) = ctx.scorer {
            let messages = history.as_slice();
            let mut old_tool_embeddings: Vec<(usize, Vec<f32>)> = Vec::new();
            for (i, msg) in messages.iter().enumerate() {
                if recent_msg_indices.contains(&i) {
                    continue;
                }
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        content,
                        is_error: false,
                        ..
                    } = block
                        && content.len() >= MIN_TOOL_RESULT_CHARS
                    {
                        if let Ok(emb) = scorer.embed_text(content).await {
                            old_tool_embeddings.push((i, emb));
                        }
                        break;
                    }
                }
            }
            for (older, _newer, _sim) in
                super::semantic::detect_semantic_overlap(&old_tool_embeddings)
            {
                set.insert(older);
            }
            if !set.is_empty() {
                info!(
                    overlapping = set.len(),
                    "semantic overlap detected in old tool results"
                );
            }
        }
        set
    };
    #[cfg(not(feature = "onnx"))]
    let overlap_indices: HashSet<usize> = HashSet::new();

    let aggressive = aggressive_config();

    // Build score lookup map for O(1) access
    let score_map: HashMap<usize, f32> = ctx
        .relevance_scores
        .map(|scores| scores.iter().map(|(idx, score)| (*idx, *score)).collect())
        .unwrap_or_default();

    // Pass 2 + 3: compress old tool results
    let messages = history.as_mut_slice();
    let msg_count = messages.len();
    let very_old_threshold = protect_recent * VERY_OLD_MULTIPLIER;

    for (i, msg) in messages.iter_mut().enumerate() {
        if recent_msg_indices.contains(&i) {
            continue;
        }

        let is_frozen = ctx.cache_tracker.is_some_and(|t| t.is_frozen(i));

        for block in &mut msg.content {
            if let ContentBlock::ToolResult {
                content,
                is_error: false,
                ..
            } = block
                && content.len() >= MIN_TOOL_RESULT_CHARS
            {
                let score = score_map.get(&i).copied().unwrap_or(0.0);

                if !score_map.is_empty() && score >= HIGH_RELEVANCE_THRESHOLD {
                    continue;
                }
                let old_len = content.len();
                let old_lines = content.lines().count();
                let mut is_very_old = if !score_map.is_empty() {
                    score <= LOW_RELEVANCE_THRESHOLD
                } else {
                    msg_count.saturating_sub(i) > very_old_threshold
                };
                if overlap_indices.contains(&i) {
                    is_very_old = true;
                }

                let new_content = if is_very_old {
                    let hash = ctx.compression_store.and_then(|store| {
                        let mut guard = store.lock().ok()?;
                        Some(guard.put(content))
                    });
                    let line_count = old_lines;
                    let first_line = content.lines().next().unwrap_or("");
                    let preview: String = first_line.chars().take(SUMMARY_PREVIEW_CHARS).collect();
                    let mut summary =
                        format!("{SUMMARY_MARKER_PREFIX}{line_count} lines. First: {preview}]");
                    if let Some(ref h) = hash {
                        summary
                            .push_str(&super::compression_store::retrieval_marker(old_lines, 1, h));
                    }
                    summary
                } else {
                    let ct = compression::detect_content_type(content);
                    let compressed = compression::compress(content, ct, &aggressive);
                    if compressed.len() < old_len {
                        let hash = ctx.compression_store.and_then(|store| {
                            let mut guard = store.lock().ok()?;
                            Some(guard.put(content))
                        });
                        let mut final_content = compressed;
                        if let Some(ref h) = hash {
                            let compressed_lines = final_content.lines().count();
                            final_content.push_str(&super::compression_store::retrieval_marker(
                                old_lines,
                                compressed_lines,
                                h,
                            ));
                        }
                        final_content
                    } else {
                        continue;
                    }
                };

                let new_len = new_content.len();
                if is_frozen
                    && let Some(tracker) = ctx.cache_tracker
                    && !tracker.should_compress(i, old_len, new_len)
                {
                    continue;
                }

                removed += old_len.saturating_sub(new_len);
                *content = new_content;
            }
        }
    }

    if removed > 0 {
        let total_after: usize = history
            .as_slice()
            .iter()
            .flat_map(|m| {
                m.content.iter().map(|b| match b {
                    ContentBlock::Text { text }
                    | ContentBlock::ToolResult { content: text, .. } => text.len(),
                    _ => 0,
                })
            })
            .sum();

        // Rough heuristic: if we removed >15% of total chars, likely enough to
        // avoid overflow on next turn. This is conservative — chars correlate
        // loosely with tokens but it's a fast check.
        let reduction_ratio = removed as f32 / total_before.max(1) as f32;
        let likely_sufficient = reduction_ratio > SUFFICIENT_REDUCTION_RATIO
            || !is_overflow(ctx.usage, ctx.model, ctx.compaction_buffer);

        info!(
            chars_removed = removed,
            total_before,
            total_after,
            reduction_pct = format!("{:.1}%", reduction_ratio * 100.0),
            likely_sufficient,
            "progressive compaction applied"
        );
    }

    removed
}

/// Check if estimated history tokens have reached the proactive compression threshold.
/// This fires before overflow to compress incrementally rather than all-at-once.
pub(super) fn is_proactive_threshold(history: &History, model: &Model, ratio: f64) -> bool {
    let estimated = history.estimate_tokens(model);
    let threshold = (model.context_window as f64 * ratio) as u32;
    estimated >= threshold
}

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, compaction_buffer: u32) -> bool {
    let reserved = compaction_buffer.max(model.max_output_tokens);
    let usable = model.context_window.saturating_sub(reserved);
    usage.context_tokens() >= usable
}

fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            if matches!(block, ContentBlock::Image { .. }) {
                *block = ContentBlock::Text {
                    text: IMAGE_PLACEHOLDER.into(),
                };
            }
        }
    }
}

fn strip_thinking(messages: &mut [Message]) {
    for msg in messages {
        msg.content.retain(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        });
    }
}

fn strip_old_tool_results(messages: &mut [Message]) {
    let total: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .count();

    let mut seen = 0;
    for msg in messages {
        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                if seen < total.saturating_sub(KEEP_LAST_TOOL_RESULTS) {
                    *content = TOOL_RESULT_PLACEHOLDER.into();
                }
                seen += 1;
            }
        }
    }
}

fn strip_tool_results_by_ratio(messages: &mut [Message], ratio: f32) -> usize {
    let mut indices: Vec<(usize, usize)> = Vec::new();
    for (mi, m) in messages.iter().enumerate() {
        for (bi, b) in m.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = b
                && content.as_str() != TOOL_RESULT_PLACEHOLDER
            {
                indices.push((mi, bi));
            }
        }
    }
    let total = indices.len();
    if total == 0 {
        return 0;
    }
    let target = (total as f32 * ratio).ceil() as usize;
    let mut dropped = 0;
    for (mi, bi) in indices.into_iter().take(target) {
        if let ContentBlock::ToolResult { content, .. } = &mut messages[mi].content[bi]
            && content.as_str() != TOOL_RESULT_PLACEHOLDER
        {
            *content = TOOL_RESULT_PLACEHOLDER.into();
            dropped += 1;
        }
    }
    dropped
}

fn static_fallback(history: &mut History) -> TokenUsage {
    let summary = build_static_summary(history.as_slice());
    history.replace(vec![
        Message::user(COMPACT_USER_PROMPT.into()),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: summary }],
            ..Default::default()
        },
    ]);
    TokenUsage::default()
}

fn truncate_oldest_round(messages: &mut Vec<Message>) {
    if messages.len() <= 1 {
        return;
    }

    let mut remove_count = 1;

    if matches!(messages.first().map(|m| &m.role), Some(Role::Assistant)) {
        let has_tool_calls = messages[0].has_tool_calls();
        if has_tool_calls {
            let next_has_tool_results = messages.get(1).is_some_and(|m| {
                matches!(m.role, Role::User)
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            });
            if next_has_tool_results {
                remove_count = 2;
            }
        }
    } else if matches!(messages.first().map(|m| &m.role), Some(Role::User))
        && matches!(messages.get(1).map(|m| &m.role), Some(Role::Assistant))
    {
        // Dropping a lone user message would leave assistant-first, which some providers reject.
        // Remove the assistant too to keep the conversation well-formed.
        remove_count = 2;
    }

    messages.drain(..remove_count);

    // After draining, the first message might still be an assistant (e.g. consecutive
    // assistant messages). Keep draining until the first message is user or we're empty.
    while messages.len() > 1 && matches!(messages.first().map(|m| &m.role), Some(Role::Assistant)) {
        let mut drop = 1;
        if matches!(messages.get(1).map(|m| &m.role), Some(Role::User)) {
            drop = 2;
        }
        messages.drain(..drop);
    }
}

pub(super) fn auto_compact_enabled() -> bool {
    env::var("CRAFT_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
}

fn build_static_summary(messages: &[Message]) -> String {
    let mut summary = String::from("[Static summary — LLM compaction failed]\n\n");
    let mut user_count = 0;
    let mut tool_names = Vec::new();
    let mut errors = Vec::new();

    for msg in messages {
        if matches!(msg.role, Role::User) {
            user_count += 1;
            let text = msg.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } if !text.is_empty() => Some(text.as_str()),
                _ => None,
            });
            if let Some(text) = text {
                let first_line = text.lines().next().unwrap_or("");
                if !first_line.is_empty() {
                    summary.push_str(&format!("**User**: {first_line}\n"));
                }
            }
        }
        for (_id, name, _input) in msg.tool_uses() {
            tool_names.push(name.to_string());
        }
        for block in &msg.content {
            if let ContentBlock::ToolResult { content, .. } = block
                && (content.contains("error") || content.contains("Error"))
            {
                let snippet: String = content.chars().take(ERROR_SNIPPET_CHARS).collect();
                errors.push(snippet);
            }
        }
    }

    if !tool_names.is_empty() {
        summary.push_str(&format!("\n**Tools used**: {}\n", tool_names.join(", ")));
    }
    if !errors.is_empty() {
        summary.push_str("\n**Errors encountered**:\n");
        for e in &errors {
            summary.push_str(&format!("- {e}\n"));
        }
    }
    summary.push_str(&format!("\n**Total user messages**: {user_count}\n"));

    summary
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use craft_providers::provider::{BoxFuture, Provider};
    use craft_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::AgentConfig;

    struct MockProvider {
        responses: Mutex<Vec<StreamResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&str>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model.max_output_tokens = max_output_tokens;
        model
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
        }
    }

    #[tokio::test]
    async fn compact_replaces_history_with_summary() {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
        ]);

        compact(
            &*provider,
            &model,
            &mut history,
            &EventSender::new(raw_tx, 0),
        )
        .await
        .unwrap();

        let msgs = history.as_slice();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));
    }

    #[test_case(159_999, 0,       0,       0,      200_000, 20_000, false ; "below_threshold")]
    #[test_case(160_000, 0,       0,       0,      200_000, 20_000, true  ; "at_threshold")]
    #[test_case(190_000, 0,       0,       0,      200_000, 10_000, true  ; "large_buffer_takes_precedence_over_small_max_output")]
    #[test_case(100,     0,       0,       0,      100,     20_000, true  ; "tiny_context_window")]
    #[test_case(5_000,   165_000, 10_000,  0,      200_000, 20_000, true  ; "cached_tokens_count_toward_overflow")]
    #[test_case(100_000, 0,       0,       80_000, 200_000, 20_000, true  ; "output_tokens_count_toward_overflow")]
    fn overflow_detection(
        input: u32,
        cache_read: u32,
        cache_creation: u32,
        output: u32,
        ctx_window: u32,
        max_out: u32,
        expected: bool,
    ) {
        let model = small_context_model(ctx_window, max_out);
        let usage = TokenUsage {
            input,
            output,
            cache_read,
            cache_creation,
        };
        assert_eq!(
            is_overflow(&usage, &model, AgentConfig::default().compaction_buffer),
            expected
        );
    }

    #[test]
    fn strip_images_replaces_with_placeholder() {
        use craft_providers::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc"));
        let mut messages = vec![Message::user_with_images("hello".into(), vec![source])];
        strip_images(&mut messages);
        assert_eq!(messages[0].content.len(), 2);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == IMAGE_PLACEHOLDER)
        );
        assert!(matches!(&messages[0].content[1], ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn strip_thinking_removes_thinking_blocks() {
        let mut messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque".into(),
                },
            ],
            ..Default::default()
        }];
        strip_thinking(&mut messages);
        assert_eq!(messages[0].content.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "hello"));
    }

    #[tokio::test]
    async fn progressive_compact_compresses_old_tool_results() {
        let long_content: String = "1: fn foo()\n".repeat(50);
        let mut history = History::new(vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "cat huge_file"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: long_content.clone(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ]);

        let usage = TokenUsage {
            input: 180_000,
            ..Default::default()
        };
        let model = default_model();
        let ctx = CompactContext {
            usage: &usage,
            model: &model,
            compaction_buffer: AgentConfig::default().compaction_buffer,
            cache_tracker: None,
            compression_store: None,
            relevance_scores: None,
            #[cfg(feature = "onnx")]
            scorer: None,
        };
        let _removed = progressive_compact(&mut history, 0, &ctx).await;
        let result_msg = &history.as_slice()[2];
        match &result_msg.content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(
                    content.len() < long_content.len(),
                    "content should be shorter"
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn progressive_compact_protects_recent_results() {
        let long_content: String = "1: fn foo()\n".repeat(50);
        let mut history = History::new(vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "cat file"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: long_content.clone(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ]);

        let usage = TokenUsage {
            input: 180_000,
            ..Default::default()
        };
        let model = default_model();
        let ctx = CompactContext {
            usage: &usage,
            model: &model,
            compaction_buffer: AgentConfig::default().compaction_buffer,
            cache_tracker: None,
            compression_store: None,
            relevance_scores: None,
            #[cfg(feature = "onnx")]
            scorer: None,
        };
        let _removed = progressive_compact(&mut history, 1, &ctx).await;
        match &history.as_slice()[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, &long_content, "content should be untouched");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn progressive_compact_very_old_gets_summary_marker() {
        let long_content: String = "line of code here\n".repeat(40);
        let mut messages: Vec<Message> = vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "build"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: long_content.clone(),
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        // Add enough messages to push t1 into "very old" territory
        for i in 0..20 {
            messages.push(Message::user(format!("msg {i}")));
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("reply {i}"),
                }],
                ..Default::default()
            });
        }

        let mut history = History::new(messages);
        let usage = TokenUsage {
            input: 180_000,
            ..Default::default()
        };
        let model = default_model();
        let ctx = CompactContext {
            usage: &usage,
            model: &model,
            compaction_buffer: AgentConfig::default().compaction_buffer,
            cache_tracker: None,
            compression_store: None,
            relevance_scores: None,
            #[cfg(feature = "onnx")]
            scorer: None,
        };
        let _removed = progressive_compact(&mut history, 0, &ctx).await;
        match &history.as_slice()[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(
                    content.starts_with("[Summary: "),
                    "very old result should get summary marker, got: {content}"
                );
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn strip_old_tool_results_keeps_newest() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old result 1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old result 2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep 1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t4".into(),
                    content: "keep 2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t5".into(),
                    content: "keep 3".into(),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "keep me".into(),
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert_eq!(messages[0].content.len(), 6);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t1")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, tool_use_id, .. } if content == TOOL_RESULT_PLACEHOLDER && tool_use_id == "t2")
        );
        assert!(
            matches!(&messages[0].content[2], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 1" && tool_use_id == "t3")
        );
        assert!(
            matches!(&messages[0].content[3], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 2" && tool_use_id == "t4")
        );
        assert!(
            matches!(&messages[0].content[4], ContentBlock::ToolResult { content, tool_use_id, .. } if content == "keep 3" && tool_use_id == "t5")
        );
        assert!(
            matches!(&messages[0].content[5], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn strip_old_tool_results_keeps_all_when_fewer_than_threshold() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "only result".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "second".into(),
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        strip_old_tool_results(&mut messages);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::ToolResult { content, .. } if content == "only result")
        );
        assert!(
            matches!(&messages[0].content[1], ContentBlock::ToolResult { content, .. } if content == "second")
        );
    }

    #[test]
    fn proactive_threshold_detects_large_history() {
        let model = small_context_model(1000, 100);
        let long_text: String = "x".repeat(4000);
        let history = History::new(vec![
            Message::user(long_text.clone()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: long_text }],
                ..Default::default()
            },
        ]);
        assert!(
            is_proactive_threshold(&history, &model, 0.50),
            "should exceed 50% threshold"
        );
    }

    #[test]
    fn proactive_threshold_false_for_small_history() {
        let model = small_context_model(200_000, 20_000);
        let history = History::new(vec![Message::user("hello".into())]);
        assert!(
            !is_proactive_threshold(&history, &model, 0.75),
            "should not exceed 75% threshold"
        );
    }

    #[test]
    fn truncate_oldest_round_removes_single_user_message() {
        let mut messages = vec![
            Message::user("first".into()),
            Message::user("second".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "second"));
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_tool_pair() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    is_error: false,
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_removes_assistant_without_matching_tool_result() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }],
                ..Default::default()
            },
            Message::user("no tool result".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "no tool result")
        );
    }

    #[test]
    fn truncate_oldest_round_noop_on_single_message() {
        let mut messages = vec![Message::user("only".into())];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn truncate_oldest_round_removes_plain_assistant() {
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0].content[0], ContentBlock::Text { text } if text == "keep me")
        );
    }

    #[test]
    fn truncate_oldest_round_consecutive_assistants_drains_until_user() {
        // [Assistant(no tools), Assistant(tools), User(results)] drains the first
        // assistant, leaving Assistant-first — keep draining until first is User.
        let mut messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "plain reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "output".into(),
                    is_error: false,
                }],
                ..Default::default()
            },
            Message::user("keep me".into()),
        ];
        truncate_oldest_round(&mut messages);
        assert!(!messages.is_empty());
        assert!(matches!(messages[0].role, Role::User));
    }

    #[test]
    fn strip_tool_results_by_ratio_removes_oldest_first() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "old1".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "old2".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t3".into(),
                    content: "keep".into(),
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        let dropped = strip_tool_results_by_ratio(&mut messages, 0.5);
        assert_eq!(dropped, 2);
        assert!(matches!(
            &messages[0].content[0],
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        ));
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        ));
        assert!(matches!(
            &messages[0].content[2],
            ContentBlock::ToolResult { content, .. } if content == "keep"
        ));
    }

    #[test]
    fn strip_tool_results_by_ratio_full_removes_all() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "a".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "b".into(),
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        let dropped = strip_tool_results_by_ratio(&mut messages, 1.0);
        assert_eq!(dropped, 2);
        assert!(messages[0].content.iter().all(|b| matches!(
            b,
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        )));
    }

    #[test]
    fn strip_tool_results_by_ratio_skips_already_placeholder() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: TOOL_RESULT_PLACEHOLDER.into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "real".into(),
                    is_error: false,
                },
            ],
            ..Default::default()
        }];
        let dropped = strip_tool_results_by_ratio(&mut messages, 1.0);
        assert_eq!(dropped, 1);
        assert!(matches!(
            &messages[0].content[1],
            ContentBlock::ToolResult { content, .. } if content == TOOL_RESULT_PLACEHOLDER
        ));
    }

    struct OverflowProvider {
        overflows_left: Mutex<usize>,
    }

    impl Provider for OverflowProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&'a str>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async move {
                let mut left = self.overflows_left.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return Err(AgentError::ContextOverflow {
                        message: "too long".into(),
                    });
                }
                Ok(text_response(StopReason::EndTurn))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
            Box::pin(async { unimplemented!() })
        }
    }

    #[tokio::test]
    async fn compact_history_recovers_from_overflow_via_progressive_removal() {
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(OverflowProvider {
            overflows_left: Mutex::new(5),
        });
        let model = default_model();
        let (raw_tx, _rx) = flume::unbounded();
        let mut history = History::new(vec![
            Message::user("do it".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "cat huge"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "x".repeat(500),
                    is_error: false,
                }],
                ..Default::default()
            },
        ]);

        compact_history(
            &*provider,
            &model,
            &mut history,
            &EventSender::new(raw_tx, 0),
            &CancelToken::none(),
            None,
        )
        .await
        .unwrap();

        let msgs = history.as_slice();
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));
    }
}
