//! Argosy [`Index`] plumbing for craft: a fastembed-5 [`EmbeddingProvider`]
//! (argosy's own provider is gated behind its `default-index` feature, whose
//! fastembed 6 pin conflicts with the workspace's ort version) and a
//! file-backed [`VectorStore`]. Reconcile/search semantics come from
//! `argosy::index::Index` itself; this module only supplies the two traits.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use argosy::Result;
use argosy::bundle::Namespace;
use argosy::context::QualifiedConceptId;
use argosy::index::{EmbeddingProvider, EmbeddingUnit, Filter, SearchHit, UnitMeta, VectorStore};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

const MODEL: EmbeddingModel = EmbeddingModel::BGEBaseENV15;
const MODEL_ID: &str = "craft/fastembed-bge-base-en-v1.5@fastembed-5";

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("embedding model unavailable: {0}")]
    Model(String),
    #[error("embedding inference failed: {0}")]
    Inference(String),
    #[error("vector store io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("vector store json error: {0}")]
    Json(#[from] serde_json::Error),
}

fn argosy_err(e: impl std::fmt::Display) -> argosy::Error {
    argosy::Error::Validation {
        reason: e.to_string(),
    }
}

/// A lazy fastembed-5 provider: model construction (and the first-run
/// download) happens on the first `embed`, so opening the index is free and
/// offline-safe.
pub struct CraftEmbeddingProvider {
    model: Mutex<Option<TextEmbedding>>,
    dimensions: usize,
}

impl CraftEmbeddingProvider {
    pub fn new() -> Result<Self> {
        let dimensions = TextEmbedding::get_model_info(&MODEL)
            .map_err(|e| argosy_err(IndexError::Model(e.to_string())))?
            .dim;
        Ok(Self {
            model: Mutex::new(None),
            dimensions,
        })
    }
}

/// True when the models cache holds at least one downloaded model, so
/// embedding-dependent callers can skip the semantic path (and its first-run
/// download) entirely.
pub fn model_cache_present() -> bool {
    crate::paths::models_dir()
        .ok()
        .and_then(|dir| std::fs::read_dir(&dir).ok())
        .is_some_and(|mut entries| entries.next().is_some())
}

impl Default for CraftEmbeddingProvider {
    fn default() -> Self {
        Self::new().expect("static model metadata always resolves")
    }
}

impl EmbeddingProvider for CraftEmbeddingProvider {
    fn model_id(&self) -> &str {
        MODEL_ID
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut slot = self
            .model
            .lock()
            .map_err(|_| argosy_err(IndexError::Model("mutex poisoned".into())))?;
        if slot.is_none() {
            let mut options = TextInitOptions::new(MODEL).with_show_download_progress(false);
            if let Ok(dir) = crate::paths::models_dir() {
                options = options.with_cache_dir(dir);
            }
            let model = TextEmbedding::try_new(options)
                .map_err(|e| argosy_err(IndexError::Model(e.to_string())))?;
            *slot = Some(model);
        }
        let model = slot.as_mut().expect("populated above");
        model
            .embed(
                texts.iter().map(String::as_str).collect::<Vec<&str>>(),
                None,
            )
            .map_err(|e| argosy_err(IndexError::Inference(e.to_string())))
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    model_id: Option<String>,
    units: BTreeMap<String, StoredUnit>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredUnit {
    qid: QualifiedConceptSerde,
    text_hash: String,
    vector: Vec<f32>,
    meta: UnitMetaSerde,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct QualifiedConceptSerde {
    argosy: String,
    namespace: String,
    id: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UnitMetaSerde {
    concept_type: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    language: Option<String>,
    category: Option<String>,
}

/// A JSON-file-backed [`VectorStore`]: one unit per concept, brute-force
/// cosine ranking over the whole file. Flow corpora are small (tens of
/// documents per project), so exact scan beats a vector database.
pub struct FileVecStore {
    path: PathBuf,
    file: StoreFile,
}

impl FileVecStore {
    /// Loads (or starts) the store persisted at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(argosy_err)?,
            Err(_) => StoreFile::default(),
        };
        Ok(Self { path, file })
    }

    fn persist(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&self.file).map_err(argosy_err)?;
        crate::atomic_write(&self.path, &bytes).map_err(argosy_err)?;
        Ok(())
    }
}

fn qid_key(qid: &QualifiedConceptId) -> String {
    qid.to_uri()
}

fn matches(hit: &StoredUnit, filter: &Filter) -> bool {
    if let Some(namespaces) = &filter.namespaces
        && !namespaces
            .iter()
            .any(|ns| ns.as_dir_name() == hit.qid.namespace)
    {
        return false;
    }
    if let Some(argosies) = &filter.argosies
        && !argosies.contains(&hit.qid.argosy)
    {
        return false;
    }
    if let Some(types) = &filter.concept_types
        && !hit
            .meta
            .concept_type
            .as_deref()
            .is_some_and(|t| types.iter().any(|w| w == t))
    {
        return false;
    }
    if let Some(tags) = &filter.tags
        && !tags.iter().any(|t| hit.meta.tags.contains(t))
    {
        return false;
    }
    if filter.language.as_deref() != hit.meta.language.as_deref() && filter.language.is_some() {
        return false;
    }
    if filter.category.as_deref() != hit.meta.category.as_deref() && filter.category.is_some() {
        return false;
    }
    true
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

impl VectorStore for FileVecStore {
    fn model_id(&self) -> Option<&str> {
        self.file.model_id.as_deref()
    }

    fn set_model_id(&mut self, id: &str) -> Result<()> {
        self.file.model_id = Some(id.to_string());
        self.persist()
    }

    fn upsert(&mut self, units: &[EmbeddingUnit]) -> Result<()> {
        for unit in units {
            let key = qid_key(&unit.concept);
            self.file.units.insert(
                key,
                StoredUnit {
                    qid: QualifiedConceptSerde {
                        argosy: unit.concept.argosy.clone(),
                        namespace: unit.concept.namespace.as_dir_name().to_string(),
                        id: unit.concept.id.as_str().to_string(),
                    },
                    text_hash: unit.text_hash.clone(),
                    vector: unit.vector.clone(),
                    meta: UnitMetaSerde {
                        concept_type: unit.meta.concept_type.clone(),
                        description: unit.meta.description.clone(),
                        tags: unit.meta.tags.clone(),
                        language: unit.meta.language.clone(),
                        category: unit.meta.category.clone(),
                    },
                },
            );
        }
        self.persist()
    }

    fn remove_concept(&mut self, concept: &QualifiedConceptId) -> Result<()> {
        self.file.units.remove(&qid_key(concept));
        self.persist()
    }

    fn unit_hashes(&self) -> Result<std::collections::HashMap<QualifiedConceptId, String>> {
        Ok(self
            .file
            .units
            .values()
            .filter_map(|u| {
                let namespace = Namespace::from_dir_name(&u.qid.namespace);
                if !namespace.is_reserved() {
                    return None;
                }
                let id = u.qid.id.parse().ok()?;
                Some((
                    QualifiedConceptId {
                        argosy: u.qid.argosy.clone(),
                        namespace,
                        id,
                    },
                    u.text_hash.clone(),
                ))
            })
            .collect())
    }

    fn clear(&mut self) -> Result<()> {
        self.file = StoreFile::default();
        self.persist()
    }

    fn search(&self, vector: &[f32], k: usize, filter: &Filter) -> Result<Vec<SearchHit>> {
        let mut scored: Vec<(f32, &StoredUnit)> = self
            .file
            .units
            .values()
            .filter(|u| matches(u, filter))
            .map(|u| (cosine(vector, &u.vector), u))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut hits = Vec::with_capacity(scored.len().min(k));
        for (score, u) in scored.into_iter().take(k) {
            let id = u.qid.id.parse().map_err(argosy_err)?;
            hits.push(SearchHit {
                concept: QualifiedConceptId {
                    argosy: u.qid.argosy.clone(),
                    namespace: Namespace::from_dir_name(&u.qid.namespace),
                    id,
                },
                score,
                meta: UnitMeta {
                    concept_type: u.meta.concept_type.clone(),
                    description: u.meta.description.clone(),
                    tags: u.meta.tags.clone(),
                    language: u.meta.language.clone(),
                    category: u.meta.category.clone(),
                },
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argosy::index::EmbeddingUnit;

    fn qid(argosy: &str, namespace: &str, id: &str) -> QualifiedConceptId {
        QualifiedConceptId {
            argosy: argosy.to_string(),
            namespace: Namespace::from_dir_name(namespace),
            id: id.parse().unwrap(),
        }
    }

    fn unit(qid: QualifiedConceptId, vector: Vec<f32>) -> EmbeddingUnit {
        EmbeddingUnit {
            concept: qid,
            chunk_ordinal: 0,
            text_hash: "h".to_string(),
            vector,
            meta: UnitMeta::default(),
        }
    }

    #[test]
    fn store_roundtrips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.json");
        let mut store = FileVecStore::open(&path).unwrap();
        store.set_model_id("m").unwrap();
        store
            .upsert(&[unit(
                qid("p", "document", "document/ws/goal"),
                vec![1.0, 0.0],
            )])
            .unwrap();
        let reloaded = FileVecStore::open(&path).unwrap();
        assert_eq!(reloaded.model_id(), Some("m"));
        assert_eq!(reloaded.unit_hashes().unwrap().len(), 1);
    }

    #[test]
    fn search_ranks_by_cosine_and_filters_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = FileVecStore::open(tmp.path().join("index.json")).unwrap();
        store
            .upsert(&[
                unit(qid("p", "document", "document/ws/a"), vec![1.0, 0.0]),
                unit(qid("p", "document", "document/ws/b"), vec![0.0, 1.0]),
                unit(qid("p", "memory", "memory/n"), vec![1.0, 0.0]),
            ])
            .unwrap();
        let filter = Filter {
            namespaces: Some(vec![Namespace::Document]),
            ..Default::default()
        };
        let hits = store.search(&[1.0, 0.0], 3, &filter).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].concept.id.as_str().ends_with("a"));
        assert!(hits[0].score > hits[1].score);
    }

    #[test]
    fn remove_and_clear_drop_units() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = FileVecStore::open(tmp.path().join("index.json")).unwrap();
        let key = qid("p", "document", "document/ws/a");
        store.upsert(&[unit(key.clone(), vec![1.0])]).unwrap();
        store.remove_concept(&key).unwrap();
        assert!(store.unit_hashes().unwrap().is_empty());
        store.upsert(&[unit(key, vec![1.0])]).unwrap();
        store.clear().unwrap();
        assert!(store.unit_hashes().unwrap().is_empty());
    }

    #[test]
    fn provider_reports_static_identity_without_loading_model() {
        let provider = CraftEmbeddingProvider::new().unwrap();
        assert!(provider.model_id().contains("fastembed-5"));
        assert_eq!(provider.dimensions(), 768);
    }
}
