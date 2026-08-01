//! Persistent storage. `atomic_write` writes to a `tempfile` in the same
//! directory then persists (atomic rename) for crash safety.
//! `atomic_write_permissions` sets file mode before persist (for auth keys at 0600).

pub mod auth;
pub mod flow;
pub mod id;
pub mod input_history;
pub mod log;
pub mod model;
pub mod paths;
pub mod plans;
pub mod sessions;
pub mod stats;
pub mod theme;
pub mod version;
pub mod wiki;

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

use paths::state_dir;

#[derive(Debug, Clone)]
pub struct StateDir(PathBuf);

impl StateDir {
    pub fn resolve() -> Result<Self, StorageError> {
        let dir = state_dir()?;
        Ok(Self(dir))
    }

    pub fn from_path(path: PathBuf) -> Self {
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn ensure_subdir(&self, name: &str) -> Result<PathBuf, StorageError> {
        let dir = self.0.join(name);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("home directory not found")]
    HomeNotSet,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("slug collision after max attempts")]
    SlugCollision,
}

/// Atomically replace `path` with `data`. The parent directory must exist.
/// Readers observe either the old file or the complete new file. Existing file
/// permissions are preserved. On Unix, new files use mode 0600.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(tmp.path(), metadata.permissions())?;
    }
    tmp.as_file().sync_data()?;
    let (_, tmp_path) = tmp.into_parts();
    retry_rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        StorageError::Io(e)
    })
}

pub(crate) fn atomic_write_permissions(
    path: &Path,
    data: &[u8],
    mode: u32,
) -> Result<(), StorageError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    #[cfg(unix)]
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;
    tmp.as_file().sync_all()?;
    let (_, tmp_path) = tmp.into_parts();
    retry_rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        StorageError::Io(e)
    })
}

fn retry_rename(src: &Path, dest: &Path) -> std::io::Result<()> {
    fs::rename(src, dest)
}

fn now_since_epoch() -> std::time::Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

pub fn now_epoch() -> u64 {
    now_since_epoch().as_secs()
}

pub fn now_millis() -> u64 {
    now_since_epoch().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &[u8] = b"original";
    const OWNER_ONLY_FILE_MODE: u32 = 0o600;
    const REPLACEMENT: &[u8] = b"replacement";
    #[cfg(unix)]
    const FILE_MODE_MASK: u32 = 0o777;

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[test]
    fn atomic_write_permissions_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write_permissions(&path, REPLACEMENT, OWNER_ONLY_FILE_MODE).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");

        atomic_write(&path, ORIGINAL).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            OWNER_ONLY_FILE_MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_destination_permissions() {
        const MODE: u32 = 0o640;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(MODE)).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_cleans_up_temp_after_replacement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("destination");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write(&destination, REPLACEMENT).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
