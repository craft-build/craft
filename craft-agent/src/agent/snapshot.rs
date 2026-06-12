use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::fs;
use tracing::{debug, warn};

const MAX_SNAPSHOT_FILE_SIZE: u64 = 5 * 1024 * 1024;

#[derive(Debug, Default)]
struct SnapshotState {
    label: Option<String>,
    originals: HashMap<PathBuf, String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotManager {
    state: Arc<Mutex<SnapshotState>>,
    workdir: PathBuf,
}

impl SnapshotManager {
    pub fn new(workdir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(SnapshotState::default())),
            workdir,
        }
    }

    pub fn begin(&self, label: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        state.label = Some(label.into());
        state.originals.clear();
        debug!(label = ?state.label, "snapshot session started");
    }

    pub async fn note(&self, path: &Path) {
        let abs = if path.is_absolute() {
            path.to_owned()
        } else {
            self.workdir.join(path)
        };

        if !abs.starts_with(&self.workdir) {
            return;
        }

        match fs::metadata(&abs).await {
            Ok(meta) if meta.len() > MAX_SNAPSHOT_FILE_SIZE => {
                warn!(path = %abs.display(), "skipping snapshot, file too large");
                return;
            }
            Err(_) => return,
            Ok(_) => {}
        }

        let content = match fs::read_to_string(&abs).await {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %abs.display(), error = %e, "failed to read file for snapshot");
                return;
            }
        };

        let mut state = self.state.lock().unwrap();
        if state.label.is_none() {
            return;
        }

        if state.originals.contains_key(&abs) {
            return;
        }

        debug!(path = %abs.display(), "snapshot saved");
        state.originals.insert(abs, content);
    }

    pub async fn rollback(&self) -> Option<String> {
        let (label, originals) = {
            let mut state = self.state.lock().unwrap();
            let label = state.label.take()?;
            let originals = std::mem::take(&mut state.originals);
            (label, originals)
        };

        let mut restored = 0;
        for (path, content) in &originals {
            match fs::write(path, content).await {
                Ok(()) => {
                    debug!(path = %path.display(), "restored from snapshot");
                    restored += 1;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to restore snapshot");
                }
            }
        }

        Some(format!(
            "rolled back '{label}': {restored}/{total} files restored",
            total = originals.len()
        ))
    }

    pub fn commit(&self) {
        let mut state = self.state.lock().unwrap();
        let label = state.label.take();
        let count = state.originals.len();
        state.originals.clear();
        drop(state);
        debug!(label = ?label, files = count, "snapshot committed (discarded)");
    }

    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().label.is_some()
    }

    #[cfg(test)]
    pub fn snapshot_count(&self) -> usize {
        self.state.lock().unwrap().originals.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_owned();
        (dir, path)
    }

    #[tokio::test]
    async fn begin_sets_label() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir);
        assert!(!mgr.is_active());
        mgr.begin("test");
        assert!(mgr.is_active());
    }

    #[tokio::test]
    async fn note_saves_original_content() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir.clone());
        mgr.begin("test");

        let file = dir.join("foo.rs");
        fs::write(&file, "original content").unwrap();
        mgr.note(&file).await;

        fs::write(&file, "modified content").unwrap();
        assert_eq!(mgr.snapshot_count(), 1);
    }

    #[tokio::test]
    async fn rollback_restores_files() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir.clone());
        mgr.begin("test");

        let file = dir.join("foo.rs");
        fs::write(&file, "original").unwrap();
        mgr.note(&file).await;
        fs::write(&file, "changed").unwrap();

        let result = mgr.rollback().await.unwrap();
        assert!(result.contains("rolled back"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "original");
        assert!(!mgr.is_active());
    }

    #[tokio::test]
    async fn commit_discards_snapshots() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir.clone());
        mgr.begin("test");

        let file = dir.join("foo.rs");
        fs::write(&file, "original").unwrap();
        mgr.note(&file).await;
        fs::write(&file, "changed").unwrap();

        mgr.commit();
        assert_eq!(fs::read_to_string(&file).unwrap(), "changed");
        assert!(!mgr.is_active());
    }

    #[tokio::test]
    async fn note_outside_workdir_is_ignored() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir);
        mgr.begin("test");

        let outside = Path::new("/tmp/definitely_outside_craft_test.rs");
        mgr.note(outside).await;
        assert_eq!(mgr.snapshot_count(), 0);
    }

    #[tokio::test]
    async fn first_snapshot_wins() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir.clone());
        mgr.begin("test");

        let file = dir.join("foo.rs");
        fs::write(&file, "first").unwrap();
        mgr.note(&file).await;
        fs::write(&file, "second").unwrap();
        mgr.note(&file).await;

        mgr.rollback().await.unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "first");
    }

    #[tokio::test]
    async fn rollback_without_begin_returns_none() {
        let (_tmp, dir) = tmp_dir();
        let mgr = SnapshotManager::new(dir);
        assert!(mgr.rollback().await.is_none());
    }
}
