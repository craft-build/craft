//! Per-project argosy store: opens-or-inits a writable [`LocalArgosy`] at a
//! fixed directory and shares it behind a mutex, because `LocalArgosy` is not
//! `Sync`. One argosy per project backs both the `memory` namespace (curated
//! notes) and the `document` namespace (Flow workstream documents).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use argosy::bundle::Namespace;
use argosy::{Concept, ConceptId, LocalArgosy};

const MEMORY_CONCEPT_TYPE: &str = "Note";
const DOCUMENT_CONCEPT_TYPE: &str = "Document";

#[derive(Debug, thiserror::Error)]
pub enum ArgosyError {
    #[error(transparent)]
    Argosy(#[from] argosy::Error),
    #[error("invalid concept name: {0}")]
    InvalidName(String),
    #[error("not found: {0}")]
    NotFound(String),
}

/// A shared handle to one project's writable argosy. All operations take the
/// internal lock; callers never hold it across an await.
pub struct ArgosyStore {
    argosy: Mutex<LocalArgosy>,
    root: PathBuf,
}

impl ArgosyStore {
    /// Opens the argosy rooted at `root`, initializing a fresh one when no
    /// manifest exists yet. `name` seeds the manifest (URI identity); it is
    /// sanitized to the bundle-name charset.
    pub fn open_or_init(root: &Path, name: &str) -> Result<Self, ArgosyError> {
        if root.join("argosy.md").is_file() {
            return Ok(Self {
                argosy: Mutex::new(LocalArgosy::open(root)?),
                root: root.to_path_buf(),
            });
        }
        let bundle_name = sanitize_bundle_name(name);
        Ok(Self {
            argosy: Mutex::new(LocalArgosy::init(root, Some(&bundle_name), None)?),
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Escape hatch for read APIs not mirrored here (e.g. concept listings
    /// used by the semantic index). Runs `f` under the lock.
    pub fn with<R>(&self, f: impl FnOnce(&LocalArgosy) -> R) -> R {
        let argosy = self.argosy.lock().unwrap_or_else(|e| e.into_inner());
        f(&argosy)
    }

    pub fn write_memory(&self, name: &str, body: &str) -> Result<(), ArgosyError> {
        let id = memory_id(name)?;
        self.write_concept(Namespace::Memory, &id, MEMORY_CONCEPT_TYPE, body)
    }

    pub fn read_memory(&self, name: &str) -> Result<String, ArgosyError> {
        let id = memory_id(name)?;
        self.read_concept(Namespace::Memory, &id, name)
    }

    pub fn delete_memory(&self, name: &str) -> Result<(), ArgosyError> {
        self.delete_concept(Namespace::Memory, &memory_id(name)?)
    }

    pub fn write_document(&self, name: &str, body: &str) -> Result<(), ArgosyError> {
        let id = document_id(name)?;
        self.write_concept(Namespace::Document, &id, DOCUMENT_CONCEPT_TYPE, body)
    }

    pub fn read_document(&self, name: &str) -> Result<String, ArgosyError> {
        let id = document_id(name)?;
        self.read_concept(Namespace::Document, &id, name)
    }

    pub fn delete_document(&self, name: &str) -> Result<(), ArgosyError> {
        self.delete_concept(Namespace::Document, &document_id(name)?)
    }

    /// Names (`id` minus the namespace prefix) of every concept in `namespace`,
    /// sorted.
    pub fn list(&self, namespace: Namespace) -> Result<Vec<String>, ArgosyError> {
        let argosy = self.argosy.lock().unwrap_or_else(|e| e.into_inner());
        let prefix = format!("{}/", namespace.as_dir_name());
        let mut names: Vec<String> = argosy
            .concepts(&namespace)?
            .into_iter()
            .filter_map(|(id, _)| {
                id.as_str()
                    .strip_prefix(&prefix)
                    .map(str::to_string)
                    .or_else(|| Some(id.as_str().to_string()))
            })
            .collect();
        names.sort();
        Ok(names)
    }

    fn write_concept(
        &self,
        namespace: Namespace,
        id: &ConceptId,
        concept_type: &str,
        body: &str,
    ) -> Result<(), ArgosyError> {
        let concept = Concept::from_str(&format!("---\ntype: {concept_type}\n---\n{body}"))?;
        let argosy = self.argosy.lock().unwrap_or_else(|e| e.into_inner());
        argosy.write_concept(namespace, id, &concept)?;
        Ok(())
    }

    fn read_concept(
        &self,
        namespace: Namespace,
        id: &ConceptId,
        name: &str,
    ) -> Result<String, ArgosyError> {
        let argosy = self.argosy.lock().unwrap_or_else(|e| e.into_inner());
        for (cid, concept) in argosy.concepts(&namespace)? {
            if &cid == id {
                return Ok(concept.body().to_string());
            }
        }
        Err(ArgosyError::NotFound(name.to_string()))
    }

    fn delete_concept(&self, namespace: Namespace, id: &ConceptId) -> Result<(), ArgosyError> {
        let argosy = self.argosy.lock().unwrap_or_else(|e| e.into_inner());
        argosy.delete_concept(namespace, id).map_err(|e| match e {
            argosy::Error::ConceptNotFound { .. } => ArgosyError::NotFound(id.as_str().to_string()),
            other => other.into(),
        })
    }
}

fn memory_id(name: &str) -> Result<ConceptId, ArgosyError> {
    parse_id(&format!("{}/{}", Namespace::Memory.as_dir_name(), name))
}

fn document_id(name: &str) -> Result<ConceptId, ArgosyError> {
    parse_id(&format!("{}/{}", Namespace::Document.as_dir_name(), name))
}

fn parse_id(raw: &str) -> Result<ConceptId, ArgosyError> {
    if raw.ends_with(".md") {
        return Err(ArgosyError::InvalidName(format!(
            "{raw}: concept names are extension-less (`.md` is implicit and              would alias the same-name concept)"
        )));
    }
    raw.parse()
        .map_err(|e: argosy::Error| ArgosyError::InvalidName(format!("{raw}: {e}")))
}

/// Bundle names are restricted to `[A-Za-z0-9._-]`; anything else collapses to
/// `-` so an arbitrary project id can still seed a manifest.
fn sanitize_bundle_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tmp: &Path) -> ArgosyStore {
        ArgosyStore::open_or_init(&tmp.join("argosy"), "My Project!").unwrap()
    }

    #[test]
    fn open_or_init_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("argosy");
        let a = ArgosyStore::open_or_init(&root, "proj").unwrap();
        a.write_memory("note", "hello").unwrap();
        let b = ArgosyStore::open_or_init(&root, "proj").unwrap();
        assert_eq!(b.read_memory("note").unwrap(), "hello");
    }

    #[test]
    fn memory_roundtrip_and_list_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write_memory("gotchas", "keep it simple").unwrap();
        s.write_memory("nested/deep/decision", "we use argosy")
            .unwrap();
        assert_eq!(s.read_memory("gotchas").unwrap(), "keep it simple");
        assert_eq!(
            s.list(Namespace::Memory).unwrap(),
            vec!["gotchas".to_string(), "nested/deep/decision".to_string()]
        );
        assert_eq!(s.list(Namespace::Memory).unwrap().len(), 2);
        s.delete_memory("gotchas").unwrap();
        assert!(matches!(
            s.read_memory("gotchas"),
            Err(ArgosyError::NotFound(_))
        ));
        assert_eq!(s.list(Namespace::Memory).unwrap().len(), 1);
    }

    #[test]
    fn document_roundtrip_is_isolated_from_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write_document("ws/goal", "ship it").unwrap();
        s.write_memory("goal", "note body").unwrap();
        assert_eq!(s.read_document("ws/goal").unwrap(), "ship it");
        assert_eq!(s.read_memory("goal").unwrap(), "note body");
        assert_eq!(
            s.list(Namespace::Document).unwrap(),
            vec!["ws/goal".to_string()]
        );
    }

    #[test]
    fn write_overwrites_existing_concept() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        s.write_memory("n", "v1").unwrap();
        s.write_memory("n", "v2").unwrap();
        assert_eq!(s.read_memory("n").unwrap(), "v2");
    }

    #[test]
    fn md_suffixed_names_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        assert!(s.write_memory("goal.md", "x").is_err());
        assert!(s.write_document("ws/goal.md", "x").is_err());
    }

    #[test]
    fn traversal_names_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        for bad in ["../escape", "a/../../etc", "a\\b", "col:on"] {
            assert!(
                s.write_memory(bad, "x").is_err(),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn body_roundtrips_multiline_markdown_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let s = store(tmp.path());
        let body = "# Title\n\n- a\n- b\n\n```rust\nfn f() {}\n```\n";
        s.write_memory("doc", body).unwrap();
        assert_eq!(s.read_memory("doc").unwrap(), body);
    }
}
