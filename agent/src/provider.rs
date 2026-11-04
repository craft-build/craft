//! The provider seam: craft's provider configs become animist providers
//! and models.

use std::sync::Arc;
use std::time::Duration;

use animist::catalog::Catalog;
use animist::provider::anthropic::Anthropic;
use animist::provider::bedrock::ListingMode;
use animist::provider::bedrock::{Bedrock, BedrockConfig, Protocol as BedrockProtocol};
use animist::provider::llama_cpp::{LlamaCpp, LlamaCppConfig};
use animist::provider::ollama::Ollama;
use animist::provider::omlx::Omlx;
use animist::provider::openai::{OpenAi, OpenAiConfig, Protocol};
use animist::provider::synthetic::Synthetic;
use animist::provider::zai_coding_plan::ZaiCodingPlan;
use animist::provider::{Model, ModelInfo, Overrides, Provider, ThinkingSupport};
use snafu::ResultExt;

use crate::config::{ModelConfig, ProviderConfig};
use crate::error::{CreateProviderSnafu, InvalidSnafu, Result, SelectModelSnafu};

// --- Catalog -------------------------------------------------------------------

/// Cache file holding the raw models.dev document.
const CATALOG_CACHE_FILE: &str = "models-dev-catalog-v2.json";
/// How long a cached catalog is served without a refresh.
const CATALOG_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a refresh fetch may take before the cached copy stands.
const CATALOG_FETCH_BUDGET: Duration = Duration::from_secs(20);

/// Load the session's models.dev catalog: a fresh cache is served as is, a
/// stale cache is refreshed (saving on success), a failed refresh falls
/// back to the stale copy, and a cold start with no network yields an
/// empty catalog rather than an error. Surfaces call this once and pass
/// the handle to provider construction and model selection.
pub async fn load_catalog() -> Arc<Catalog> {
    let Some(path) = crate::paths::cache_dir()
        .ok()
        .map(|dir| dir.join(CATALOG_CACHE_FILE))
    else {
        tracing::warn!("no cache directory; models.dev metadata unavailable");
        return Arc::new(Catalog::default());
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) if cache_is_fresh(&path).await => match serde_json::from_slice::<Catalog>(&bytes)
        {
            Ok(catalog) => Arc::new(catalog),
            Err(error) => {
                tracing::warn!(%error, "cached models.dev catalog is unreadable; refetching");
                refresh(&path).await
            }
        },
        // Stale, missing, or unreadable on disk: refresh, keeping the old
        // bytes as the fallback.
        _ => refresh(&path).await,
    }
}

async fn refresh(path: &std::path::Path) -> Arc<Catalog> {
    let fetch = tokio::time::timeout(
        CATALOG_FETCH_BUDGET,
        Catalog::fetch(&animist::reqwest::Client::new()),
    )
    .await;
    match fetch {
        Ok(Ok(catalog)) => {
            save_cache(path, &catalog).await;
            Arc::new(catalog)
        }
        Ok(Err(error)) => stale_or_empty(path, Some(error.to_string())).await,
        Err(_) => stale_or_empty(path, None).await,
    }
}

/// Persist a fetched catalog. An empty parse is never cached: it would
/// poison the next session against a working refresh.
async fn save_cache(path: &std::path::Path, catalog: &Catalog) {
    if catalog.providers.is_empty() {
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(catalog)
        && let Err(error) = tokio::fs::write(path, &bytes).await
    {
        tracing::warn!(%error, "saving the models.dev catalog failed");
    }
}

/// Serve the stale cache after a failed refresh, or an empty catalog when
/// there is nothing to fall back to.
async fn stale_or_empty(path: &std::path::Path, error: Option<String>) -> Arc<Catalog> {
    // The cache holds the normalized serialization (see `save_cache`);
    // `Catalog::from_slice` parses the models.dev source document, which
    // is not the shape a cache round trip needs.
    let stale = match tokio::fs::read(path).await {
        Ok(bytes) => match serde_json::from_slice::<Catalog>(&bytes) {
            Ok(catalog) => Some(Arc::new(catalog)),
            Err(parse) => {
                tracing::warn!(%parse, "models.dev refresh failed and the cache is unreadable");
                None
            }
        },
        _ => None,
    };
    match (stale, error) {
        (Some(catalog), Some(error)) => {
            tracing::warn!(%error, "models.dev refresh failed; serving the stale cache");
            catalog
        }
        (Some(catalog), None) => {
            tracing::warn!("models.dev refresh timed out; serving the stale cache");
            catalog
        }
        (None, Some(error)) => {
            tracing::warn!(%error, "model metadata is unknown this session");
            Arc::new(Catalog::default())
        }
        (None, None) => {
            tracing::warn!("models.dev refresh timed out; model metadata is unknown this session");
            Arc::new(Catalog::default())
        }
    }
}

async fn cache_is_fresh(path: &std::path::Path) -> bool {
    match tokio::fs::metadata(path).await {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age < CATALOG_CACHE_TTL),
        Err(_) => false,
    }
}

// --- Side completions -----------------------------------------------------------

/// Production reauth hook (see [`harness::ReauthHook`]): waits for the
/// provider credential to be refreshed, then rebuilds the provider and
/// returns the model for the run to retry with. A process's environment
/// only changes in-process, so this serves flows that update credentials
/// while craft runs (plugins, config reloads, future auth flows).
#[expect(
    clippy::result_large_err,
    reason = "the crate error type is deliberately unboxed"
)]
pub async fn reauth_hook(
    config: &ProviderConfig,
    id: &str,
) -> crate::error::Result<Arc<dyn crate::harness::ReauthHook>> {
    Ok(Arc::new(EnvReauth {
        config: config.clone(),
        id: id.to_owned(),
    }))
}

/// How often the reauth hook polls the credential, and how long it waits
/// overall before giving up (the run's cancellation still ends it
/// immediately).
const REAUTH_POLL: Duration = Duration::from_secs(2);
const REAUTH_MAX_WAIT: Duration = Duration::from_secs(600);

struct EnvReauth {
    config: ProviderConfig,
    id: String,
}

#[async_trait::async_trait]
impl crate::harness::ReauthHook for EnvReauth {
    async fn reauth(&self, _attempt: u32) -> std::result::Result<Option<ModelHandle>, String> {
        let env_name = self
            .config
            .credential_env()
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "provider {} has no refreshable credential; restart after re-authenticating",
                    config_kind_str(&self.config)
                )
            })?;
        let original = std::env::var(&env_name).unwrap_or_default();
        // tokio's clock, so a paused test runtime walks the deadline.
        let deadline = tokio::time::Instant::now() + REAUTH_MAX_WAIT;
        loop {
            let current = std::env::var(&env_name).unwrap_or_default();
            if !current.trim().is_empty() && current != original {
                let provider = ProviderHandle::from_config(&self.config, None)
                    .await
                    .map_err(|e| e.to_string())?;
                return provider
                    .model(&self.config, None, &self.id)
                    .await
                    .map(Some)
                    .map_err(|e| e.to_string());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "credentials in {env_name} were not refreshed within {}s; set a new value and retry",
                    REAUTH_MAX_WAIT.as_secs()
                ));
            }
            tokio::time::sleep(REAUTH_POLL).await;
        }
    }
}

/// One no-tools completion for auxiliary callers (compaction summaries,
/// auto-review, the advisor, memory extraction): maps the messages onto
/// the wire, drains the exchange ignoring deltas, and returns the
/// assembled assistant text. Callers keep their own deadlines and
/// fail-closed behavior around this.
pub async fn complete_text(
    handle: &ModelHandle,
    messages: &[crate::history::Message],
    system: &str,
) -> Result<String> {
    let wire =
        crate::harness::view::shape_messages(messages.to_vec(), handle.relocates_tool_images());
    let model = handle.model.clone();
    let mut options = animist::provider::RequestOptions::builder().build();
    // Aux calls carry no user preference; a model that refuses to answer
    // without reasoning (Bedrock validates the field server-side) needs a
    // real level, so default to the lowest supported one instead of none.
    options.thinking = crate::thinking::effective_for(
        crate::thinking::ThinkingConfig::default(),
        Some(&handle.model.info),
    )
    .to_animist();
    if handle.moves_system_to_preamble() {
        options.system_message_policy = animist::provider::SystemMessagePolicy::MoveToPreamble;
    }
    let mut exchange = animist::provider::InferenceExchange::new(move |event_tx| {
        let model = model.clone();
        async move {
            model
                .provider
                .stream_message(&model, &wire, system, &[], &event_tx, options)
                .await
        }
    });
    while let Some(event) = exchange.next().await {
        match event {
            animist::provider::ExchangeEvent::Finished(Ok(response)) => {
                return Ok(response
                    .message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        animist::history::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""));
            }
            animist::provider::ExchangeEvent::Finished(Err(error)) => {
                return Err(crate::error::client_error(error));
            }
            animist::provider::ExchangeEvent::Provider(_) => {}
        }
    }
    Ok(String::new())
}

/// The longest provider error body an auxiliary failure line carries.
const FAILURE_BODY_LIMIT: usize = 1024;

/// The failure line auxiliary callers show: the error's display plus the
/// body of a Bedrock HTTP error from the chain. Animist's generic HTTP
/// display carries the body; its Bedrock one carries only status, request
/// ID, and error type — and the body holds the ValidationException
/// message naming the rejected field.
pub(crate) fn provider_failure_text(error: &crate::error::Error) -> String {
    let mut text = error.to_string();
    // `ProviderClient` boxes its source behind `Box<dyn Error>`, which the
    // snafu chain does not surface, and animist's transparent Bedrock
    // variant hides its own source the same way, so both steps destructure.
    let root: &dyn std::error::Error = match error {
        crate::error::Error::ProviderClient { source } => source.as_ref(),
        _ => return text,
    };
    let mut provider = root.downcast_ref::<animist::provider::ProviderError>();
    let mut source = Some(root);
    let body = loop {
        if let Some(animist::provider::bedrock::BedrockError::Http { body, .. }) =
            source.and_then(|err| err.downcast_ref())
        {
            break Some(body);
        }
        if let Some(animist::provider::ProviderError::Bedrock {
            source: animist::provider::bedrock::BedrockError::Http { body, .. },
        }) = provider
        {
            break Some(body);
        }
        source = source.and_then(|err| err.source());
        provider = None;
        if source.is_none() {
            break None;
        }
    };
    if let Some(body) = body {
        let body = String::from_utf8_lossy(body);
        let body = body.trim();
        if !body.is_empty() && !text.contains(body) {
            let cap = body
                .char_indices()
                .nth(FAILURE_BODY_LIMIT)
                .map_or(body.len(), |(index, _)| index);
            text.push_str(": ");
            text.push_str(&body[..cap]);
        }
    }
    text
}

// --- Construction --------------------------------------------------------------

/// A constructed provider for one config entry.
///
/// Every kind but Bedrock is one animist provider. Bedrock's wire protocol
/// is a per-instance config field, so one config holding mixed model
/// families (Claude, OpenAI, everything else) needs one provider per
/// protocol actually used; they are built lazily from the same config and
/// cached here.
#[derive(Clone)]
pub struct ProviderHandle {
    inner: HandleInner,
}

#[derive(Clone)]
enum HandleInner {
    Single(Provider),
    Bedrock(Arc<BedrockProviders>),
}

/// Bedrock providers, one per wire protocol, built on demand.
struct BedrockProviders {
    /// The shared base config: same region, profile, endpoints, catalog,
    /// and Converse fields for every protocol.
    base: BedrockConfig,
    /// Constructed providers by protocol, built under the lock.
    by_protocol: tokio::sync::Mutex<Vec<(BedrockProtocol, Provider)>>,
    /// Test seam: when set, protocols build through `from_sdk_config`
    /// with static credentials instead of AWS discovery.
    sdk: Option<aws_seed::Seed>,
}

/// Static AWS credentials for tests; production never constructs this.
mod aws_seed {
    use animist::provider::bedrock::{Credentials, SdkConfig, SharedCredentialsProvider};

    /// Static credentials; the region comes from the Bedrock config.
    #[derive(Debug, Clone)]
    pub(crate) struct Seed;

    impl Seed {
        pub fn sdk_config(&self) -> SdkConfig {
            SdkConfig::builder()
                .credentials_provider(SharedCredentialsProvider::new(Credentials::new(
                    "test-key",
                    "test-secret",
                    None,
                    None,
                    "craft-tests",
                )))
                .build()
        }
    }
}

use aws_seed::Seed;

impl ProviderHandle {
    /// Construct the provider for `config`, reading credentials from the
    /// environment exactly when the config names an env var (animist
    /// resolves and later reloads those itself). `catalog` feeds the
    /// models.dev slice so `list_models` merges metadata by exact ID.
    pub async fn from_config(config: &ProviderConfig, catalog: Option<&Catalog>) -> Result<Self> {
        match config {
            ProviderConfig::AmazonBedrock(entry) => {
                let base = bedrock_base_config(&entry.config, catalog);
                // Build the default protocol eagerly so region and
                // credential problems surface here, with actionable text,
                // instead of on the first request.
                let provider = Provider::new(
                    Bedrock::new(base.clone())
                        .await
                        .map_err(crate::error::client_error)?,
                );
                let protocol = base.protocol.clone();
                Ok(Self {
                    inner: HandleInner::Bedrock(Arc::new(BedrockProviders {
                        base,
                        by_protocol: tokio::sync::Mutex::new(vec![(protocol, provider)]),
                        sdk: None,
                    })),
                })
            }
            other => Self::single(other, catalog, None).await,
        }
    }

    /// Test seam: credentials come
    /// from the injected lookup instead of the process environment, and
    /// Bedrock builds from static credentials without AWS discovery.
    pub async fn from_config_with(
        config: &ProviderConfig,
        catalog: Option<&Catalog>,
        credential: &(dyn Fn(&str) -> Result<String> + Send + Sync),
    ) -> Result<Self> {
        match config {
            ProviderConfig::AmazonBedrock(entry) => {
                let mut base = bedrock_base_config(&entry.config, catalog);
                if base.region.is_none() {
                    // Keep offline construction free of IMDS probing.
                    base.region = Some("us-east-1".to_owned());
                }
                let protocol = base.protocol.clone();
                let provider = Provider::new(
                    Bedrock::from_sdk_config(base.clone(), &Seed.sdk_config())
                        .map_err(crate::error::client_error)?,
                );
                Ok(Self {
                    inner: HandleInner::Bedrock(Arc::new(BedrockProviders {
                        base,
                        by_protocol: tokio::sync::Mutex::new(vec![(protocol, provider)]),
                        sdk: Some(Seed),
                    })),
                })
            }
            other => Self::single(other, catalog, Some(credential)).await,
        }
    }

    async fn single(
        config: &ProviderConfig,
        catalog: Option<&Catalog>,
        credential: Option<&(dyn Fn(&str) -> Result<String> + Send + Sync)>,
    ) -> Result<Self> {
        let kind = config_kind_str(config);
        let provider = (|| -> Result<Provider> {
            Ok(match config {
                ProviderConfig::Anthropic(entry) => {
                    let mut settings = entry.config.clone();
                    settings.catalog = catalog.and_then(|c| c.provider("anthropic")).cloned();
                    if let Some(credential) = credential {
                        settings.api_key = Some(credential(&settings.api_key_env)?);
                    }
                    Provider::new(Anthropic::new(settings).map_err(crate::error::client_error)?)
                }
                ProviderConfig::Openai(entry) => {
                    let mut settings = entry.config.clone();
                    settings.catalog = catalog.and_then(|c| c.provider("openai")).cloned();
                    if let Some(credential) = credential {
                        settings.api_key = Some(credential(&settings.api_key_env)?);
                    }
                    Provider::new(OpenAi::new(settings).map_err(crate::error::client_error)?)
                }
                ProviderConfig::OpenaiCompatible(entry) => {
                    // Keyless local endpoints (llama.cpp, vLLM, LM Studio…)
                    // never see a fake key: with no credential configured
                    // and a local host, the request rides animist's keyless
                    // llama.cpp client over the same Chat Completions
                    // surface. Non-local hosts keep requiring a key.
                    if keyless_local(&entry.config, credential.is_some()) {
                        let settings = LlamaCppConfig::builder()
                            .base_url(entry.config.base_url.clone())
                            .build();
                        Provider::new(LlamaCpp::new(settings).map_err(crate::error::client_error)?)
                    } else {
                        let mut settings = entry.config.clone();
                        settings.catalog = catalog
                            .and_then(|c| c.provider(catalog_key(config)))
                            .cloned();
                        if let Some(credential) = credential {
                            settings.api_key = Some(credential(&settings.api_key_env)?);
                        }
                        Provider::new(OpenAi::new(settings).map_err(crate::error::client_error)?)
                    }
                }
                ProviderConfig::Synthetic(entry) => {
                    let mut settings = entry.config.clone();
                    if let Some(credential) = credential {
                        settings.api_key = Some(credential(&settings.api_key_env)?);
                    }
                    Provider::new(Synthetic::new(settings).map_err(crate::error::client_error)?)
                }
                ProviderConfig::ZaiCodingPlan(entry) => {
                    let mut settings = entry.config.clone();
                    settings.catalog = catalog.and_then(|c| c.provider("zai-coding-plan")).cloned();
                    if let Some(credential) = credential {
                        settings.api_key = Some(credential(&settings.api_key_env)?);
                    }
                    Provider::new(ZaiCodingPlan::new(settings).map_err(crate::error::client_error)?)
                }
                ProviderConfig::Ollama(entry) => {
                    let mut settings = entry.config.clone();
                    settings.catalog = catalog.and_then(|c| c.provider("ollama")).cloned();
                    Provider::new(Ollama::new(settings))
                }
                ProviderConfig::LlamaCpp(entry) => {
                    let mut settings = entry.config.clone();
                    settings.catalog = catalog.and_then(|c| c.provider("llama-cpp")).cloned();
                    if let (Some(credential), Some(env)) =
                        (credential, settings.api_key_env.as_deref())
                    {
                        settings.api_key = Some(credential(env)?);
                    }
                    Provider::new(LlamaCpp::new(settings).map_err(crate::error::client_error)?)
                }
                ProviderConfig::Omlx(entry) => {
                    let mut settings = entry.config.clone();
                    settings.catalog = catalog.and_then(|c| c.provider("omlx")).cloned();
                    if let (Some(credential), Some(env)) =
                        (credential, settings.api_key_env.as_deref())
                    {
                        settings.api_key = Some(credential(env)?);
                    }
                    Provider::new(Omlx::new(settings).map_err(crate::error::client_error)?)
                }
                ProviderConfig::AmazonBedrock(_) => {
                    return InvalidSnafu {
                        reason: "bedrock construction needs async credential discovery",
                    }
                    .fail();
                }
            })
        })()
        .with_context(|_| CreateProviderSnafu { kind })?;
        Ok(Self {
            inner: HandleInner::Single(provider),
        })
    }

    /// The model the run loop and side completions call: an animist model
    /// carrying the config's per-model overrides, plus the craft-only
    /// metadata (catalog key, effective window, protocol quirks).
    /// `catalog` supplies the info for models the provider has not listed.
    pub async fn model(
        &self,
        config: &ProviderConfig,
        catalog: Option<&Catalog>,
        id: &str,
    ) -> Result<ModelHandle> {
        let kind = config_kind_str(config);
        if id.trim().is_empty() {
            return InvalidSnafu {
                reason: "model ID must not be empty",
            }
            .fail();
        }
        let (provider, key) = match &self.inner {
            HandleInner::Single(inner) => (inner.clone(), catalog_key(config)),
            HandleInner::Bedrock(bedrock) => {
                let protocol = bedrock_protocol_for(config, id);
                (bedrock.provider_for(protocol).await?, "amazon-bedrock")
            }
        };
        let settings = config.models().get(id);
        let mut info = catalog
            .and_then(|catalog| catalog.model(key, id))
            .map(|entry| entry.info.clone())
            .unwrap_or_else(|| bare_info(id));
        info.ids = vec![id.to_owned()];
        if let Some(settings) = settings {
            if let Some(context) = settings.context_length {
                info.config.context_window = Some(context);
            }
            if let Some(output) = settings.max_output_tokens {
                info.config.max_output_tokens = Some(output);
            }
        }
        let mut model = Model::builder()
            .id(id)
            .provider(Arc::new(provider.clone()))
            .info(info.clone())
            .overrides(thinking_overrides(settings))
            .build();
        provider
            .adjust_model(&mut model)
            .await
            .map_err(crate::error::client_error)?;
        Ok(ModelHandle {
            context_length: settings
                .and_then(|s| s.context_length)
                .or(info.config.context_window),
            chat_completions: chat_completions_target(config, id),
            move_system_to_preamble: moves_system_to_preamble(config, id),
            catalog_key: key.to_owned(),
            model,
        })
        .with_context(|_| SelectModelSnafu {
            model: id.to_owned(),
            kind,
        })
    }

    /// Discovery for the config's models: the provider's own listing
    /// (animist merges the fed catalog by exact ID), plus the config's
    /// manual models table. `discover_models = false` returns the manual
    /// table only. `catalog` supplies metadata for manual models the
    /// listing does not carry.
    pub async fn models(
        &self,
        config: &ProviderConfig,
        catalog: Option<&Catalog>,
    ) -> Result<Vec<ModelInfo>> {
        let provider = match &self.inner {
            HandleInner::Single(inner) => inner.clone(),
            // Listing is protocol-independent: the control plane is the
            // same surface for every protocol.
            HandleInner::Bedrock(bedrock) => {
                bedrock
                    .provider_for(BedrockProtocol::AnthropicMessages)
                    .await?
            }
        };
        let mut models = if config.discover_models() {
            provider
                .list_models()
                .await
                .map_err(crate::error::client_error)?
        } else {
            Vec::new()
        };
        merge_manual_models(config, catalog, &mut models);
        Ok(models)
    }

    /// Re-read credentials from their original source. For Bedrock this
    /// always suffices (animist rebuilds and validates the default
    /// chain); config changes need a rebuild instead.
    pub async fn reload_auth(&self) -> Result<()> {
        let providers = match &self.inner {
            HandleInner::Single(provider) => vec![provider.clone()],
            HandleInner::Bedrock(bedrock) => {
                let guard = bedrock.by_protocol.lock().await;
                guard.iter().map(|(_, provider)| provider.clone()).collect()
            }
        };
        for provider in providers {
            provider
                .reload_auth()
                .await
                .map_err(crate::error::client_error)?;
        }
        Ok(())
    }
}

impl BedrockProviders {
    /// The cached provider for `protocol`, building it on first use.
    async fn provider_for(&self, protocol: BedrockProtocol) -> Result<Provider> {
        if let Some((_, provider)) = self
            .by_protocol
            .lock()
            .await
            .iter()
            .find(|(cached, _)| *cached == protocol)
        {
            return Ok(provider.clone());
        }
        let mut config = self.base.clone();
        config.protocol = protocol.clone();
        let provider = match &self.sdk {
            Some(seed) => Provider::new(
                Bedrock::from_sdk_config(config, &seed.sdk_config())
                    .map_err(crate::error::client_error)?,
            ),
            None => Provider::new(
                Bedrock::new(config)
                    .await
                    .map_err(crate::error::client_error)?,
            ),
        };
        self.by_protocol
            .lock()
            .await
            .push((protocol, provider.clone()));
        Ok(provider)
    }
}

// --- The model handle ------------------------------------------------------------

/// The model the run loop calls, with the craft-only metadata that travels
/// with a selection. Cheap to clone; the underlying animist model owns its
/// provider through an `Arc`.
#[derive(Clone)]
pub struct ModelHandle {
    pub model: Model,
    /// models.dev catalog key used for pricing and limit lookups.
    catalog_key: String,
    /// Effective context window: config override or catalog value.
    context_length: Option<u32>,
    /// The target rejects images inside tool results; the edge moves them
    /// into a following user message.
    chat_completions: bool,
    /// The target rejects at-depth system messages unless they are moved
    /// into the preamble.
    move_system_to_preamble: bool,
}

impl ModelHandle {
    /// Effective context window, when known.
    pub fn context_length(&self) -> Option<u32> {
        self.context_length
    }

    /// Output ceiling from the model info (config override, then catalog).
    pub fn max_output(&self) -> Option<u32> {
        self.model.info.config.max_output_tokens
    }

    /// Whether tool-result images must be relocated for this target.
    pub fn relocates_tool_images(&self) -> bool {
        self.chat_completions
    }

    /// Whether at-depth system messages move to the preamble.
    pub fn moves_system_to_preamble(&self) -> bool {
        self.move_system_to_preamble
    }

    /// models.dev key for pricing.
    pub fn catalog_key(&self) -> &str {
        &self.catalog_key
    }

    /// A test handle over a scripted provider: the model's own metadata,
    /// no catalog or protocol mapping.
    #[cfg(test)]
    pub(crate) fn scripted(model: Model) -> Self {
        Self {
            context_length: model.info.config.context_window,
            model,
            catalog_key: "scripted".to_owned(),
            chat_completions: false,
            move_system_to_preamble: false,
        }
    }
}

// --- Kind-level helpers -----------------------------------------------------------

/// The config kind string: the serde tag, also the model-registry key.
pub fn config_kind_str(config: &ProviderConfig) -> &'static str {
    match config {
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Openai(_) => "openai",
        ProviderConfig::OpenaiCompatible(_) => "openai-compatible",
        ProviderConfig::Synthetic(_) => "synthetic",
        ProviderConfig::ZaiCodingPlan(_) => "zai-coding-plan",
        ProviderConfig::Ollama(_) => "ollama",
        ProviderConfig::LlamaCpp(_) => "llama-cpp",
        ProviderConfig::Omlx(_) => "omlx",
        ProviderConfig::AmazonBedrock(_) => "amazon-bedrock",
    }
}

/// The models.dev catalog key for a config, with the endpoint-host
/// overrides gateway vendors get right and kind names cannot.
pub fn catalog_key(config: &ProviderConfig) -> &'static str {
    match config {
        ProviderConfig::OpenaiCompatible(entry) => {
            let host = entry
                .config
                .base_url
                .split("://")
                .nth(1)
                .unwrap_or(&entry.config.base_url)
                .split(['/', ':'])
                .next()
                .unwrap_or_default();
            if host == "api.synthetic.new" {
                "synthetic"
            } else if host == "api.z.ai" {
                "zai-coding-plan"
            } else {
                "openai-compatible"
            }
        }
        ProviderConfig::Openai(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Synthetic(_) => "synthetic",
        ProviderConfig::ZaiCodingPlan(_) => "zai-coding-plan",
        ProviderConfig::Ollama(_) => "ollama",
        ProviderConfig::LlamaCpp(_) => "llama-cpp",
        ProviderConfig::Omlx(_) => "omlx",
        ProviderConfig::AmazonBedrock(_) => "amazon-bedrock",
    }
}

/// Keyless routing: no explicit key, the default env var unset or empty,
/// and a local (loopback or private-network) host. Test credential
/// injection counts as an explicit key and disables the route.
fn keyless_local(settings: &OpenAiConfig, injected: bool) -> bool {
    !injected
        && settings.api_key.is_none()
        && settings.api_key_env == "OPENAI_API_KEY"
        && std::env::var("OPENAI_API_KEY").map_or(true, |key| key.trim().is_empty())
        && is_local_host(&settings.base_url)
}

/// Whether a base URL's host is loopback or RFC1918 private.
fn is_local_host(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => match host.parse::<std::net::IpAddr>() {
            Ok(ip) => ip.is_loopback() || matches!(ip, std::net::IpAddr::V4(v4) if v4.is_private()),
            Err(_) => host.ends_with(".local") || host.ends_with(".internal"),
        },
        None => false,
    }
}

// --- Bedrock ---------------------------------------------------------------------

/// The shared Bedrock config: control-plane listing, the models.dev
/// slice, and every routing field except protocol (per instance).
fn bedrock_base_config(settings: &BedrockConfig, catalog: Option<&Catalog>) -> BedrockConfig {
    let mut base = settings.clone();
    base.listing_mode = ListingMode::ControlPlane;
    base.catalog = catalog.and_then(|c| c.provider("amazon-bedrock")).cloned();
    base
}

/// Protocol per model: Claude speaks Anthropic Messages, Bedrock's
/// `openai.*` deployments speak Responses, other OpenAI-surface models
/// speak Chat Completions, everything else speaks Converse. The config's
/// per-model `protocol` override wins because animist never infers.
fn bedrock_protocol_for(config: &ProviderConfig, id: &str) -> BedrockProtocol {
    if let ProviderConfig::AmazonBedrock(entry) = config
        && let Some(protocol) = entry
            .models
            .get(id)
            .and_then(|model| model.protocol.clone())
    {
        return protocol;
    }
    let lower = id.to_ascii_lowercase();
    if lower.starts_with("anthropic.") || lower.contains("claude") {
        BedrockProtocol::AnthropicMessages
    } else if lower.starts_with("openai.") {
        BedrockProtocol::Responses
    } else if bedrock_openai_surface(&lower) {
        BedrockProtocol::ChatCompletions
    } else {
        BedrockProtocol::Converse
    }
}

/// Bedrock models served through the OpenAI compatibility surface (the
/// `/openai/v1` routes), per AWS's documented model list.
fn bedrock_openai_surface(lower_id: &str) -> bool {
    ["deepseek.", "moonshotai."]
        .iter()
        .any(|prefix| lower_id.starts_with(prefix))
}

// --- Model metadata ----------------------------------------------------------------

/// Config-driven thinking overrides layered over the catalog's claims.
fn thinking_overrides(settings: Option<&ModelConfig>) -> Overrides {
    let mut overrides = Overrides::default();
    let Some(settings) = settings else {
        return overrides;
    };
    overrides.thinking = match (settings.supports_thinking, settings.requires_thinking) {
        (_, Some(true)) => Some(ThinkingSupport::Required),
        (Some(false), _) => Some(ThinkingSupport::No),
        (Some(true), _) => Some(ThinkingSupport::Yes),
        (None, Some(false) | None) => None,
    };
    overrides
}

/// Fold the config's manual models table into a discovery listing.
/// Configured entries win field-by-field; unknown IDs are appended with
/// catalog metadata when available.
fn merge_manual_models(
    config: &ProviderConfig,
    catalog: Option<&Catalog>,
    models: &mut Vec<ModelInfo>,
) {
    let manual = config.models();
    if manual.is_empty() {
        return;
    }
    for (id, settings) in manual {
        match models.iter_mut().find(|info| info.ids.first() == Some(id)) {
            Some(existing) => {
                existing.config.context_window =
                    settings.context_length.or(existing.config.context_window);
                existing.config.max_output_tokens = settings
                    .max_output_tokens
                    .or(existing.config.max_output_tokens);
                apply_thinking_settings(existing, settings);
            }
            None => {
                let mut info = catalog
                    .and_then(|catalog| catalog.model(catalog_key(config), id))
                    .map(|entry| entry.info.clone())
                    .unwrap_or_else(|| bare_info(id));
                info.ids = vec![id.clone()];
                if let Some(context) = settings.context_length {
                    info.config.context_window = Some(context);
                }
                if let Some(output) = settings.max_output_tokens {
                    info.config.max_output_tokens = Some(output);
                }
                apply_thinking_settings(&mut info, settings);
                models.push(info);
            }
        }
    }
    models.sort_by(|a, b| a.ids.cmp(&b.ids));
    models.dedup_by(|a, b| a.ids == b.ids);
}

/// The manual table's thinking switches override the listing's claims.
fn apply_thinking_settings(info: &mut ModelInfo, settings: &crate::config::ModelConfig) {
    if let Some(supported) = settings.supports_thinking {
        info.features.thinking.supported = supported;
    }
    if let Some(required) = settings.requires_thinking {
        info.features.thinking.required = required;
    }
}

/// A minimal info for a model neither the listing nor the catalog knows:
/// an ID and default metadata, so unlisted deployments still run.
fn bare_info(id: &str) -> ModelInfo {
    let mut info = ModelInfo::default();
    info.ids = vec![id.to_owned()];
    info
}

// --- Protocol quirks ----------------------------------------------------------------

/// Whether the target protocol rejects images inside tool results.
fn chat_completions_target(config: &ProviderConfig, id: &str) -> bool {
    match config {
        ProviderConfig::Ollama(_)
        | ProviderConfig::LlamaCpp(_)
        | ProviderConfig::Omlx(_)
        | ProviderConfig::ZaiCodingPlan(_)
        | ProviderConfig::OpenaiCompatible(_) => true,
        ProviderConfig::Openai(entry) => {
            matches!(entry.config.protocol, Protocol::ChatCompletions)
        }
        ProviderConfig::Anthropic(_) | ProviderConfig::Synthetic(_) => false,
        ProviderConfig::AmazonBedrock(_) => {
            bedrock_protocol_for(config, id) == BedrockProtocol::ChatCompletions
        }
    }
}

/// Whether at-depth system messages must move into the preamble
/// (Anthropic Messages and Bedrock Converse reject them in place).
fn moves_system_to_preamble(config: &ProviderConfig, id: &str) -> bool {
    match config {
        ProviderConfig::Anthropic(_) => true,
        ProviderConfig::AmazonBedrock(_) => matches!(
            bedrock_protocol_for(config, id),
            BedrockProtocol::AnthropicMessages | BedrockProtocol::Converse
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use snafu::GenerateImplicitData;

    // --- provider failure lines ---

    fn bedrock_http_error(status: u16, body: &str) -> crate::error::Error {
        crate::error::Error::ProviderClient {
            source: Box::new(animist::provider::ProviderError::Bedrock {
                source: animist::provider::bedrock::BedrockError::Http {
                    status,
                    request_id: Some("req-1".to_owned()),
                    error_type: Some("ValidationException".to_owned()),
                    body: body.as_bytes().to_vec(),
                    backtrace: snafu::Backtrace::generate(),
                },
            }),
        }
    }

    /// A Bedrock HTTP failure carries the response body — the
    /// ValidationException message — that animist's display drops.
    #[test]
    fn failure_line_carries_the_bedrock_error_body() {
        let error = bedrock_http_error(
            400,
            "{\"message\":\"max_tokens: requested 8192, max allowed 4096\"}",
        );
        let text = provider_failure_text(&error);
        assert!(text.contains("HTTP 400"), "{text}");
        assert!(
            text.contains("max allowed 4096"),
            "the body naming the rejected field must ride the failure line: {text}"
        );
    }

    /// A non-Bedrock failure keeps its own display untouched, and a huge
    /// body is capped so one denial line cannot flood the surface.
    #[test]
    fn failure_line_caps_bodies_and_leaves_other_errors_alone() {
        let error = bedrock_http_error(400, &"x".repeat(4_096));
        assert!(provider_failure_text(&error).len() < 1_400);
        let other = crate::error::Error::ProviderClient {
            source: Box::new(animist::provider::ProviderError::Http {
                status: 400,
                body: "maximum context length exceeded".to_owned(),
                body_truncated: false,
                backtrace: snafu::Backtrace::generate(),
            }),
        };
        assert_eq!(provider_failure_text(&other), other.to_string());
    }

    // --- catalog cache policy ---

    fn cache_with_provider(id: &str) -> Catalog {
        let mut catalog = Catalog::default();
        catalog.providers.insert(
            "openai".to_owned(),
            animist::catalog::ProviderCatalog {
                info: animist::catalog::ProviderInfo::default(),
                models: [(
                    id.to_owned(),
                    animist::catalog::CatalogModel {
                        info: crate::testing::model_info_with(id, Some(8_192), None),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
            },
        );
        catalog
    }

    #[tokio::test]
    async fn a_fresh_cache_is_served_without_refetch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CATALOG_CACHE_FILE);
        let catalog = cache_with_provider("gpt-x");
        save_cache(&path, &catalog).await;
        assert!(cache_is_fresh(&path).await);
        let bytes = tokio::fs::read(&path).await.unwrap();
        let loaded: Catalog =
            serde_json::from_slice(&bytes).expect("the normalized serialization round trips");
        assert!(
            loaded.providers.contains_key("openai"),
            "the cached row round trips"
        );
    }

    #[tokio::test]
    async fn an_empty_parse_is_never_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CATALOG_CACHE_FILE);
        save_cache(&path, &Catalog::default()).await;
        assert!(
            !path.exists(),
            "an empty catalog would poison the next session against a working refresh"
        );
    }

    // --- env reauth hook ---

    fn reauth_provider_config(var: &str) -> ProviderConfig {
        let config = crate::config::Config::parse(&format!(
            "provider \"t\" {{ kind = \"openai-compatible\"\n base_url = \"http://127.0.0.1:9/v1\"\n api_key_env = \"{var}\" }}",
        ))
        .expect("test provider config parses");
        config.providers.get("t").cloned().expect("provider row")
    }

    /// The hook waits for the env credential to change, then rebuilds the
    /// provider and hands back the model to retry with. Paused time walks
    /// the 2 s poll instantly.
    #[tokio::test(start_paused = true)]
    async fn env_reauth_rebuilds_when_the_credential_changes() {
        let var = "CRAFT_TEST_REAUTH_REFRESH";
        unsafe { std::env::set_var(var, "original") };
        let config = reauth_provider_config(var);
        let hook = reauth_hook(&config, "m").await.expect("hook builds");
        let task = tokio::spawn(async move { hook.reauth(1).await });
        // Let the hook take its baseline reading and park in the poll.
        tokio::time::sleep(Duration::from_millis(1)).await;
        unsafe { std::env::set_var(var, "refreshed") };
        let model = task
            .await
            .expect("hook task joins")
            .expect("refresh succeeds")
            .expect("rebuilt model");
        assert_eq!(model.model.id, "m");
        unsafe { std::env::remove_var(var) };
    }

    /// No refresh within the deadline: the hook reports failure with the
    /// variable name, and the run surfaces it.
    #[tokio::test(start_paused = true)]
    async fn env_reauth_times_out_when_nothing_changes() {
        let var = "CRAFT_TEST_REAUTH_STALE";
        unsafe { std::env::set_var(var, "stale") };
        let config = reauth_provider_config(var);
        let hook = reauth_hook(&config, "m").await.expect("hook builds");
        let outcome = hook.reauth(1).await;
        let Err(message) = &outcome else {
            panic!("an unchanged credential must time out")
        };
        assert!(
            message.contains(var) && message.contains("were not refreshed"),
            "the deadline reports the variable name: {message}"
        );
        unsafe { std::env::remove_var(var) };
    }

    /// A provider with no refreshable credential (AWS chain) fails the
    /// wait immediately with an actionable message.
    #[tokio::test(start_paused = true)]
    async fn env_reauth_rejects_providers_without_a_credential_env() {
        let config = crate::config::Config::parse("provider \"b\" { kind = \"amazon-bedrock\" }")
            .expect("bedrock config parses");
        let entry = config.providers.get("b").expect("provider row");
        let hook = reauth_hook(entry, "m").await.expect("hook builds");
        let outcome = hook.reauth(1).await;
        let Err(message) = &outcome else {
            panic!("a credential-less provider must refuse")
        };
        assert!(
            message.contains("no refreshable credential"),
            "the hook refuses immediately: {message}"
        );
    }

    // --- keyless local routing ---

    fn openai_config(base_url: &str) -> OpenAiConfig {
        let mut config = OpenAiConfig::default();
        config.base_url = base_url.to_owned();
        config
    }

    #[test]
    fn keyless_local_requires_a_private_host_and_no_key_anywhere() {
        let unset = std::env::var("OPENAI_API_KEY").is_err()
            || std::env::var("OPENAI_API_KEY").unwrap().trim().is_empty();
        if !unset {
            // A developer machine with a real key cannot exercise the
            // env branch; the remaining cases still hold.
            assert!(!keyless_local(
                &openai_config("http://localhost:9931/v1"),
                false
            ));
            return;
        }
        assert!(keyless_local(
            &openai_config("http://localhost:9931/v1"),
            false
        ));
        assert!(keyless_local(
            &openai_config("http://192.168.1.10:8080/v1"),
            false
        ));
        assert!(keyless_local(
            &openai_config("http://box.internal/v1"),
            false
        ));
        // An injected credential or a public host disqualifies.
        assert!(!keyless_local(
            &openai_config("http://localhost:9931/v1"),
            true
        ));
        assert!(!keyless_local(
            &openai_config("https://api.example.com/v1"),
            false
        ));
    }

    #[tokio::test]
    async fn a_failed_refresh_serves_the_stale_cache_or_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CATALOG_CACHE_FILE);
        save_cache(&path, &cache_with_provider("gpt-x")).await;
        let stale = stale_or_empty(&path, Some("fetch failed".to_owned())).await;
        assert!(stale.providers.contains_key("openai"), "stale bytes serve");

        let missing = tmp.path().join("absent.json");
        let empty = stale_or_empty(&missing, Some("fetch failed".to_owned())).await;
        assert!(empty.providers.is_empty(), "nothing to serve but empty");
    }

    #[tokio::test]
    async fn cache_freshness_follows_the_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(CATALOG_CACHE_FILE);
        std::fs::write(&path, b"x").unwrap();
        assert!(cache_is_fresh(&path).await);
        // Backdate past the TTL.
        let old =
            std::time::SystemTime::now() - CATALOG_CACHE_TTL - std::time::Duration::from_secs(60);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(file);
        assert!(!cache_is_fresh(&path).await);
    }

    // --- Bedrock protocol table ---

    fn bedrock_config_with(protocol: Option<BedrockProtocol>, id: &str) -> ProviderConfig {
        let mut entry = crate::config::ProviderEntry::default();
        if let Some(protocol) = protocol {
            entry.models.insert(
                id.to_owned(),
                crate::config::ModelConfig {
                    protocol: Some(protocol),
                    ..Default::default()
                },
            );
        }
        ProviderConfig::AmazonBedrock(entry)
    }

    #[test]
    fn bedrock_protocol_follows_the_family_table_and_per_model_override() {
        let config = bedrock_config_with(None, "");
        assert_eq!(
            bedrock_protocol_for(&config, "anthropic.claude-sonnet-5"),
            BedrockProtocol::AnthropicMessages
        );
        assert_eq!(
            bedrock_protocol_for(&config, "us.anthropic.claude-opus-4"),
            BedrockProtocol::AnthropicMessages
        );
        assert_eq!(
            bedrock_protocol_for(&config, "openai.gpt-5.6"),
            BedrockProtocol::Responses
        );
        assert_eq!(
            bedrock_protocol_for(&config, "deepseek.r1-v3"),
            BedrockProtocol::ChatCompletions
        );
        assert_eq!(
            bedrock_protocol_for(&config, "meta.llama4-405b"),
            BedrockProtocol::Converse
        );
        // The per-model override wins over the family guess.
        let overridden =
            bedrock_config_with(Some(BedrockProtocol::Converse), "anthropic.claude-sonnet-5");
        assert_eq!(
            bedrock_protocol_for(&overridden, "anthropic.claude-sonnet-5"),
            BedrockProtocol::Converse
        );
    }
}
