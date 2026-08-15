use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use async_trait::async_trait;
use craft_config::ModelPolicy;
use flume::Sender;
use serde_json::Value;
use strum::{Display, EnumIter, EnumString};
use tracing::{debug, warn};

use crate::manifest::ManifestRegistry;
use crate::model::{Model, ModelFamily};
use crate::providers::Timeouts;
use crate::providers::anthropic::Anthropic;
use crate::providers::anthropic::bedrock;
use crate::providers::copilot::Copilot;
use crate::providers::deepseek::DeepSeek;
use crate::providers::dynamic;
use crate::providers::google::Google;
use crate::providers::local::{LLAMACPP, LocalEndpoint, OLLAMA};
use crate::providers::mistral::Mistral;
use crate::providers::openai::OpenAi;
use crate::providers::opencode::Opencode;
use crate::providers::openrouter::OpenRouter;
use crate::providers::synthetic::Synthetic;
use crate::providers::tensorx::TensorX;
use crate::providers::xai::Xai;
use crate::{AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse};
use craft_storage::id::SessionRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter)]
#[strum(serialize_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    #[strum(serialize = "openai")]
    OpenAi,
    Google,
    Copilot,
    Ollama,
    LlamaCpp,
    Mistral,
    #[strum(serialize = "deepseek")]
    DeepSeek,
    #[strum(serialize = "openrouter")]
    OpenRouter,
    Synthetic,
    #[strum(serialize = "tensorx")]
    TensorX,
    #[strum(serialize = "opencode")]
    Opencode,
    #[strum(serialize = "xai")]
    Xai,
    Bedrock,
}

impl ProviderKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::Google => "Google",
            Self::Copilot => "Copilot",
            Self::Ollama => "Ollama",
            Self::LlamaCpp => "LlamaCpp",
            Self::Mistral => "Mistral",
            Self::DeepSeek => "DeepSeek",
            Self::OpenRouter => "OpenRouter",
            Self::Synthetic => "Synthetic",
            Self::TensorX => "TensorX",
            Self::Opencode => "Opencode Zen",
            Self::Xai => "xAI",
            Self::Bedrock => "Bedrock",
        }
    }

    pub const fn api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Google => "GEMINI_API_KEY",
            Self::Copilot => "GH_COPILOT_TOKEN",
            Self::Ollama => "OLLAMA_API_KEY",
            Self::LlamaCpp => "LLAMA_CPP_API_KEY",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Synthetic => "SYNTHETIC_API_KEY",
            Self::TensorX => "TENSORX_API_KEY",
            Self::Opencode => "OPENCODE_API_KEY",
            Self::Xai => "XAI_API_KEY",
            // Bedrock auth is the AWS SDK credential chain, not a static key.
            // Surfaced only so `craft auth status` has a column to show.
            Self::Bedrock => "AWS_REGION",
        }
    }

    pub const fn base_url(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1/messages",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Google => "https://generativelanguage.googleapis.com/v1beta",
            Self::Copilot => {
                "https://api.githubcopilot.com (or GraphQL-discovered Copilot API endpoint)"
            }
            Self::Ollama => "http://localhost:11434/v1",
            Self::LlamaCpp => "http://localhost:8080/v1",
            Self::Mistral => "https://api.mistral.ai/v1",
            Self::DeepSeek => "https://api.deepseek.com",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::Synthetic => "https://api.synthetic.new/openai/v1",
            Self::TensorX => "https://api.tensorx.ai/v1",
            Self::Opencode => "https://opencode.ai/zen/v1",
            Self::Xai => "https://api.x.ai/v1",
            Self::Bedrock => "https://bedrock-runtime.<region>.amazonaws.com (AWS SDK)",
        }
    }

    pub const fn supports_thinking(self) -> bool {
        matches!(
            self,
            Self::Anthropic
                | Self::Google
                | Self::Mistral
                | Self::DeepSeek
                | Self::Synthetic
                | Self::OpenAi
                | Self::OpenRouter
                | Self::LlamaCpp
                | Self::TensorX
                | Self::Opencode
                | Self::Xai
                | Self::Bedrock
        )
    }

    pub const fn features(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => {
                Some("Prompt caching, thinking mode (adaptive/budgeted), advanced tool use")
            }
            Self::Google => Some("Native Gemini API with thinking support"),
            Self::Copilot => Some("Native Copilot Chat HTTP API with model endpoint discovery"),
            Self::Ollama => {
                Some("Local or remote inference via OLLAMA_HOST, cloud fallback via OLLAMA_API_KEY")
            }
            Self::LlamaCpp => Some(
                "Local or remote inference via LLAMA_CPP_HOST, set optional key via LLAMA_CPP_API_KEY",
            ),
            Self::Synthetic => {
                Some("Reasoning effort support (low/medium/high), open-weight models")
            }
            Self::TensorX => Some("Open-weight models, zero data retention, prompt caching"),
            Self::DeepSeek => Some("Thinking mode toggle (on/off), open-weight models"),
            Self::OpenRouter => {
                Some("300+ models from all providers, prompt caching, provider routing")
            }
            Self::Opencode => Some(
                "Dynamically discovered models via [models.dev](https://models.dev/) + all the models provided by Opencode Zen API",
            ),
            Self::Xai => Some(
                "OAuth login, account-specific model catalog, Grok reasoning (low/medium/high/xhigh)",
            ),
            Self::Bedrock => Some(
                "AWS SDK auth (SSO/IMDS/profiles/env), ConverseStream, inference-profile discovery",
            ),
            _ => None,
        }
    }

    pub const fn family(self) -> ModelFamily {
        match self {
            Self::Anthropic => ModelFamily::Claude,
            Self::OpenAi => ModelFamily::Gpt,
            Self::Google => ModelFamily::Gemini,
            Self::Copilot => ModelFamily::Generic,
            Self::Ollama => ModelFamily::Generic,
            Self::LlamaCpp => ModelFamily::Generic,
            Self::Mistral => ModelFamily::Generic,
            Self::DeepSeek => ModelFamily::Generic,
            Self::OpenRouter => ModelFamily::Generic,
            Self::Synthetic => ModelFamily::Synthetic,
            Self::TensorX => ModelFamily::Generic,
            Self::Opencode => ModelFamily::Generic,
            Self::Xai => ModelFamily::Generic,
            Self::Bedrock => ModelFamily::Claude,
        }
    }

    pub const fn accepts_arbitrary_models(self) -> bool {
        matches!(
            self,
            Self::Ollama
                | Self::LlamaCpp
                | Self::Google
                | Self::Copilot
                | Self::OpenRouter
                | Self::TensorX
                | Self::Mistral
                | Self::Opencode
                | Self::Xai
                | Self::Bedrock
        )
    }

    /// `None` when we honestly don't know the output window: llama.cpp
    /// serves whatever model the user loaded. Unknown means "don't limit",
    /// never "assume small"; a `0` sentinel here once silently capped
    /// llama.cpp thinking budgets at the floor.
    pub const fn fallback_max_output(self) -> Option<u32> {
        match self {
            Self::Anthropic => Some(128_000),
            Self::OpenAi => Some(100_000),
            Self::Google => Some(65_536),
            Self::Copilot => Some(100_000),
            Self::Ollama => Some(16_384),
            Self::LlamaCpp => None,
            Self::Mistral => None,
            Self::DeepSeek => Some(384_000),
            Self::OpenRouter => Some(128_000),
            Self::Synthetic => Some(32_000),
            Self::TensorX => Some(32_000),
            Self::Opencode => Some(128_000),
            Self::Xai => Some(131_072),
            Self::Bedrock => Some(128_000),
        }
    }

    pub const fn fallback_context_window(self) -> u32 {
        match self {
            Self::Anthropic => 200_000,
            Self::OpenAi => 200_000,
            Self::Google => 1_000_000,
            Self::Copilot => 200_000,
            Self::Ollama => 128_000,
            Self::LlamaCpp => 128_000,
            Self::Mistral => 128_000,
            Self::DeepSeek => 1_000_000,
            Self::OpenRouter => 200_000,
            Self::Synthetic => 128_000,
            Self::TensorX => 200_000,
            Self::Opencode => 256_000,
            Self::Xai => 500_000,
            Self::Bedrock => 200_000,
        }
    }

    pub async fn create(self, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
        match self {
            Self::Anthropic => {
                if bedrock::is_enabled() {
                    Ok(Box::new(bedrock::Bedrock::new(timeouts).await?))
                } else {
                    Ok(Box::new(Anthropic::new(timeouts)?))
                }
            }
            Self::OpenAi => Ok(Box::new(OpenAi::new(timeouts).await?)),
            Self::Google => Ok(Box::new(Google::new(timeouts)?)),
            Self::Copilot => Ok(Box::new(Copilot::new(timeouts)?)),
            Self::Ollama => Ok(Box::new(LocalEndpoint::new(&OLLAMA, timeouts)?)),
            Self::LlamaCpp => Ok(Box::new(LocalEndpoint::new(&LLAMACPP, timeouts)?)),
            Self::Mistral => Ok(Box::new(Mistral::new(timeouts)?)),
            Self::DeepSeek => Ok(Box::new(DeepSeek::new(timeouts)?)),
            Self::OpenRouter => Ok(Box::new(OpenRouter::new(timeouts)?)),
            Self::Synthetic => Ok(Box::new(Synthetic::new(timeouts)?)),
            Self::TensorX => Ok(Box::new(TensorX::new(timeouts)?)),
            Self::Opencode => Ok(Box::new(Opencode::new(timeouts)?)),
            Self::Xai => Ok(Box::new(Xai::new(timeouts).await?)),
            Self::Bedrock => {
                #[cfg(feature = "bedrock")]
                {
                    crate::providers::bedrock::create(timeouts)
                        .await
                        .map(|b| Box::new(b) as Box<dyn Provider>)
                }
                #[cfg(not(feature = "bedrock"))]
                Err(AgentError::Config {
                    message: "the `bedrock` cargo feature is not enabled; rebuild craft with \
                              the feature on"
                        .into(),
                })
            }
        }
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[async_trait]
pub trait Provider: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&SessionRef>,
    ) -> Result<StreamResponse, AgentError>;

    async fn list_models(&self) -> Result<Vec<String>, AgentError>;

    /// Richer variant of `list_models` that may include per-model metadata
    /// discovered from the provider's API. Defaults to wrapping `list_models`.
    async fn list_models_with_info(&self) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let ids = self.list_models().await?;
        Ok(ids.into_iter().map(crate::model::ModelInfo::new).collect())
    }

    /// Fetch provider-side usage quota (remaining percentage / reset times).
    /// `Ok(None)` means the provider does not expose a programmatic usage endpoint.
    async fn fetch_usage(&self) -> Result<Option<ProviderUsage>, AgentError> {
        Ok(None)
    }

    async fn refresh_auth(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn reload_auth(&self) -> Result<(), AgentError> {
        Ok(())
    }

    async fn rotate_key(&self) -> Result<bool, AgentError> {
        Ok(false)
    }

    fn adjust_model(&self, _model: &mut Model) {}
}

pub async fn provider_for_slug(
    slug: &str,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    if let Ok(kind) = ProviderKind::from_str(slug) {
        return kind.create(timeouts).await;
    }
    if crate::providers::opencode::is_catalog_family_slug(slug) {
        return crate::providers::opencode::create_for_slug(slug, timeouts);
    }
    if dynamic::display_name(slug).is_some() {
        dynamic::create(slug, timeouts).await
    } else {
        crate::providers::custom::create(slug, timeouts)
    }
}

pub async fn provider_available(slug: &str) -> bool {
    provider_for_slug(slug, Timeouts::default()).await.is_ok()
}

pub async fn from_model(
    model: &mut Model,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let provider = provider_for_slug(&model.provider, timeouts).await?;
    provider.adjust_model(model);
    debug!(provider = %model.provider, model = %model.id, "provider created");
    Ok(provider)
}

pub async fn from_model_fallback(model: &mut Model, timeouts: Timeouts) -> Box<dyn Provider> {
    match from_model(model, timeouts).await {
        Ok(provider) => provider,
        Err(e) => {
            warn!(error = %e, "provider creation failed, using unconfigured provider");
            Box::new(UnconfiguredProvider)
        }
    }
}

pub(crate) struct UnconfiguredProvider;

const NOT_CONFIGURED: &str = "no provider configured — run /login or `craft auth login`";

#[async_trait]
impl Provider for UnconfiguredProvider {
    async fn stream_message(
        &self,
        _model: &Model,
        _messages: &[Message],
        _system: &str,
        _tools: &Value,
        _event_tx: &Sender<ProviderEvent>,
        _opts: RequestOptions,
        _session_id: Option<&SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        Err(AgentError::Config {
            message: NOT_CONFIGURED.to_string(),
        })
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        Err(AgentError::Config {
            message: NOT_CONFIGURED.to_string(),
        })
    }
}

pub struct ModelBatch {
    pub models: Vec<String>,
    pub warnings: Vec<String>,
}

pub fn available_model_specs(policy: &ModelPolicy) -> Vec<String> {
    let mut specs: Vec<String> = crate::manifest::ManifestRegistry::builtins()
        .iter()
        .flat_map(|manifest| {
            manifest
                .models
                .iter()
                .flat_map(|entry| entry.prefixes)
                .map(|prefix| format!("{}/{prefix}", manifest.slug))
        })
        .collect();
    for slug in crate::providers::dynamic::discovered_slugs() {
        for spec in crate::providers::dynamic::dynamic_model_specs_for(slug) {
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
    }
    for spec in crate::providers::custom::declared_model_specs() {
        if !specs.contains(&spec) {
            specs.push(spec);
        }
    }
    specs.retain(|spec| policy.allows(spec));
    specs
}

pub async fn fetch_all_models(
    policy: &ModelPolicy,
    mut on_ready: impl FnMut(ModelBatch),
    on_done: Option<Box<dyn FnOnce() + Send>>,
) {
    let timeouts = Timeouts::default();
    let mut futs: futures::stream::FuturesUnordered<BoxFuture<'static, ModelBatch>> =
        futures::stream::FuturesUnordered::new();

    for manifest in ManifestRegistry::builtins() {
        let slug = manifest.slug;
        let Ok(provider) = provider_for_slug(slug, timeouts).await else {
            warn!(provider = slug, "failed to create provider, skipping");
            continue;
        };
        let display_name = manifest.display_name;
        let static_models = manifest.models;
        futs.push(Box::pin(async move {
            match provider.list_models_with_info().await {
                Ok(models) => {
                    let mut specs: Vec<String> =
                        models.iter().map(|m| format!("{slug}/{}", m.id)).collect();
                    crate::model_registry::set_known_models(slug, models);
                    for entry in static_models {
                        for prefix in entry.prefixes {
                            let spec = format!("{slug}/{prefix}");
                            if !specs.contains(&spec) {
                                specs.push(spec);
                            }
                        }
                    }
                    ModelBatch {
                        models: specs,
                        warnings: Vec::new(),
                    }
                }
                Err(e) => {
                    warn!(provider = slug, error = %e, "failed to list models, using static fallback");
                    let fallback: Vec<String> = static_models
                        .iter()
                        .flat_map(|entry| entry.prefixes.iter())
                        .map(|p| format!("{slug}/{p}"))
                        .collect();
                    ModelBatch {
                        models: fallback,
                        warnings: vec![format!(
                            "{display_name}: {e} (using static fallback)"
                        )],
                    }
                }
            }
        }));
    }

    for slug in dynamic::discovered_slugs() {
        let slug = slug.to_string();
        futs.push(Box::pin(async move {
            let static_fallback = |reason: String| {
                warn!(
                    slug,
                    error = reason,
                    "dynamic model listing failed, using static fallback"
                );
                ModelBatch {
                    models: dynamic::dynamic_model_specs_for(&slug),
                    warnings: vec![format!("{slug}: {reason} (using static fallback)")],
                }
            };
            match dynamic::create(&slug, timeouts).await {
                Ok(provider) => match provider.list_models_with_info().await {
                    Ok(models) => ModelBatch {
                        models: models.iter().map(|m| format!("{slug}/{}", m.id)).collect(),
                        warnings: Vec::new(),
                    },
                    Err(e) => static_fallback(e.to_string()),
                },
                Err(e) => static_fallback(e.to_string()),
            }
        }));
    }

    futs.push(Box::pin(async move {
        match crate::providers::custom::discover_models(timeouts).await {
            models if !models.is_empty() => ModelBatch {
                models,
                warnings: Vec::new(),
            },
            _ => ModelBatch {
                models: Vec::new(),
                warnings: Vec::new(),
            },
        }
    }));

    let declared = crate::providers::custom::declared_model_specs();
    if !declared.is_empty() {
        on_ready(ModelBatch {
            models: declared,
            warnings: Vec::new(),
        });
    }

    use futures::StreamExt;
    while let Some(mut batch) = futs.next().await {
        batch.models.retain(|spec| policy.allows(spec));
        on_ready(batch);
    }
    if let Some(done) = on_done {
        done();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(allowed: &[&str], excluded: &[&str]) -> ModelPolicy {
        ModelPolicy::new(
            &allowed
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
            &excluded
                .iter()
                .map(|pattern| (*pattern).into())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn available_specs_apply_model_policy() {
        let policy = policy(&["openai/*"], &["*/gpt-5.6-terra"]);

        let specs = available_model_specs(&policy);

        assert!(!specs.is_empty());
        assert!(specs.iter().all(|spec| spec.starts_with("openai/")));
        assert!(!specs.iter().any(|spec| spec == "openai/gpt-5.6-terra"));
    }
}
