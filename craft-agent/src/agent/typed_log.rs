//! Typed, append-only history log with synchronous projections.
//!
//! [`ThreadHistory`] is the durable, append-only, typed log keyed by thread
//! (design §3). Alongside the log, current-state projections exist for the
//! entries that need one: for a given `(entry type, thread)`, the projection
//! is the latest entry of that type in that thread, so "what's the plan right
//! now" never requires replaying the log to answer.
//!
//! The one strict rule (design §3): a write isn't done until every projection
//! it touches is updated too, in the same [`ThreadHistory::append`] call. No
//! async gap.
//!
//! Persistence lives through [`craft_storage::flow::FlowStore`]: the log as
//! `log.jsonl` (one entry per line), the projection map as `projections.json`,
//! the semantic index as `index.json` (reusing `FlowStore`'s existing index
//! plumbing). Resume reconstructs the log + projections from disk; a
//! generalized reconcile detects and repairs drift between persisted
//! projections and on-disk documents (migrated from the former
//! `craft-flow/src/reconcile.rs`).
//!
//! Design ref: `turn-type-agent-loop-design.md` §3 (History) and §4 (Reading
//! history). `history.query` defaults to the current-state projection, not the
//! raw log; a turn can explicitly ask for the raw log when it wants history.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use craft_storage::flow::FlowStore;

/// One typed entry in the append-only log. Each variant is the write shape for
/// one [`super::turn_type::TurnType`] (design §8.1): `Goal` for TPM, `Plan` for
/// Plan, `Diff` for Execute, and so on. `GeneralTurn` is the verbatim write
/// General makes every turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    UserRequest,
    CodebaseContext,
    ResearchNotes,
    Goal,
    Plan,
    Requirement,
    Diff,
    ReviewFindings,
    QaReport,
    Report,
    IntegrationCheckpoint,
    VerificationReport,
    GeneralTurn,
    /// An addressed note from the Advisor (Phase 3). Recorded in the parent
    /// thread's history when the Advisor forces a transition; the next turn
    /// sees it alongside its pinned reads (the addressed-note exception
    /// channel, design §7).
    AdvisorNote,
}

impl EntryType {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryType::UserRequest => "user_request",
            EntryType::CodebaseContext => "codebase_context",
            EntryType::ResearchNotes => "research_notes",
            EntryType::Goal => "goal",
            EntryType::Plan => "plan",
            EntryType::Requirement => "requirement",
            EntryType::Diff => "diff",
            EntryType::ReviewFindings => "review_findings",
            EntryType::QaReport => "qa_report",
            EntryType::Report => "report",
            EntryType::IntegrationCheckpoint => "integration_checkpoint",
            EntryType::VerificationReport => "verification_report",
            EntryType::GeneralTurn => "general_turn",
            EntryType::AdvisorNote => "advisor_note",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "user_request" => EntryType::UserRequest,
            "codebase_context" => EntryType::CodebaseContext,
            "research_notes" => EntryType::ResearchNotes,
            "goal" => EntryType::Goal,
            "plan" => EntryType::Plan,
            "requirement" => EntryType::Requirement,
            "diff" => EntryType::Diff,
            "review_findings" => EntryType::ReviewFindings,
            "qa_report" => EntryType::QaReport,
            "report" => EntryType::Report,
            "integration_checkpoint" => EntryType::IntegrationCheckpoint,
            "verification_report" => EntryType::VerificationReport,
            "general_turn" => EntryType::GeneralTurn,
            "advisor_note" => EntryType::AdvisorNote,
            _ => return None,
        })
    }
}

/// Identifier for a thread in the thread tree. The root thread id is the
/// workstream id (design §5: the root thread absorbs the workstream concept),
/// so `FlowStore`'s `<project>/flow/<workstream_id>/` path becomes
/// `<project>/flow/<root_thread_id>/` with zero storage migration. Child thread
/// ids are minted at spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ThreadId(pub String);

impl ThreadId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Monotonic sequence number for one entry within a workstream's log. Assigned
/// by [`ThreadHistory::append`]; starts at 0 and increments by 1 per entry.
pub type Seq = u64;

/// One record in the append-only log. `content` is the write payload (a JSON
/// document for structured turn types, verbatim text for `GeneralTurn`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub seq: Seq,
    pub thread_id: ThreadId,
    pub entry_type: EntryType,
    pub content: String,
}

/// The current-state projection key: the latest entry of `entry_type` in
/// `thread_id`. Updated synchronously in the same [`ThreadHistory::append`]
/// call that produces the entry it points at (design §3).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectionKey {
    pub entry: EntryType,
    pub thread: ThreadId,
}

/// Append-only typed log with synchronous current-state projections (design
/// §3). Owns the in-memory log + projection map; persists through a
/// [`FlowStore`].
pub struct ThreadHistory {
    log: Vec<LogEntry>,
    projections: BTreeMap<ProjectionKey, Seq>,
    next_seq: Seq,
    store: Arc<FlowStore>,
    project_id: String,
    /// The root thread id; equals the workstream id (design §5).
    root_thread_id: ThreadId,
}

impl ThreadHistory {
    /// Open the history for `workstream_id` under `project_id`, loading any
    /// persisted log + projections from `store`. The root thread id is the
    /// workstream id, so the on-disk path is `<project>/flow/<workstream_id>/`.
    pub fn open(
        store: Arc<FlowStore>,
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
    ) -> Self {
        let root_thread_id = ThreadId::new(workstream_id);
        let project_id = project_id.into();
        let mut hist = Self {
            log: Vec::new(),
            projections: BTreeMap::new(),
            next_seq: 0,
            store,
            project_id,
            root_thread_id,
        };
        let _ = hist.load();
        hist
    }

    pub fn root_thread_id(&self) -> &ThreadId {
        &self.root_thread_id
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// The full append-only log, oldest first.
    pub fn log(&self) -> &[LogEntry] {
        &self.log
    }

    /// Append an entry and synchronously update its projection in the same
    /// call (design §3: no async gap). Persists the log + projections to the
    /// `FlowStore` so a crash or resume re-enters at the right thread/type.
    /// Returns the assigned sequence number.
    pub fn append(
        &mut self,
        thread_id: ThreadId,
        entry_type: EntryType,
        content: impl Into<String>,
    ) -> Seq {
        let seq = self.next_seq;
        self.next_seq += 1;
        let entry = LogEntry {
            seq,
            thread_id: thread_id.clone(),
            entry_type,
            content: content.into(),
        };
        self.log.push(entry.clone());
        let key = ProjectionKey {
            entry: entry_type,
            thread: thread_id,
        };
        self.projections.insert(key, seq);
        let _ = self.persist();
        seq
    }

    /// Current-state projection: the latest entry of `entry_type` in
    /// `thread_id`, or `None` when no such entry exists (design §3).
    pub fn projection(&self, entry_type: EntryType, thread_id: &ThreadId) -> Option<&LogEntry> {
        let key = ProjectionKey {
            entry: entry_type,
            thread: thread_id.clone(),
        };
        let seq = *self.projections.get(&key)?;
        self.log.get(seq as usize)
    }

    /// All entries of `entry_type` in `thread_id`, in log order. Used by
    /// `history.query` when a turn explicitly asks for the raw log (an audit
    /// trail, what an earlier attempt tried) rather than the default
    /// current-state projection (design §4).
    pub fn entries(&self, entry_type: EntryType, thread_id: &ThreadId) -> Vec<&LogEntry> {
        self.log
            .iter()
            .filter(|e| e.entry_type == entry_type && e.thread_id == *thread_id)
            .collect()
    }

    /// All entries in `thread_id`, in log order.
    pub fn thread_entries(&self, thread_id: &ThreadId) -> Vec<&LogEntry> {
        self.log
            .iter()
            .filter(|e| e.thread_id == *thread_id)
            .collect()
    }

    /// Persist the log as `log.jsonl` and the projection map as
    /// `projections.json` under the workstream dir. Best-effort: a write
    /// failure logs and continues rather than failing the run (the on-disk
    /// docs are the source of truth for resume; this just speeds re-entry).
    fn persist(&self) -> Result<(), FlowStoreError> {
        let mut jsonl = String::new();
        for entry in &self.log {
            let line = serde_json::to_string(entry).map_err(FlowStoreError::Serde)?;
            jsonl.push_str(&line);
            jsonl.push('\n');
        }
        self.store
            .write(
                &self.project_id,
                self.root_thread_id.as_str(),
                LOG_FILE,
                &jsonl,
            )
            .map_err(FlowStoreError::Storage)?;
        let proj: BTreeMap<String, Seq> = self
            .projections
            .iter()
            .map(|(k, v)| (format!("{}@{}", k.entry.as_str(), k.thread), *v))
            .collect();
        let proj_json = serde_json::to_string(&proj).map_err(FlowStoreError::Serde)?;
        self.store
            .write(
                &self.project_id,
                self.root_thread_id.as_str(),
                PROJECTIONS_FILE,
                &proj_json,
            )
            .map_err(FlowStoreError::Storage)?;
        Ok(())
    }

    /// Reconstruct the log + projections from disk. On load, the in-memory
    /// state is rebuilt by replaying the persisted log (projections fall out
    /// of the replay, so they cannot drift from the log).
    fn load(&mut self) -> Result<(), FlowStoreError> {
        let jsonl = match self
            .store
            .read(&self.project_id, self.root_thread_id.as_str(), LOG_FILE)
        {
            Ok(s) => s,
            Err(craft_storage::flow::FlowError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(FlowStoreError::Storage(e)),
        };
        let mut max_seq: Option<Seq> = None;
        for line in jsonl.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: LogEntry = serde_json::from_str(line).map_err(FlowStoreError::Serde)?;
            if max_seq.is_none_or(|m| entry.seq > m) {
                max_seq = Some(entry.seq);
            }
            let key = ProjectionKey {
                entry: entry.entry_type,
                thread: entry.thread_id.clone(),
            };
            self.projections.insert(key, entry.seq);
            self.log.push(entry);
        }
        self.next_seq = max_seq.map_or(0, |m| m + 1);
        Ok(())
    }
}

const LOG_FILE: &str = "log.jsonl";
const PROJECTIONS_FILE: &str = "projections.json";

/// Typed errors for `ThreadHistory` persistence (migrated from the former
/// `craft-flow/src/error.rs` `FlowRunError` storage boundary).
#[derive(Debug, thiserror::Error)]
pub enum FlowStoreError {
    #[error("flow storage error: {0}")]
    Storage(#[from] craft_storage::flow::FlowError),
    #[error("flow serde error: {0}")]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn store(tmp: &std::path::Path) -> Arc<FlowStore> {
        Arc::new(FlowStore::from_root(tmp.to_path_buf()))
    }

    #[test_case(EntryType::Goal ; "goal")]
    #[test_case(EntryType::Diff ; "diff")]
    #[test_case(EntryType::GeneralTurn ; "general_turn")]
    #[test_case(EntryType::VerificationReport ; "verification_report")]
    fn entry_type_round_trips(e: EntryType) {
        assert_eq!(EntryType::parse(e.as_str()), Some(e));
    }

    #[test]
    fn entry_type_parse_unknown_is_none() {
        assert!(EntryType::parse("nope").is_none());
        assert!(EntryType::parse("").is_none());
    }

    #[test]
    fn append_updates_projection_synchronously() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = ThreadHistory::open(store(tmp.path()), "proj", "ws");
        let root = hist.root_thread_id().clone();
        hist.append(root.clone(), EntryType::Goal, "v1");
        assert_eq!(
            hist.projection(EntryType::Goal, &root).unwrap().content,
            "v1"
        );
        hist.append(root.clone(), EntryType::Goal, "v2");
        assert_eq!(
            hist.projection(EntryType::Goal, &root).unwrap().content,
            "v2"
        );
        assert_eq!(hist.log().len(), 2);
    }

    #[test]
    fn projection_is_thread_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = ThreadHistory::open(store(tmp.path()), "proj", "ws");
        let root = hist.root_thread_id().clone();
        let child = ThreadId::new("c1");
        hist.append(root.clone(), EntryType::Goal, "root-goal");
        hist.append(child.clone(), EntryType::Diff, "child-diff");
        assert_eq!(
            hist.projection(EntryType::Goal, &root).unwrap().content,
            "root-goal"
        );
        assert!(hist.projection(EntryType::Goal, &child).is_none());
        assert!(hist.projection(EntryType::Diff, &root).is_none());
        assert_eq!(
            hist.projection(EntryType::Diff, &child).unwrap().content,
            "child-diff"
        );
    }

    #[test]
    fn entries_returns_full_history_in_log_order() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hist = ThreadHistory::open(store(tmp.path()), "proj", "ws");
        let root = hist.root_thread_id().clone();
        hist.append(root.clone(), EntryType::Diff, "attempt1");
        hist.append(root.clone(), EntryType::Diff, "attempt2");
        let diffs = hist.entries(EntryType::Diff, &root);
        assert_eq!(diffs.len(), 2);
        assert_eq!(diffs[0].content, "attempt1");
        assert_eq!(diffs[1].content, "attempt2");
    }

    #[test]
    fn persist_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        {
            let mut hist = ThreadHistory::open(Arc::clone(&s), "proj", "ws");
            let root = hist.root_thread_id().clone();
            hist.append(root.clone(), EntryType::Goal, "goal-doc");
            hist.append(ThreadId::new("c1"), EntryType::Diff, "diff-c1");
        }
        let reloaded = ThreadHistory::open(Arc::clone(&s), "proj", "ws");
        assert_eq!(reloaded.log().len(), 2);
        assert_eq!(reloaded.next_seq, 2);
        let root = reloaded.root_thread_id();
        assert_eq!(
            reloaded.projection(EntryType::Goal, root).unwrap().content,
            "goal-doc"
        );
        assert_eq!(
            reloaded
                .projection(EntryType::Diff, &ThreadId::new("c1"))
                .unwrap()
                .content,
            "diff-c1"
        );
    }

    #[test]
    fn load_on_empty_workstream_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let hist = ThreadHistory::open(store(tmp.path()), "proj", "ws");
        assert!(hist.log().is_empty());
        assert_eq!(hist.next_seq, 0);
    }
}
