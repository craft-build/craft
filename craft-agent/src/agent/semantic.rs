#![cfg(feature = "onnx")]

use craft_providers::{ContentBlock, Message};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

const EMBED_DIM: usize = 384;
const SUMMARY_CHARS_USER: usize = 500;
const SUMMARY_CHARS_TOOL_INPUT: usize = 100;
const SUMMARY_CHARS_TOOL_OUTPUT: usize = 200;
const CLASSIFY_TRUNCATE_CHARS: usize = 500;

#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("failed to load embedding model: {0}")]
    ModelLoad(String),
    #[error("embedding inference failed: {0}")]
    Inference(String),
    #[error("embed task failed: {0}")]
    TaskFailed(String),
    #[error("no embedding returned")]
    NoResult,
}

pub struct EmbeddingService {
    model: Arc<Mutex<Option<TextEmbedding>>>,
}

impl Clone for EmbeddingService {
    fn clone(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
        }
    }
}

impl Default for EmbeddingService {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingService {
    pub fn new() -> Self {
        Self {
            model: Arc::new(Mutex::new(None)),
        }
    }

    fn init_model() -> Result<TextEmbedding, EmbeddingError> {
        let options = TextInitOptions::new(EmbeddingModel::BGEBaseENV15)
            .with_show_download_progress(false);
        let options = match craft_storage::paths::models_dir() {
            Ok(dir) => options.with_cache_dir(dir),
            Err(_) => options,
        };
        TextEmbedding::try_new(options).map_err(|e| EmbeddingError::ModelLoad(e.to_string()))
    }

    pub async fn download_model(&self) -> Result<(), EmbeddingError> {
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            let mut guard = model.lock().map_err(|e| EmbeddingError::TaskFailed(e.to_string()))?;
            if guard.is_none() {
                *guard = Some(Self::init_model()?);
            }
            Ok(())
        })
        .await
        .map_err(|e| EmbeddingError::TaskFailed(e.to_string()))?
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        let model = Arc::clone(&self.model);
        let text = text.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut guard = model.lock().map_err(|e| EmbeddingError::TaskFailed(e.to_string()))?;
            if guard.is_none() {
                *guard = Some(Self::init_model()?);
            }
            let m = guard.as_mut().expect("initialized above");
            m.embed(vec![text], Default::default())
                .map_err(|e| EmbeddingError::Inference(e.to_string()))?
                .into_iter()
                .next()
                .ok_or(EmbeddingError::NoResult)
        })
        .await
        .map_err(|e| EmbeddingError::TaskFailed(e.to_string()))?
    }

    pub async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let model = Arc::clone(&self.model);
        tokio::task::spawn_blocking(move || {
            let mut guard = model.lock().map_err(|e| EmbeddingError::TaskFailed(e.to_string()))?;
            if guard.is_none() {
                *guard = Some(Self::init_model()?);
            }
            let m = guard.as_mut().expect("initialized above");
            m.embed(texts, Default::default())
                .map_err(|e| EmbeddingError::Inference(e.to_string()))
        })
        .await
        .map_err(|e| EmbeddingError::TaskFailed(e.to_string()))?
    }

    pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
        cosine_similarity(a, b)
    }
}

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

pub struct RelevanceScorer {
    service: EmbeddingService,
}

impl Clone for RelevanceScorer {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
        }
    }
}

impl RelevanceScorer {
    pub fn new() -> Self {
        Self {
            service: EmbeddingService::new(),
        }
    }

    pub async fn build_intent(&self, history: &[Message]) -> Result<Vec<f32>, EmbeddingError> {
        let summary = intent_summary(history);
        if summary.is_empty() {
            return Ok(vec![0.0; EMBED_DIM]);
        }
        info!(chars = summary.len(), "building intent vector");
        self.service.embed(&summary).await
    }

    pub async fn score_messages(
        &self,
        messages: &[Message],
        intent: &[f32],
    ) -> Result<Vec<(usize, f32)>, EmbeddingError> {
        let indexed_summaries: Vec<(usize, String)> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| (i, message_summary(m)))
            .filter(|(_, s)| !s.is_empty())
            .collect();

        if indexed_summaries.is_empty() {
            return Ok(Vec::new());
        }

        let summaries: Vec<String> = indexed_summaries.iter().map(|(_, s)| s.clone()).collect();
        let embeddings = self.service.embed_batch(summaries).await?;

        let mut scores: Vec<(usize, f32)> = indexed_summaries
            .into_iter()
            .zip(embeddings.iter())
            .map(|((i, _), emb)| (i, cosine_similarity(intent, emb)))
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scores)
    }

    pub async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        self.service.embed(text).await
    }

    pub async fn embed_batch(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        self.service.embed_batch(texts).await
    }

    pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
        EmbeddingService::similarity(a, b)
    }
}

pub fn intent_summary(history: &[Message]) -> String {
    let mut parts = Vec::new();

    for msg in history.iter().rev().take(3) {
        if matches!(msg.role, craft_providers::Role::User) {
            for block in &msg.content {
                if let ContentBlock::Text { text } = block {
                    let truncated: String = text.chars().take(SUMMARY_CHARS_USER).collect();
                    parts.push(truncated);
                }
            }
        }
    }

    for msg in history.iter().rev().take(5) {
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block {
                let input_str = input.to_string();
                let truncated: String = input_str.chars().take(SUMMARY_CHARS_TOOL_INPUT).collect();
                parts.push(format!("{name}: {truncated}"));
            }
        }
    }

    parts.reverse();
    parts.join(" ")
}

fn message_summary(msg: &Message) -> String {
    let mut parts = Vec::new();

    if matches!(msg.role, craft_providers::Role::User) {
        for block in &msg.content {
            if let ContentBlock::Text { text } = block {
                let truncated: String = text.chars().take(SUMMARY_CHARS_USER).collect();
                parts.push(truncated);
            }
        }
    } else if matches!(msg.role, craft_providers::Role::Assistant) {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    let truncated: String = text.chars().take(SUMMARY_CHARS_TOOL_INPUT).collect();
                    parts.push(truncated);
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let input_str = input.to_string();
                    let truncated: String =
                        input_str.chars().take(SUMMARY_CHARS_TOOL_INPUT).collect();
                    parts.push(format!("{name}({truncated})"));
                }
                _ => {}
            }
        }
    }

    for block in &msg.content {
        if let ContentBlock::ToolResult { content, .. } = block {
            let truncated: String = content.chars().take(SUMMARY_CHARS_TOOL_OUTPUT).collect();
            parts.push(format!("[result: {truncated}]"));
        }
    }

    if parts.is_empty() {
        return String::new();
    }
    parts.join(" ")
}

pub const HIGH_RELEVANCE: f32 = 0.7;
pub const LOW_RELEVANCE: f32 = 0.3;

pub fn select_messages(
    scores: &[(usize, f32)],
    history_len: usize,
    token_budget: u32,
    mandatory_recent: usize,
    frozen_count: usize,
    estimate_tokens: &dyn Fn(usize) -> u32,
) -> Vec<usize> {
    let mut selected = Vec::new();
    let mut budget_used: u32 = 0;

    for i in 0..frozen_count.min(history_len) {
        selected.push(i);
        budget_used += estimate_tokens(i);
    }

    let recent_start = history_len.saturating_sub(mandatory_recent);
    for i in recent_start..history_len {
        if i >= frozen_count {
            selected.push(i);
            budget_used += estimate_tokens(i);
        }
    }

    let mut scored: Vec<(usize, f32)> = scores
        .iter()
        .filter(|(idx, _)| {
            *idx >= frozen_count && *idx < recent_start
        })
        .cloned()
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    for (idx, score) in scored {
        if score < LOW_RELEVANCE {
            continue;
        }
        let cost = estimate_tokens(idx);
        if budget_used + cost > token_budget {
            break;
        }
        selected.push(idx);
        budget_used += cost;
    }

    selected.sort_unstable();
    selected
}

pub fn detect_stagnation(recent_embeddings: &[Vec<f32>], threshold: f32) -> bool {
    if recent_embeddings.len() < 3 {
        return false;
    }
    let consecutive_high = recent_embeddings
        .windows(2)
        .filter(|w| cosine_similarity(&w[0], &w[1]) > threshold)
        .count();
    consecutive_high >= recent_embeddings.len().saturating_sub(1)
}

pub async fn auto_retrieve(
    scorer: &RelevanceScorer,
    store: &crate::agent::compression_store::SharedCompressionStore,
    intent: &[f32],
    history: &mut crate::agent::History,
) -> usize {
    let messages = history.as_slice();
    let mut candidates = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        for block in &msg.content {
            if let ContentBlock::Text { text } = block
                && (text.contains("[earlier messages omitted") || text.contains("retrieve"))
            {
                candidates.push((i, text.clone()));
            }
        }
    }

    if candidates.is_empty() {
        return 0;
    }

    let texts: Vec<String> = candidates.iter().map(|(_, t)| t.clone()).collect();
    let embeddings = match scorer.embed_batch(texts).await {
        Ok(embs) => embs,
        Err(e) => {
            warn!(error = %e, "auto-retrieve embedding failed, skipping");
            return 0;
        }
    };

    let mut restored = 0;
    let guard = match store.lock() {
        Ok(g) => g,
        Err(e) => {
            warn!(error = %e, "auto-retrieve store lock failed, skipping");
            return 0;
        }
    };

    for ((i, _), emb) in candidates.iter().zip(embeddings.iter()) {
        if cosine_similarity(intent, emb) > HIGH_RELEVANCE
            && let Some(msg) = history.as_mut_slice().get_mut(*i)
        {
            for block in &mut msg.content {
                if let ContentBlock::Text { text } = block
                    && let Some(hash) = extract_retrieval_hash(text)
                    && let Some(original) = guard.get(&hash)
                {
                    *text = original.to_string();
                    restored += 1;
                }
            }
        }
    }

    restored
}

fn extract_retrieval_hash(text: &str) -> Option<String> {
    let marker_start = text.find("retrieve#");
    marker_start.map(|start| {
        let rest = &text[start + "retrieve#".len()..];
        rest.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    })
}

pub const STALE_RELEVANCE_THRESHOLD: f32 = 0.6;

pub async fn classify_reads_semantic(
    history: &[Message],
    scorer: &RelevanceScorer,
    stale_classifications: &[(String, String)],
) -> Vec<(String, String, bool)> {
    let mut results = Vec::new();

    for (tool_call_id, file_path) in stale_classifications {
        let read_text = find_read_content(history, tool_call_id);
        let edit_text = find_latest_edit_content(history, file_path);

        let (read_text, edit_text) = match (read_text, edit_text) {
            (Some(r), Some(e)) => (r, e),
            _ => {
                results.push((tool_call_id.clone(), file_path.clone(), true));
                continue;
            }
        };

        let read_emb = match scorer.embed_text(&read_text).await {
            Ok(e) => e,
            Err(_) => {
                results.push((tool_call_id.clone(), file_path.clone(), true));
                continue;
            }
        };

        let edit_emb = match scorer.embed_text(&edit_text).await {
            Ok(e) => e,
            Err(_) => {
                results.push((tool_call_id.clone(), file_path.clone(), true));
                continue;
            }
        };

        let sim = cosine_similarity(&read_emb, &edit_emb);
        let is_stale = sim >= STALE_RELEVANCE_THRESHOLD;
        if !is_stale {
            info!(file = file_path.as_str(), sim, "semantic stale detection: downgraded Stale -> Fresh");
        }
        results.push((tool_call_id.clone(), file_path.clone(), is_stale));
    }

    results
}

fn find_read_content(history: &[Message], tool_call_id: &str) -> Option<String> {
    for msg in history {
        for block in &msg.content {
            if let ContentBlock::ToolResult { tool_use_id, content, .. } = block
                && tool_use_id == tool_call_id
            {
                let truncated: String = content.chars().take(CLASSIFY_TRUNCATE_CHARS).collect();
                return Some(truncated);
            }
        }
    }
    None
}

fn find_latest_edit_content(history: &[Message], file_path: &str) -> Option<String> {
    let mut latest: Option<String> = None;
    for msg in history {
        for block in &msg.content {
            if let ContentBlock::ToolUse { name, input, .. } = block
                && matches!(name.as_str(), "edit" | "write")
                && let Some(path) = input.get("file_path").and_then(|v| v.as_str())
                && path == file_path
            {
                let content = input
                    .get("new_string")
                    .or_else(|| input.get("content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let truncated: String = content.chars().take(CLASSIFY_TRUNCATE_CHARS).collect();
                latest = Some(truncated);
            }
        }
    }
    latest
}

pub const SEMANTIC_DEDUP_THRESHOLD: f32 = 0.9;

pub fn detect_semantic_overlap(
    embeddings: &[(usize, Vec<f32>)],
) -> Vec<(usize, usize, f32)> {
    let mut overlaps = Vec::new();
    for i in 0..embeddings.len() {
        for j in (i + 1)..embeddings.len() {
            let (idx_a, emb_a) = &embeddings[i];
            let (idx_b, emb_b) = &embeddings[j];
            if *idx_b > *idx_a {
                let sim = cosine_similarity(emb_a, emb_b);
                if sim > SEMANTIC_DEDUP_THRESHOLD {
                    overlaps.push((*idx_a, *idx_b, sim));
                }
            }
        }
    }
    overlaps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_similarity_identical() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_similarity_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn test_detect_stagnation_loops() {
        let embeddings = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.99, 0.01, 0.0],
            vec![0.98, 0.02, 0.0],
            vec![0.97, 0.03, 0.0],
        ];
        assert!(detect_stagnation(&embeddings, 0.9));
    }

    #[test]
    fn test_detect_stagnation_diverging() {
        let embeddings = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.0, 0.0, 1.0],
            vec![1.0, 0.0, 0.0],
        ];
        assert!(!detect_stagnation(&embeddings, 0.9));
    }

    #[test]
    fn test_select_messages_always_includes_frozen_and_recent() {
        let scores = vec![(2, 0.9), (3, 0.5), (4, 0.1)];
        let selected = select_messages(
            &scores,
            8,
            10000,
            2,
            2,
            &|_| 100,
        );
        assert!(selected.contains(&0));
        assert!(selected.contains(&1));
        assert!(selected.contains(&6));
        assert!(selected.contains(&7));
    }

    #[test]
    fn test_message_summary() {
        let msg = Message::user("hello world".to_owned());
        let summary = message_summary(&msg);
        assert!(!summary.is_empty());
    }
}
