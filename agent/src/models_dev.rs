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
//! - Every provider in the catalog is kept: lookup is slug-keyed metadata
//!   only, and several slugs we consult (`amazon-bedrock`, `anthropic`,
//!   `google`, …) declare no REST `api` URL in the catalog.
//! - Entries whose shape we cannot read are skipped and counted in a
//!   warning, never fatal: models.dev ships hundreds of models nobody here
//!   controls, and one unknown field must not cost the whole side-table.
//!   (It has: a `-1` budget floor and `null` effort slots failed the whole
//!   parse, the error was swallowed, and no cache file ever appeared.)
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
// v1 cache serialization discarded reasoning fields; don't treat that reduced
// payload as a fresh catalog after upgrading.
const CATALOG_CACHE_FILE: &str = "models-dev-catalog-v2.json";
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

/// Entries the tolerant parser dropped: field shapes we do not read.
/// Purely informative (a warning) — never a startup failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Skipped {
    models: usize,
    providers: usize,
}

impl Skipped {
    fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogProvider {
    #[serde(default)]
    name: String,
    #[serde(default)]
    npm: String,
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
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    reasoning_options: Vec<crate::thinking::ReasoningOption>,
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
    pub supports_thinking: Option<bool>,
    pub reasoning_options: Vec<crate::thinking::ReasoningOption>,
}

impl ModelMeta {
    fn from_catalog(model: &CatalogModel) -> Self {
        Self {
            supports_thinking: model.reasoning,
            reasoning_options: model.reasoning_options.clone(),
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

/// Flatten each provider's models to [`ModelMeta`]. Unlike the reference —
/// which skips providers with no API URL because it builds an
/// API-addressable meta-provider — this repo uses the catalog purely as a
/// slug-keyed metadata side-table, and slugs like `amazon-bedrock` carry no
/// REST `api` URL while holding exactly the limits we need.
fn flatten(index: CatalogIndex) -> HashMap<String, HashMap<String, ModelMeta>> {
    index
        .into_iter()
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

/// Parse a catalog payload into a [`CatalogIndex`], tolerating entries we
/// cannot read. The root must be a JSON object (anything else is a broken
/// document, not a rough one); below it, a provider that fails to decode
/// wholesale is salvaged model-by-model, so a bad entry drops only itself.
/// An empty result is rejected too — a catalog with no providers is a
/// broken payload, and caching it would pin "fresh" nothing for a day.
fn parse_index(text: &str) -> Result<(CatalogIndex, Skipped)> {
    let root: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| invalid(format!("failed to parse models.dev catalog JSON: {e}")))?;
    let Some(providers) = root.as_object() else {
        return Err(invalid("models.dev catalog root is not a JSON object"));
    };
    let mut index = CatalogIndex::new();
    let mut skipped = Skipped::default();
    for (slug, provider) in providers {
        // Fast path: a provider that decodes wholesale.
        if let Ok(parsed) = serde_json::from_value::<CatalogProvider>(provider.clone()) {
            index.insert(slug.clone(), parsed);
            continue;
        }
        // Salvage: decode the models one at a time, so an entry whose shape
        // models.dev grew overnight drops only itself, not its provider and
        // not the catalog.
        let mut models = HashMap::new();
        if let Some(entries) = provider
            .get("models")
            .and_then(serde_json::Value::as_object)
        {
            for (id, model) in entries {
                match serde_json::from_value::<CatalogModel>(model.clone()) {
                    Ok(model) => {
                        models.insert(id.clone(), model);
                    }
                    Err(_) => skipped.models += 1,
                }
            }
        }
        if models.is_empty() {
            skipped.providers += 1;
            continue;
        }
        index.insert(
            slug.clone(),
            CatalogProvider {
                name: provider
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                npm: provider
                    .get("npm")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                models,
            },
        );
    }
    if index.is_empty() {
        return Err(invalid(
            "models.dev catalog payload contained no readable providers",
        ));
    }
    if !skipped.is_empty() {
        tracing::warn!(
            models = skipped.models,
            providers = skipped.providers,
            "models.dev catalog: skipped entries with unreadable shapes"
        );
    }
    Ok((index, skipped))
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
            if let Some(text) = std::fs::read_to_string(path).ok()
                && let Ok((index, _)) = parse_index(&text)
            {
                let flattened = flatten(index);
                if !flattened.is_empty() {
                    *catalog().write().unwrap() = Arc::new(flattened);
                }
                return Ok(());
            }
            // Warming is best-effort and callers ignore this error, so the
            // trail must be left here: a silently cold catalog looks exactly
            // like "no token limits anywhere" until someone goes looking.
            tracing::warn!("models.dev catalog unavailable (fetch failed, no usable cache): {e}");
            return Err(e);
        }
    };
    let (index, _) = parse_index(&text)?;
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
    let provider_kind = match provider_kind {
        "gemini" => "google",
        "chatgpt" => "openai",
        "copilot" => "github-copilot",
        "moonshot" => "moonshotai",
        other => other,
    };
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
    parse_index(&text).ok().map(|(index, _)| index)
}

fn save_cached(path: &Path, index: &CatalogIndex) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(index) {
        // A read-only cache dir is not worth failing startup over, but the
        // next launch has to know why it keeps refetching instead of using
        // the file ("did not update it" must be diagnosable).
        if let Err(e) = std::fs::write(path, &text) {
            tracing::warn!(
                "failed to write the models.dev cache at {}: {e}",
                path.display()
            );
        }
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

// --- Test support -----------------------------------------------------------

/// Hooks over the process-global catalog for cross-module tests: bedrock's
/// discovery tests install fixtures through the same lock and swap logic
/// this module's own tests use, so the global is never mutated from two
/// tests at once.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// The catalog global is process-wide; serialize the tests that touch
    /// it. Async so the guards may be held across `.await`s in tokio tests.
    pub(crate) static GLOBAL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Swap the global catalog for `data`, returning the previous catalog
    /// for restoration.
    pub(crate) fn swap(data: HashMap<String, HashMap<String, ModelMeta>>) -> SharedCatalog {
        let mut guard = catalog().write().unwrap();
        let previous = guard.clone();
        *guard = Arc::new(data);
        previous
    }

    /// Install the catalog parsed from `text` (tolerant parse), returning
    /// the previous catalog to hand back to [`restore`] when the test ends.
    pub(crate) fn install(text: &str) -> SharedCatalog {
        let (index, _) = parse_index(text).expect("test catalog payload parses");
        swap(flatten(index))
    }

    /// Put back what [`swap`]/[`install`] replaced.
    pub(crate) fn restore(previous: SharedCatalog) {
        swap(previous.as_ref().clone());
    }
}

// --- Tests -----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use super::testing::GLOBAL_LOCK;

    #[test]
    fn thinking_choices_use_synthetic_and_zai_endpoint_catalogs() {
        use crate::thinking::{Effort, ThinkingConfig, choices_for};
        let _guard = GLOBAL_LOCK.blocking_lock();
        let index = serde_json::from_value(serde_json::json!({
            "synthetic": {
                "api": "https://api.synthetic.new/openai/v1",
                "models": {
                    "hf:zai-org/GLM-5.3": {
                        "reasoning": true,
                        "reasoning_options": [{"type": "effort", "values": ["low", "high", "max"]}]
                    }
                }
            },
            "zai-coding-plan": {
                "api": "https://api.z.ai/api/coding/paas/v4",
                "models": {
                    "glm-5.2-highspeed": {
                        "reasoning": true,
                        "reasoning_options": [{"type": "effort", "values": ["high", "max"]}]
                    }
                }
            }
        }))
        .unwrap();
        let previous = swap_catalog(flatten(index));
        let config = crate::config::Config::parse(
            r#"
            provider "gateway" {
                kind = "openai-compatible"
                base_url = "https://api.synthetic.new/openai/v1"
            }
            provider "coding" {
                kind = "zai"
                base_url = "https://api.z.ai/api/coding/paas/v4"
            }
            "#,
        )
        .unwrap();
        let synthetic = choices_for(
            ThinkingConfig::Effort(Effort::Medium),
            &config.providers["gateway"],
            "hf:zai-org/GLM-5.3",
        );
        let zai = choices_for(
            ThinkingConfig::Off,
            &config.providers["coding"],
            "glm-5.2-highspeed",
        );
        swap_catalog(previous.as_ref().clone());
        assert_eq!(
            synthetic,
            vec![
                ThinkingConfig::Adaptive,
                ThinkingConfig::Effort(Effort::Low),
                ThinkingConfig::Effort(Effort::High),
                ThinkingConfig::Effort(Effort::Max),
            ]
        );
        assert_eq!(
            zai,
            vec![
                ThinkingConfig::Adaptive,
                ThinkingConfig::Effort(Effort::High),
                ThinkingConfig::Effort(Effort::Max),
            ]
        );
    }

    #[test]
    fn reasoning_metadata_survives_cache_serialization() {
        let model: CatalogModel = serde_json::from_value(serde_json::json!({
            "reasoning": true,
            "reasoning_options": [
                {"type": "effort", "values": ["none", "low", "xhigh"]},
                {"type": "budget_tokens", "min": 1024, "max": 32768},
                {"type": "future_knob"}
            ]
        }))
        .unwrap();
        let meta = ModelMeta::from_catalog(&model);
        assert_eq!(meta.supports_thinking, Some(true));
        assert_eq!(meta.reasoning_options[0].values, ["none", "low", "xhigh"]);
        let cached: CatalogModel =
            serde_json::from_str(&serde_json::to_string(&model).unwrap()).unwrap();
        assert_eq!(ModelMeta::from_catalog(&cached), meta);
    }

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

    /// The exact rough edges models.dev shipped that previously failed the
    /// whole parse (and, with every caller swallowing the error, left the
    /// catalog empty and the cache file unwritten): a `-1` budget floor on
    /// an nvidia model and `null` slots inside sarvam effort lists.
    #[test]
    fn parse_index_tolerates_the_live_catalogs_rough_edges() {
        let (index, skipped) = parse_index(
            r#"{
              "nvidia": {"models": {
                "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning": {
                  "limit": {"context": 256000, "output": 65536},
                  "reasoning_options": [
                    {"type": "toggle"},
                    {"type": "budget_tokens", "min": -1, "max": 32768}
                  ]
                }
              }},
              "sarvam": {"models": {
                "sarvam-30b": {
                  "limit": {"context": 1000, "output": 500},
                  "reasoning_options": [
                    {"type": "effort", "values": [null, "low", "medium", "high"]}
                  ]
                }
              }}
            }"#,
        )
        .unwrap();
        // Before the lenient parse this failed wholesale: no entry parsed.
        assert!(
            skipped.is_empty(),
            "rough shapes must be tolerated, not skipped"
        );

        let flattened = flatten(index);
        let nemotron = &flattened["nvidia"]["nvidia/nemotron-3-nano-omni-30b-a3b-reasoning"];
        assert_eq!(nemotron.context, 256_000);
        assert_eq!(nemotron.output, 65_536);
        assert_eq!(nemotron.reasoning_options[1].kind, "budget_tokens");
        assert_eq!(
            nemotron.reasoning_options[1].min, None,
            "-1 floors read as unset"
        );
        assert_eq!(nemotron.reasoning_options[1].max, Some(32_768));

        let sarvam = &flattened["sarvam"]["sarvam-30b"];
        assert_eq!(sarvam.context, 1_000);
        assert_eq!(sarvam.output, 500);
        assert_eq!(
            sarvam.reasoning_options[0].values,
            ["low", "medium", "high"]
        );
    }

    /// One undecodable model drops only itself; a provider that fails
    /// wholesale (a null `name`, no `models`) is salvaged per-model or
    /// skipped without dragging anything else down. (A positional
    /// `"limit": [context, output]` array is valid input to serde's derived
    /// struct deserializer, so the fixture needs a genuinely unreadable
    /// field to exercise the drop.)
    #[test]
    fn parse_index_drops_unreadable_entries_without_losing_the_rest() {
        let (index, skipped) = parse_index(
            r#"{
              "vendor": {"name": null, "models": {
                "good": {"limit": {"context": 9000}},
                "stringlimit": {"limit": {"context": "not-a-number"}},
                "bare": {}
              }},
              "modelsless": {"name": "Empty"},
              "healthy": {"models": {"m": {"limit": {"context": 1, "output": 2}}}}
            }"#,
        )
        .unwrap();
        assert_eq!(skipped.models, 1, "only the string-limit model is dropped");
        assert_eq!(
            skipped.providers, 1,
            "the provider without models is dropped"
        );

        let vendor = index.get("vendor").expect("salvaged provider is kept");
        assert_eq!(
            vendor.name, "",
            "a null name degrades to empty, not to a drop"
        );
        let good = vendor.models.get("good").expect("the good model survives");
        assert_eq!(good.limit.as_ref().and_then(|l| l.context), Some(9_000));
        assert!(
            vendor.models.contains_key("bare"),
            "a model with no known fields still parses"
        );
        assert!(!vendor.models.contains_key("stringlimit"));

        let healthy = index.get("healthy").expect("untouched provider is kept");
        assert_eq!(healthy.models.len(), 1);
    }

    #[test]
    fn parse_index_rejects_broken_documents() {
        assert!(parse_index("not json").is_err());
        assert!(parse_index("[]").is_err(), "a non-object root is broken");
        assert!(parse_index("42").is_err());
        assert!(
            parse_index("{}").is_err(),
            "no readable providers means broken"
        );
    }

    /// An empty payload must not be written as a fresh cache: it would pin
    /// nothing for a day while every lookup still misses.
    #[tokio::test]
    async fn populate_refuses_to_cache_an_empty_catalog() {
        let _guard = GLOBAL_LOCK.lock().await;
        let (_tmp, path) = cache_path();
        let previous = swap_catalog(HashMap::new());
        let fetch = || {
            Box::pin(async { Ok("{}".to_string()) })
                as futures::future::BoxFuture<'static, Result<String>>
        };
        assert!(populate(&path, SystemTime::now(), fetch).await.is_err());
        assert!(!path.exists(), "an empty catalog is never cached");
        swap_catalog(previous.as_ref().clone());
    }

    fn swap_catalog(data: HashMap<String, HashMap<String, ModelMeta>>) -> SharedCatalog {
        testing::swap(data)
    }

    #[test]
    fn flatten_keeps_all_providers_and_applies_defaults() {
        let flattened = flatten(parsed_index());
        assert_eq!(flattened.len(), 2);
        assert!(flattened.contains_key("some-vendor"));
        // Providers without an API URL are kept: lookup is slug-keyed, and
        // e.g. `amazon-bedrock` declares none while holding real limits.
        assert!(flattened.contains_key("no-api"));
        assert_eq!(flattened["no-api"]["m"].context, 128_000);

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
        assert_eq!(loaded["some-vendor"].name, "Some Vendor");
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
