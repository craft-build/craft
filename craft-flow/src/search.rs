//! Semantic search over a Flow workstream's persisted documents, plus the
//! `FlowSearchBackend` impl injected into the agent's `flow_search` tool.
//!
//! Two layers:
//! - [`search`] / [`reindex`] are pure async functions that take an injectable
//!   [`Embedder`] (so the ranking logic is unit-testable without the real embedding
//!   model) and a `FlowStore`. They maintain a flat per-workstream index:
//!   documents are embedded lazily as they are written,
//!   and the index only activates once a workstream exceeds
//!   [`SEMANTIC_INDEX_MIN_DOCS`] documents (below that, linear scan is cheaper).
//! - [`FlowSearchBackendImpl`] wires those functions to craft-agent's
//!   `FlowSearchBackend` trait using the real `EmbeddingService`, and is what
//!   the pipeline hands to `ToolContext::flow_search`.

use std::sync::Arc;

use craft_agent::EmbeddingService;
use craft_agent::tools::flow_search::FlowSearchHit;
use craft_storage::flow::{FlowStore, SEMANTIC_INDEX_MIN_DOCS};
use tracing::warn;

use crate::{FlowConfig, FlowRunError};

/// Minimum relevance score for a hit to be returned. Mirrors the LOW_RELEVANCE
/// floor used by craft-agent's semantic selection: below it, a doc is noise.
const MIN_RESULT_SCORE: f32 = 0.30;

/// Abstraction over embedding production so the ranking logic is testable
/// without the ONNX runtime. The real impl (`OnnxEmbedder`) wraps
/// `EmbeddingService`; tests inject a deterministic fake.
pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, texts: Vec<String>) -> EmbedFuture<'a>;
}

/// Future returned by [`Embedder::embed`]. Factored out so the trait body stays
/// readable (the raw `Pin<Box<dyn Future>>` form is verbose to inline).
pub type EmbedFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>, FlowRunError>> + Send + 'a>,
>;

/// Future returned by `FlowSearchBackend::search`, factored out for the same
/// reason as [`EmbedFuture`]. Output is `Result<_, String>` because the upstream
/// `craft_agent::FlowSearchBackend` trait is locked to `String`; the impl maps
/// [`FlowRunError`] to its display string at this boundary.
type FlowSearchFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<FlowSearchHit>, String>> + Send + 'a>,
>;

/// Real embedder backed by the shared ONNX `EmbeddingService`.
pub struct OnnxEmbedder {
    service: EmbeddingService,
}

impl OnnxEmbedder {
    pub fn new(service: EmbeddingService) -> Self {
        Self { service }
    }
}

impl Embedder for OnnxEmbedder {
    fn embed<'a>(&'a self, texts: Vec<String>) -> EmbedFuture<'a> {
        let service = &self.service;
        std::boxed::Box::pin(async move {
            service
                .embed_batch(texts)
                .await
                .map_err(|e| FlowRunError::Embedder(e.to_string()))
        })
    }
}

/// One ranked search result.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub score: f32,
}

/// Embed every persisted document that lacks an embedding, persisting the
/// updated index. Returns the number of newly embedded docs. A no-op (returns
/// 0) when the workstream is below the activation threshold: keeping the index
/// empty there means `search` falls through to a cheap linear scan.
pub async fn reindex(
    store: &FlowStore,
    embedder: &dyn Embedder,
    project_id: &str,
    workstream_id: &str,
) -> Result<usize, FlowRunError> {
    let live = store
        .list(project_id, workstream_id)
        .map_err(FlowRunError::from)?;
    if live.len() <= SEMANTIC_INDEX_MIN_DOCS {
        return Ok(0);
    }
    let mut index = store
        .read_index(project_id, workstream_id)
        .map_err(FlowRunError::from)?;
    let missing = index.reconcile(live.iter().map(String::as_str));
    if missing.is_empty() {
        return Ok(0);
    }
    let contents: Vec<String> = missing
        .iter()
        .map(|p| match store.read(project_id, workstream_id, p) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %p, error = %e, "flow: failed to read doc for reindex");
                String::new()
            }
        })
        .collect();
    let embeddings = embedder.embed(contents).await?;
    for (path, emb) in missing.into_iter().zip(embeddings) {
        index.upsert(path, emb);
    }
    let added = index.len();
    store
        .write_index(project_id, workstream_id, &index)
        .map_err(FlowRunError::from)?;
    Ok(added)
}

/// Rank a workstream's documents against `query`, returning the top `k` by
/// cosine similarity. Uses the persisted index when active; below the threshold
/// (or with an empty/stale index) it falls back to embedding the query against
/// all live docs on the fly so search still works, just slower.
pub async fn search(
    store: &FlowStore,
    embedder: &dyn Embedder,
    project_id: &str,
    workstream_id: &str,
    query: &str,
    k: usize,
) -> Result<Vec<SearchHit>, FlowRunError> {
    let live = store
        .list(project_id, workstream_id)
        .map_err(FlowRunError::from)?;
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let k = k.max(1);

    let query_emb = embedder.embed(vec![query.to_string()]).await?;
    let query_emb = query_emb
        .into_iter()
        .next()
        .ok_or_else(|| FlowRunError::Embedder("embedder returned no query embedding".into()))?;

    let index = store
        .read_index(project_id, workstream_id)
        .map_err(FlowRunError::from)?;
    let use_index = live.len() > SEMANTIC_INDEX_MIN_DOCS
        && !index.is_empty()
        && live.iter().all(|p| index.get(p).is_some());

    let mut scored: Vec<SearchHit> = if use_index {
        live.iter()
            .filter_map(|p| {
                index.get(p).map(|emb| SearchHit {
                    path: p.clone(),
                    score: craft_agent::agent::cosine_similarity(&query_emb, emb),
                })
            })
            .collect()
    } else {
        let contents: Vec<String> = live
            .iter()
            .map(|p| match store.read(project_id, workstream_id, p) {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %p, error = %e, "flow: failed to read doc for search");
                    String::new()
                }
            })
            .collect();
        let embs = embedder.embed(contents).await?;
        live.iter()
            .zip(embs)
            .map(|(p, emb)| SearchHit {
                path: p.clone(),
                score: craft_agent::agent::cosine_similarity(&query_emb, &emb),
            })
            .collect()
    };

    scored.retain(|h| h.score >= MIN_RESULT_SCORE);
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(k);
    Ok(scored)
}

/// craft-agent `FlowSearchBackend` impl backed by the real ONNX embedder and
/// the shared `FlowStore`. Held in `ToolContext::flow_search` so the
/// `flow_search` tool can call back into craft-flow without a circular dep.
pub struct FlowSearchBackendImpl {
    store: Arc<FlowStore>,
    embedder: Arc<dyn Embedder>,
    project_id: String,
    workstream_id: String,
}

impl FlowSearchBackendImpl {
    pub fn new(
        store: Arc<FlowStore>,
        embedder: Arc<dyn Embedder>,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            embedder,
            project_id: project_id.into(),
            workstream_id: workstream_id.into(),
        }
    }
}

impl craft_agent::tools::flow_search::FlowSearchBackend for FlowSearchBackendImpl {
    fn workstream(&self) -> Option<(String, String)> {
        Some((self.project_id.clone(), self.workstream_id.clone()))
    }

    fn search<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
        query: &'a str,
        k: usize,
    ) -> FlowSearchFuture<'a> {
        std::boxed::Box::pin(async move {
            let hits = search(
                &self.store,
                self.embedder.as_ref(),
                project_id,
                workstream_id,
                query,
                k,
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(hits
                .into_iter()
                .map(|h| FlowSearchHit {
                    path: h.path,
                    score: h.score,
                })
                .collect())
        })
    }

    fn read_document<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
        rel_path: &'a str,
    ) -> craft_agent::tools::flow_search::ReadFuture<'a> {
        std::boxed::Box::pin(async move {
            self.store
                .read(project_id, workstream_id, rel_path)
                .map_err(|e| e.to_string())
        })
    }

    fn list_documents<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
    ) -> craft_agent::tools::flow_search::ListFuture<'a> {
        std::boxed::Box::pin(async move {
            self.store
                .list(project_id, workstream_id)
                .map_err(|e| e.to_string())
        })
    }
}

/// Whether the index should be maintained for a run. The semantic index is
/// always on, so this is always `true`; kept as a hook for callers that branch
/// on it.
pub fn index_enabled(_config: &FlowConfig) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Deterministic embedder: maps each doc's first line to a fixed vector via
    /// a lookup, so similarity is fully controlled by the test. Docs whose first
    /// line isn't in the map get the zero vector (score 0 against anything).
    struct ScriptedEmbedder {
        table: Mutex<HashMap<String, Vec<f32>>>,
    }

    impl ScriptedEmbedder {
        fn new() -> Self {
            Self {
                table: Mutex::new(HashMap::new()),
            }
        }

        fn put(&self, key: &str, vec: Vec<f32>) {
            self.table.lock().unwrap().insert(key.to_string(), vec);
        }
    }

    const DIM: usize = 4;

    impl Embedder for ScriptedEmbedder {
        fn embed<'a>(
            &'a self,
            texts: Vec<String>,
        ) -> std::pin::Pin<
            std::boxed::Box<
                dyn std::future::Future<Output = Result<Vec<Vec<f32>>, FlowRunError>> + Send + 'a,
            >,
        > {
            std::boxed::Box::pin(async move {
                let table = self.table.lock().unwrap();
                let out = texts
                    .into_iter()
                    .map(|t| {
                        let key = t.lines().next().unwrap_or("").to_string();
                        table.get(&key).cloned().unwrap_or_else(|| vec![0.0; DIM])
                    })
                    .collect();
                Ok(out)
            })
        }
    }

    fn store(tmp: &std::path::Path) -> FlowStore {
        FlowStore::from_root(tmp.to_path_buf())
    }

    fn write_docs(s: &FlowStore, project: &str, ws: &str, docs: &[(&str, &str)]) {
        for (path, content) in docs {
            s.write(project, ws, path, content).unwrap();
        }
    }

    /// Leak a formatted name to `&'static str` so test doc tables can be built
    /// dynamically. Test-only; the leak is bounded by the test's doc count.
    fn leak_name(s: String) -> &'static str {
        std::boxed::Box::leak(s.into_boxed_str())
    }

    #[tokio::test]
    async fn search_returns_relevant_doc_below_threshold_via_linear_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        // Fewer than SEMANTIC_INDEX_MIN_DOCS: index stays off, search scans.
        write_docs(
            &s,
            "proj",
            "ws",
            &[
                ("goal.md", "auth"),
                ("plan.md", "payments"),
                ("qa.md", "auth"),
            ],
        );
        let emb = ScriptedEmbedder::new();
        emb.put("auth", vec![1.0, 0.0, 0.0, 0.0]);
        emb.put("payments", vec![0.0, 1.0, 0.0, 0.0]);
        emb.put("auth-query", vec![1.0, 0.0, 0.0, 0.0]);

        let hits = search(&s, &emb, "proj", "ws", "auth-query", 5)
            .await
            .unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert!(paths.contains(&"goal.md"));
        assert!(paths.contains(&"qa.md"));
        assert!(!paths.contains(&"plan.md"));
        assert_eq!(hits[0].score, 1.0);
    }

    #[tokio::test]
    async fn search_uses_index_above_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        // > SEMANTIC_INDEX_MIN_DOCS docs so the index activates.
        let docs: Vec<(&str, &str)> = (0..SEMANTIC_INDEX_MIN_DOCS + 2)
            .map(|i| {
                (
                    leak_name(format!("d{i}.md")),
                    if i == 3 { "auth" } else { "other" },
                )
            })
            .collect();
        write_docs(&s, "proj", "ws", &docs);

        let emb = ScriptedEmbedder::new();
        emb.put("auth", vec![1.0, 0.0, 0.0, 0.0]);
        emb.put("other", vec![0.0, 1.0, 0.0, 0.0]);
        emb.put("auth-query", vec![1.0, 0.0, 0.0, 0.0]);

        let added = reindex(&s, &emb, "proj", "ws").await.unwrap();
        assert_eq!(added, SEMANTIC_INDEX_MIN_DOCS + 2);

        let hits = search(&s, &emb, "proj", "ws", "auth-query", 1)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "d3.md");
    }

    #[tokio::test]
    async fn reindex_noops_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        write_docs(&s, "proj", "ws", &[("a.md", "x"), ("b.md", "y")]);
        let emb = ScriptedEmbedder::new();
        let added = reindex(&s, &emb, "proj", "ws").await.unwrap();
        assert_eq!(added, 0);
        assert!(s.read_index("proj", "ws").unwrap().is_empty());
    }

    #[tokio::test]
    async fn reindex_only_embeds_new_docs() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let docs: Vec<(&str, &str)> = (0..SEMANTIC_INDEX_MIN_DOCS + 1)
            .map(|i| (leak_name(format!("d{i}.md")), "x"))
            .collect();
        write_docs(&s, "proj", "ws", &docs);
        let emb = ScriptedEmbedder::new();

        let first = reindex(&s, &emb, "proj", "ws").await.unwrap();
        assert_eq!(first, SEMANTIC_INDEX_MIN_DOCS + 1);

        s.write("proj", "ws", "new.md", "x").unwrap();
        let second = reindex(&s, &emb, "proj", "ws").await.unwrap();
        assert_eq!(second, SEMANTIC_INDEX_MIN_DOCS + 2);
    }

    #[tokio::test]
    async fn reindex_drops_stale_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let docs: Vec<(&str, &str)> = (0..SEMANTIC_INDEX_MIN_DOCS + 1)
            .map(|i| (leak_name(format!("d{i}.md")), "x"))
            .collect();
        write_docs(&s, "proj", "ws", &docs);
        let emb = ScriptedEmbedder::new();
        reindex(&s, &emb, "proj", "ws").await.unwrap();
        assert!(s.read_index("proj", "ws").unwrap().get("d0.md").is_some());

        s.delete_workstream("proj", "ws").unwrap();
        let docs2: Vec<(&str, &str)> = (0..SEMANTIC_INDEX_MIN_DOCS + 1)
            .map(|i| (leak_name(format!("e{i}.md")), "y"))
            .collect();
        write_docs(&s, "proj", "ws", &docs2);
        reindex(&s, &emb, "proj", "ws").await.unwrap();
        let idx = s.read_index("proj", "ws").unwrap();
        assert!(idx.get("d0.md").is_none(), "stale entry should be gone");
        assert!(idx.get("e0.md").is_some());
    }

    #[tokio::test]
    async fn search_empty_workstream_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let emb = ScriptedEmbedder::new();
        let hits = search(&s, &emb, "proj", "ws", "anything", 5).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn search_filters_low_score_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        write_docs(&s, "proj", "ws", &[("a.md", "auth"), ("b.md", "unrelated")]);
        let emb = ScriptedEmbedder::new();
        emb.put("auth", vec![1.0, 0.0, 0.0, 0.0]);
        emb.put("unrelated", vec![0.0, 0.0, 1.0, 0.0]);
        emb.put("auth-query", vec![1.0, 0.0, 0.0, 0.0]);
        let hits = search(&s, &emb, "proj", "ws", "auth-query", 5)
            .await
            .unwrap();
        // "unrelated" is orthogonal (score 0) and filtered out by MIN_RESULT_SCORE.
        assert!(hits.iter().all(|h| h.path == "a.md"));
    }
}
