//! `ThreadHistory`-backed implementation of [`FlowSearchBackend`].
//!
//! Keyword-based for this cut: substring + simple term-frequency scoring over
//! each typed-log entry's content. The semantic embedding index is deferred
//! (plan out of scope). The backend copies entries out under the lock and
//! scores them without holding it, so the mutex is never held across `.await`.

use std::sync::Arc;

use super::flow_search::{FlowSearchBackend, FlowSearchHit, ListFuture, ReadFuture, SearchFuture};
use crate::agent::typed_log::{EntryType, ThreadHistory, ThreadId};

/// Backend built from a shared typed log. `project_id`/`workstream_id` are
/// captured so the tool can resolve the active workstream, and `root` scopes
/// the default projection reads (the root thread's writes are the workstream's
/// main documents: goal, plan, etc.).
pub(crate) struct HistorySearchBackend {
    history: Arc<std::sync::Mutex<ThreadHistory>>,
    project_id: String,
    workstream_id: String,
    root: ThreadId,
}

impl HistorySearchBackend {
    pub(crate) fn new(
        history: Arc<std::sync::Mutex<ThreadHistory>>,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
        root: ThreadId,
    ) -> Self {
        Self {
            history,
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
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(k.max(1));
            Ok(scored)
        })
    }

    fn read_document<'a>(
        &'a self,
        _project_id: &'a str,
        _workstream_id: &'a str,
        rel_path: &'a str,
    ) -> ReadFuture<'a> {
        let body = read_path(&self.history, rel_path);
        Box::pin(async move { body.ok_or_else(|| format!("flow document not found: {rel_path}")) })
    }

    fn list_documents<'a>(
        &'a self,
        _project_id: &'a str,
        _workstream_id: &'a str,
    ) -> ListFuture<'a> {
        let hist = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let mut paths: Vec<String> = ENTRY_TYPES
            .iter()
            .filter_map(|&et| {
                hist.projection(et, &self.root)
                    .map(|_| format!("{}:{}", et.as_str(), self.root.as_str()))
            })
            .collect();
        drop(hist);
        paths.sort();
        Box::pin(async move { Ok(paths) })
    }
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
            store, "proj", "ws",
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
        let backend = HistorySearchBackend::new(hist, "proj", "ws", ThreadId::new("ws"));
        let hits = backend.search("proj", "ws", "login goal", 5).await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].path.starts_with("goal:"), "got: {:?}", hits[0].path);
    }

    #[tokio::test]
    async fn read_document_returns_projection_body() {
        let (_guard, store) = tmp_store();
        let hist = Arc::new(std::sync::Mutex::new(ThreadHistory::open(
            store, "proj", "ws",
        )));
        {
            let mut h = hist.lock().unwrap();
            h.append(ThreadId::new("ws"), EntryType::Goal, "the goal body");
        }
        let backend = HistorySearchBackend::new(hist, "proj", "ws", ThreadId::new("ws"));
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
            store, "proj", "ws",
        )));
        let backend = HistorySearchBackend::new(hist, "proj", "ws", ThreadId::new("ws"));
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
            store, "proj", "ws",
        )));
        {
            let mut h = hist.lock().unwrap();
            h.append(ThreadId::new("ws"), EntryType::Goal, "g");
            h.append(ThreadId::new("ws"), EntryType::Plan, "p");
        }
        let backend = HistorySearchBackend::new(hist, "proj", "ws", ThreadId::new("ws"));
        let docs = backend.list_documents("proj", "ws").await.unwrap();
        assert!(docs.contains(&"goal:ws".to_string()));
        assert!(docs.contains(&"plan:ws".to_string()));
    }
}
