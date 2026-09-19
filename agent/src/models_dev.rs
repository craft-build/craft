//! models.dev catalog with a 24h disk cache.
//!
//! Ported from the reference `craft-providers/src/providers/opencode.rs`
//! catalog half (fetch/cache/parse/lookup), adapted to this repo's
//! Rig-based architecture: there is no catalog-meta provider here, so the
//! catalog serves as a metadata side-table for provider/model discovery —
//! it fills context windows and output limits the Rig listing lacks, and
//! carries per-model pricing for later cost wiring (task 54).
//!
//! Semantics kept from the reference:
//! - `https://models.dev/api.json`, fetched at most once per day; the disk
//!   cache in the XDG cache dir is keyed by file mtime age vs a 24h TTL.
//! - A stale cache with a failing fetch leaves the warm catalog untouched
//!   and lookups simply return `None` until one succeeds. Catalog failures
//!   never break startup.
//! - Providers without an API URL are skipped: they are not
//!   API-addressable, so their metadata can never be used.
//!
//! One deliberate divergence: the reference keys models by exact ID because
//! its own model IDs *are* catalog IDs. Here discovery comes from Rig, whose
//! IDs sometimes carry dated suffixes (`claude-sonnet-5-20260901`), so lookup
//! falls back to longest-prefix match, same strategy as `usage::resolve`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, client_error};

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(86_400);
/// Fetch budget: the catalog is a nice-to-have, never worth hanging startup.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall budget a caller should give [`warm`]; the catalog is a
/// nice-to-have and must never hang startup.
pub const FETCH_BUDGET: Duration = Duration::from_secs(20);

fn invalid(reason: impl Into<String>) -> Error {
    Error::Invalid {
        reason: reason.into(),
    }
}

type CatalogIndex = HashMap<String, CatalogProvider>;

#[derive(Deserialize, Serialize, Clone)]
struct CatalogProvider {
    #[serde(default)]
    name: String,
    #[serde(default)]
    npm: String,
    api: Option<String>,
    models: HashMap<String, CatalogModel>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogModel {
    #[serde(default)]
    limit: Option<CatalogLimits>,
    #[serde(default)]
    cost: Option<CatalogCost>,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    modalities: Option<CatalogModalities>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogModalities {
    #[serde(default)]
    input: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogLimits {
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    output: Option<u32>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogCost {
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

/// Flattened per-model metadata, the repo-side equivalent of the reference
/// `CatalogMeta`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelMeta {
    pub context: u32,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub supports_vision: bool,
}

impl ModelMeta {
    fn from_catalog(model: &CatalogModel) -> Self {
        Self {
            context: model
                .limit
                .as_ref()
                .and_then(|l| l.context)
                .unwrap_or(128_000),
            output: model
                .limit
                .as_ref()
                .and_then(|l| l.output)
                .unwrap_or(64_000),
            input_price: model.cost.as_ref().and_then(|c| c.input).unwrap_or(0.0),
            output_price: model.cost.as_ref().and_then(|c| c.output).unwrap_or(0.0),
            cache_read: model
                .cost
                .as_ref()
                .and_then(|c| c.cache_read)
                .unwrap_or(0.0),
            cache_write: model
                .cost
                .as_ref()
                .and_then(|c| c.cache_write)
                .unwrap_or(0.0),
            supports_vision: model.attachment
                || model
                    .modalities
                    .as_ref()
                    .is_some_and(|m| m.input.iter().any(|s| s == "image")),
        }
    }
}

/// Keep only API-addressable providers, flattening each model to
/// [`ModelMeta`]. Mirrors the reference's skip rule of dropping providers
/// with no API URL; the other reference filters (blocked slugs, npm
/// allowlist, builtin overlap) exist to keep the catalog from shadowing
/// native providers inside its meta-provider, which this repo does not have.
fn flatten(index: CatalogIndex) -> HashMap<String, HashMap<String, ModelMeta>> {
    index
        .into_iter()
        .filter(|(_, provider)| provider.api.is_some())
        .map(|(slug, provider)| {
            let models = provider
                .models
                .iter()
                .map(|(id, model)| (id.clone(), ModelMeta::from_catalog(model)))
                .collect();
            (slug, models)
        })
        .collect()
}

// --- Global state ----------------------------------------------------------

type SharedCatalog = Arc<HashMap<String, HashMap<String, ModelMeta>>>;

static CATALOG: OnceLock<RwLock<SharedCatalog>> = OnceLock::new();

fn catalog() -> &'static RwLock<SharedCatalog> {
    CATALOG.get_or_init(|| RwLock::new(Arc::new(HashMap::new())))
}

/// Load the catalog from the on-disk cache, fetching once if the cache is
/// cold or stale. Best-effort: failures are returned to the caller, who
/// should ignore them — lookups just stay `None` until a later warm succeeds.
/// Idempotent within the process once it succeeds.
pub async fn warm() -> Result<()> {
    let dir: PathBuf = crate::paths::cache_dir().map_err(|e| invalid(e.to_string()))?;
    let path = dir.join(CATALOG_CACHE_FILE);
    populate(&path, SystemTime::now(), || Box::pin(fetch_remote()) as _).await
}

/// Path- and fetch-injectable core of [`warm`], so tests exercise the whole
/// cache lifecycle without the network.
async fn populate(
    path: &Path,
    now: SystemTime,
    fetch: impl FnOnce() -> futures::future::BoxFuture<'static, Result<String>>,
) -> Result<()> {
    if let Some(index) = load_cached(path, now) {
        *catalog().write().unwrap() = Arc::new(flatten(index));
        return Ok(());
    }
    let fetched = fetch().await;
    let text = match fetched {
        Ok(text) => text,
        Err(e) => {
            // Last resort: a stale (TTL-expired) cache still beats an empty
            // catalog when the refresh cannot be fetched (offline, models.dev
            // down). A later successful refresh overwrites it.
            if let Some(index) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
            {
                let flattened = flatten(index);
                if !flattened.is_empty() {
                    *catalog().write().unwrap() = Arc::new(flattened);
                }
                return Ok(());
            }
            return Err(e);
        }
    };
    let index: CatalogIndex = serde_json::from_str(&text)
        .map_err(|e| invalid(format!("failed to parse models.dev catalog JSON: {e}")))?;
    save_cached(path, &index);
    let flattened = flatten(index);
    if !flattened.is_empty() {
        *catalog().write().unwrap() = Arc::new(flattened);
    }
    Ok(())
}

/// Metadata for `provider/model_id`: exact ID match first, then longest
/// prefix match on a token boundary — the remainder after the prefix must
/// start with a separator (`-`, `.`, `:`), so `gpt-4` never matches
/// `gpt-4o`. `None` until [`warm`] succeeds (or forever, offline).
pub fn metadata_for(provider: &str, model_id: &str) -> Option<ModelMeta> {
    let guard = catalog().read().unwrap();
    let models = guard.get(provider)?;
    models.get(model_id).cloned().or_else(|| {
        models
            .iter()
            .filter(|(id, _)| {
                model_id.starts_with(id.as_str())
                    && model_id[id.len()..]
                        .chars()
                        .next()
                        .is_some_and(|c| matches!(c, '-' | '.' | ':'))
            })
            .max_by_key(|(id, _)| id.len())
            .map(|(_, meta)| meta.clone())
    })
}

/// Context window / output limit for a model, for discovery enrichment.
pub fn context_for(provider: &str, model_id: &str) -> Option<(u32, u32)> {
    metadata_for(provider, model_id).map(|meta| (meta.context, meta.output))
}

/// Fill `None` context/output fields on discovered catalog entries. Known
/// values win: discovery metadata is never overwritten, and models the
/// catalog does not know keep whatever discovery gave them.
pub fn enrich_catalog<M: crate::providers::CatalogEntry>(provider_kind: &str, models: &mut [M]) {
    for model in models {
        if let Some((context, output)) = context_for(provider_kind, model.id()) {
            model.context_length_mut().get_or_insert(context);
            model.max_output_tokens_mut().get_or_insert(output);
        }
    }
}

// --- Cache -----------------------------------------------------------------

fn load_cached(path: &Path, now: SystemTime) -> Option<CatalogIndex> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age > CATALOG_CACHE_TTL {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_cached(path: &Path, index: &CatalogIndex) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(index) {
        // A read-only cache dir is not worth failing startup over.
        let _ = std::fs::write(path, &text);
    }
}

async fn fetch_remote() -> Result<String> {
    let client = reqwest::Client::builder()
        .connect_timeout(FETCH_CONNECT_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| invalid(format!("failed to build models.dev catalog client: {e}")))?;
    let response = client.get(CATALOG_URL).send().await.map_err(client_error)?;
    if !response.status().is_success() {
        return Err(invalid(format!(
            "models.dev catalog fetch returned HTTP {}",
            response.status().as_u16()
        )));
    }
    response.text().await.map_err(client_error)
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// The catalog global is process-wide; serialize the tests that touch it.
    /// Async so the guards may be held across `.await`s in tokio tests.
    static GLOBAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn sample_catalog_json() -> String {
        r#"{
          "some-vendor": {
            "name": "Some Vendor",
            "npm": "@ai-sdk/openai-compatible",
            "api": "https://api.somevendor.test/v1",
            "models": {
              "big": { "limit": {"context": 200000, "output": 32000},
                        "cost": {"input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.75} },
              "visionary": { "attachment": true },
              "reader": { "modalities": {"input": ["text", "image"]} },
              "dated-20260101": { "limit": {"context": 9000} }
            }
          },
          "no-api": {
            "name": "Skipped",
            "npm": "@ai-sdk/x",
            "models": { "m": {} }
          }
        }"#
        .to_string()
    }

    fn parsed_index() -> CatalogIndex {
        serde_json::from_str(&sample_catalog_json()).unwrap()
    }

    fn cache_path() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(CATALOG_CACHE_FILE);
        (tmp, path)
    }

    fn swap_catalog(data: HashMap<String, HashMap<String, ModelMeta>>) -> SharedCatalog {
        let mut guard = catalog().write().unwrap();
        let previous = guard.clone();
        *guard = Arc::new(data);
        previous
    }

    #[test]
    fn flatten_skips_providers_without_api_and_applies_defaults() {
        let flattened = flatten(parsed_index());
        assert_eq!(flattened.len(), 1);
        assert!(flattened.contains_key("some-vendor"));

        let big = &flattened["some-vendor"]["big"];
        assert_eq!(big.context, 200_000);
        assert_eq!(big.output, 32_000);
        assert_eq!(big.input_price, 3.0);
        assert_eq!(big.cache_write, 3.75);
        assert!(!big.supports_vision);

        // Missing limits/cost default to the reference's 128k/64k and 0.0.
        let visionary = &flattened["some-vendor"]["visionary"];
        assert_eq!(visionary.context, 128_000);
        assert_eq!(visionary.output, 64_000);
        assert_eq!(visionary.input_price, 0.0);
        assert!(visionary.supports_vision); // attachment

        let reader = &flattened["some-vendor"]["reader"];
        assert!(reader.supports_vision); // image input modality

        let dated = &flattened["some-vendor"]["dated-20260101"];
        assert_eq!(dated.context, 9_000);
        assert_eq!(dated.output, 64_000);
    }

    #[test]
    fn cache_round_trip_and_ttl() {
        let (_tmp, path) = cache_path();
        let index = parsed_index();
        // `now` sits one hour after the cache was written.
        let now = SystemTime::now() + Duration::from_secs(3_600);

        assert!(load_cached(&path, now).is_none());

        save_cached(&path, &index);
        let loaded = load_cached(&path, now).unwrap();
        assert_eq!(
            loaded["some-vendor"].api.as_deref(),
            Some("https://api.somevendor.test/v1")
        );
        assert_eq!(loaded["some-vendor"].models.len(), 4);

        // A cache older than the TTL (written "25h ago" relative to `now`)
        // is stale.
        let stale_now = now + CATALOG_CACHE_TTL + Duration::from_secs(3_600);
        assert!(load_cached(&path, stale_now).is_none());
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let (_tmp, path) = cache_path();
        std::fs::write(&path, "not json").unwrap();
        assert!(load_cached(&path, SystemTime::now()).is_none());
    }

    #[tokio::test]
    async fn populate_fresh_cache_fetches_saves_and_sets_global() {
        let _guard = GLOBAL_LOCK.lock().await;
        let (_tmp, path) = cache_path();
        let previous = swap_catalog(HashMap::new());

        let calls = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(sample_catalog_json()) })
                as futures::future::BoxFuture<'static, Result<String>>
        };
        populate(&path, SystemTime::now(), fetch).await.unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Cache written for next time.
        assert!(path.exists());
        let meta = metadata_for("some-vendor", "big").unwrap();
        assert_eq!(meta.context, 200_000);

        // Second populate hits the cache: no new fetch.
        populate(&path, SystemTime::now(), fetch).await.unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        swap_catalog(previous.as_ref().clone());
    }

    #[test]
    fn metadata_for_exact_then_longest_prefix() {
        let _guard = GLOBAL_LOCK.blocking_lock();
        let previous = swap_catalog(flatten(parsed_index()));

        assert_eq!(
            metadata_for("some-vendor", "big").map(|m| m.context),
            Some(200_000)
        );
        // Dated snapshot resolves via prefix to its base model.
        assert_eq!(
            metadata_for("some-vendor", "dated-20260101-x").map(|m| m.context),
            Some(9_000)
        );
        assert!(metadata_for("some-vendor", "unknown").is_none());
        // Prefix match requires a separator after the prefix.
        assert_eq!(
            metadata_for("some-vendor", "dated-20260101-x").map(|m| m.context),
            Some(9_000)
        );
        assert!(metadata_for("some-vendor", "dated-202601012").is_none());
        assert!(metadata_for("some-vendor", "bigge").is_none());
        assert!(metadata_for("no-api", "m").is_none());
        assert!(metadata_for("other", "big").is_none());
        assert_eq!(context_for("some-vendor", "big"), Some((200_000, 32_000)));

        swap_catalog(previous.as_ref().clone());
    }

    #[test]
    fn enrich_catalog_fills_only_missing_fields() {
        let _guard = GLOBAL_LOCK.blocking_lock();
        let previous = swap_catalog(flatten(parsed_index()));

        let entry =
            |id: &str, context: Option<u32>, output: Option<u32>| crate::providers::CatalogModel {
                id: id.to_string(),
                name: None,
                description: None,
                context_length: context,
                max_output_tokens: output,
            };
        let mut models = vec![
            entry("big", None, None),
            entry("big", Some(1_000), Some(2_000)), // discovery wins
            entry("unknown", None, None),           // not in catalog: untouched
        ];
        super::enrich_catalog("some-vendor", &mut models);

        assert_eq!(models[0].context_length, Some(200_000));
        assert_eq!(models[0].max_output_tokens, Some(32_000));
        assert_eq!(models[1].context_length, Some(1_000));
        assert_eq!(models[1].max_output_tokens, Some(2_000));
        assert_eq!(models[2].context_length, None);
        assert_eq!(models[2].max_output_tokens, None);

        swap_catalog(previous.as_ref().clone());
    }

    #[tokio::test]
    async fn populate_fetch_failure_keeps_global_untouched_and_no_cache() {
        let _guard = GLOBAL_LOCK.lock().await;
        let (_tmp, path) = cache_path();
        let previous = swap_catalog(flatten(parsed_index()));

        let fetch = || {
            Box::pin(async { Err(invalid("network down")) })
                as futures::future::BoxFuture<'static, Result<String>>
        };
        assert!(populate(&path, SystemTime::now(), fetch).await.is_err());
        assert!(!path.exists());
        // A warm catalog from an earlier run survives a failed refresh.
        assert!(metadata_for("some-vendor", "big").is_some());

        swap_catalog(previous.as_ref().clone());
    }

    #[tokio::test]
    async fn populate_fetch_failure_serves_stale_cache() {
        let _guard = GLOBAL_LOCK.lock().await;
        let (_tmp, path) = cache_path();
        let previous = swap_catalog(HashMap::new());

        // Seed a cache, then view it from a `now` far past the TTL so the
        // fast path rejects it and the fetch is attempted (and fails).
        std::fs::write(&path, serde_json::to_string(&parsed_index()).unwrap()).unwrap();
        let stale_now = SystemTime::now() + CATALOG_CACHE_TTL + Duration::from_secs(3_600);
        let fetch_err = || {
            Box::pin(async { Err(invalid("network down")) })
                as futures::future::BoxFuture<'static, Result<String>>
        };
        populate(&path, stale_now, fetch_err).await.unwrap();
        assert!(
            metadata_for("some-vendor", "big").is_some(),
            "stale disk cache is served when the refresh fetch fails"
        );

        swap_catalog(previous.as_ref().clone());
    }
}
