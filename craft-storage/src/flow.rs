//! `flow` namespace: per-project, per-workstream persisted documents for Flow
//! mode. Lives under `<state-dir>/projects/<project-id>/flow/<workstream_id>/`.
//!
//! Distinct from the Lua `memory` plugin on purpose: memory is curated and
//! bulk-loaded with an aggregate cap; Flow docs are machine-generated,
//! path-addressed, and bounded per-document instead.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::{StateDir, StorageError, atomic_write};

const FLOW_DIR_NAME: &str = "flow";
const PROJECTS_DIR_NAME: &str = "projects";
const MAX_DOC_BYTES: usize = 256 * 1024;
const WORKSTREAM_STATE_FILE: &str = "workstream.json";
/// Per-workstream documents must exceed this count before the semantic index
/// activates. Below it, linear scan over `list()` is cheaper than maintaining
/// an embedding index and re-running inference on every write.
pub const SEMANTIC_INDEX_MIN_DOCS: usize = 8;
const INDEX_FILE_NAME: &str = "index.json";

/// Project id: lowercase basename of `cwd` plus the fnv1a-64 hash of the full
/// path, mirroring the Lua `memory_helpers.project_id` so the Flow and memory
/// namespaces share a per-project key. Migrated from the former `craft-flow`
/// crate so the Flow namespace path `<project>/flow/<workstream>/` stays
/// stable without a second storage crate.
pub fn project_id(cwd: &std::path::Path) -> String {
    let basename = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "root".to_string());
    let path_str = cwd.to_string_lossy();
    format!("{basename}-{}", fnv1a_64(path_str.as_bytes()))
}

/// FNV-1a 64-bit as a 16-hex-char string, matching `memory_helpers.fnv1a_64`'s
/// `%08x%08x` output exactly.
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
    Json(#[from] serde_json::Error),
    #[error("path must be relative: {0}")]
    PathNotRelative(String),
    #[error("path traversal outside flow directory is not allowed: {0}")]
    PathTraversal(String),
    #[error("document exceeds {MAX_DOC_BYTES} byte ceiling ({actual} bytes)")]
    DocTooLarge { actual: usize },
    #[error("not found: {0}")]
    NotFound(String),
}

/// Per-project, per-workstream document store for Flow mode.
pub struct FlowStore {
    root: PathBuf,
}

impl FlowStore {
    pub fn new(state: &StateDir) -> Result<Self, FlowError> {
        let root = state.ensure_subdir(PROJECTS_DIR_NAME)?;
        Ok(Self { root })
    }

    /// Construct a store rooted at an explicit directory (testing / custom roots).
    pub fn from_root(root: PathBuf) -> Self {
        Self { root }
    }

    /// `<root>/<project_id>/flow/<workstream_id>/<rel_path>`.
    fn doc_path(
        &self,
        project_id: &str,
        workstream_id: &str,
        rel_path: &str,
    ) -> Result<PathBuf, FlowError> {
        let safe = safe_relative(rel_path)?;
        let mut p = self
            .root
            .join(project_id)
            .join(FLOW_DIR_NAME)
            .join(workstream_id);
        for component in safe.components() {
            use std::path::Component;
            match component {
                Component::Normal(c) => p.push(c),
                Component::CurDir => {}
                _ => return Err(FlowError::PathNotRelative(rel_path.to_string())),
            }
        }
        Ok(p)
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
        let path = self.doc_path(project_id, workstream_id, rel_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(atomic_write(&path, content.as_bytes())?)
    }

    pub fn read(
        &self,
        project_id: &str,
        workstream_id: &str,
        rel_path: &str,
    ) -> Result<String, FlowError> {
        let path = self.doc_path(project_id, workstream_id, rel_path)?;
        fs::read_to_string(&path).map_err(|_| FlowError::NotFound(rel_path.to_string()))
    }

    pub fn list(&self, project_id: &str, workstream_id: &str) -> Result<Vec<String>, FlowError> {
        let dir = self
            .root
            .join(project_id)
            .join(FLOW_DIR_NAME)
            .join(workstream_id);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        collect_relative(&dir, &dir, &mut out)?;
        Ok(out)
    }

    pub fn delete_workstream(
        &self,
        project_id: &str,
        workstream_id: &str,
    ) -> Result<(), FlowError> {
        let dir = self
            .root
            .join(project_id)
            .join(FLOW_DIR_NAME)
            .join(workstream_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Remove workstream directories whose newest file is older than `cutoff`.
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
            let flow_dir = project_entry.path().join(FLOW_DIR_NAME);
            if !flow_dir.exists() {
                continue;
            }
            for workstream_entry in fs::read_dir(&flow_dir)? {
                let workstream_entry = workstream_entry?;
                if !workstream_entry.file_type()?.is_dir() {
                    continue;
                }
                let ws_dir = workstream_entry.path();
                if let Ok(newest) = newest_mtime(&ws_dir)
                    && newest < cutoff
                    && fs::remove_dir_all(&ws_dir).is_ok()
                {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Count of documents currently in a workstream. Used by the semantic-index
    /// gate to decide whether maintaining an embedding index is worth the cost.
    pub fn doc_count(&self, project_id: &str, workstream_id: &str) -> Result<usize, FlowError> {
        Ok(self.list(project_id, workstream_id)?.len())
    }

    /// `<root>/<project_id>/flow/<workstream_id>/index.json` (sibling to docs).
    fn index_path(&self, project_id: &str, workstream_id: &str) -> PathBuf {
        self.root
            .join(project_id)
            .join(FLOW_DIR_NAME)
            .join(workstream_id)
            .join(INDEX_FILE_NAME)
    }

    /// Load a workstream's semantic index. Returns an empty index when none is
    /// persisted yet (first run, or indexing not yet activated).
    pub fn read_index(
        &self,
        project_id: &str,
        workstream_id: &str,
    ) -> Result<FlowIndex, FlowError> {
        let path = self.index_path(project_id, workstream_id);
        match fs::read_to_string(&path) {
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FlowIndex::default()),
            Err(e) => Err(FlowError::Io(e)),
        }
    }

    /// Persist a workstream's semantic index, replacing any prior index. Writes
    /// atomically so a crash mid-write cannot corrupt the index.
    pub fn write_index(
        &self,
        project_id: &str,
        workstream_id: &str,
        index: &FlowIndex,
    ) -> Result<(), FlowError> {
        let path = self.index_path(project_id, workstream_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized = serde_json::to_vec(index)?;
        if serialized.len() > MAX_DOC_BYTES {
            return Err(FlowError::DocTooLarge {
                actual: serialized.len(),
            });
        }
        Ok(atomic_write(&path, &serialized)?)
    }

    /// Delete a workstream's index along with its docs. Called by `prune`'s
    /// callers and tests; `delete_workstream` already removes the whole dir.
    pub fn delete_index(&self, project_id: &str, workstream_id: &str) -> Result<(), FlowError> {
        let path = self.index_path(project_id, workstream_id);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    fn workstream_state_path(&self, project_id: &str, workstream_id: &str) -> PathBuf {
        self.root
            .join(project_id)
            .join(FLOW_DIR_NAME)
            .join(workstream_id)
            .join(WORKSTREAM_STATE_FILE)
    }

    /// Load a workstream's persisted mutable state (stage, approval flag, chunk
    /// statuses, iteration counts). Returns `None` when no state has been
    /// persisted yet (first run). The bytes are opaque to this crate; craft-flow
    /// owns the `Workstream` schema and deserializes them.
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

/// Flat embedding index for a single workstream: document rel-path to its
/// embedding vector. The embedding dimension is fixed by the model
/// (`EMBED_DIM` in craft-agent's `semantic` module), so we store untyped
/// `Vec<f32>` and let the caller validate lengths.
///
/// Storage-only: this type knows nothing about how vectors are produced. The
/// embedding model lives behind craft-agent's `EmbeddingService`, so craft-flow
/// computes vectors and hands them here to persist. This keeps the storage
/// crate free of the heavy ML dependency.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FlowIndex {
    pub entries: BTreeMap<String, Vec<f32>>,
}

impl FlowIndex {
    pub fn get(&self, rel_path: &str) -> Option<&[f32]> {
        self.entries.get(rel_path).map(Vec::as_slice)
    }

    pub fn upsert(&mut self, rel_path: impl Into<String>, embedding: Vec<f32>) {
        self.entries.insert(rel_path.into(), embedding);
    }

    pub fn remove(&mut self, rel_path: &str) -> Option<Vec<f32>> {
        self.entries.remove(rel_path)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Drop any entry whose rel-path is no longer present in `live_paths` and
    /// return the set of live paths that have no embedding yet. Keeps the index
    /// in sync after docs are added/removed between index rebuilds.
    pub fn reconcile<'a, I>(&mut self, live_paths: I) -> Vec<String>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let live: std::collections::BTreeSet<&str> = live_paths.into_iter().collect();
        self.entries.retain(|p, _| live.contains(p.as_str()));
        live.into_iter()
            .filter(|p| !self.entries.contains_key(*p))
            .map(str::to_string)
            .collect()
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

fn collect_relative(base: &Path, current: &Path, out: &mut Vec<String>) -> Result<(), FlowError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_relative(base, &path, out)?;
        } else if ft.is_file()
            && entry.file_name() != INDEX_FILE_NAME
            && let Ok(rel) = path.strip_prefix(base)
        {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn newest_mtime(dir: &Path) -> Result<SystemTime, FlowError> {
    let mut newest = SystemTime::UNIX_EPOCH;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_file()
            && entry.file_name() != INDEX_FILE_NAME
            && let Ok(m) = entry.metadata()?.modified()
            && m > newest
        {
            newest = m;
        } else if ft.is_dir()
            && let Ok(m) = newest_mtime(&path)
            && m > newest
        {
            newest = m;
        }
    }
    Ok(newest)
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
        s.write("proj", "ws", "goal.md", "hello").unwrap();
        assert_eq!(s.read("proj", "ws", "goal.md").unwrap(), "hello");
    }

    #[test]
    fn list_returns_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "a").unwrap();
        s.write("proj", "ws", "nested/plan.md", "b").unwrap();
        let mut listed = s.list("proj", "ws").unwrap();
        listed.sort();
        assert_eq!(
            listed,
            vec!["goal.md".to_string(), "nested/plan.md".to_string()]
        );
    }

    #[test]
    fn delete_workstream_removes_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "a").unwrap();
        s.delete_workstream("proj", "ws").unwrap();
        assert!(s.list("proj", "ws").unwrap().is_empty());
    }

    #[test]
    fn read_missing_returns_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        match s.read("proj", "ws", "missing.md") {
            Err(FlowError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn doc_too_large_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let big = "x".repeat(MAX_DOC_BYTES + 1);
        match s.write("proj", "ws", "big.md", &big) {
            Err(FlowError::DocTooLarge { .. }) => {}
            other => panic!("expected DocTooLarge, got {other:?}"),
        }
    }

    #[test_case("/abs/path" ; "absolute_unix")]
    #[test_case("\\windows\\path" ; "absolute_windows")]
    #[test_case("C:/drive" ; "windows_drive")]
    #[test_case("../escape" ; "parent_dir")]
    #[test_case("a/../../etc" ; "nested_parent")]
    fn traversal_rejected(rel: &str) {
        assert!(safe_relative(rel).is_err(), "{rel} should be rejected");
    }

    #[test_case("goal.md" ; "plain")]
    #[test_case("nested/deep/plan.md" ; "nested")]
    #[test_case("./goal.md" ; "cur_dir")]
    fn relative_paths_allowed(rel: &str) {
        assert!(safe_relative(rel).is_ok(), "{rel} should be allowed");
    }

    #[test]
    fn prune_removes_old_workstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "a").unwrap();
        let old = SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 31);
        let path = tmp
            .path()
            .join("proj")
            .join("flow")
            .join("ws")
            .join("goal.md");
        let _ = filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(old));
        let removed = s.prune(Duration::from_secs(60 * 60 * 24 * 30)).unwrap();
        assert_eq!(removed, 1);
        assert!(s.list("proj", "ws").unwrap().is_empty());
    }

    #[test]
    fn prune_keeps_recent_workstreams() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "a").unwrap();
        let removed = s.prune(Duration::from_secs(60 * 60 * 24 * 30)).unwrap();
        assert_eq!(removed, 0);
        assert!(!s.list("proj", "ws").unwrap().is_empty());
    }

    #[test]
    fn read_index_returns_empty_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let idx = s.read_index("proj", "ws").unwrap();
        assert!(idx.is_empty());
    }

    #[test]
    fn write_then_read_index_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let mut idx = FlowIndex::default();
        idx.upsert("goal.md", vec![0.1, 0.2, 0.3]);
        idx.upsert("plan.md", vec![0.4, 0.5, 0.6]);
        s.write_index("proj", "ws", &idx).unwrap();
        let loaded = s.read_index("proj", "ws").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.get("goal.md").unwrap(), &[0.1, 0.2, 0.3]);
        assert_eq!(loaded.get("plan.md").unwrap(), &[0.4, 0.5, 0.6]);
    }

    #[test]
    fn index_is_persisted_alongside_docs_not_inside_them() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "a").unwrap();
        s.write_index("proj", "ws", &FlowIndex::default()).unwrap();
        let listed = s.list("proj", "ws").unwrap();
        assert_eq!(listed, vec!["goal.md".to_string()]);
    }

    #[test]
    fn doc_count_reflects_written_docs() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        assert_eq!(s.doc_count("proj", "ws").unwrap(), 0);
        s.write("proj", "ws", "a.md", "a").unwrap();
        s.write("proj", "ws", "b.md", "b").unwrap();
        assert_eq!(s.doc_count("proj", "ws").unwrap(), 2);
    }

    #[test]
    fn index_too_large_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let mut idx = FlowIndex::default();
        let big = vec![0.0f32; MAX_DOC_BYTES / 4 + 1];
        idx.upsert("big.md", big);
        match s.write_index("proj", "ws", &idx) {
            Err(FlowError::DocTooLarge { .. }) => {}
            other => panic!("expected DocTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn delete_index_removes_only_the_index_file() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write("proj", "ws", "goal.md", "a").unwrap();
        let mut idx = FlowIndex::default();
        idx.upsert("goal.md", vec![0.0]);
        s.write_index("proj", "ws", &idx).unwrap();
        s.delete_index("proj", "ws").unwrap();
        assert!(s.read_index("proj", "ws").unwrap().is_empty());
        assert_eq!(s.list("proj", "ws").unwrap(), vec!["goal.md".to_string()]);
    }

    #[test]
    fn reconcile_drops_missing_and_reports_unembedded() {
        let mut idx = FlowIndex::default();
        idx.upsert("stale.md", vec![0.0]);
        idx.upsert("goal.md", vec![1.0]);
        let missing = idx.reconcile(["goal.md", "plan.md"]);
        assert_eq!(idx.len(), 1);
        assert!(idx.get("stale.md").is_none());
        assert_eq!(missing, vec!["plan.md".to_string()]);
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
        // FNV-1a 64-bit of the empty input is the offset basis.
        assert_eq!(fnv1a_64(b""), "cbf29ce484222325");
    }

    #[test]
    fn fnv1a_64_is_16_hex_chars() {
        let h = fnv1a_64(b"hello");
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn project_id_is_basename_dash_hash() {
        let id = project_id(std::path::Path::new("/Users/me/my-project"));
        assert!(id.starts_with("my-project-"), "got {id}");
        let hash = &id["my-project-".len()..];
        assert_eq!(hash.len(), 16);
    }
}
