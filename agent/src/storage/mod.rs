//! Persistent misc state: input history, model/theme preferences, rotating
//! logs, and the atomic IO they share.
//!
//! Ported from the reference `craft-storage` crate (thiserror → snafu, and
//! flock replaced with a std-only best-effort lock because the lock crate is
//! not a dependency here).

pub mod atomic;
pub mod input_history;
pub mod log;
pub mod model;
pub mod stats;
pub mod theme;

use std::fs;
use std::path::{Path, PathBuf};

use snafu::Snafu;

pub use atomic::{atomic_write, atomic_write_permissions};

#[derive(Debug, Clone)]
pub struct StateDir(PathBuf);

impl StateDir {
    pub fn resolve() -> Result<Self, StorageError> {
        let dir = crate::paths::state_dir()?;
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

#[derive(Debug, Snafu)]
pub enum StorageError {
    #[snafu(display("io error"))]
    #[snafu(context(false))]
    Io { source: std::io::Error },

    #[snafu(display("serialization error"))]
    #[snafu(context(false))]
    Json { source: serde_json::Error },
}

pub type StorageResult<T> = Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_subdir_creates_nested_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        let sub = dir.ensure_subdir("a/b").unwrap();

        assert_eq!(sub, tmp.path().join("a/b"));
        assert!(sub.is_dir());
    }
}
