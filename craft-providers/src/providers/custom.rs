use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use flume::Sender;
use serde_json::Value;

use craft_config::providers::{
    Protocol, ProviderDef, ProvidersConfig, builtin_provider, resolve_api_key_env,
    resolve_base_url, resolve_protocol,
};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{ResolvedAuth, lock_unpoison};
use crate::model::{Model, ModelTier};
use crate::provider::{BoxFuture, Provider, ProviderKind};
use crate::providers::Timeouts;
use crate::types::ThinkingConfig;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

#[derive(Debug, Clone, Default)]
struct CachedModelInfo {
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
}

static DISCOVERED: OnceLock<RwLock<HashMap<String, CachedModelInfo>>> = OnceLock::new();

fn discovered_cache() -> &'static RwLock<HashMap<String, CachedModelInfo>> {
    DISCOVERED.get_or_init(|| RwLock::new(HashMap::new()))
}

static CUSTOM_OPENAI_CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "custom",
};

fn resolve_provider_kind(slug: &str) -> Option<ProviderKind> {
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    match def.protocol? {
        Protocol::Openai => Some(ProviderKind::OpenAi),
        Protocol::Anthropic => Some(ProviderKind::Anthropic),
        Protocol::Google => Some(ProviderKind::Google),
    }
}

fn resolve_custom_auth_from_def(slug: &str, def: &ProviderDef) -> Result<ResolvedAuth, AgentError> {
    let env_var = resolve_api_key_env(slug, Some(def));
    let pool = super::KeyPool::resolve(slug, &env_var)?;

    let base_url = resolve_base_url(slug, Some(def)).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;
    let mut auth = ResolvedAuth::bearer(pool.current());
    auth.base_url = Some(base_url);
    Ok(auth)
}

pub fn create(slug: &str, timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    let config = ProvidersConfig::load();
    let def = config.get(slug).ok_or_else(|| AgentError::Config {
        message: format!("unknown custom provider '{slug}'"),
    })?;
    create_from_def(slug, def, timeouts)
}

fn create_from_def(
    slug: &str,
    def: &ProviderDef,
    timeouts: Timeouts,
) -> Result<Box<dyn Provider>, AgentError> {
    let kind = match def.protocol {
        Some(Protocol::Openai) => ProviderKind::OpenAi,
        Some(Protocol::Anthropic) => ProviderKind::Anthropic,
        Some(Protocol::Google) => ProviderKind::Google,
        None => {
            return Err(AgentError::Config {
                message: format!("unknown custom provider '{slug}'"),
            });
        }
    };
    let resolved = resolve_custom_auth_from_def(slug, def)?;
    let auth = Arc::new(Mutex::new(resolved));

    match kind {
        ProviderKind::Anthropic => Ok(Box::new(super::anthropic::Anthropic::with_auth(
            auth, timeouts,
        )?)),
        ProviderKind::OpenAi => Ok(Box::new(CustomOpenAiProvider {
            compat: OpenAiCompatProvider::new(&CUSTOM_OPENAI_CONFIG, timeouts)?,
            auth,
        })),
        ProviderKind::Google => Ok(Box::new(super::google::Google::with_auth(auth, timeouts)?)),
        _ => Err(AgentError::Config {
            message: format!(
                "unsupported protocol for custom provider '{slug}', only openai/anthropic/google are supported"
            ),
        }),
    }
}

pub fn lookup_model(slug: &str, model_id: &str) -> Option<Model> {
    let kind = resolve_provider_kind(slug)?;
    let config = ProvidersConfig::load();
    let def = config.get(slug);

    let explicit = def.and_then(|d| d.models.as_ref()?.iter().find(|m| m.id == model_id));

    let cache_key = format!("{slug}/{model_id}");
    let cached = discovered_cache().read().unwrap().get(&cache_key).cloned();

    let context_window = explicit
        .and_then(|m| m.context_window)
        .or(cached.as_ref().and_then(|c| c.context_window))
        .unwrap_or_else(|| kind.fallback_context_window());

    let max_output_tokens = explicit
        .and_then(|m| m.max_output_tokens)
        .or(cached.as_ref().and_then(|c| c.max_output_tokens))
        .unwrap_or_else(|| kind.fallback_max_output());

    Some(Model {
        id: model_id.to_string(),
        provider: kind,
        dynamic_slug: Some(slug.to_string()),
        tier: ModelTier::Medium,
        family: kind.family(),
        supports_tool_examples_override: None,
        pricing: Default::default(),
        max_output_tokens,
        context_window,
    })
}

pub async fn discover_models(timeouts: Timeouts) -> Vec<String> {
    let config = ProvidersConfig::load();
    let mut all_specs = Vec::new();
    for (slug, def) in &config.providers {
        if builtin_provider(slug).is_some() {
            continue;
        }
        if !def.discover_models {
            continue;
        }
        if resolve_protocol(slug, Some(def)).is_none() {
            continue;
        }
        match create_from_def(slug, def, timeouts) {
            Ok(provider) => {
                let slug_c = slug.clone();
                match provider.list_models_with_info().await {
                    Ok(models) => {
                        let mut cache = discovered_cache().write().unwrap();
                        for m in models {
                            cache.insert(
                                format!("{slug_c}/{}", m.id),
                                CachedModelInfo {
                                    context_window: m.context_window,
                                    max_output_tokens: m.max_output_tokens,
                                },
                            );
                            all_specs.push(format!("{slug_c}/{}", m.id));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(slug, error = %e, "failed to list models for custom provider");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(slug, error = %e, "failed to create custom provider");
            }
        }
    }
    all_specs
}

struct CustomOpenAiProvider {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
}

impl Provider for CustomOpenAiProvider {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        let auth = lock_unpoison(&self.auth).clone();
        let mut body = self.compat.build_body(model, messages, system, tools);
        if matches!(opts.thinking, ThinkingConfig::Off) {
            body["thinking"] = serde_json::json!({"type": "disabled"});
        }
        Box::pin(async move {
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        let auth = lock_unpoison(&self.auth).clone();
        Box::pin(async move { self.compat.do_list_models(&auth).await })
    }

    fn list_models_with_info(
        &self,
    ) -> BoxFuture<'_, Result<Vec<crate::model::ModelInfo>, AgentError>> {
        let auth = lock_unpoison(&self.auth).clone();
        Box::pin(async move { self.compat.do_list_models_with_info(&auth).await })
    }
}
