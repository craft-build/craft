//! `flow` namespace: per-project, per-workstream persisted documents for Flow
//! mode. Documents live as argosy concepts under
//! `<state-dir>/projects/<project-id>/argosy/document/<workstream-id>/`;
//! the mutable `workstream.json` state stays a plain file under
//! `<project-id>/flow/<workstream-id>/` (it is agent state, not a document).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use argosy::bundle::Namespace;

use crate::argosy_store::ArgosyStore;
use crate::{StateDir, StorageError, atomic_write};

const FLOW_DIR_NAME: &str = "flow";
const PROJECTS_DIR_NAME: &str = "projects";
const ARGOSY_DIR_NAME: &str = "argosy";
const MAX_DOC_BYTES: usize = 256 * 1024;
const WORKSTREAM_STATE_FILE: &str = "workstream.json";

/// Project id: lowercase basename of `cwd` plus the fnv1a-64 hash of the full
/// path. Shared key for the per-project argosy (memory + flow documents), so
/// both namespaces address the same `<state>/projects/<project-id>/argosy`.
pub fn project_id(cwd: &std::path::Path) -> String {
    let basename = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "root".to_string());
    let path_str = cwd.to_string_lossy();
    format!("{basename}-{}", fnv1a_64(path_str.as_bytes()))
}

/// FNV-1a 64-bit as a 16-hex-char string.
fn fnv1a_64(data: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Argosy(#[from] crate::argosy_store::ArgosyError),
    #[error("path must be relative: {0}")]
    PathNotRelative(String),
    #[error("path traversal outside flow directory is not allowed: {0}")]
    PathTraversal(String),
    #[error("document exceeds {MAX_DOC_BYTES} byte ceiling ({actual} bytes)")]
    DocTooLarge { actual: usize },
    #[error("not found: {0}")]
    NotFound(String),
}

/// Per-project, per-workstream document store for Flow mode, backed by one
/// argosy per project (shared with the memory namespace).
pub struct FlowStore {
    root: PathBuf,
    argosies: Mutex<HashMap<String, Arc<ArgosyStore>>>,
}

impl FlowStore {
    pub fn new(state: &StateDir) -> Result<Self, FlowError> {
        let root = state.ensure_subdir(PROJECTS_DIR_NAME)?;
        Ok(Self {
            root,
            argosies: Mutex::new(HashMap::new()),
        })
    }

    /// Construct a store rooted at an explicit directory (testing / custom roots).
    pub fn from_root(root: PathBuf) -> Self {
        Self {
            root,
            argosies: Mutex::new(HashMap::new()),
        }
    }

    /// The per-project argosy, opened or initialized on first use and cached.
    fn project_argosy(&self, project_id: &str) -> Result<Arc<ArgosyStore>, FlowError> {
        if let Some(store) = self
            .argosies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project_id)
        {
            return Ok(Arc::clone(store));
        }
        let store = Arc::new(ArgosyStore::open_or_init(
            &self.root.join(project_id).join(ARGOSY_DIR_NAME),
            project_id,
        )?);
        self.argosies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(project_id.to_string(), Arc::clone(&store));
        Ok(store)
    }

    /// Absolute path of the project's argosy (the semantic index's source).
    pub fn argosy_dir(&self, project_id: &str) -> PathBuf {
        self.root.join(project_id).join(ARGOSY_DIR_NAME)
    }

    /// A concept name under the document namespace: `<workstream>/<rel_path>`
    /// with a trailing `.md` dropped, so `goal.md` and `goal` address one
    /// document by design instead of aliasing inside the argosy.
    fn doc_name(workstream_id: &str, rel_path: &str) -> Result<String, FlowError> {
        safe_relative(rel_path)?;
        let rel = rel_path.trim_end_matches(".md");
        Ok(format!("{workstream_id}/{rel}"))
    }

    pub fn write(
        &self,
        project_id: &str,
        workstream_id: &str,
        rel_path: &str,
        content: &str,
    ) -> Result<(), FlowError> {
        if content.len() > MAX_DOC_BYTES {
            return Err(FlowError::DocTooLarge {
                actual: content.len(),
            });
        }
        let name = Self::doc_name(workstream_id, rel_path)?;
        Ok(self
            .project_argosy(project_id)?
            .write_document(&name, content)?)
    }

    pub fn read(
        &self,
        project_id: &str,
        workstream_id: &str,
        rel_path: &str,
    ) -> Result<String, FlowError> {
        let name = Self::doc_name(workstream_id, rel_path)?;
        self.project_argosy(project_id)?
            .read_document(&name)
            .map_err(|_| FlowError::NotFound(rel_path.to_string()))
    }

    /// Every document rel-path in the workstream. A rel_path ending in `.md`
    /// is stored under the extension-stripped concept id, so listings report
    /// the canonical (extension-less) spelling.
    pub fn list(&self, project_id: &str, workstream_id: &str) -> Result<Vec<String>, FlowError> {
        let prefix = format!("{workstream_id}/");
        Ok(self
            .project_argosy(project_id)?
            .list(Namespace::Document)?
            .into_iter()
            .filter_map(|name| name.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }

    pub fn delete_workstream(
        &self,
        project_id: &str,
        workstream_id: &str,
    ) -> Result<(), FlowError> {
        let store = self.project_argosy(project_id)?;
        for name in self.list(project_id, workstream_id)? {
            store.delete_document(&format!("{workstream_id}/{name}"))?;
        }
        let dir = self.flow_dir(project_id, workstream_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Remove workstreams whose newest artifact (document or state file) is
    /// older than `cutoff`.
    pub fn prune(&self, older_than: Duration) -> Result<u32, FlowError> {
        let now = SystemTime::now();
        let cutoff = now - older_than;
        let mut removed = 0;
        if !self.root.exists() {
            return Ok(0);
        }
        for project_entry in fs::read_dir(&self.root)? {
            let project_entry = project_entry?;
            if !project_entry.file_type()?.is_dir() {
                continue;
            }
            let project_id = project_entry.file_name().to_string_lossy().into_owned();
            let flow_dir = project_entry.path().join(FLOW_DIR_NAME);
            let doc_dir = self
                .argosy_dir(&project_id)
                .join(Namespace::Document.as_dir_name());
            let mut workstream_ids: Vec<String> = Vec::new();
            for dir in [&flow_dir, &doc_dir] {
                if !dir.exists() {
                    continue;
                }
                for entry in fs::read_dir(dir)?.flatten() {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        && let Some(name) = entry.file_name().into_string().ok()
                        && !workstream_ids.contains(&name)
                    {
                        workstream_ids.push(name);
                    }
                }
            }
            for ws in workstream_ids {
                let newest = [
                    newest_mtime(&flow_dir.join(&ws)),
                    newest_mtime(&doc_dir.join(&ws)),
                ]
                .into_iter()
                .flatten()
                .max();
                if newest.is_some_and(|m| m < cutoff) {
                    let _ = self.delete_workstream(&project_id, &ws);
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    fn flow_dir(&self, project_id: &str, workstream_id: &str) -> PathBuf {
        self.root
            .join(project_id)
            .join(FLOW_DIR_NAME)
            .join(workstream_id)
    }

    fn workstream_state_path(&self, project_id: &str, workstream_id: &str) -> PathBuf {
        self.flow_dir(project_id, workstream_id)
            .join(WORKSTREAM_STATE_FILE)
    }

    /// Load a workstream's persisted mutable state (stage, approval flag, chunk
    /// statuses, iteration counts). Returns `None` when no state has been
    /// persisted yet (first run). The bytes are opaque to this crate; the flow
    /// loop owns the `Workstream` schema and deserializes them.
    pub fn read_workstream_state(
        &self,
        project_id: &str,
        workstream_id: &str,
    ) -> Result<Option<Vec<u8>>, FlowError> {
        let path = self.workstream_state_path(project_id, workstream_id);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(FlowError::Io(e)),
        }
    }

    /// Persist a workstream's mutable state atomically. Called after every
    /// stage/chunk transition so a crash or resume re-enters at the right place.
    pub fn write_workstream_state(
        &self,
        project_id: &str,
        workstream_id: &str,
        bytes: &[u8],
    ) -> Result<(), FlowError> {
        if bytes.len() > MAX_DOC_BYTES {
            return Err(FlowError::DocTooLarge {
                actual: bytes.len(),
            });
        }
        let path = self.workstream_state_path(project_id, workstream_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(atomic_write(&path, bytes)?)
    }
}

fn safe_relative(rel: &str) -> Result<&Path, FlowError> {
    if rel.is_empty() || rel.contains('\0') {
        return Err(FlowError::PathNotRelative(rel.to_string()));
    }
    let first = rel.chars().next().unwrap();
    if first == '/' || first == '\\' {
        return Err(FlowError::PathNotRelative(rel.to_string()));
    }
    if rel.len() >= 2 {
        let bytes = rel.as_bytes();
        if bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            return Err(FlowError::PathNotRelative(rel.to_string()));
        }
    }
    let path = Path::new(rel);
    for component in path.components() {
        use std::path::Component;
        if matches!(component, Component::ParentDir) {
            return Err(FlowError::PathTraversal(rel.to_string()));
        }
    }
    Ok(path)
}

fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut seen = false;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let ft = entry.file_type().ok()?;
        let m = if ft.is_dir() {
            newest_mtime(&path)?
        } else {
            entry.metadata().ok()?.modified().ok()?
        };
        seen = true;
        if m > newest {
            newest = m;
        }
    }
    seen.then_some(newest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn store(tmp: &Path) -> FlowStore {
        FlowStore::from_root(tmp.to_path_buf())
    }

    #[test]
    fn write_read_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal", "hello").unwrap();
        assert_eq!(s.read("proj", "ws", "goal").unwrap(), "hello");
    }

    #[test]
    fn md_paths_and_extensionless_paths_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "hello").unwrap();
        assert_eq!(s.read("proj", "ws", "goal").unwrap(), "hello");
        assert_eq!(s.list("proj", "ws").unwrap(), vec!["goal".to_string()]);
    }

    #[test]
    fn list_returns_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal", "a").unwrap();
        s.write("proj", "ws", "nested/plan", "b").unwrap();
        let mut listed = s.list("proj", "ws").unwrap();
        listed.sort();
        assert_eq!(listed, vec!["goal".to_string(), "nested/plan".to_string()]);
    }

    #[test]
    fn workstreams_are_isolated_namespaces() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws1", "goal", "one").unwrap();
        s.write("proj", "ws2", "goal", "two").unwrap();
        assert_eq!(s.read("proj", "ws1", "goal").unwrap(), "one");
        assert_eq!(s.read("proj", "ws2", "goal").unwrap(), "two");
    }

    #[test]
    fn delete_workstream_removes_docs_but_not_other_workstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws1", "goal", "a").unwrap();
        s.write("proj", "ws2", "goal", "b").unwrap();
        s.delete_workstream("proj", "ws1").unwrap();
        assert!(s.list("proj", "ws1").unwrap().is_empty());
        assert_eq!(s.list("proj", "ws2").unwrap().len(), 1);
    }

    #[test]
    fn read_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        match s.read("proj", "ws", "missing") {
            Err(FlowError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn doc_too_large_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let big = "x".repeat(MAX_DOC_BYTES + 1);
        match s.write("proj", "ws", "big", &big) {
            Err(FlowError::DocTooLarge { .. }) => {}
            other => panic!("expected DocTooLarge, got {other:?}"),
        }
    }

    #[test_case("/abs/path" ; "absolute_unix")]
    #[test_case("\\windows\\path" ; "absolute_windows")]
    #[test_case("C:/drive" ; "windows_drive")]
    #[test_case("../escape" ; "parent_dir")]
    #[test_case("a/../../etc" ; "nested_parent")]
    #[test_case("a\\b" ; "backslash")]
    #[test_case("col:on" ; "colon")]
    fn traversal_rejected(rel: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        assert!(
            s.write("proj", "ws", rel, "x").is_err(),
            "{rel} should be rejected"
        );
    }

    #[test_case("goal" ; "plain")]
    #[test_case("nested/deep/plan" ; "nested")]
    #[test_case("./goal" ; "cur_dir")]
    #[test_case("log.jsonl" ; "extensionful")]
    fn relative_paths_allowed(rel: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", rel, "ok").unwrap();
        assert_eq!(s.read("proj", "ws", rel).unwrap(), "ok");
    }

    #[test]
    fn prune_removes_old_workstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal", "a").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 31);
        let doc = s
            .argosy_dir("proj")
            .join("document")
            .join("ws")
            .join("goal.md");
        let _ = filetime::set_file_mtime(&doc, filetime::FileTime::from_system_time(old));
        let removed = s.prune(Duration::from_secs(60 * 60 * 24 * 30)).unwrap();
        assert_eq!(removed, 1);
        assert!(s.list("proj", "ws").unwrap().is_empty());
    }

    #[test]
    fn prune_keeps_recent_workstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal", "a").unwrap();
        let removed = s.prune(Duration::from_secs(60 * 60 * 24 * 30)).unwrap();
        assert_eq!(removed, 0);
        assert!(!s.list("proj", "ws").unwrap().is_empty());
    }

    #[test]
    fn workstream_state_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        assert!(s.read_workstream_state("proj", "ws").unwrap().is_none());
        s.write_workstream_state("proj", "ws", b"{\"stage\":\"plan\"}")
            .unwrap();
        let loaded = s.read_workstream_state("proj", "ws").unwrap();
        assert_eq!(loaded.as_deref(), Some(b"{\"stage\":\"plan\"}" as &[u8]));
    }

    #[test]
    fn workstream_state_too_large_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let big = vec![0u8; MAX_DOC_BYTES + 1];
        match s.write_workstream_state("proj", "ws", &big) {
            Err(FlowError::DocTooLarge { .. }) => {}
            other => panic!("expected DocTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn fnv1a_64_empty_is_offset_basis() {
        assert_eq!(fnv1a_64(b""), "cbf29ce484222325");
    }

    #[test]
    fn project_id_is_basename_dash_hash() {
        let id = project_id(std::path::Path::new("/Users/me/my-project"));
        assert!(id.starts_with("my-project-"), "got {id}");
        let hash = &id["my-project-".len()..];
        assert_eq!(hash.len(), 16);
    }
}
