use std::collections::{HashMap, HashSet};

use craft_providers::{ContentBlock, Model, TokenUsage};
use tracing::info;

use super::super::compression_store;
use super::super::history::History;
use super::super::read_lifecycle::run_lifecycle;
use super::super::semantic::{RelevanceScorer, detect_semantic_overlap};

use super::{
    AGGRESSIVE_CODE_RATE, AGGRESSIVE_MAX_DIFF_LINES, AGGRESSIVE_MAX_JSON_ITEMS,
    AGGRESSIVE_MAX_LOG_LINES, AGGRESSIVE_MAX_SEARCH_FILES, HIGH_RELEVANCE_THRESHOLD,
    LOW_RELEVANCE_THRESHOLD, MIN_TOOL_RESULT_CHARS, SUFFICIENT_REDUCTION_RATIO,
    SUMMARY_MARKER_PREFIX, SUMMARY_PREVIEW_CHARS, VERY_OLD_MULTIPLIER, is_overflow,
};

const AGGRESSIVE_MAX_MATCHES_PER_FILE: usize = 3;
const AGGRESSIVE_JSON_FIRST_KEEP: usize = 2;
const AGGRESSIVE_JSON_LAST_KEEP: usize = 2;
const AGGRESSIVE_PROTECT_RECENT: usize = 0;

pub(crate) struct CompactContext<'a> {
    pub usage: &'a TokenUsage,
    pub model: &'a Model,
    pub compaction_buffer: u32,
    pub cache_tracker: Option<&'a super::super::cache::PrefixCacheTracker>,
    pub compression_store: Option<&'a compression_store::SharedCompressionStore>,
    pub relevance_scores: Option<&'a [(usize, f32)]>,
    pub scorer: Option<&'a RelevanceScorer>,
}

pub(super) fn aggressive_config() -> crate::compression::CompressionConfig {
    crate::compression::CompressionConfig {
        enabled: true,
        code_compression_rate: AGGRESSIVE_CODE_RATE,
        max_log_lines: AGGRESSIVE_MAX_LOG_LINES,
        max_search_files: AGGRESSIVE_MAX_SEARCH_FILES,
        max_matches_per_file: AGGRESSIVE_MAX_MATCHES_PER_FILE,
        max_diff_lines: AGGRESSIVE_MAX_DIFF_LINES,
        max_json_items: AGGRESSIVE_MAX_JSON_ITEMS,
        json_first_keep: AGGRESSIVE_JSON_FIRST_KEEP,
        json_last_keep: AGGRESSIVE_JSON_LAST_KEEP,
        protect_recent_tool_outputs: AGGRESSIVE_PROTECT_RECENT,
    }
}

/// Compress old tool outputs in-place without LLM summarization.
/// Returns total characters removed.
/// Pass 1: read lifecycle. Pass 2: compress old results.
/// Pass 3: summarize very old results with compact markers.
pub(crate) async fn progressive_compact(
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

    let mut removed = run_lifecycle(history, ctx.scorer, ctx.compression_store).await;

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
            for (older, _newer, _sim) in detect_semantic_overlap(&old_tool_embeddings) {
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

    let aggressive = aggressive_config();

    let score_map: HashMap<usize, f32> = ctx
        .relevance_scores
        .map(|scores| scores.iter().map(|(idx, score)| (*idx, *score)).collect())
        .unwrap_or_default();

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
                        summary.push_str(&compression_store::retrieval_marker(old_lines, 1, h));
                    }
                    summary
                } else {
                    let ct = crate::compression::detect_content_type(content);
                    let compressed = crate::compression::compress(content, ct, &aggressive);
                    if compressed.len() < old_len {
                        let hash = ctx.compression_store.and_then(|store| {
                            let mut guard = store.lock().ok()?;
                            Some(guard.put(content))
                        });
                        let mut final_content = compressed;
                        if let Some(ref h) = hash {
                            let compressed_lines = final_content.lines().count();
                            final_content.push_str(&compression_store::retrieval_marker(
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

#[cfg(test)]
mod tests {
    use super::*;
    use craft_providers::{Message, TokenUsage};

    fn long_tool_result_history(content: String) -> History {
        History::new(vec![
            Message::user("do it".into()),
            Message {
                role: craft_providers::Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "cat huge_file"}),
                )],
                ..Default::default()
            },
            Message {
                role: craft_providers::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content,
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ])
    }

    fn ctx<'a>(usage: &'a TokenUsage, model: &'a Model) -> CompactContext<'a> {
        CompactContext {
            usage,
            model,
            compaction_buffer: crate::AgentConfig::default()
                .compaction_buffer
                .resolve(model.context_window),
            cache_tracker: None,
            compression_store: None,
            relevance_scores: None,
            scorer: None,
        }
    }

    #[tokio::test]
    async fn progressive_compact_compresses_old_tool_results() {
        let long_content: String = "1: fn foo()\n".repeat(50);
        let mut history = long_tool_result_history(long_content.clone());
        let usage = TokenUsage {
            input: 180_000,
            ..Default::default()
        };
        let model = super::super::test_support::default_model();
        let ctx = ctx(&usage, &model);
        let _removed = progressive_compact(&mut history, 0, &ctx).await;
        match &history.as_slice()[2].content[0] {
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
        let mut history = long_tool_result_history(long_content.clone());
        let usage = TokenUsage {
            input: 180_000,
            ..Default::default()
        };
        let model = super::super::test_support::default_model();
        let ctx = ctx(&usage, &model);
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
                role: craft_providers::Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "t1",
                    "bash",
                    serde_json::json!({"command": "build"}),
                )],
                ..Default::default()
            },
            Message {
                role: craft_providers::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: long_content,
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        for i in 0..20 {
            messages.push(Message::user(format!("msg {i}")));
            messages.push(Message {
                role: craft_providers::Role::Assistant,
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
        let model = super::super::test_support::default_model();
        let ctx = ctx(&usage, &model);
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
}
