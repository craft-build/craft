use std::sync::{Arc, Mutex};

use craft_storage::StateDir;
use craft_storage::id::SessionRef;
use flume::Sender;
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::model::{Model, ModelInfo};
use crate::provider::Provider;
use crate::providers::openai::responses;
use crate::providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::providers::{ResolvedAuth, lock_unpoison};
use crate::{
    AgentError, Message, ProviderEvent, ProviderUsage, RequestOptions, StreamResponse, dialect,
};
use async_trait::async_trait;

use super::{auth, catalog};

static CONFIG: OpenAiCompatConfig = OpenAiCompatConfig {
    slug: "xai",
    api_key_env: "XAI_API_KEY",
    base_url: "https://api.x.ai/v1",
    max_tokens_field: "max_tokens",
    include_stream_usage: true,
    provider_name: "xAI",
};

const ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";
const OAUTH_TOKEN_MISSING: &str = "xAI OAuth token missing from resolved auth";

pub struct Xai {
    compat: OpenAiCompatProvider,
    auth: Arc<Mutex<ResolvedAuth>>,
    storage: Option<StateDir>,
    system_prefix: Option<String>,
}

impl Xai {
    pub async fn new(timeouts: crate::providers::Timeouts) -> Result<Self, AgentError> {
        let storage = StateDir::resolve()?;
        let resolved = auth::resolve(&storage).await?;
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts)?,
            auth: Arc::new(Mutex::new(resolved)),
            storage: Some(storage),
            system_prefix: None,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<ResolvedAuth>>,
        timeouts: crate::providers::Timeouts,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            compat: OpenAiCompatProvider::new(&CONFIG, timeouts)?,
            auth,
            storage: None,
            system_prefix: None,
        })
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn current_auth(&self) -> ResolvedAuth {
        lock_unpoison(&self.auth).clone()
    }

    fn is_oauth(&self) -> bool {
        self.storage.as_ref().is_some_and(auth::is_oauth)
    }

    async fn refresh_oauth(&self) -> Result<(), AgentError> {
        let storage = self.storage.clone().ok_or_else(|| AgentError::Config {
            message: "OAuth refresh not available for externally-managed auth".into(),
        })?;
        let tokens = tokio::task::spawn_blocking({
            let storage = storage.clone();
            move || craft_storage::auth::load_tokens(&storage, auth::PROVIDER)
        })
        .await
        .map_err(|e| AgentError::Config {
            message: format!("xai load_tokens task: {e}"),
        })?
        .ok_or_else(|| AgentError::Api {
            status: 401,
            message: "xAI OAuth tokens not found on disk".into(),
        })?;
        let resolved = match auth::refresh_tokens(&tokens).await {
            Ok(fresh) => {
                let resolved = auth::build_oauth_resolved(&fresh);
                tokio::task::spawn_blocking({
                    let storage = storage.clone();
                    move || craft_storage::auth::save_tokens(&storage, auth::PROVIDER, &fresh)
                })
                .await
                .map_err(|e| AgentError::Config {
                    message: format!("xai save_tokens task: {e}"),
                })??;
                resolved
            }
            Err(e) => {
                warn!(error = %e, "xAI OAuth refresh failed, clearing stale tokens");
                let _ = tokio::task::spawn_blocking({
                    let storage = storage.clone();
                    move || craft_storage::auth::delete_tokens(&storage, auth::PROVIDER)
                })
                .await
                .map_err(|e| AgentError::Config {
                    message: format!("xai delete_tokens task: {e}"),
                })?;
                catalog::invalidate();
                return Err(e);
            }
        };
        *lock_unpoison(&self.auth) = resolved;
        debug!("refreshed xAI OAuth token");
        Ok(())
    }

    async fn with_oauth_retry<T, F, Fut>(&self, f: F) -> Result<T, AgentError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<T, AgentError>>,
    {
        let result = f().await;
        if self.is_oauth()
            && matches!(&result, Err(e) if e.is_auth_error())
            && self.refresh_oauth().await.is_ok()
        {
            return f().await;
        }
        result
    }

    async fn catalog_models(&self) -> Result<Vec<ModelInfo>, AgentError> {
        self.with_oauth_retry(|| async {
            let auth = self.current_auth();
            let access = bearer_token(&auth).ok_or_else(|| AgentError::Config {
                message: OAUTH_TOKEN_MISSING.into(),
            })?;
            catalog::list_models(&access, false).await
        })
        .await
    }
}

fn apply_grok_reasoning(body: &mut Value, opts: &RequestOptions, model: &Model) {
    if !model.supports_thinking() {
        return;
    }
    if let Some(effort) = opts.thinking.effort_str(&dialect::GROK, model) {
        body["reasoning"] = json!({ "effort": effort });
    }
    let include = body["include"].as_array_mut();
    match include {
        Some(arr) => {
            if !arr.iter().any(|v| v.as_str() == Some(ENCRYPTED_REASONING)) {
                arr.push(json!(ENCRYPTED_REASONING));
            }
        }
        None => body["include"] = json!([ENCRYPTED_REASONING]),
    }
}

fn proxy_request_headers(model: &Model, session_id: Option<&SessionRef>) -> Vec<(String, String)> {
    let session = session_id
        .map(ToString::to_string)
        .unwrap_or_else(random_id);
    vec![
        ("accept".into(), "text/event-stream".into()),
        ("x-grok-conv-id".into(), session.clone()),
        ("x-grok-session-id".into(), session),
        ("x-grok-req-id".into(), random_id()),
        (
            "x-grok-model-override".into(),
            model.id.to_ascii_lowercase(),
        ),
    ]
}

fn random_id() -> String {
    format!("{:016x}{:016x}", fastrand::u64(..), fastrand::u64(..))
}

#[async_trait]
impl Provider for Xai {
    async fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
        session_id: Option<&SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        let mut buf = String::new();
        let system = super::super::with_prefix(&self.system_prefix, system, &mut buf);

        if self.is_oauth() {
            let mut body = responses::build_body(model, messages, system, tools);
            apply_grok_reasoning(&mut body, &opts, model);
            if let Some(session) = session_id {
                body["prompt_cache_key"] = json!(session.as_str());
            }
            let stream_timeout = self.compat.stream_timeout();
            return self
                .with_oauth_retry(|| async {
                    let mut auth = self.current_auth();
                    auth.headers
                        .extend(proxy_request_headers(model, session_id));
                    responses::do_stream(
                        self.compat.client(),
                        model,
                        &body,
                        event_tx,
                        &auth,
                        stream_timeout,
                    )
                    .await
                })
                .await;
        }

        let mut body = self.compat.build_body(model, messages, system, tools);
        opts.thinking
            .apply_reasoning_effort(&mut body, &dialect::GROK, model);
        self.with_oauth_retry(|| async {
            let auth = self.current_auth();
            self.compat
                .do_stream(model, &[], &body, event_tx, &auth)
                .await
        })
        .await
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        if self.is_oauth() {
            let models = self.catalog_models().await?;
            return Ok(models.into_iter().map(|m| m.id).collect());
        }
        self.with_oauth_retry(|| async {
            let auth = self.current_auth();
            self.compat.do_list_models(&auth).await
        })
        .await
    }

    async fn list_models_with_info(&self) -> Result<Vec<ModelInfo>, AgentError> {
        if self.is_oauth() {
            return self.catalog_models().await;
        }
        let ids = self.list_models().await?;
        Ok(ids.into_iter().map(ModelInfo::new).collect())
    }

    async fn fetch_usage(&self) -> Result<Option<ProviderUsage>, AgentError> {
        Ok(None)
    }

    async fn refresh_auth(&self) -> Result<(), AgentError> {
        if self.is_oauth() {
            self.refresh_oauth().await
        } else {
            Ok(())
        }
    }

    async fn reload_auth(&self) -> Result<(), AgentError> {
        let Some(storage) = self.storage.clone() else {
            return Ok(());
        };
        let resolved = auth::resolve(&storage).await?;
        *lock_unpoison(&self.auth) = resolved;
        debug!("reloaded xAI auth from storage");
        Ok(())
    }

    fn adjust_model(&self, model: &mut Model) {
        let Some(cached) = catalog::cached_model(&model.id) else {
            return;
        };
        model.context_window = cached.context_window;
        model.max_output_tokens = Some(cached.max_tokens);
        if cached.pricing.input > 0.0 || cached.pricing.output > 0.0 {
            model.pricing.input = cached.pricing.input;
            model.pricing.output = cached.pricing.output;
            model.pricing.cache_write = cached.pricing.cache_write;
            model.pricing.cache_read = cached.pricing.cache_read;
        }
        model.supports_vision_override = Some(cached.vision);
        model.thinking_override = Some(if cached.reasoning {
            crate::model::ThinkingSupport::Yes
        } else {
            crate::model::ThinkingSupport::No
        });
    }
}

fn bearer_token(auth: &ResolvedAuth) -> Option<String> {
    auth.headers.iter().find_map(|(key, value)| {
        if !key.eq_ignore_ascii_case("authorization") {
            return None;
        }
        value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(ToOwned::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ThinkingConfig;
    use crate::{ModelFamily, ModelPricing, ModelTier};

    fn test_model(thinking: bool) -> Model {
        Model {
            id: "grok-4.6".into(),
            provider: "xai".into(),
            tier: ModelTier::Strong,
            family: ModelFamily::Generic,
            supports_tool_examples_override: None,
            thinking_override: Some(if thinking {
                crate::model::ThinkingSupport::Yes
            } else {
                crate::model::ThinkingSupport::No
            }),
            supports_vision_override: Some(true),
            pricing: ModelPricing::ZERO,
            max_output_tokens: Some(131_072),
            context_window: 500_000,
        }
    }

    #[test]
    fn grok_reasoning_sets_effort_and_include() {
        let model = test_model(true);
        let mut body = json!({"model": "grok-4.6"});
        apply_grok_reasoning(
            &mut body,
            &RequestOptions {
                thinking: ThinkingConfig::Effort(craft_storage::sessions::Effort::High),
                ..RequestOptions::default()
            },
            &model,
        );
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["include"][0], ENCRYPTED_REASONING);
    }

    #[test]
    fn grok_reasoning_skipped_when_model_has_no_thinking() {
        let model = test_model(false);
        let mut body = json!({"model": "grok-4.6"});
        apply_grok_reasoning(&mut body, &RequestOptions::default(), &model);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn bearer_token_extracts_access() {
        let auth = ResolvedAuth {
            base_url: None,
            headers: vec![("authorization".into(), "Bearer tok-123".into())],
        };
        assert_eq!(bearer_token(&auth).as_deref(), Some("tok-123"));
    }
}
