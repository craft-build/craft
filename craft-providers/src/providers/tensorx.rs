use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use craft_storage::id::SessionRef;
use flume::Sender;
use serde_json::{Value, json};

use crate::model::{Model, ModelEntry, ModelInfo};
use crate::provider::Provider;
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse, dialect};

use super::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use super::{KeyPool, ResolvedAuth, lock_unpoison};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    api_key_env: "TENSORX_API_KEY",
    base_url: "https://api.tensorx.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "TensorX",
};

const OUTPUT_TOKEN_MARGIN: u32 = 4096;

inventory::submit!(craft_config::providers::BuiltInProvider {
    slug: "tensorx",
    display_name: "TensorX",
    protocol: craft_config::providers::Protocol::Openai,
    default_base_url: "https://api.tensorx.ai/v1",
    default_api_key_env: "TENSORX_API_KEY",
    default_model: "tensorx/z-ai/glm-5.2",
    plans: None,
    login_url: Some("https://tensorx.ai"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[]
}

#[derive(Debug)]
struct TensorXModelInfo {
    has_thinking: bool,
    has_reasoning_effort: bool,
}

pub struct TensorX {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
}

impl TensorX {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("tensorx", CONFIG.api_key_env)?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts)?,
            auth: Arc::new(Mutex::new(ResolvedAuth::bearer(pool.current()))),
            key_pool: Some(pool),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts)?,
            auth,
            key_pool: None,
            system_prefix: None,
        })
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }
}

#[async_trait]
impl Provider for TensorX {
    #[allow(clippy::too_many_arguments)]
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
        let mut buf = String::new();
        let system = super::with_prefix(&self.system_prefix, system, &mut buf);
        let mut body = self.compat.build_body(model, messages, system, tools);

        let (has_thinking, has_reasoning_effort) = {
            let guard = crate::model_registry::model_registry().read().unwrap();
            // Discovery keys by the builtin slug; a dynamic wrap's model
            // carries its own slug, so don't key by model.provider.
            let info = guard
                .discovered("tensorx", &model.id)
                .and_then(|d| d.provider_info.clone())
                .map(|arc| {
                    Arc::downcast::<TensorXModelInfo>(arc).expect("wrong provider info type")
                });
            if let Some(info) = info {
                (info.has_thinking, info.has_reasoning_effort)
            } else {
                (false, false)
            }
        };

        if has_thinking {
            body["thinking"] = json!(opts.thinking.is_enabled());
        }
        if has_reasoning_effort {
            opts.thinking
                .apply_reasoning_effort(&mut body, &dialect::TENSORX, model);
        }
        // Fallback for deepseek models that use chat_template_kwargs
        else if !has_thinking
            && opts.thinking.is_enabled()
            && model.id.starts_with("deepseek/deepseek-v4")
        {
            body["chat_template_kwargs"] = json!({"thinking": true});
        }

        self.compat
            .do_stream(model, &[], &body, event_tx, &auth)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        let models = self.list_models_with_info().await?;
        Ok(models.into_iter().map(|m| m.id).collect())
    }

    async fn list_models_with_info(&self) -> Result<Vec<ModelInfo>, AgentError> {
        let auth = lock_unpoison(&self.auth).clone();
        let url = format!("{}/model/info", CONFIG.base_url);
        let text = self.compat.get_text(&auth, &url).await?;
        let body: Value = serde_json::from_str(&text)?;

        let mut models: Vec<ModelInfo> = body["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let id = entry["model_name"].as_str()?;
                        let info = entry.get("model_info")?;

                        // Only include models with mode "chat" or mode null
                        let mode_ok = info
                            .get("mode")
                            .and_then(|v| v.as_str())
                            .is_none_or(|m| m == "chat");
                        if !mode_ok {
                            return None;
                        }

                        // Context window: prefer max_tokens, fall back to max_input_tokens
                        let context_window = info["max_tokens"]
                            .as_u64()
                            .or_else(|| info["max_input_tokens"].as_u64())
                            .and_then(|v| u32::try_from(v).ok());

                        // The API enforces input+max_output<=context_window, so cap
                        // below the window to leave room for the prompt.
                        let max_output_tokens =
                            context_window.map(|cw| cw.saturating_sub(OUTPUT_TOKEN_MARGIN));

                        let supports_vision = info
                            .get("supports_vision")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);

                        let supports_thinking =
                            info.get("supports_reasoning").and_then(Value::as_bool);

                        let supported_params = info
                            .get("supported_openai_params")
                            .and_then(Value::as_array)
                            .map(|params| TensorXModelInfo {
                                has_thinking: params.iter().any(|v| v.as_str() == Some("thinking")),
                                has_reasoning_effort: params
                                    .iter()
                                    .any(|v| v.as_str() == Some("reasoning_effort")),
                            });

                        Some(ModelInfo {
                            id: id.to_string(),
                            context_window,
                            max_output_tokens,
                            supports_thinking,
                            supports_vision: Some(supports_vision),
                            pricing: None,
                            provider_info: supported_params
                                .map(|p| Arc::new(p) as Arc<dyn std::any::Any + Send + Sync>),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    async fn rotate_key(&self) -> Result<bool, AgentError> {
        Ok(self
            .key_pool
            .as_ref()
            .is_some_and(|p| p.rotate_auth(&self.auth, ResolvedAuth::bearer)))
    }
}
