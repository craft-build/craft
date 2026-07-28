//! Semantic search over the typed log's current-state projections, plus the
//! `FlowSearchBackend` impl injected into the `flow_search`/`history.query`
//! tool.
//!
//! Migrated from the former `craft-flow/src/search.rs`. Two layers:
//! - [`reindex`] / [`search_docs`] are pure async functions over a `FlowStore`
//!   doc-level index (the former `craft-flow` search). Kept so the `flow://`
//!   internal-URL read path and workstream-level listing keep working.
//! - [`FlowSearchBackendImpl`] backs the agent's `flow_search` tool. In Phase 2
//!   it searches the [`super::typed_log::ThreadHistory`]'s current-state
//!   projections **within an explicit [`Scope`]**, so a narrow turn type's
//!   `history.query` cannot surface another thread's diff or the root goal
//!   (design §4: "semantic search inside that scope, never outside it").
//!
//! Design ref: `turn-type-agent-loop-design.md` §4 (Reading history).

use std::sync::Arc;

use tracing::warn;

use craft_storage::flow::{FlowStore, SEMANTIC_INDEX_MIN_DOCS};

use super::typed_log::{EntryType, LogEntry, ThreadHistory, ThreadId};

/// Minimum relevance score for a hit to be returned (mirrors craft-agent's
/// semantic LOW_RELEVANCE floor).
const MIN_RESULT_SCORE: f32 = 0.30;

/// Abstraction over embedding production so the ranking logic is testable
/// without the ONNX runtime. The real impl ([`OnnxEmbedder`]) wraps
/// `EmbeddingService`; tests inject a deterministic fake.
pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, texts: Vec<String>) -> EmbedFuture<'a>;
}

pub type EmbedFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<Vec<f32>>, EmbedError>> + Send + 'a>,
>;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("flow embedder error: {0}")]
    Other(String),
    #[error("embedder returned no embedding")]
    NoResult,
}

/// Real embedder backed by the shared ONNX `EmbeddingService`.
pub struct OnnxEmbedder {
    service: crate::agent::semantic::EmbeddingService,
}

impl OnnxEmbedder {
    pub fn new(service: crate::agent::semantic::EmbeddingService) -> Self {
        Self { service }
    }
}

impl Embedder for OnnxEmbedder {
    fn embed<'a>(&'a self, texts: Vec<String>) -> EmbedFuture<'a> {
        let service = &self.service;
        Box::pin(async move {
            service
                .embed_batch(texts)
                .await
                .map_err(|e| EmbedError::Other(e.to_string()))
        })
    }
}

/// Which entries a `history.query` may search. Built from a turn type's
/// [`super::turn_type::ReadPolicy`] (the `query_scope` slice). A projection is
/// in-scope when its `entry_type` is allowed at the given thread level
/// (own/parent/root). General's scope is unrestricted: an empty `Scope` (no
/// constraints) searches everything (design §4: "General's scope is not 'a
/// bigger window,' it's no filter at all").
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// `(entry_type, thread_level)` constraints. Empty = unrestricted.
    pub constraints: Vec<ScopeEntry>,
    /// The thread the querying turn lives in. `Own` projections resolve
    /// against this; `Parent` against its parent; `Root` against the root.
    pub own_thread: ThreadId,
    pub parent_thread: Option<ThreadId>,
    pub root_thread: ThreadId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeEntry {
    pub entry: EntryType,
    pub level: super::turn_type::ThreadLevel,
}

impl Scope {
    /// General's unrestricted scope: searches every projection in the log.
    pub fn unrestricted(own: ThreadId, root: ThreadId) -> Self {
        Self {
            constraints: Vec::new(),
            own_thread: own,
            parent_thread: None,
            root_thread: root,
        }
    }

    /// Does this projection fall within the scope? An empty constraints list is
    /// unrestricted (General). Otherwise the projection's (entry, thread) must
    /// match a constraint at the right level.
    pub fn allows(&self, entry: EntryType, thread: &ThreadId) -> bool {
        if self.constraints.is_empty() {
            return true;
        }
        for c in &self.constraints {
            if c.entry != entry {
                continue;
            }
            let target = match c.level {
                super::turn_type::ThreadLevel::Own => &self.own_thread,
                super::turn_type::ThreadLevel::Parent => match &self.parent_thread {
                    Some(p) => p,
                    None => continue,
                },
                super::turn_type::ThreadLevel::Root => &self.root_thread,
            };
            if target == thread {
                return true;
            }
        }
        false
    }
}

/// Embed every persisted doc that lacks an embedding, persisting the updated
/// index. Returns the number of newly embedded docs. A no-op (returns 0) when
/// the workstream is below the activation threshold. Migrated from
/// `craft-flow::search::reindex`.
pub async fn reindex(
    store: &FlowStore,
    embedder: &dyn Embedder,
    project_id: &str,
    workstream_id: &str,
) -> Result<usize, EmbedError> {
    let live = store
        .list(project_id, workstream_id)
        .map_err(|e| EmbedError::Other(e.to_string()))?;
    if live.len() <= SEMANTIC_INDEX_MIN_DOCS {
        return Ok(0);
    }
    let mut index = store
        .read_index(project_id, workstream_id)
        .map_err(|e| EmbedError::Other(e.to_string()))?;
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
        .map_err(|e| EmbedError::Other(e.to_string()))?;
    Ok(added)
}

/// Rank a workstream's documents against `query` (doc-level, migrated from
/// `craft-flow::search::search`). Used by the workstream-level index; the
/// scope-enforced projection search lives in [`search_projections`].
pub async fn search_docs(
    store: &FlowStore,
    embedder: &dyn Embedder,
    project_id: &str,
    workstream_id: &str,
    query: &str,
    k: usize,
) -> Result<Vec<DocHit>, EmbedError> {
    let live = store
        .list(project_id, workstream_id)
        .map_err(|e| EmbedError::Other(e.to_string()))?;
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let k = k.max(1);
    let query_emb = embed_query(embedder, query).await?;
    let index = store
        .read_index(project_id, workstream_id)
        .map_err(|e| EmbedError::Other(e.to_string()))?;
    let use_index = live.len() > SEMANTIC_INDEX_MIN_DOCS
        && !index.is_empty()
        && live.iter().all(|p| index.get(p).is_some());
    let mut scored: Vec<DocHit> = if use_index {
        live.iter()
            .filter_map(|p| {
                index.get(p).map(|emb| DocHit {
                    path: p.clone(),
                    score: crate::agent::cosine_similarity(&query_emb, emb),
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
            .map(|(p, emb)| DocHit {
                path: p.clone(),
                score: crate::agent::cosine_similarity(&query_emb, &emb),
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

/// One ranked doc-level result.
#[derive(Debug, Clone)]
pub struct DocHit {
    pub path: String,
    pub score: f32,
}

/// One ranked projection-level result (a current-state entry in the typed log).
#[derive(Debug, Clone)]
pub struct ProjectionHit {
    pub entry_type: EntryType,
    pub thread_id: ThreadId,
    pub seq: super::typed_log::Seq,
    pub score: f32,
}

/// Rank the typed log's current-state projections within `scope` against
/// `query`, returning the top `k` by cosine similarity (design §4: query the
/// current-state projection by default). Projections outside `scope` are never
/// ranked — they are not in the corpus to begin with.
pub async fn search_projections(
    history: &ThreadHistory,
    scope: &Scope,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<ProjectionHit>, EmbedError> {
    let candidates = collect_candidates(history, scope);
    rank_candidates(candidates, embedder, query, k).await
}

/// Collect in-scope current-state projection entries (cloned) from `history`.
/// Factored out so a caller holding a `Mutex<ThreadHistory>` can collect under
/// the lock, drop the guard, then rank without holding it across an await.
pub fn collect_candidates(history: &ThreadHistory, scope: &Scope) -> Vec<LogEntry> {
    history
        .log()
        .iter()
        .filter(|e| scope.allows(e.entry_type, &e.thread_id))
        .cloned()
        .collect()
}

/// Rank already-collected candidate entries against `query`.
pub async fn rank_candidates(
    candidates: Vec<LogEntry>,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<ProjectionHit>, EmbedError> {
    let k = k.max(1);
    let query_emb = embed_query(embedder, query).await?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let contents: Vec<String> = candidates.iter().map(|e| e.content.clone()).collect();
    let embs = embedder.embed(contents).await?;
    let mut scored: Vec<ProjectionHit> = candidates
        .iter()
        .zip(embs)
        .map(|(e, emb)| ProjectionHit {
            entry_type: e.entry_type,
            thread_id: e.thread_id.clone(),
            seq: e.seq,
            score: crate::agent::cosine_similarity(&query_emb, &emb),
        })
        .collect();
    scored.retain(|h| h.score >= MIN_RESULT_SCORE);
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(k);
    Ok(scored)
}

async fn embed_query(embedder: &dyn Embedder, query: &str) -> Result<Vec<f32>, EmbedError> {
    embedder
        .embed(vec![query.to_string()])
        .await?
        .into_iter()
        .next()
        .ok_or(EmbedError::NoResult)
}

/// `FlowSearchBackend` impl backed by the typed log's projections + scope.
/// `search` ranks in-scope projections; `read_document`/`list_documents`
/// delegate to the `FlowStore` so the `flow://<path>` read path keeps working.
pub struct FlowSearchBackendImpl {
    store: Arc<FlowStore>,
    embedder: Arc<dyn Embedder>,
    history: Arc<std::sync::Mutex<ThreadHistory>>,
    scope: Scope,
    project_id: String,
    workstream_id: String,
}

impl FlowSearchBackendImpl {
    pub fn new(
        store: Arc<FlowStore>,
        embedder: Arc<dyn Embedder>,
        history: Arc<std::sync::Mutex<ThreadHistory>>,
        scope: Scope,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            embedder,
            history,
            scope,
            project_id: project_id.into(),
            workstream_id: workstream_id.into(),
        }
    }
}

impl crate::tools::flow_search::FlowSearchBackend for FlowSearchBackendImpl {
    fn workstream(&self) -> Option<(String, String)> {
        Some((self.project_id.clone(), self.workstream_id.clone()))
    }

    fn search<'a>(
        &'a self,
        _project_id: &'a str,
        _workstream_id: &'a str,
        query: &'a str,
        k: usize,
    ) -> crate::tools::flow_search::SearchFuture<'a> {
        let embedder = Arc::clone(&self.embedder);
        let history = Arc::clone(&self.history);
        let scope = self.scope.clone();
        Box::pin(async move {
            let candidates = {
                let hist = history.lock().unwrap_or_else(|e| e.into_inner());
                collect_candidates(&hist, &scope)
            };
            let hits = rank_candidates(candidates, embedder.as_ref(), query, k)
                .await
                .map_err(|e| e.to_string())?;
            Ok(hits
                .into_iter()
                .map(|h| crate::tools::flow_search::FlowSearchHit {
                    path: format!("{}@{}", h.entry_type.as_str(), h.thread_id),
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
    ) -> crate::tools::flow_search::ReadFuture<'a> {
        Box::pin(async move {
            self.store
                .read(project_id, workstream_id, rel_path)
                .map_err(|e| e.to_string())
        })
    }

    fn list_documents<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
    ) -> crate::tools::flow_search::ListFuture<'a> {
        Box::pin(async move {
            self.store
                .list(project_id, workstream_id)
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

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
        fn embed<'a>(&'a self, texts: Vec<String>) -> EmbedFuture<'a> {
            Box::pin(async move {
                let table = self.table.lock().unwrap();
                Ok(texts
                    .into_iter()
                    .map(|t| {
                        let key = t.lines().next().unwrap_or("").to_string();
                        table.get(&key).cloned().unwrap_or_else(|| vec![0.0; DIM])
                    })
                    .collect())
            })
        }
    }

    fn store(tmp: &std::path::Path) -> FlowStore {
        FlowStore::from_root(tmp.to_path_buf())
    }

    #[tokio::test]
    async fn search_projections_returns_only_in_scope_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Arc::new(store(tmp.path()));
        let mut hist = ThreadHistory::open(Arc::clone(&s), "proj", "ws");
        let root = hist.root_thread_id().clone();
        hist.append(root.clone(), EntryType::Goal, "root-goal");
        let c1 = ThreadId::new("c1");
        let c2 = ThreadId::new("c2");
        hist.append(c1.clone(), EntryType::Diff, "child1-diff");
        hist.append(c2.clone(), EntryType::Diff, "child2-diff");
        let arc = Arc::new(std::sync::Mutex::new(hist));
        let emb = ScriptedEmbedder::new();
        emb.put("root-goal", vec![1.0, 0.0, 0.0, 0.0]);
        emb.put("child1-diff", vec![0.0, 1.0, 0.0, 0.0]);
        emb.put("child2-diff", vec![0.0, 1.0, 0.0, 0.0]);
        emb.put("q", vec![0.0, 1.0, 0.0, 0.0]);

        // Scope: only Diff at Own level for c1. Must NOT surface c2's diff or root goal.
        let scope = Scope {
            constraints: vec![ScopeEntry {
                entry: EntryType::Diff,
                level: super::super::turn_type::ThreadLevel::Own,
            }],
            own_thread: c1.clone(),
            parent_thread: None,
            root_thread: root.clone(),
        };
        let candidates = {
            let hist = arc.lock().unwrap();
            collect_candidates(&hist, &scope)
        };
        let hits = rank_candidates(candidates, &emb, "q", 5).await.unwrap();
        assert!(
            hits.iter().all(|h| h.thread_id == c1),
            "c2 or root leaked into c1's scope"
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].thread_id, c1);
    }

    #[tokio::test]
    async fn unrestricted_scope_sees_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Arc::new(store(tmp.path()));
        let mut hist = ThreadHistory::open(Arc::clone(&s), "proj", "ws");
        let root = hist.root_thread_id().clone();
        hist.append(root.clone(), EntryType::Goal, "root-goal");
        hist.append(ThreadId::new("c1"), EntryType::Diff, "child1-diff");
        let arc = Arc::new(std::sync::Mutex::new(hist));
        let emb = ScriptedEmbedder::new();
        emb.put("root-goal", vec![1.0, 0.0, 0.0, 0.0]);
        emb.put("child1-diff", vec![1.0, 0.0, 0.0, 0.0]);
        emb.put("q", vec![1.0, 0.0, 0.0, 0.0]);
        let scope = Scope::unrestricted(root.clone(), root.clone());
        let candidates = {
            let hist = arc.lock().unwrap();
            collect_candidates(&hist, &scope)
        };
        let hits = rank_candidates(candidates, &emb, "q", 5).await.unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn search_projections_empty_workstream_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Arc::new(store(tmp.path()));
        let hist = ThreadHistory::open(Arc::clone(&s), "proj", "ws");
        let emb = ScriptedEmbedder::new();
        let scope =
            Scope::unrestricted(hist.root_thread_id().clone(), hist.root_thread_id().clone());
        let hits = search_projections(&hist, &scope, &emb, "q", 5)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
