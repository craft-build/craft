use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use tracing::{debug, warn};

const STALE_READ_MSG: &str = "file changed since last read";

/// Dead `Weak`s are only ever cleared once the map grows to this many entries,
/// so the map is bounded by construction instead of by a drop hook.
const LOCK_PRUNE_AT: usize = 512;

/// A path that has been through [`craft_storage::paths::canonical_key`]. Both
/// maps here are keyed by one, so no caller can key them with a raw path. Two
/// spellings of one file would get two entries, and for the lock map that
/// means no mutual exclusion at all.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileKey(PathBuf);

impl FileKey {
    pub fn new(path: &Path) -> Self {
        Self(craft_storage::paths::canonical_key(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

type FileLock = Arc<tokio::sync::Mutex<()>>;
pub type FileGuard = tokio::sync::OwnedMutexGuard<()>;

/// Who read what and when, plus the per-file write locks that make one tool's
/// read-modify-write safe against another's.
#[derive(Default)]
pub struct FileAccess {
    mtimes: Mutex<HashMap<FileKey, SystemTime>>,
    locks: Mutex<HashMap<FileKey, Weak<tokio::sync::Mutex<()>>>>,
}

fn get_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl FileAccess {
    pub fn fresh() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The write lock for one file, held for a tool's entire execution so its
    /// read-modify-write cannot interleave with another tool's on the same
    /// file. The dispatcher is the only caller: a tool that declares
    /// `mutable_path` gets this for free and must not re-implement it.
    ///
    /// One path only. A tool mutating several would need a globally sorted
    /// acquire order to avoid lock-order inversion, and none exists yet.
    pub async fn acquire(&self, key: &FileKey) -> FileGuard {
        let lock = self.lock_for(key);
        match lock.clone().try_lock_owned() {
            Ok(guard) => guard,
            Err(_) => {
                debug!(path = %key.as_path().display(), "waiting for the file lock");
                lock.lock_owned().await
            }
        }
    }

    /// The guard and every waiter own an `Arc` clone, so an entry lives
    /// exactly as long as it is in use. [`LOCK_PRUNE_AT`] only bounds the
    /// residue of dead `Weak`s left behind.
    fn lock_for(&self, key: &FileKey) -> FileLock {
        let mut map = self.locks.lock().unwrap();
        if map.len() >= LOCK_PRUNE_AT {
            map.retain(|_, weak| weak.strong_count() > 0);
        }
        map.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let created = FileLock::default();
            map.insert(key.clone(), Arc::downgrade(&created));
            created
        })
    }

    pub fn record_read(&self, key: &FileKey) {
        match get_mtime(key.as_path()) {
            Some(mtime) => {
                self.mtimes.lock().unwrap().insert(key.clone(), mtime);
            }
            None => warn!(
                path = %key.as_path().display(),
                "record_read: could not get mtime, file will not be tracked"
            ),
        }
    }

    pub fn check_before_edit(&self, key: &FileKey) -> Result<(), String> {
        let mut guard = self.mtimes.lock().unwrap();
        let Some(&recorded) = guard.get(key) else {
            return Ok(());
        };
        let Some(current) = get_mtime(key.as_path()) else {
            guard.remove(key);
            return Ok(());
        };
        if recorded != current {
            return Err(format!(
                "{STALE_READ_MSG}: {} - re-read using read tool before editing",
                key.as_path().display(),
            ));
        }
        Ok(())
    }

    pub fn read_paths(&self) -> Vec<PathBuf> {
        self.mtimes
            .lock()
            .unwrap()
            .keys()
            .map(|k| k.0.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const FILE: &str = "f.rs";
    const CONTENT: &str = "content";

    fn future_mtime(path: &Path) {
        let future = SystemTime::now() + Duration::from_secs(10);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(future)
            .unwrap();
    }

    #[test]
    fn stale_read_rejects_edit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, CONTENT).unwrap();
        let key = FileKey::new(&path);

        let access = FileAccess::default();
        access.record_read(&key);
        future_mtime(&path);
        let err = access.check_before_edit(&key).unwrap_err();
        assert!(err.contains(STALE_READ_MSG), "{err}");
    }

    #[test]
    fn re_read_after_change_allows_edit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(FILE);
        fs::write(&path, CONTENT).unwrap();
        let key = FileKey::new(&path);

        let access = FileAccess::default();
        access.record_read(&key);
        future_mtime(&path);
        access.record_read(&key);
        access.check_before_edit(&key).unwrap();
    }

    #[tokio::test]
    async fn one_lock_per_file_held_until_the_guard_drops() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = FileKey::new(&dir.path().join(FILE));
        let other = FileKey::new(&dir.path().join("other.rs"));

        let access = FileAccess::default();
        let held = access.acquire(&path).await;
        assert!(
            try_acquire_now(&access, &path).is_none(),
            "the same file must not be lockable twice"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), access.acquire(&other))
                .await
                .is_ok(),
            "another file must not wait on this one"
        );
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), access.acquire(&path))
                .await
                .is_ok()
        );
    }

    fn try_acquire_now(access: &FileAccess, key: &FileKey) -> Option<FileGuard> {
        access.lock_for(key).clone().try_lock_owned().ok()
    }

    #[tokio::test]
    async fn dead_locks_do_not_accumulate() {
        let dir = tempfile::TempDir::new().unwrap();
        let access = FileAccess::default();
        for i in 0..=LOCK_PRUNE_AT {
            let key = FileKey::new(&dir.path().join(format!("{i}.rs")));
            drop(access.acquire(&key).await);
        }
        assert!(access.locks.lock().unwrap().len() <= LOCK_PRUNE_AT);
    }
}
