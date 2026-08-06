use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use craft_config::providers::builtin_provider;
use craft_storage::StateDir;
use craft_storage::auth::load_provider_credentials;
use craft_storage::id::SessionRef;

use flume::Sender;
use futures::TryStreamExt;
use futures::io::BufReader;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::io::StreamReader;
use tracing::{debug, warn};

use crate::manifest::ManifestRegistry;
use crate::model::{Model, ModelInfo, ModelPricing};
use crate::provider::Provider;
use crate::providers::anthropic::shared;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::{ResolvedAuth, http_client};

const BLOCKED_PROVIDER_IN_CATALOG: &[&str] = &["zai", "zai-coding-plan", "github-copilot"];

/// Catalog-backed opencode-family slugs. These are built-in manifests without a
/// `ProviderKind`, surfaced through the models.dev catalog, so they must be
/// exempt from the "skip slugs owned by a builtin provider" filter in
/// `CatalogData::from_index`.
pub(crate) const OPENCODE_FAMILY_SLUGS: &[&str] = &["opencode", "opencode-go"];

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(86400);

const MESSAGES_PATH: &str = "/messages";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    ChatCompletions,
    Messages,
}

type CatalogIndex = HashMap<String, CatalogProvider>;

#[derive(Deserialize, Serialize)]
struct CatalogProvider {
    name: String,
    #[serde(default)]
    env: Vec<String>,
    npm: String,
    api: Option<String>,
    models: HashMap<String, CatalogModel>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogModel {
    limit: Option<CatalogLimits>,
    #[serde(default)]
    cost: Option<CatalogCost>,
    #[serde(default)]
    provider: Option<CatalogShape>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogLimits {
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    input: Option<u32>,
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

#[derive(Deserialize, Serialize, Clone)]
struct CatalogShape {
    #[serde(default)]
    shape: Option<String>,
}

/// Provider metadata from the catalog, exposed for the login UI.
#[derive(Clone, Debug)]
pub struct ProviderData {
    pub slug: String,
    pub display_name: String,
    pub env_keys: Vec<String>,
    pub base_url: Option<String>,
    pub npm: String,
    pub api_format: EndpointType,
    pub models: HashMap<String, CatalogMeta>,
}

fn is_free_model(meta: &CatalogMeta) -> bool {
    meta.input_price == 0.0 && meta.output_price == 0.0
}

impl ProviderData {
    fn new(
        slug: String,
        catalog_provider: &CatalogProvider,
        api_format: EndpointType,
        models: HashMap<String, CatalogMeta>,
    ) -> Self {
        Self {
            slug,
            display_name: catalog_provider.name.clone(),
            env_keys: catalog_provider.env.clone(),
            base_url: catalog_provider.api.clone(),
            npm: catalog_provider.npm.clone(),
            api_format,
            models,
        }
    }

    pub fn load_key_from_storage(&self, state_dir: &StateDir) -> Option<String> {
        let creds = load_provider_credentials(state_dir, &self.slug)?;
        Some(creds.api_key)
    }

    pub fn resolve_api_key(&self, state_dir: &StateDir) -> Option<String> {
        for var in &self.env_keys {
            if let Ok(val) = std::env::var(var) {
                debug!(provider = %self.display_name, var = %var, "api key resolved from env");
                return Some(val);
            }
        }
        if let Some(key) = self.load_key_from_storage(state_dir) {
            debug!(provider = %self.display_name, "api key resolved from storage");
            return Some(key);
        }
        None
    }

    pub fn env_key_set(&self) -> Option<&str> {
        self.env_keys
            .iter()
            .find(|e| std::env::var(e).is_ok())
            .map(|s| s.as_str())
    }

    fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        match self.npm.as_str() {
            "@ai-sdk/anthropic" => vec![("x-api-key".into(), api_key.into())],
            _ => vec![("authorization".into(), format!("Bearer {api_key}"))],
        }
    }

    pub fn build_auth(&self, state_dir: &StateDir) -> Authentication {
        let api_key = match self.resolve_api_key(state_dir) {
            Some(key) => key,
            None if OPENCODE_FAMILY_SLUGS.contains(&self.slug.as_str()) => {
                return Authentication::OpenCodeFreeKey(ResolvedAuth {
                    base_url: self.base_url.clone(),
                    headers: self.auth_headers("public"),
                });
            }
            None => return Authentication::NoAuth,
        };
        Authentication::KeyBased(ResolvedAuth {
            base_url: self.base_url.clone(),
            headers: self.auth_headers(&api_key),
        })
    }

    pub fn resolve_auth(&self, state_dir: &StateDir) -> Option<ResolvedAuth> {
        match self.build_auth(state_dir) {
            Authentication::KeyBased(auth) | Authentication::OpenCodeFreeKey(auth) => Some(auth),
            Authentication::NoAuth => None,
        }
    }

    pub fn resolve_auth_with_override(
        &self,
        override_auth: Option<&Arc<Mutex<ResolvedAuth>>>,
        state_dir: &StateDir,
    ) -> Option<ResolvedAuth> {
        if OPENCODE_FAMILY_SLUGS.contains(&self.slug.as_str())
            && let Some(auth) = override_auth
        {
            return Some(auth.lock().unwrap().clone());
        }
        self.resolve_auth(state_dir)
    }

    pub fn available_models(
        &self,
        state_dir: &StateDir,
        enable_free_models: bool,
    ) -> Vec<ModelInfo> {
        let auth = self.build_auth(state_dir);
        let mut models: Vec<ModelInfo> = self
            .models
            .iter()
            .filter_map(|(model_id, meta)| {
                let is_free = is_free_model(meta);
                if is_free && !enable_free_models {
                    return None;
                }
                let allow_model = match &auth {
                    Authentication::KeyBased(_) => true,
                    Authentication::OpenCodeFreeKey(_) => is_free,
                    Authentication::NoAuth => false,
                };
                if !allow_model {
                    return None;
                }
                Some(ModelInfo {
                    id: format!("{}/{}", self.slug, model_id),
                    context_window: Some(meta.context),
                    max_output_tokens: Some(meta.output),
                    pricing: Some(ModelPricing {
                        input: meta.input_price,
                        output: meta.output_price,
                        cache_read: meta.cache_read,
                        cache_write: meta.cache_write,
                        fast: None,
                    }),
                    supports_thinking: None,
                    supports_vision: None,
                    tier: None,
                    provider_info: None,
                })
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }
}

#[derive(Clone, Debug)]
pub struct CatalogMeta {
    pub context: u32,
    pub output: u32,
    pub input_price: f64,
    pub output_price: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Clone)]
pub enum Authentication {
    KeyBased(ResolvedAuth),
    OpenCodeFreeKey(ResolvedAuth),
    NoAuth,
}

struct CatalogData {
    providers: HashMap<String, ProviderData>,
    enable_free_models: bool,
    state_dir: StateDir,
}

fn enable_free_models_config() -> bool {
    craft_config::providers::ProvidersConfig::load()
        .get("opencode")
        .and_then(|d| d.enable_free_models)
        .unwrap_or(false)
}

impl CatalogData {
    fn from_index(index: CatalogIndex, enable_free_models: bool, state_dir: &StateDir) -> Self {
        let mut providers = HashMap::new();

        for (provider_id, provider) in index {
            if !ALLOWED_NPM.contains(&provider.npm.as_str()) {
                debug!(npm = %provider.npm, "skipping provider: unsupported npm package");
                continue;
            }
            if BLOCKED_PROVIDER_IN_CATALOG.contains(&provider_id.as_str()) {
                debug!(
                    provider = &provider_id,
                    "skipping providers from the catalog"
                );
                continue;
            }

            let Some(_base_url) = &provider.api else {
                debug!(provider = %provider_id, "skipping: no API URL in catalog");
                continue;
            };

            if builtin_provider(&provider_id).is_some()
                && !OPENCODE_FAMILY_SLUGS.contains(&provider_id.as_str())
            {
                debug!(
                    provider = &provider_id,
                    "skipping providers supported by built-in providers"
                );
                continue;
            }

            let api_format = determine_catalog_format(&provider.npm);

            let mut models = HashMap::new();
            for (model_id, model_data) in &provider.models {
                let input_price = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.input)
                    .unwrap_or(0.0);
                let output_price = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.output)
                    .unwrap_or(0.0);

                let context = model_data
                    .limit
                    .as_ref()
                    .and_then(|l| l.context)
                    .unwrap_or(128_000);
                let output = model_data
                    .limit
                    .as_ref()
                    .and_then(|l| l.output)
                    .unwrap_or(64_000);

                let cache_read = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.cache_read)
                    .unwrap_or(0.0);
                let cache_write = model_data
                    .cost
                    .as_ref()
                    .and_then(|c| c.cache_write)
                    .unwrap_or(0.0);

                models.insert(
                    model_id.clone(),
                    CatalogMeta {
                        context,
                        output,
                        input_price,
                        output_price,
                        cache_read,
                        cache_write,
                    },
                );
            }

            let model_count = models.len();
            let provider_data =
                ProviderData::new(provider_id.clone(), &provider, api_format, models);
            providers.insert(provider_id.clone(), provider_data);

            debug!(
                provider = %provider_id,
                models = model_count,
                format = %provider.npm,
                "catalog provider registered",
            );
        }

        Self {
            providers,
            enable_free_models,
            state_dir: state_dir.clone(),
        }
    }

    fn lookup(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<(&CatalogMeta, &ProviderData), AgentError> {
        let provider_data = self
            .providers
            .get(provider)
            .ok_or_else(|| config_error(format!("provider '{provider}' not found in catalog")))?;
        let meta = provider_data.models.get(model_id).ok_or_else(|| {
            config_error(format!(
                "model '{provider}/{model_id}' not found in catalog"
            ))
        })?;
        Ok((meta, provider_data))
    }

    fn all_models(&self) -> Vec<ModelInfo> {
        let mut models: Vec<ModelInfo> = self
            .providers
            .values()
            .flat_map(|provider_data| {
                provider_data.available_models(&self.state_dir, self.enable_free_models)
            })
            .collect();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    /// Models surfaced for a given opencode-family slug. The bare `opencode`
    /// slug is the meta-provider that addresses every catalog sub-provider, so
    /// it lists everything; any other family slug (e.g. `opencode-go`) lists
    /// only its own catalog entry.
    fn models_for_slug(&self, slug: &str) -> Vec<ModelInfo> {
        if slug == "opencode" {
            return self.all_models();
        }
        let Some(data) = self.providers.get(slug) else {
            return Vec::new();
        };
        let mut models = data.available_models(&self.state_dir, self.enable_free_models);
        models.sort_by(|a, b| a.id.cmp(&b.id));
        models
    }

    fn all_providers(&self) -> Vec<ProviderData> {
        let mut providers: Vec<ProviderData> = self.providers.values().cloned().collect();
        providers.sort_by_key(|p| p.display_name.to_lowercase());
        providers
    }
}

static CATALOG_CHAT_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "opencode",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "Opencode (Catalog)",
};

pub struct Opencode {
    slug: &'static str,
    client: Client,
    chat_compat: OpenAiCompatProvider,
    auth: Option<Arc<Mutex<ResolvedAuth>>>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

static CATALOG: OnceLock<Mutex<CatalogData>> = OnceLock::new();

fn init_catalog_if_needed() -> &'static Mutex<CatalogData> {
    CATALOG.get_or_init(|| Mutex::new(init_catalog_from_cache()))
}

impl Opencode {
    fn new_impl(
        slug: &'static str,
        timeouts: super::Timeouts,
        auth: Option<Arc<Mutex<ResolvedAuth>>>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            slug,
            client: http_client(timeouts)?,
            chat_compat: OpenAiCompatProvider::new(&CATALOG_CHAT_CONFIG, timeouts)?,
            auth,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        })
    }

    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        Self::new_impl("opencode", timeouts, None)
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        Self::new_impl("opencode", timeouts, Some(auth))
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    async fn ensure_catalog_populated(&self) -> Result<(), AgentError> {
        let needs_fetch = {
            let guard = init_catalog_if_needed().lock().unwrap();
            guard.providers.is_empty()
        };
        if !needs_fetch {
            return Ok(());
        }
        let enable_free_models = enable_free_models_config();
        match fetch_remote_catalog(&self.client).await {
            Ok(index) => {
                save_cached_catalog(&index);
                let state_dir = resolve_state_dir_or_empty();
                let data = CatalogData::from_index(index, enable_free_models, &state_dir);
                if !data.providers.is_empty() {
                    *init_catalog_if_needed().lock().unwrap() = data;
                }
                Ok(())
            }
            Err(e) => {
                warn!(error = %e, "catalog fetch failed");
                Err(e)
            }
        }
    }

    async fn do_list_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        self.ensure_catalog_populated().await.ok();
        Ok(init_catalog_if_needed()
            .lock()
            .unwrap()
            .models_for_slug(self.slug))
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_chat_completions(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let mut body = self.chat_compat.build_body(model, messages, system, tools);
        opts.thinking
            .apply_reasoning_effort(&mut body, &dialect::PREFER_HIGH, model);
        self.chat_compat
            .do_stream(model, &[], &body, event_tx, auth)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_catalog_messages(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
        opts: &RequestOptions,
    ) -> Result<StreamResponse, AgentError> {
        let system_blocks = vec![shared::SystemBlock {
            r#type: "text",
            text: system,
            cache_control: Some(shared::EPHEMERAL),
        }];
        let mut body = shared::build_request_body_with_system(
            model,
            messages,
            &system_blocks,
            tools,
            opts.thinking,
        );
        body["model"] = json!(model.id);
        body["stream"] = json!(true);
        let json_body = serde_json::to_vec(&body)?;
        let base = auth.base_url.as_deref().unwrap_or("");
        let request = auth.configure_request(
            self.client
                .post(format!("{base}{MESSAGES_PATH}"))
                .header("user-agent", super::user_agent())
                .header("content-type", super::MIME_JSON)
                .header("anthropic-version", "2023-06-01"),
        );

        debug!(model = %model.id, "sending Anthropic-format request via catalog");

        let response = request.body(json_body).send().await?;
        let status = response.status().as_u16();

        if status == 200 {
            let stream = response.bytes_stream();
            let reader = StreamReader::new(stream.map_err(std::io::Error::other));
            crate::providers::anthropic::parse_sse(
                BufReader::new(reader.compat()),
                event_tx,
                self.stream_timeout,
            )
            .await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn lookup(
        &self,
        sub_provider: &str,
        actual_id: &str,
    ) -> Result<(CatalogMeta, EndpointType, ResolvedAuth), AgentError> {
        let guard = init_catalog_if_needed().lock().unwrap();
        let (meta, provider_data) = guard.lookup(sub_provider, actual_id)?;
        let state_dir = &guard.state_dir;
        let auth = provider_data
            .resolve_auth_with_override(self.auth.as_ref(), state_dir)
            .ok_or_else(|| {
                config_error(format!(
                    "authentication required for provider '{sub_provider}', run `craft auth login {sub_provider}`"
                ))
            })?;
        Ok((meta.clone(), provider_data.api_format, auth))
    }
}

#[async_trait]
impl Provider for Opencode {
    async fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        let model_for_stream = model.clone();

        self.ensure_catalog_populated().await.ok();
        let model_id = &model_for_stream.id;
        let (sub_provider, actual_id) = model_id.split_once('/').unwrap_or((self.slug, model_id));

        let (meta, api_format, auth) = self.lookup(sub_provider, actual_id).await?;

        let mut buf = String::new();
        let system = super::with_prefix(&self.system_prefix, system, &mut buf);

        let model = Model {
            id: actual_id.to_string(),
            max_output_tokens: Some(meta.output),
            context_window: meta.context,
            ..model_for_stream
        };

        match api_format {
            EndpointType::ChatCompletions => {
                self.handle_catalog_chat_completions(
                    &model, messages, system, tools, event_tx, &auth, &opts,
                )
                .await
            }
            EndpointType::Messages => {
                self.handle_catalog_messages(
                    &model, messages, system, tools, event_tx, &auth, &opts,
                )
                .await
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        let models = self.do_list_models().await?;
        Ok(models.into_iter().map(|m| m.id).collect())
    }

    async fn list_models_with_info(&self) -> Result<Vec<ModelInfo>, AgentError> {
        self.do_list_models().await
    }

    async fn reload_auth(&self) -> Result<(), AgentError> {
        Ok(())
    }
}

fn config_error(message: String) -> AgentError {
    AgentError::Config { message }
}

fn resolve_state_dir_or_empty() -> StateDir {
    match StateDir::resolve() {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to resolve state dir");
            StateDir::from_path("".into())
        }
    }
}

/// Returns the list of all catalog providers in alphabetical order.
pub fn catalog_providers() -> Vec<ProviderData> {
    let guard = init_catalog_if_needed().lock().unwrap();
    guard.all_providers()
}

/// Returns the list of catalog providers only if the catalog has already been
/// downloaded. Does NOT trigger downloading.
pub fn catalog_providers_if_available() -> Option<Vec<ProviderData>> {
    let catalog = CATALOG.get()?;
    let guard = catalog.lock().ok()?;
    Some(guard.all_providers())
}

/// Returns the `ProviderData` for a specific catalog provider, if found.
pub fn catalog_provider(provider_id: &str) -> Option<ProviderData> {
    let guard = init_catalog_if_needed().lock().ok()?;
    guard.providers.get(provider_id).cloned()
}

/// True for opencode-family slugs that resolve to the catalog-backed `Opencode`
/// provider but have no `ProviderKind`. `opencode` itself keeps routing through
/// `ProviderKind::Opencode`; this covers catalog-backed manifests like
/// `opencode-go`.
pub fn is_catalog_family_slug(slug: &str) -> bool {
    slug != "opencode"
        && OPENCODE_FAMILY_SLUGS.contains(&slug)
        && ManifestRegistry::get(slug).is_some()
}

/// Construct the catalog-backed `Opencode` provider for an opencode-family
/// slug. `opencode` itself still routes through `ProviderKind::Opencode`; this
/// covers the remaining catalog-backed manifests (e.g. `opencode-go`).
pub fn create_for_slug(
    slug: &str,
    timeouts: super::Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let manifest = ManifestRegistry::get(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown opencode-family slug '{slug}'"),
    })?;
    Opencode::new_impl(manifest.slug, timeouts, None).map(|p| Box::new(p) as Box<dyn Provider>)
}

// --- Catalog helpers ---

fn catalog_cache_path() -> Option<PathBuf> {
    let dir = craft_storage::paths::cache_dir().ok()?;
    Some(dir.join(CATALOG_CACHE_FILE))
}

fn load_cached_catalog() -> Option<CatalogIndex> {
    let path = catalog_cache_path()?;
    let meta = fs::metadata(&path).ok()?;

    let modified = meta.modified().ok()?;
    let age = SystemTime::now().duration_since(modified).ok()?;
    if age > CATALOG_CACHE_TTL {
        debug!("catalog cache expired");
        return None;
    }

    let text = fs::read_to_string(&path).ok()?;
    let index: CatalogIndex = serde_json::from_str(&text).ok()?;
    debug!("loaded catalog from cache");
    Some(index)
}

fn save_cached_catalog(index: &CatalogIndex) {
    let path = match catalog_cache_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(index) {
        Ok(text) => {
            if let Err(e) = fs::write(&path, &text) {
                warn!(error = %e, path = %path.display(), "failed to write catalog cache");
            } else {
                debug!(path = %path.display(), "cached catalog");
            }
        }
        Err(e) => warn!(error = %e, "failed to serialize catalog for cache"),
    }
}

async fn fetch_remote_catalog(client: &Client) -> Result<CatalogIndex, AgentError> {
    let response = client.get(CATALOG_URL).send().await.map_err(|e| {
        warn!(error = %e, CATALOG_URL, "failed to fetch catalog");
        config_error(format!("failed to fetch catalog from {CATALOG_URL}: {e}"))
    })?;

    let status = response.status().as_u16();
    if status != 200 {
        return Err(AgentError::Api {
            status,
            message: format!("catalog fetch returned HTTP {status}"),
        });
    }

    let text = response
        .text()
        .await
        .map_err(|e| config_error(format!("failed to read catalog response body: {e}")))?;

    serde_json::from_str(&text)
        .map_err(|e| config_error(format!("failed to parse catalog JSON: {e}")))
}

fn determine_catalog_format(npm: &str) -> EndpointType {
    match npm {
        "@ai-sdk/anthropic" => EndpointType::Messages,
        _ => EndpointType::ChatCompletions,
    }
}

const ALLOWED_NPM: &[&str] = &["@ai-sdk/openai-compatible", "@ai-sdk/anthropic"];

fn init_catalog_from_cache() -> CatalogData {
    let enable_free_models = enable_free_models_config();
    let state_dir = resolve_state_dir_or_empty();
    if let Some(index) = load_cached_catalog() {
        debug!("using cached catalog");
        return CatalogData::from_index(index, enable_free_models, &state_dir);
    }
    CatalogData {
        providers: HashMap::new(),
        enable_free_models: false,
        state_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state_dir() -> (tempfile::TempDir, StateDir) {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = StateDir::from_path(tmp.path().to_path_buf());
        (tmp, state_dir)
    }

    #[test]
    fn catalog_format_messages_for_anthropic() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/anthropic"),
            EndpointType::Messages
        );
    }

    #[test]
    fn catalog_format_chat_for_openai_compat() {
        assert_eq!(
            determine_catalog_format("@ai-sdk/openai-compatible"),
            EndpointType::ChatCompletions
        );
    }

    #[test]
    fn catalog_provider_roundtrip_json() {
        let provider = CatalogProvider {
            name: "Test Provider".into(),
            env: vec!["TEST_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://test.api/v1".into()),
            models: HashMap::from([(
                "test-model".into(),
                CatalogModel {
                    limit: Some(CatalogLimits {
                        context: Some(128_000),
                        input: None,
                        output: Some(64_000),
                    }),
                    cost: Some(CatalogCost {
                        input: Some(0.5),
                        output: Some(1.5),
                        cache_read: Some(0.1),
                        cache_write: Some(0.2),
                    }),
                    provider: None,
                },
            )]),
        };

        let json = serde_json::to_string_pretty(&provider).unwrap();
        let deserialized: CatalogProvider = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "Test Provider");
        assert_eq!(deserialized.npm, "@ai-sdk/openai-compatible");
        assert!(deserialized.models.contains_key("test-model"));
        let model = &deserialized.models["test-model"];
        let cost = model.cost.as_ref().unwrap();
        assert_eq!(cost.input, Some(0.5));
        assert_eq!(cost.output, Some(1.5));
    }

    #[test]
    fn catalog_index_roundtrip_json() {
        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "test-provider".into(),
            CatalogProvider {
                name: "Test".into(),
                env: vec![],
                npm: "@ai-sdk/openai".into(),
                api: Some("https://test.api/v1".into()),
                models: HashMap::from([(
                    "test-model".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                    },
                )]),
            },
        );

        let json = serde_json::to_string_pretty(&providers).unwrap();
        let deserialized: CatalogIndex = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.len(), 1);
        assert!(deserialized.contains_key("test-provider"));
    }

    #[test]
    fn catalog_provider_missing_optional_fields() {
        let json = r#"{
            "name": "Minimal",
            "npm": "@ai-sdk/openai",
            "models": {}
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        assert_eq!(provider.name, "Minimal");
        assert!(provider.env.is_empty());
        assert!(provider.api.is_none());
        assert!(provider.models.is_empty());
    }

    #[test]
    fn catalog_model_missing_cost_and_provider() {
        let json = r#"{
            "name": "Test",
            "npm": "@ai-sdk/openai",
            "api": "https://test.api/v1",
            "models": {
                "m1": { "limit": {"context": 64000} }
            }
        }"#;
        let provider: CatalogProvider = serde_json::from_str(json).unwrap();
        let model = &provider.models["m1"];
        assert_eq!(model.limit.as_ref().unwrap().context, Some(64000));
        assert!(model.cost.is_none());
        assert!(model.provider.is_none());
    }

    #[test]
    fn catalog_provider_resolve_api_key_from_env() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["CRAFT_TEST_UNUSED_VAR_1".into()],
            npm: "@ai-sdk/openai".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        assert!(provider_data.resolve_api_key(&state_dir).is_none());
    }

    #[test]
    fn catalog_provider_resolve_api_key_anthropic_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["ANTHROPIC_SECRET_KEY".into()],
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "anthropic".into(),
            &provider,
            EndpointType::Messages,
            HashMap::new(),
        );
        assert!(provider_data.resolve_api_key(&state_dir).is_none());
    }

    #[test]
    fn catalog_provider_build_auth_no_key_returns_none() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec![],
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        assert!(matches!(
            provider_data.build_auth(&state_dir),
            Authentication::NoAuth
        ));
    }

    #[test]
    fn catalog_provider_build_auth_public_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "opencode".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir);
        match auth {
            Authentication::OpenCodeFreeKey(resolved) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer public");
            }
            _ => panic!("expected OpenCodeFreeKey"),
        }
    }

    #[test]
    fn catalog_provider_build_auth_key_based() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("CRAFT_TEST_AUTH_KEY", "sk-real-key") };
        let provider = CatalogProvider {
            name: "Test".into(),
            env: vec!["CRAFT_TEST_AUTH_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "test".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir);
        match auth {
            Authentication::KeyBased(resolved) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer sk-real-key");
            }
            _ => panic!("expected KeyBased"),
        }
        unsafe { std::env::remove_var("CRAFT_TEST_AUTH_KEY") };
    }

    fn opencode_go_provider_data(env_key: &str) -> ProviderData {
        let provider = CatalogProvider {
            name: "Opencode Go".into(),
            env: vec![env_key.into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/go/v1".into()),
            models: HashMap::new(),
        };
        let models = HashMap::from([
            (
                "paid-model".to_string(),
                CatalogMeta {
                    context: 128_000,
                    output: 64_000,
                    input_price: 1.0,
                    output_price: 2.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
            ),
            (
                "free-model".to_string(),
                CatalogMeta {
                    context: 128_000,
                    output: 64_000,
                    input_price: 0.0,
                    output_price: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
            ),
        ]);
        ProviderData::new(
            "opencode-go".into(),
            &provider,
            EndpointType::ChatCompletions,
            models,
        )
    }

    #[test]
    fn catalog_provider_available_models_free_fallback_hides_paid_models() {
        let (_tmp, state_dir) = temp_state_dir();
        let data = opencode_go_provider_data("CRAFT_TEST_OPENCODE_GO_UNSET_KEY_91472");
        let models = data.available_models(&state_dir, true);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["opencode-go/free-model"]);
    }

    #[test]
    fn catalog_provider_available_models_with_key_shows_all() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("CRAFT_TEST_OPENCODE_GO_KEY_41827", "real-key") };
        let data = opencode_go_provider_data("CRAFT_TEST_OPENCODE_GO_KEY_41827");
        let models = data.available_models(&state_dir, true);
        let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["opencode-go/free-model", "opencode-go/paid-model"]);
        unsafe { std::env::remove_var("CRAFT_TEST_OPENCODE_GO_KEY_41827") };
    }

    #[test]
    fn catalog_provider_build_auth_x_api_key() {
        let (_tmp, state_dir) = temp_state_dir();
        unsafe { std::env::set_var("CRAFT_TEST_ANTHROPIC_KEY", "sk-ant-key") };
        let provider = CatalogProvider {
            name: "Anthropic".into(),
            env: vec!["CRAFT_TEST_ANTHROPIC_KEY".into()],
            npm: "@ai-sdk/anthropic".into(),
            api: None,
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "anthropic".into(),
            &provider,
            EndpointType::Messages,
            HashMap::new(),
        );
        let auth = provider_data.build_auth(&state_dir);
        match auth {
            Authentication::KeyBased(resolved) => {
                assert_eq!(resolved.headers[0].0, "x-api-key");
                assert_eq!(resolved.headers[0].1, "sk-ant-key");
            }
            _ => panic!("expected KeyBased"),
        }
        unsafe { std::env::remove_var("CRAFT_TEST_ANTHROPIC_KEY") };
    }

    #[test]
    fn catalog_to_data_filters_nonfree_without_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(2.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["CRAFT_TEST_VENDOR_KEY_60924".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        let result = CatalogData::from_index(providers, true, &state_dir);
        let vendor = result.providers.get("some-vendor").unwrap();
        assert_eq!(vendor.models.len(), 2, "all models included");
    }

    #[test]
    fn catalog_to_data_opencode_free_models_without_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let result = CatalogData::from_index(providers, true, &state_dir);
        let opencode = result.providers.get("opencode").unwrap();
        assert_eq!(opencode.models.len(), 2, "all models included");
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::OpenCodeFreeKey(_)
        ));
    }

    #[test]
    fn catalog_to_data_opencode_all_models_with_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["CRAFT_TEST_OPENCODE_ALL_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CRAFT_TEST_OPENCODE_ALL_81274", "real-key") };
        let result = CatalogData::from_index(providers, true, &state_dir);

        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::KeyBased(_)
        ));
        unsafe { std::env::remove_var("CRAFT_TEST_OPENCODE_ALL_81274") };
    }

    fn opencode_catalog_with_free_and_paid(_env_var: &str) -> CatalogIndex {
        let mut models = HashMap::new();
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(5.0),
                    output: Some(25.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );
        providers
    }

    #[test]
    fn catalog_to_data_opencode_hides_free_models_when_disabled() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = opencode_catalog_with_free_and_paid("unused");
        let result = CatalogData::from_index(index, false, &state_dir);

        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::OpenCodeFreeKey(_)
        ));
        assert_eq!(result.all_models().len(), 0);
    }

    #[test]
    fn catalog_to_data_opencode_no_models_without_key_when_disabled() {
        let (_tmp, state_dir) = temp_state_dir();
        let index = opencode_catalog_with_free_and_paid("unused");
        let result = CatalogData::from_index(index, false, &state_dir);

        let opencode = result.providers.get("opencode").unwrap();
        assert!(opencode.models.contains_key("free-model"));
        assert!(opencode.models.contains_key("paid-model"));
        assert!(matches!(
            opencode.build_auth(&state_dir),
            Authentication::OpenCodeFreeKey(_)
        ));
        assert_eq!(result.all_models().len(), 0);
    }

    #[test]
    fn catalog_to_data_all_models_with_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "cheap".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "freebie".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "some-vendor".into(),
            CatalogProvider {
                name: "Vendor".into(),
                env: vec!["CRAFT_TEST_VENDOR_KEY_81274".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://vendor.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CRAFT_TEST_VENDOR_KEY_81274", "test-key") };
        let result = CatalogData::from_index(providers, true, &state_dir);
        unsafe { std::env::remove_var("CRAFT_TEST_VENDOR_KEY_81274") };

        assert!(
            result
                .providers
                .get("some-vendor")
                .unwrap()
                .models
                .contains_key("cheap")
        );
        assert!(
            result
                .providers
                .get("some-vendor")
                .unwrap()
                .models
                .contains_key("freebie")
        );
    }

    #[test]
    fn catalog_to_data_skips_providers_without_api_url() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut providers = HashMap::new();
        providers.insert(
            "no-api".into(),
            CatalogProvider {
                name: "No API".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: None,
                models: HashMap::new(),
            },
        );

        let result = CatalogData::from_index(providers, true, &state_dir);
        assert!(result.providers.is_empty());
    }

    #[test]
    fn catalog_to_data_handles_model_id_collisions() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models: HashMap<String, CatalogModel> = HashMap::new();
        models.insert(
            "shared-model".into(),
            CatalogModel {
                limit: Some(CatalogLimits {
                    context: Some(64_000),
                    input: Some(64_000),
                    output: Some(8_000),
                }),
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();

        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models: models.clone(),
            },
        );

        providers.insert(
            "other-vendor".into(),
            CatalogProvider {
                name: "Other".into(),
                env: vec!["CRAFT_TEST_OTHER_KEY_COLLISION".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://other.api/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CRAFT_TEST_OTHER_KEY_COLLISION", "key") };
        let result = CatalogData::from_index(providers, true, &state_dir);
        unsafe { std::env::remove_var("CRAFT_TEST_OTHER_KEY_COLLISION") };

        assert!(
            result
                .providers
                .get("opencode")
                .unwrap()
                .models
                .contains_key("shared-model")
        );
        assert!(
            result
                .providers
                .get("other-vendor")
                .unwrap()
                .models
                .contains_key("shared-model")
        );
        assert_eq!(result.providers.len(), 2);

        let (_meta, provider_data) = result.lookup("opencode", "shared-model").unwrap();
        assert_eq!(provider_data.slug, "opencode");
    }

    #[test]
    fn lookup_finds_opencode_own_models() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "opus".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = CatalogData::from_index(providers, true, &state_dir);
        let (_meta, provider_data) = data.lookup("opencode", "opus").unwrap();
        assert_eq!(provider_data.slug, "opencode");
    }

    #[test]
    fn lookup_finds_model_id_with_slashes() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["CRAFT_TEST_NVIDIA_KEY_LOOKUP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CRAFT_TEST_NVIDIA_KEY_LOOKUP", "key") };
        let data = CatalogData::from_index(providers, true, &state_dir);
        unsafe { std::env::remove_var("CRAFT_TEST_NVIDIA_KEY_LOOKUP") };

        let (_meta, provider_data) = data.lookup("nvidia", "openai/gpt-oss-120b").unwrap();
        assert_eq!(provider_data.slug, "nvidia");
    }

    #[test]
    fn lookup_spec_is_sub_provider_plus_model_id() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "openai/gpt-oss-120b".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "nvidia".into(),
            CatalogProvider {
                name: "NVIDIA".into(),
                env: vec!["CRAFT_TEST_NVIDIA_DIRECT".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://nvapi.xyz/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CRAFT_TEST_NVIDIA_DIRECT", "key") };
        let data = CatalogData::from_index(providers, true, &state_dir);
        unsafe { std::env::remove_var("CRAFT_TEST_NVIDIA_DIRECT") };

        let _key = format!("{}/{}", "nvidia", "openai/gpt-oss-120b");
        let (_meta, provider_data) = data.lookup("nvidia", "openai/gpt-oss-120b").unwrap();
        assert_eq!(provider_data.slug, "nvidia");
    }

    #[test]
    fn lookup_nested_model_id_uses_sub_provider_key() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "deepseek-ai/DeepSeek-R1".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        let mut providers = HashMap::new();
        providers.insert(
            "fireworks".into(),
            CatalogProvider {
                name: "Fireworks".into(),
                env: vec!["CRAFT_TEST_FIREWORKS_DEEP".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://fireworks.ai/v1".into()),
                models,
            },
        );

        unsafe { std::env::set_var("CRAFT_TEST_FIREWORKS_DEEP", "key") };
        let data = CatalogData::from_index(providers, true, &state_dir);
        unsafe { std::env::remove_var("CRAFT_TEST_FIREWORKS_DEEP") };

        let _key = format!("{}/{}", "fireworks", "deepseek-ai/DeepSeek-R1");
        let (_meta, provider_data) = data.lookup("fireworks", "deepseek-ai/DeepSeek-R1").unwrap();
        assert_eq!(provider_data.slug, "fireworks");
    }

    #[test]
    fn catalog_all_models_filters_keyless_providers() {
        let (_tmp, state_dir) = temp_state_dir();
        let auth_dir = state_dir.path().join("auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("keyed.json"), r#"{"api_key": "sk-abc123"}"#).unwrap();

        let mut providers: CatalogIndex = HashMap::new();
        providers.insert(
            "keyed".into(),
            CatalogProvider {
                name: "Keyed".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://keyed.api/v1".into()),
                models: HashMap::from([(
                    "m1".into(),
                    CatalogModel {
                        limit: None,
                        cost: None,
                        provider: None,
                    },
                )]),
            },
        );
        providers.insert(
            "keyless".into(),
            CatalogProvider {
                name: "Keyless".into(),
                env: vec![],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://keyless.api/v1".into()),
                models: HashMap::from([(
                    "m2".into(),
                    CatalogModel {
                        limit: None,
                        cost: Some(CatalogCost {
                            input: Some(5.0),
                            output: Some(10.0),
                            cache_read: None,
                            cache_write: None,
                        }),
                        provider: None,
                    },
                )]),
            },
        );

        let data = CatalogData::from_index(providers, true, &state_dir);

        assert_eq!(data.providers.len(), 2);

        let models = data.all_models();
        assert_eq!(
            models.len(),
            1,
            "should only include models from providers with keys"
        );
        assert_eq!(models[0].id, "keyed/m1");
    }

    #[test]
    fn catalog_all_models_public_fallback_shows_only_free() {
        let (_tmp, state_dir) = temp_state_dir();
        let mut models = HashMap::new();
        models.insert(
            "free-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(0.0),
                    output: Some(0.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );
        models.insert(
            "paid-model".into(),
            CatalogModel {
                limit: None,
                cost: Some(CatalogCost {
                    input: Some(1.0),
                    output: Some(3.0),
                    cache_read: None,
                    cache_write: None,
                }),
                provider: None,
            },
        );

        let mut providers = HashMap::new();
        providers.insert(
            "opencode".into(),
            CatalogProvider {
                name: "Opencode".into(),
                env: vec!["OPENCODE_API_KEY".into()],
                npm: "@ai-sdk/openai-compatible".into(),
                api: Some("https://opencode.ai/zen/v1".into()),
                models,
            },
        );

        let data = CatalogData::from_index(providers, true, &state_dir);

        let opencode = data.providers.get("opencode").unwrap();
        assert_eq!(opencode.models.len(), 2);
        let result = data.all_models();
        assert_eq!(
            result.len(),
            1,
            "public fallback should only show free models"
        );
        assert_eq!(result[0].id, "opencode/free-model");
        assert_eq!(result[0].pricing.as_ref().unwrap().input, 0.0);
    }

    #[test]
    fn is_catalog_family_slug_recognizes_opencode_go_only() {
        assert!(!is_catalog_family_slug("opencode"));
        assert!(is_catalog_family_slug("opencode-go"));
        assert!(!is_catalog_family_slug("anthropic"));
        assert!(!is_catalog_family_slug("opencode-zoom"));
    }

    #[test]
    fn opencode_go_build_auth_uses_public_fallback() {
        let (_tmp, state_dir) = temp_state_dir();
        let provider = CatalogProvider {
            name: "Opencode Go".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/go/v1".into()),
            models: HashMap::new(),
        };
        let provider_data = ProviderData::new(
            "opencode-go".into(),
            &provider,
            EndpointType::ChatCompletions,
            HashMap::new(),
        );
        match provider_data.build_auth(&state_dir) {
            Authentication::OpenCodeFreeKey(resolved) => {
                assert_eq!(resolved.headers[0].0, "authorization");
                assert_eq!(resolved.headers[0].1, "Bearer public");
            }
            _ => panic!("expected OpenCodeFreeKey, got a different variant"),
        }
    }

    #[test]
    fn models_for_slug_scopes_opencode_go_to_own_entry() {
        let (_tmp, state_dir) = temp_state_dir();
        let zen = CatalogProvider {
            name: "OpenCode Zen".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/v1".into()),
            models: HashMap::from([(
                "zen-model".into(),
                CatalogModel {
                    limit: Some(CatalogLimits {
                        context: Some(128_000),
                        input: None,
                        output: Some(64_000),
                    }),
                    cost: Some(CatalogCost {
                        input: Some(0.0),
                        output: Some(0.0),
                        cache_read: None,
                        cache_write: None,
                    }),
                    provider: None,
                },
            )]),
        };
        let go = CatalogProvider {
            name: "OpenCode Go".into(),
            env: vec!["OPENCODE_API_KEY".into()],
            npm: "@ai-sdk/openai-compatible".into(),
            api: Some("https://opencode.ai/zen/go/v1".into()),
            models: HashMap::from([(
                "go-model".into(),
                CatalogModel {
                    limit: Some(CatalogLimits {
                        context: Some(128_000),
                        input: None,
                        output: Some(64_000),
                    }),
                    cost: Some(CatalogCost {
                        input: Some(0.0),
                        output: Some(0.0),
                        cache_read: None,
                        cache_write: None,
                    }),
                    provider: None,
                },
            )]),
        };
        let providers: CatalogIndex =
            HashMap::from([("opencode".into(), zen), ("opencode-go".into(), go)]);
        let data = CatalogData::from_index(providers, true, &state_dir);

        let all = data.models_for_slug("opencode");
        assert_eq!(
            all.len(),
            2,
            "opencode meta-provider lists every sub-provider"
        );

        let go_only = data.models_for_slug("opencode-go");
        assert_eq!(go_only.len(), 1);
        assert_eq!(go_only[0].id, "opencode-go/go-model");
    }
}
