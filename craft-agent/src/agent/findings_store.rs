//! Session-scoped findings storage. Review subagents deliver findings via
//! `report_finding`; the parent's review tool extends this store so the main agent can
//! query them later via `read_findings`, even after compaction strips the original
//! tool result text from history.
//!
//! In-memory only. One store per top-level `Agent`; subagents do not get a store.

use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::types::{Finding, Priority};

const REVIEW_TASK_TRUNCATE: usize = 200;

#[derive(Debug, Clone)]
pub struct StoredFinding {
    pub finding: Finding,
    pub review_task: String,
    pub recorded_at: SystemTime,
}

#[derive(Debug, Default)]
pub struct FindingsStore {
    entries: Vec<StoredFinding>,
}

pub type SharedFindingsStore = Arc<Mutex<FindingsStore>>;

impl FindingsStore {
    pub fn new_shared() -> SharedFindingsStore {
        Arc::new(Mutex::new(Self::default()))
    }

    pub fn extend(&mut self, review_task: &str, findings: impl IntoIterator<Item = Finding>) {
        let task = truncate_task(review_task);
        let now = SystemTime::now();
        for finding in findings {
            self.entries.push(StoredFinding {
                finding,
                review_task: task.clone(),
                recorded_at: now,
            });
        }
    }

    pub fn snapshot(&self) -> Vec<StoredFinding> {
        self.entries.clone()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn filter(
        &self,
        priority: Option<Priority>,
        file_path_contains: Option<&str>,
        limit: usize,
    ) -> Vec<StoredFinding> {
        self.entries
            .iter()
            .filter(|e| priority.is_none_or(|p| e.finding.priority == p))
            .filter(|e| file_path_contains.is_none_or(|s| e.finding.file_path.contains(s)))
            .take(limit)
            .cloned()
            .collect()
    }
}

fn truncate_task(s: &str) -> String {
    if s.len() <= REVIEW_TASK_TRUNCATE {
        return s.to_owned();
    }
    let boundary = s.floor_char_boundary(REVIEW_TASK_TRUNCATE);
    format!("{}...", &s[..boundary])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(priority: Priority, file_path: &str, title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: "body".into(),
            priority,
            confidence: 0.9,
            file_path: file_path.into(),
            line_start: 1,
            line_end: 1,
            rule_ids: vec![],
            suggestion: None,
        }
    }

    #[test]
    fn extend_appends_with_task() {
        let mut store = FindingsStore::default();
        store.extend("review auth", vec![finding(Priority::P1, "a.rs", "x")]);
        store.extend("review db", vec![finding(Priority::P0, "b.rs", "y")]);
        let snap = store.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].review_task, "review auth");
        assert_eq!(snap[1].review_task, "review db");
    }

    #[test]
    fn filter_by_priority() {
        let mut store = FindingsStore::default();
        store.extend(
            "task",
            vec![
                finding(Priority::P0, "a.rs", "x"),
                finding(Priority::P1, "b.rs", "y"),
                finding(Priority::P0, "c.rs", "z"),
            ],
        );
        let p0 = store.filter(Some(Priority::P0), None, 100);
        assert_eq!(p0.len(), 2);
        assert!(p0.iter().all(|e| e.finding.priority == Priority::P0));
    }

    #[test]
    fn filter_by_file_path_substring() {
        let mut store = FindingsStore::default();
        store.extend(
            "task",
            vec![
                finding(Priority::P1, "src/auth/login.rs", "x"),
                finding(Priority::P1, "src/db/query.rs", "y"),
            ],
        );
        let auth = store.filter(None, Some("auth"), 100);
        assert_eq!(auth.len(), 1);
        assert!(auth[0].finding.file_path.contains("auth"));
    }

    #[test]
    fn filter_limit_caps_results() {
        let mut store = FindingsStore::default();
        let many: Vec<Finding> = (0..10)
            .map(|i| finding(Priority::P2, &format!("f{i}.rs"), "t"))
            .collect();
        store.extend("task", many);
        assert_eq!(store.filter(None, None, 3).len(), 3);
    }

    #[test]
    fn truncate_task_preserves_short() {
        let s = "review the whole module";
        assert_eq!(truncate_task(s), s);
    }

    #[test]
    fn truncate_task_caps_long() {
        let s = "x".repeat(REVIEW_TASK_TRUNCATE * 2);
        let t = truncate_task(&s);
        assert!(t.ends_with("..."));
        assert!(t.len() <= REVIEW_TASK_TRUNCATE + 3);
    }
}
