//! File system abstraction so callers (e.g. ACP) can route file reads/writes
//! through the client when the client advertises `fs/read_text_file` /
//! `fs/write_text_file` capabilities. The default `LocalFs` impl just calls
//! through to `std::fs`, preserving existing behavior.
//!
//! Only text-file read/write are abstracted because that's all ACP exposes.
//! Directory creation, deletion, and metadata lookups stay local.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

pub type FsFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

pub trait FsBackend: Send + Sync {
    fn read_text_file<'a>(&'a self, path: &'a Path) -> FsFuture<'a, String>;
    fn write_text_file<'a>(&'a self, path: &'a Path, contents: &'a str) -> FsFuture<'a, ()>;
}

pub struct LocalFs;

impl FsBackend for LocalFs {
    fn read_text_file<'a>(&'a self, path: &'a Path) -> FsFuture<'a, String> {
        let path = PathBuf::from(path);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                std::fs::read_to_string(&path).map_err(|e| format!("read error: {e}"))
            })
            .await
            .map_err(|e| format!("spawn_blocking failed: {e}"))?
        })
    }

    fn write_text_file<'a>(&'a self, path: &'a Path, contents: &'a str) -> FsFuture<'a, ()> {
        let path = PathBuf::from(path);
        let contents = contents.to_owned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                std::fs::write(&path, &contents).map_err(|e| format!("write error: {e}"))
            })
            .await
            .map_err(|e| format!("spawn_blocking failed: {e}"))?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn local_fs_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        let fs = LocalFs;
        fs.write_text_file(&path, "hello").await.unwrap();
        assert_eq!(fs.read_text_file(&path).await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn local_fs_read_missing_file_errors() {
        let fs = LocalFs;
        let err = fs
            .read_text_file(Path::new("/nonexistent/path/does/not/exist.txt"))
            .await
            .unwrap_err();
        assert!(err.contains("read error"), "got: {err}");
    }
}
