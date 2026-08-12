use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use craft_storage::id::SessionRef;
use flume::Sender;
use serde_json::Value;

use craft_config::providers::{
    Protocol, ProviderDef, ProvidersConfig, resolve_api_key_env, resolve_base_url, resolve_protocol,
};

use super::openai::responses;
use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{ResolvedAuth, lock_unpoison};
use crate::manifest::ManifestRegistry;
use crate::model::{FastPricing, Model, ModelPricing, ModelTier};
use crate::provider::{Provider, ProviderKind};
use crate::providers::Timeouts;
use crate::types::ThinkingConfig;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use async_trait::async_trait;

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
    slug: "",
    api_key_env: "",
    base_url: "",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "custom",
};

fn protocol_kind(protocol: Protocol) -> ProviderKind {
    match protocol {
        Protocol::Openai | Protocol::OpenaiResponses => ProviderKind::OpenAi,
        Protocol::Anthropic => ProviderKind::Anthropic,
        Protocol::Google => ProviderKind::Google,
    }
}

/// Builtins win their slug in `from_spec`/`create`, so every custom path skips
/// them. Key off the manifest (every builtin), not `builtin_provider`, which
/// omits the `opencode` slugs and would let them shadow the builtin.
fn is_builtin_slug(slug: &str) -> bool {
    ManifestRegistry::get(slug).is_some()
}

pub fn base_kind(slug: &str) -> Option<ProviderKind> {
    let config = ProvidersConfig::load();
    Some(protocol_kind(config.get(slug)?.protocol?))
}

/// Specs declared statically in `providers.toml` (no HTTP).
pub fn declared_model_specs() -> Vec<String> {
    declared_specs_from(&ProvidersConfig::load())
}

fn declared_specs_from(config: &ProvidersConfig) -> Vec<String> {
    let mut specs = Vec::new();
    for (slug, def) in &config.providers {
        if is_builtin_slug(slug) {
            continue;
        }
        if resolve_protocol(slug, Some(def)).is_none() {
            continue;
        }
        if let Some(models) = &def.models {
            for m in models {
                specs.push(format!("{slug}/{}", m.id));
            }
        }
    }
    specs
}

/// Outcome of resolving a tier against `providers.toml` in a single read.
pub enum TierLookup {
    Model(Model),
    /// Provider exists but declares no model at this tier; carries the base kind
    /// so the caller can inherit the base protocol's default.
    NoModelForTier(ProviderKind),
    Unknown,
}

pub fn resolve_tier(slug: &str, tier: ModelTier) -> TierLookup {
    // Builtins are never overridden through providers.toml (from_spec/create
    // check builtin first); keep the tier path consistent with that.
    if is_builtin_slug(slug) {
        return TierLookup::Unknown;
    }
    let config = ProvidersConfig::load();
    let Some(def) = config.get(slug) else {
        return TierLookup::Unknown;
    };
    let Some(protocol) = def.protocol else {
        return TierLookup::Unknown;
    };
    let kind = protocol_kind(protocol);
    match def
        .models
        .as_ref()
        .and_then(|ms| ms.iter().find(|m| ModelTier::from(m.tier) == tier))
    {
        Some(declared) => TierLookup::Model(model_from_def(def, kind, slug, &declared.id)),
        None => TierLookup::NoModelForTier(kind),
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
        Some(Protocol::Openai) | Some(Protocol::OpenaiResponses) => ProviderKind::OpenAi,
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
            protocol: def.protocol.unwrap_or(Protocol::Openai),
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
    if is_builtin_slug(slug) {
        return None;
    }
    let config = ProvidersConfig::load();
    let def = config.get(slug)?;
    let kind = protocol_kind(def.protocol?);
    Some(model_from_def(def, kind, slug, model_id))
}

/// Build a model from an already-loaded provider definition so tier resolution
/// and id lookup can share one `providers.toml` read instead of loading twice.
fn model_from_def(def: &ProviderDef, kind: ProviderKind, slug: &str, model_id: &str) -> Model {
    let declared = def
        .models
        .as_ref()
        .and_then(|ms| ms.iter().find(|m| m.id == model_id));

    let cache_key = format!("{slug}/{model_id}");
    let cached = discovered_cache().read().unwrap().get(&cache_key).cloned();

    let tier = declared
        .map(|m| ModelTier::from(m.tier))
        .unwrap_or(ModelTier::Medium);
    let max_output_tokens = declared
        .and_then(|m| m.max_output_tokens)
        .or(cached.as_ref().and_then(|c| c.max_output_tokens))
        .or_else(|| kind.fallback_max_output());
    let context_window = declared
        .and_then(|m| m.context_window)
        .or(cached.as_ref().and_then(|c| c.context_window))
        .unwrap_or_else(|| kind.fallback_context_window());
    let supports_tool_examples_override = declared.and_then(|m| m.supports_tool_examples);
    let supports_thinking_override = declared
        .and_then(|m| m.supports_thinking)
        .or_else(|| ManifestRegistry::get(&kind.to_string()).map(|m| m.supports_thinking));
    let supports_vision_override = declared.and_then(|m| m.supports_vision);
    let pricing = declared
        .filter(|m| m.has_pricing())
        .map(|m| ModelPricing {
            input: m.pricing_input.unwrap_or(0.0),
            output: m.pricing_output.unwrap_or(0.0),
            cache_write: m.pricing_cache_write.unwrap_or(0.0),
            cache_read: m.pricing_cache_read.unwrap_or(0.0),
            fast: declared
                .filter(|d| d.has_fast_pricing())
                .map(|d| FastPricing {
                    input: d.pricing_fast_input.unwrap_or(0.0),
                    output: d.pricing_fast_output.unwrap_or(0.0),
                }),
        })
        .unwrap_or_default();

    Model {
        id: model_id.to_string(),
        provider: Arc::from(slug),
        tier,
        family: kind.family(),
        supports_tool_examples_override,
        supports_thinking_override,
        supports_vision_override,
        pricing,
        max_output_tokens,
        context_window,
    }
}

pub async fn discover_models(timeouts: Timeouts) -> Vec<String> {
    let config = ProvidersConfig::load();
    let mut all_specs = Vec::new();
    for (slug, def) in &config.providers {
        if is_builtin_slug(slug) {
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
    protocol: Protocol,
}

#[async_trait]
impl Provider for CustomOpenAiProvider {
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
        let auth = lock_unpoison(&self.auth).clone();

        if self.protocol == Protocol::OpenaiResponses {
            let body = responses::build_body(model, messages, system, tools);
            return responses::do_stream(
                self.compat.client(),
                model,
                &body,
                event_tx,
                &auth,
                self.compat.stream_timeout(),
            )
            .await;
        }

        let mut body = self.compat.build_body(model, messages, system, tools);
        if matches!(opts.thinking, ThinkingConfig::Off) {
            body["thinking"] = serde_json::json!({"type": "disabled"});
        }
        self.compat
            .do_stream(model, &[], &body, event_tx, &auth)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        let auth = lock_unpoison(&self.auth).clone();
        self.compat.do_list_models(&auth).await
    }

    async fn list_models_with_info(&self) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let auth = lock_unpoison(&self.auth).clone();
        self.compat.do_list_models_with_info(&auth).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn openai_def(model_id: &str) -> ProviderDef {
        serde_json::from_str(&format!(
            r#"{{"protocol":"openai","models":[{{"id":"{model_id}"}}]}}"#
        ))
        .unwrap()
    }

    // `opencode` is a builtin whose slug is absent from the `builtin_provider`
    // inventory; the old guard leaked it into the picker, where it then resolved
    // as the builtin and silently dropped the custom model. Listing must skip
    // every builtin slug so a providers.toml entry can never shadow one.
    #[test]
    fn declared_specs_skip_builtin_named_entries_but_keep_custom() {
        let mut config = ProvidersConfig::default();
        config.upsert("opencode".to_string(), openai_def("shadow-model"));
        config.upsert("my-custom".to_string(), openai_def("real-model"));

        let specs = declared_specs_from(&config);
        assert!(
            !specs.iter().any(|s| s.starts_with("opencode/")),
            "builtin slug must be skipped in custom listing: {specs:?}"
        );
        assert!(specs.contains(&"my-custom/real-model".to_string()));

        // Resolution owns the builtin slug regardless of the providers.toml entry.
        let model = Model::from_spec("opencode/shadow-model").unwrap();
        assert_eq!(model.provider.as_ref(), "opencode");
    }
}
