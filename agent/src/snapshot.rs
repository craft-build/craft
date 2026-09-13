//! Pre-write snapshots and `/undo` rollback.
//!
//! Write-family tools capture a file's current contents before their first
//! mutation of the turn (first snapshot wins). A clean run-end commits the
//! session onto the undo stack; `/undo` pops the stack and restores. Files
//! are workdir-scoped, capped at 5 MiB, and text-only — binary or oversized
//! files are skipped. This is a convenience rollback, not a crash-safe
//! journal: captures and restores ride ordinary filesystem operations.
//! Files a write tool created during the turn are not removed by `/undo`
//! (matching the reference, which only records pre-existing contents).
//! Out of scope by design: `delete` has no pre-delete capture, and bash
//! in-place-edit detection (B.9, Phase 3) should call
//! [`SnapshotManager::note`] when it lands.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_SNAPSHOT_FILE_SIZE: u64 = 5 * 1024 * 1024;
/// Sessions retained for `/undo` beyond the one still being captured.
const MAX_UNDO_SESSIONS: usize = 20;
/// Label used when a write tool starts a session implicitly.
pub const AUTO_LABEL: &str = "auto";

#[derive(Debug, Default)]
struct SnapshotSession {
    label: Option<String>,
    originals: HashMap<PathBuf, String>,
}

impl SnapshotSession {
    fn is_empty(&self) -> bool {
        self.originals.is_empty()
    }
}

#[derive(Debug, Default)]
struct SnapshotState {
    active: SnapshotSession,
    undo_stack: Vec<SnapshotSession>,
}

/// Session-shared snapshot manager; clones share one state.
#[derive(Debug, Clone)]
pub struct SnapshotManager {
    state: Arc<Mutex<SnapshotState>>,
    workdir: PathBuf,
}

/// The shared handle other subsystems (dispatch, TUI) hold.
pub type SharedSnapshotManager = Arc<SnapshotManager>;

pub fn shared(workdir: impl Into<PathBuf>) -> SharedSnapshotManager {
    Arc::new(SnapshotManager::new(workdir))
}

impl SnapshotManager {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            state: Arc::new(Mutex::new(SnapshotState::default())),
            workdir: workdir.into(),
        }
    }

    /// Start a capture session, discarding any uncommitted one.
    pub fn begin(&self, label: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.active = SnapshotSession {
            label: Some(label.into()),
            originals: HashMap::new(),
        };
    }

    /// Capture `path`'s current contents into the active session, starting an
    /// `auto` session if none is active. Skips files outside the workdir,
    /// oversized files, and unreadable (binary/missing) content. The first
    /// capture of a path wins for the whole session.
    pub fn note(&self, path: &Path) {
        let abs = if path.is_absolute() {
            path.to_owned()
        } else {
            self.workdir.join(path)
        };
        if !abs.starts_with(&self.workdir) {
            return;
        }
        match fs::metadata(&abs) {
            Ok(meta) if meta.len() > MAX_SNAPSHOT_FILE_SIZE => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let Ok(content) = fs::read_to_string(&abs) else {
            return;
        };
        let mut state = self.state.lock().unwrap();
        if state.active.label.is_none() {
            state.active.label = Some(AUTO_LABEL.to_owned());
        }
        state.active.originals.entry(abs).or_insert(content);
    }

    /// Close the active session and push it onto the undo stack. Empty
    /// sessions are discarded. The oldest session is dropped past the cap.
    pub fn commit(&self) {
        let mut state = self.state.lock().unwrap();
        let mut session = std::mem::take(&mut state.active);
        if session.is_empty() {
            session.label = None;
            return;
        }
        state.undo_stack.push(session);
        let excess = state.undo_stack.len().saturating_sub(MAX_UNDO_SESSIONS);
        state.undo_stack.drain(0..excess);
    }

    /// Pop the most recent committed session and restore its files. Falls
    /// back to the active (uncommitted) session when the stack is empty,
    /// mirroring the reference's mid-run rollback. Returns a summary, or
    /// `None` when there is nothing to undo.
    pub async fn rollback(&self) -> Option<String> {
        let mut session = {
            let mut state = self.state.lock().unwrap();
            state
                .undo_stack
                .pop()
                .unwrap_or_else(|| std::mem::take(&mut state.active))
        };
        if session.label.is_none() || session.is_empty() {
            return None;
        }
        let label = session.label.take().unwrap();
        let total = session.originals.len();
        // Restores are blocking file I/O: keep them off the async executor.
        let restored = tokio::task::spawn_blocking(move || {
            let mut restored = 0;
            for (path, content) in &session.originals {
                if fs::write(path, content).is_ok() {
                    restored += 1;
                }
            }
            restored
        })
        .await
        .unwrap_or(0);
        Some(format!(
            "rolled back '{label}': {restored}/{total} files restored"
        ))
    }

    /// Whether a capture session is active.
    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().active.label.is_some()
    }

    /// Committed sessions available for `/undo`.
    pub fn undo_depth(&self) -> usize {
        self.state.lock().unwrap().undo_stack.len()
    }

    #[cfg(test)]
    pub(crate) fn snapshot_count(&self) -> usize {
        self.state.lock().unwrap().active.originals.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        (dir, path)
    }

    #[test]
    fn begin_sets_label() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir);
        assert!(!mgr.is_active());
        mgr.begin("test");
        assert!(mgr.is_active());
    }

    #[test]
    fn note_starts_auto_session_lazily() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        let file = dir.join("foo.rs");
        fs::write(&file, "original").unwrap();
        assert!(!mgr.is_active());
        mgr.note(&file);
        assert!(mgr.is_active());
        assert_eq!(mgr.snapshot_count(), 1);
    }

    #[test]
    fn note_saves_original_content() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir.clone());
        mgr.begin("test");
        let file = dir.join("foo.rs");
        fs::write(&file, "original content").unwrap();
        mgr.note(&file);
        fs::write(&file, "modified content").unwrap();
        assert_eq!(mgr.snapshot_count(), 1);
    }

    #[tokio::test]
    async fn rollback_pops_committed_session() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        let file = dir.join("foo.rs");
        fs::write(&file, "original").unwrap();
        mgr.note(&file);
        mgr.commit();
        fs::write(&file, "changed").unwrap();

        let result = mgr.rollback().await.unwrap();
        assert!(result.contains("rolled back"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert_eq!(mgr.undo_depth(), 0);
        assert!(mgr.rollback().await.is_none(), "stack is now empty");
    }

    #[tokio::test]
    async fn rollback_falls_back_to_active_session() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        let file = dir.join("foo.rs");
        fs::write(&file, "original").unwrap();
        mgr.note(&file);
        fs::write(&file, "changed").unwrap();
        // No commit: the active session is still undoable mid-run.
        let result = mgr.rollback().await.unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert!(result.contains("rolled back"));
    }

    #[test]
    fn commit_discards_empty_sessions() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir);
        mgr.begin("empty");
        mgr.commit();
        assert_eq!(mgr.undo_depth(), 0);
    }

    #[tokio::test]
    async fn commit_keeps_file_changes() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir.clone());
        let file = dir.join("foo.rs");
        fs::write(&file, "original").unwrap();
        mgr.note(&file);
        fs::write(&file, "changed").unwrap();
        mgr.commit();
        assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
    }

    #[test]
    fn note_outside_workdir_is_ignored() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        let outside = std::env::temp_dir().join("definitely_outside_craft_test.rs");
        mgr.note(&outside);
        assert_eq!(mgr.snapshot_count(), 0);
    }

    #[test]
    fn first_snapshot_wins() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        mgr.begin("test");
        let file = dir.join("foo.rs");
        fs::write(&file, "first").unwrap();
        mgr.note(&file);
        fs::write(&file, "second").unwrap();
        mgr.note(&file);
        assert_eq!(mgr.snapshot_count(), 1, "no duplicate capture");
    }

    #[tokio::test]
    async fn first_snapshot_wins_across_rollback() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        let file = dir.join("foo.rs");
        fs::write(&file, "first").unwrap();
        mgr.note(&file);
        fs::write(&file, "second").unwrap();
        mgr.note(&file);
        mgr.commit();
        mgr.rollback().await.unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "first");
    }

    #[test]
    fn undo_stack_is_bounded() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(&dir);
        let file = dir.join("foo.rs");
        for i in 0..(MAX_UNDO_SESSIONS + 5) {
            fs::write(&file, format!("v{i}")).unwrap();
            mgr.note(&file);
            mgr.commit();
        }
        assert_eq!(mgr.undo_depth(), MAX_UNDO_SESSIONS);
    }
}
