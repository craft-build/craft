use std::env;

use craft_providers::{ContentBlock, Message, Model, RequestOptions, TokenUsage};
use tracing::info;

use super::history::History;
use super::streaming::stream_with_retry;
use crate::cancel::CancelToken;
use crate::compression::{self, CompressionConfig as AgentCompressionConfig};
use crate::{AgentError, AgentEvent, EventSender, TurnCompleteEvent};

pub(super) const CONTINUE_AFTER_COMPACT: &str = "Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed. If you learned important project context during this session, consider saving it to memory before it's lost.";
const IMAGE_PLACEHOLDER: &str = "[image]";
const SUMMARY_MARKER_PREFIX: &str = "[Summary: ";

const AGGRESSIVE_CODE_RATE: f32 = 0.15;
const AGGRESSIVE_MAX_LOG_LINES: usize = 20;
const AGGRESSIVE_MAX_DIFF_LINES: usize = 40;
const AGGRESSIVE_MAX_SEARCH_FILES: usize = 10;
const AGGRESSIVE_MAX_JSON_ITEMS: usize = 8;
const MIN_TOOL_RESULT_CHARS: usize = 300;

fn aggressive_config() -> AgentCompressionConfig {
    AgentCompressionConfig {
        enabled: true,
        code_compression_rate: AGGRESSIVE_CODE_RATE,
        max_log_lines: AGGRESSIVE_MAX_LOG_LINES,
        max_search_files: AGGRESSIVE_MAX_SEARCH_FILES,
        max_matches_per_file: 2,
        max_diff_lines: AGGRESSIVE_MAX_DIFF_LINES,
        max_json_items: AGGRESSIVE_MAX_JSON_ITEMS,
        json_first_keep: 2,
        json_last_keep: 1,
        protect_recent_tool_outputs: 0,
    }
}

pub(super) async fn compact_history(
    provider: &dyn craft_providers::provider::Provider,
    model: &Model,
    history: &mut History,
    event_tx: &EventSender,
    cancel: &CancelToken,
) -> Result<TokenUsage, AgentError> {
    let compact_start = std::time::Instant::now();

    let lifecycle_removed = super::read_lifecycle::run_lifecycle(history);
    if lifecycle_removed > 0 {
        info!(chars_removed = lifecycle_removed, "read lifecycle applied before compaction");
    }

    let mut compaction_history: Vec<Message> = history.as_slice().to_vec();
    strip_images(&mut compaction_history);
    strip_thinking(&mut compaction_history);
    compaction_history.push(Message::user(crate::prompt::COMPACTION_USER.to_string()));

    let empty_tools = serde_json::json!([]);
    let response = stream_with_retry(
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
    .await?;

    event_tx.send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
        message: response.message.clone(),
        usage: response.usage,
        model: model.id.clone(),
        context_size: Some(response.usage.context_tokens()),
    })))?;

    let new_history = vec![
        Message::user("What did we do so far?".into()),
        response.message,
    ];
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
    let usage = compact_history(provider, model, history, event_tx, &cancel).await?;

    event_tx.send(AgentEvent::Done {
        usage,
        num_turns: 1,
        stop_reason: None,
    })?;

    Ok(())
}

/// Attempt progressive compaction: compress old tool outputs in-place without
/// LLM summarization. Returns total characters removed.
///
/// Passes:
/// 1. Read lifecycle — replace stale/superseded reads
/// 2. Compress old tool results — aggressive compression on results past `protect_recent`
/// 3. Summarize very old tool results — replace with compact markers
pub(super) fn progressive_compact(
    history: &mut History,
    protect_recent: usize,
    usage: &TokenUsage,
    model: &Model,
    compaction_buffer: u32,
    cache_tracker: Option<&super::cache::PrefixCacheTracker>,
    compression_store: Option<&super::compression_store::SharedCompressionStore>,
) -> usize {
    let total_before: usize = history
        .as_slice()
        .iter()
        .flat_map(|m| m.content.iter().map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::ToolResult { content: text, .. } => text.len(),
            _ => 0,
        }))
        .sum();

    // Pass 1: read lifecycle
    let mut removed = super::read_lifecycle::run_lifecycle(history);

    // Count tool result messages from the end to determine which are "recent"
    let tool_result_indices: Vec<usize> = history
        .as_slice()
        .iter()
        .enumerate()
        .filter(|(_, m)| m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
        .map(|(i, _)| i)
        .collect();

    let recent_cutoff = tool_result_indices
        .len()
        .saturating_sub(protect_recent);
    let recent_msg_indices: std::collections::HashSet<usize> = tool_result_indices
        .into_iter()
        .skip(recent_cutoff)
        .collect();

    let aggressive = aggressive_config();

    // Pass 2 + 3: compress old tool results
    let messages = history.as_mut_slice();
    let msg_count = messages.len();
    let very_old_threshold = protect_recent * 3;

    for (i, msg) in messages.iter_mut().enumerate() {
        if recent_msg_indices.contains(&i) {
            continue;
        }

        // Skip messages in the frozen prefix cache unless savings are huge
        if let Some(tracker) = cache_tracker
            && tracker.is_frozen(i)
        {
            continue;
        }

        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, is_error: false, .. } = block
                && content.len() >= MIN_TOOL_RESULT_CHARS
            {
                let old_len = content.len();
                let old_lines = content.lines().count();
                let is_very_old = msg_count.saturating_sub(i) > very_old_threshold;

                // Store original in compression store for CCR retrieval
                let hash = compression_store.and_then(|store| {
                    let mut guard = store.lock().ok()?;
                    Some(guard.put(content))
                });

                if is_very_old {
                    let line_count = old_lines;
                    let first_line = content.lines().next().unwrap_or("");
                    let preview: String = first_line.chars().take(80).collect();
                    let mut summary = format!("{SUMMARY_MARKER_PREFIX}{line_count} lines. First: {preview}]");
                    if let Some(ref h) = hash {
                        summary.push_str(&super::compression_store::retrieval_marker(old_lines, 1, h));
                    }
                    removed += old_len.saturating_sub(summary.len());
                    *content = summary;
                } else {
                    let ct = compression::detect_content_type(content);
                    let compressed = compression::compress(content, ct, &aggressive);
                    if compressed.len() < old_len {
                        let mut final_content = compressed;
                        if let Some(ref h) = hash {
                            let compressed_lines = final_content.lines().count();
                            final_content.push_str(&super::compression_store::retrieval_marker(old_lines, compressed_lines, h));
                        }
                        removed += old_len.saturating_sub(final_content.len());
                        *content = final_content;
                    }
                }
            }
        }
    }

    if removed > 0 {
        let total_after: usize = history
            .as_slice()
            .iter()
            .flat_map(|m| m.content.iter().map(|b| match b {
                ContentBlock::Text { text } | ContentBlock::ToolResult { content: text, .. } => text.len(),
                _ => 0,
            }))
            .sum();

        // Rough heuristic: if we removed >15% of total chars, likely enough to
        // avoid overflow on next turn. This is conservative — chars correlate
        // loosely with tokens but it's a fast check.
        let reduction_ratio = removed as f32 / total_before.max(1) as f32;
        let likely_sufficient = reduction_ratio > 0.15 || !is_overflow(usage, model, compaction_buffer);

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
pub(super) fn is_proactive_threshold(history: &History, model: &Model, ratio: f32) -> bool {
    let estimated = history.estimate_tokens(model);
    let threshold = (model.context_window as f32 * ratio) as u32;
    estimated >= threshold
}

pub(super) fn is_overflow(usage: &TokenUsage, model: &Model, compaction_buffer: u32) -> bool {
    let reserved = compaction_buffer.min(model.max_output_tokens);
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

pub(super) fn auto_compact_enabled() -> bool {
    env::var("CRAFT_DISABLE_AUTOCOMPACT")
        .map(|v| v != "1" && v != "true")
        .unwrap_or(true)
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

    #[test_case(179_999, 0,       0,       0,      200_000, 20_000, false ; "below_threshold")]
    #[test_case(180_000, 0,       0,       0,      200_000, 20_000, true  ; "at_threshold")]
    #[test_case(190_000, 0,       0,       0,      200_000, 10_000, true  ; "small_max_output_uses_it_as_reserve")]
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

    #[test]
    fn progressive_compact_compresses_old_tool_results() {
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

        let usage = TokenUsage { input: 180_000, ..Default::default() };
        let model = default_model();
        let removed = progressive_compact(&mut history, 0, &usage, &model, AgentConfig::default().compaction_buffer, None, None);

        assert!(removed > 0, "should have compressed the old tool result");
        let result_msg = &history.as_slice()[2];
        match &result_msg.content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.len() < long_content.len(), "content should be shorter");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn progressive_compact_protects_recent_results() {
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

        let usage = TokenUsage { input: 180_000, ..Default::default() };
        let model = default_model();
        let removed = progressive_compact(&mut history, 1, &usage, &model, AgentConfig::default().compaction_buffer, None, None);

        assert_eq!(removed, 0, "should not compress when protect_recent covers the only result");
        match &history.as_slice()[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, &long_content, "content should be untouched");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn progressive_compact_very_old_gets_summary_marker() {
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
                content: vec![ContentBlock::Text { text: format!("reply {i}") }],
                ..Default::default()
            });
        }

        let mut history = History::new(messages);
        let usage = TokenUsage { input: 180_000, ..Default::default() };
        let model = default_model();
        let removed = progressive_compact(&mut history, 0, &usage, &model, AgentConfig::default().compaction_buffer, None, None);

        assert!(removed > 0, "should have compressed the very old tool result");
        match &history.as_slice()[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.starts_with("[Summary: "), "very old result should get summary marker, got: {content}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
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
        assert!(is_proactive_threshold(&history, &model, 0.50), "should exceed 50% threshold");
    }

    #[test]
    fn proactive_threshold_false_for_small_history() {
        let model = small_context_model(200_000, 20_000);
        let history = History::new(vec![
            Message::user("hello".into()),
        ]);
        assert!(!is_proactive_threshold(&history, &model, 0.75), "should not exceed 75% threshold");
    }
}
