//! `ThreadHistory`-backed implementation of [`FlowSearchBackend`].
//!
//! Two corpora: the typed log's entries (thread-history domain, keyword-scored
//! here; the scoped semantic ranking lives in `flow_index.rs`) and the
//! workstream's persisted documents, searched through the argosy [`Index`]
//! with a fastembed provider and a file-backed vector store. When the
//! embedding model is unavailable (offline first run), document search falls
//! back to the same keyword scoring. The backend copies entries out under the
//! lock and scores them without holding it, so the mutex is never held across
//! `.await`.

use std::sync::Arc;

use argosy::bundle::Namespace;
use argosy::context::ProjectContext;
use argosy::index::{Filter, Index, Query};
use craft_storage::argosy_index::{CraftEmbeddingProvider, FileVecStore, model_cache_present};
use craft_storage::flow::FlowStore;

use super::flow_search::{FlowSearchBackend, FlowSearchHit, ListFuture, ReadFuture, SearchFuture};
use crate::agent::typed_log::{EntryType, ThreadHistory, ThreadId};

/// Backend built from a shared typed log and the workstream's document store.
/// `project_id`/`workstream_id` are captured so the tool can resolve the
/// active workstream, and `root` scopes the default projection reads (the
/// root thread's writes are the workstream's main documents: goal, plan,
/// etc.).
pub(crate) struct HistorySearchBackend {
    history: Arc<std::sync::Mutex<ThreadHistory>>,
    store: Arc<FlowStore>,
    project_id: String,
    workstream_id: String,
    root: ThreadId,
}

impl HistorySearchBackend {
    pub(crate) fn new(
        history: Arc<std::sync::Mutex<ThreadHistory>>,
        store: Arc<FlowStore>,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
        root: ThreadId,
    ) -> Self {
        Self {
            history,
            store,
            project_id: project_id.into(),
            workstream_id: workstream_id.into(),
            root,
        }
    }

    fn snapshot_entries(&self) -> Vec<(EntryType, ThreadId, String)> {
        let hist = self.history.lock().unwrap_or_else(|e| e.into_inner());
        hist.log()
            .iter()
            .map(|e| (e.entry_type, e.thread_id.clone(), e.content.clone()))
            .collect()
    }
}

const ENTRY_TYPES: [EntryType; 14] = [
    EntryType::UserRequest,
    EntryType::CodebaseContext,
    EntryType::ResearchNotes,
    EntryType::Goal,
    EntryType::Plan,
    EntryType::Requirement,
    EntryType::Diff,
    EntryType::ReviewFindings,
    EntryType::QaReport,
    EntryType::Report,
    EntryType::IntegrationCheckpoint,
    EntryType::VerificationReport,
    EntryType::GeneralTurn,
    EntryType::AdvisorNote,
];

impl FlowSearchBackend for HistorySearchBackend {
    fn workstream(&self) -> Option<(String, String)> {
        Some((self.project_id.clone(), self.workstream_id.clone()))
    }

    fn search<'a>(
        &'a self,
        _project_id: &'a str,
        _workstream_id: &'a str,
        query: &'a str,
        k: usize,
    ) -> SearchFuture<'a> {
        let terms = tokenize(query);
        let entries = self.snapshot_entries();
        let store = Arc::clone(&self.store);
        let project_id = self.project_id.clone();
        let workstream_id = self.workstream_id.clone();
        Box::pin(async move {
            let mut scored: Vec<FlowSearchHit> = entries
                .into_iter()
                .map(|(entry_type, thread, content)| {
                    let score = score_entry(&content, &terms);
                    FlowSearchHit {
                        path: format!("{}:{}", entry_type.as_str(), thread.as_str()),
                        score,
                    }
                })
                .filter(|h| h.score > 0.0)
                .collect();
            // Projection and document scores live on different scales
            // (keyword term-frequency vs embedding cosine), so they are not
            // merged by score: the typed log's current state comes first, and
            // documents fill the remaining slots.
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            if scored.len() < k {
                scored.extend(
                    search_documents(
                        store,
                        &project_id,
                        &workstream_id,
                        query,
                        &terms,
                        k - scored.len(),
                    )
                    .await
                    .unwrap_or_default(),
                );
            }
            Ok(scored)
        })
    }

    fn read_document<'a>(
        &'a self,
        project_id: &'a str,
        workstream_id: &'a str,
        rel_path: &'a str,
    ) -> ReadFuture<'a> {
        let projection = read_path(&self.history, rel_path);
        let store = Arc::clone(&self.store);
        let project_id = project_id.to_string();
        let workstream_id = workstream_id.to_string();
        let rel_path = rel_path.to_string();
        Box::pin(async move {
            if let Some(body) = projection {
                return Ok(body);
            }
            store
                .read(&project_id, &workstream_id, &rel_path)
                .map_err(|_| format!("flow document not found: {rel_path}"))
        })
    }

    fn list_documents<'a>(&'a self, project_id: &'a str, workstream_id: &'a str) -> ListFuture<'a> {
        let hist = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let mut paths: Vec<String> = ENTRY_TYPES
            .iter()
            .filter_map(|&et| {
                hist.projection(et, &self.root)
                    .map(|_| format!("{}:{}", et.as_str(), self.root.as_str()))
            })
            .collect();
        drop(hist);
        let store = Arc::clone(&self.store);
        let project_id = project_id.to_string();
        let workstream_id = workstream_id.to_string();
        Box::pin(async move {
            paths.extend(
                store
                    .list(&project_id, &workstream_id)
                    .map_err(|e| e.to_string())?,
            );
            paths.sort();
            Ok(paths)
        })
    }
}

/// Over-fetch factor for the semantic pass: namespace-filtered hits include
/// every workstream, so fetch enough to survive the workstream post-filter.
const SEMANTIC_OVERFETCH: usize = 4;

/// Rank the workstream's persisted documents against `query` through the
/// argosy index (embedding model downloads on first use). Falls back to
/// keyword scoring when the index cannot be built or searched.
async fn search_documents(
    store: Arc<FlowStore>,
    project_id: &str,
    workstream_id: &str,
    query: &str,
    terms: &[String],
    k: usize,
) -> Result<Vec<FlowSearchHit>, String> {
    if !model_cache_present() {
        return keyword_document_hits(&store, project_id, workstream_id, terms, k);
    }
    let argosy_dir = store.argosy_dir(project_id);
    let index_path = argosy_dir.join(".argosy").join("index.json");
    let project_id = project_id.to_string();
    let workstream_id = workstream_id.to_string();
    let query = query.to_string();
    let semantic = tokio::task::spawn_blocking(move || {
        let context =
            ProjectContext::open(&argosy_dir, []).map_err(|e| format!("argosy open: {e}"))?;
        let provider = CraftEmbeddingProvider::new().map_err(|e| format!("embedder init: {e}"))?;
        let vec_store =
            FileVecStore::open(&index_path).map_err(|e| format!("vector store: {e}"))?;
        let mut index = Index::new(provider, vec_store);
        index
            .reconcile(&context)
            .map_err(|e| format!("index reconcile: {e}"))?;
        let search = Query {
            filter: Filter {
                namespaces: Some(vec![Namespace::Document]),
                ..Default::default()
            },
            ..Query::unscoped(query, k * SEMANTIC_OVERFETCH)
        };
        index
            .search(&context, &search)
            .map_err(|e| format!("index search: {e}"))
    })
    .await
    .map_err(|e| format!("index task: {e}"))?;

    let prefix = format!("document/{workstream_id}/");
    match semantic {
        Ok(hits) => Ok(hits
            .into_iter()
            .filter(|h| h.concept.id.as_str().starts_with(&prefix))
            .filter(|h| h.score > 0.0)
            .map(|h| FlowSearchHit {
                path: h
                    .concept
                    .id
                    .as_str()
                    .strip_prefix(&prefix)
                    .unwrap_or(h.concept.id.as_str())
                    .to_string(),
                score: h.score,
            })
            .take(k)
            .collect()),
        Err(_) => keyword_document_hits(&store, &project_id, &workstream_id, terms, k),
    }
}

fn keyword_document_hits(
    store: &FlowStore,
    project_id: &str,
    workstream_id: &str,
    terms: &[String],
    k: usize,
) -> Result<Vec<FlowSearchHit>, String> {
    let mut hits = Vec::new();
    for path in store
        .list(project_id, workstream_id)
        .map_err(|e| e.to_string())?
    {
        let content = store
            .read(project_id, workstream_id, &path)
            .map_err(|e| e.to_string())?;
        let score = score_entry(&content, terms);
        if score > 0.0 {
            hits.push(FlowSearchHit { path, score });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(k);
    Ok(hits)
}

fn read_path(history: &Arc<std::sync::Mutex<ThreadHistory>>, rel_path: &str) -> Option<String> {
    let (entry_str, thread_str) = rel_path.split_once(':')?;
    let entry_type = EntryType::parse(entry_str)?;
    let thread = ThreadId::new(thread_str);
    let hist = history.lock().unwrap_or_else(|e| e.into_inner());
    hist.projection(entry_type, &thread)
        .map(|e| e.content.clone())
        .or_else(|| {
            hist.entries(entry_type, &thread)
                .last()
                .map(|e| e.content.clone())
        })
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

/// Score an entry by counting non-overlapping case-insensitive occurrences of
/// each query term, normalized by the entry's length so short, focused docs
/// rank above long ones with the same raw hits.
fn score_entry(content: &str, terms: &[String]) -> f32 {
    if terms.is_empty() {
        return 0.0;
    }
    let lower = content.to_ascii_lowercase();
    let mut hits = 0usize;
    for term in terms {
        if let Some(at) = lower.find(term) {
            hits += 1;
            let mut rest = &lower[at + term.len()..];
            while let Some(next) = rest.find(term) {
                hits += 1;
                rest = &rest[next + term.len()..];
            }
        }
    }
    if hits == 0 {
        return 0.0;
    }
    let len = lower.split_whitespace().count().max(1) as f32;
    hits as f32 / len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::flow_search::FlowSearchBackend;
    use craft_storage::flow::FlowStore;

    fn tmp_store() -> (tempfile::TempDir, Arc<FlowStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::from_root(dir.path().to_path_buf()));
        (dir, store)
    }

    #[tokio::test]
    async fn search_ranks_goal_for_goal_query() {
        let (_guard, store) = tmp_store();
        let hist = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            Arc::clone(&store),
            "proj",
            "ws",
        )));
        {
            let mut h = hist.lock().unwrap();
            h.append(ThreadId::new("ws"), EntryType::Goal, "ship the login flow");
            h.append(
                ThreadId::new("ws"),
                EntryType::Plan,
                "unrelated notes about docker",
            );
        }
        let backend = HistorySearchBackend::new(hist, store, "proj", "ws", ThreadId::new("ws"));
        let hits = backend.search("proj", "ws", "login goal", 5).await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].path.starts_with("goal:"), "got: {:?}", hits[0].path);
    }

    #[tokio::test]
    async fn read_document_returns_projection_body() {
        let (_guard, store) = tmp_store();
        let hist = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            Arc::clone(&store),
            "proj",
            "ws",
        )));
        {
            let mut h = hist.lock().unwrap();
            h.append(ThreadId::new("ws"), EntryType::Goal, "the goal body");
        }
        let backend = HistorySearchBackend::new(hist, store, "proj", "ws", ThreadId::new("ws"));
        let body = backend
            .read_document("proj", "ws", "goal:ws")
            .await
            .unwrap();
        assert!(body.contains("the goal body"));
    }

    #[tokio::test]
    async fn read_document_errors_on_unknown_path() {
        let (_guard, store) = tmp_store();
        let hist = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            Arc::clone(&store),
            "proj",
            "ws",
        )));
        let backend = HistorySearchBackend::new(hist, store, "proj", "ws", ThreadId::new("ws"));
        let err = backend
            .read_document("proj", "ws", "goal:nope")
            .await
            .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[tokio::test]
    async fn list_documents_enumerates_root_projections() {
        let (_guard, store) = tmp_store();
        let hist = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            Arc::clone(&store),
            "proj",
            "ws",
        )));
        {
            let mut h = hist.lock().unwrap();
            h.append(ThreadId::new("ws"), EntryType::Goal, "g");
            h.append(ThreadId::new("ws"), EntryType::Plan, "p");
        }
        let backend = HistorySearchBackend::new(hist, store, "proj", "ws", ThreadId::new("ws"));
        let docs = backend.list_documents("proj", "ws").await.unwrap();
        assert!(docs.contains(&"goal:ws".to_string()));
        assert!(docs.contains(&"plan:ws".to_string()));
    }
}
