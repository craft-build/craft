//! Type-erased completion models.

use rig_core::ProviderError;
use rig_core::completion::{CompletionRequest, CompletionResponse};
use rig_core::driver::Transport;
use rig_core::operation::Completion;
use rig_core::streaming::CompletionStream;
use rig_core::wire::Wire;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The crate-local model interface, standing in for rig-core's removed
/// `CompletionModel` trait: one request in, one reply or one event stream
/// out, cheap to clone. Every model the rest of the crate sees is either a
/// [`DynamicModel`] or a test double implementing this trait.
pub trait CompletionSource: Clone + Send + Sync {
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse, ProviderError>> + Send;

    fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ProviderError>;
}

type SourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletionResponse, ProviderError>> + Send + 'a>>;

trait ErasedSource: Send + Sync + 'static {
    fn completion(&self, request: CompletionRequest) -> SourceFuture<'_>;
    fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ProviderError>;
}

impl<M: CompletionSource + 'static> ErasedSource for M {
    fn completion(&self, request: CompletionRequest) -> SourceFuture<'_> {
        Box::pin(self.completion(request))
    }

    fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ProviderError> {
        self.stream(request)
    }
}

/// A type-erased completion model: clones cheaply and forwards to the
/// wrapped [`CompletionSource`] behind an `Arc`. This is the only model
/// type the rest of the crate names.
#[derive(Clone)]
pub struct DynamicModel {
    label: Option<String>,
    inner: Arc<dyn ErasedSource>,
    kind: Option<super::ProviderKind>,
    settings: Option<crate::config::ModelConfig>,
    catalog: Option<&'static str>,
    endpoint: Option<String>,
    context_length: Option<u32>,
}

impl DynamicModel {
    pub(crate) fn wrap(label: Option<&str>, model: impl CompletionSource + 'static) -> Self {
        Self {
            label: label.map(str::to_owned),
            inner: Arc::new(model),
            kind: None,
            settings: None,
            catalog: None,
            endpoint: None,
            context_length: None,
        }
    }

    /// Context window for this handle, when known (config override or the
    /// models.dev catalog); fallback-hop budgeting consults it, `None`
    /// leaves requests untouched.
    pub(crate) fn with_limits(mut self, context_length: Option<u32>) -> Self {
        self.context_length = context_length;
        self
    }

    pub(crate) fn with_kind(mut self, kind: super::ProviderKind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub(crate) fn with_settings(mut self, settings: Option<&crate::config::ModelConfig>) -> Self {
        self.settings = settings.cloned();
        self
    }

    pub(crate) fn with_discovery(mut self, config: &crate::config::ProviderConfig) -> Self {
        self.catalog = Some(crate::thinking::provider_catalog_key(config));
        self.endpoint = config.base_url.clone();
        self
    }

    /// The model/deployment ID this handle was built for, when known.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn context_length(&self) -> Option<u32> {
        self.context_length
    }

    /// Strip the private preference envelope, then resolve for this handle.
    /// Both completion and streaming use this, so auxiliary calls default off
    /// and fallbacks never inherit another provider's JSON dialect.
    pub(crate) fn prepare(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionRequest, ProviderError> {
        let thinking = crate::thinking::take(&mut request).map_err(ProviderError::request)?;
        if let Some(kind) = self.kind {
            let id = request
                .model
                .as_deref()
                .or(self.label.as_deref())
                .unwrap_or("");
            let settings = self.settings.as_ref().filter(|_| {
                request
                    .model
                    .as_deref()
                    .is_none_or(|id| Some(id) == self.label.as_deref())
            });
            let info = crate::thinking::model_info_for_endpoint(
                self.catalog
                    .unwrap_or_else(|| crate::thinking::catalog_key(kind)),
                self.endpoint.as_deref(),
                id,
                settings,
            );
            if request.max_tokens.is_none()
                && (kind == super::ProviderKind::Anthropic
                    || (kind == super::ProviderKind::Bedrock && id.contains("claude")))
            {
                let fallback = info.max_output.unwrap_or(8192);
                if info.max_output.is_none() {
                    // Claude requires an explicit maxTokens; 8192 is the
                    // safe default when no catalog entry names the model's
                    // real ceiling. That default is tight for adaptive
                    // thinking (issue #17's empty first turns), so say so.
                    tracing::warn!(
                        model = id,
                        fallback_max_tokens = fallback,
                        "no catalog output limit for this model; set provider model max_output_tokens \
                         or refresh the models.dev catalog for the model's real ceiling"
                    );
                }
                request.max_tokens = Some(u64::from(fallback));
            }
            let extra = crate::thinking::wire(thinking, kind, id, &info, request.max_tokens);
            let effective_output = match (request.max_tokens, info.max_output.map(u64::from)) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            if matches!(
                kind,
                super::ProviderKind::Anthropic | super::ProviderKind::Bedrock
            ) && extra
                .pointer("/thinking/budget_tokens")
                .and_then(serde_json::Value::as_u64)
                .zip(effective_output)
                .is_some_and(|(thinking, output)| thinking > output / 2)
            {
                return Err(ProviderError::request(
                    "output cap is too small for the minimum thinking budget and an answer; increase agent.max_tokens",
                ));
            }
            if extra.as_object().is_some_and(|o| !o.is_empty()) {
                crate::thinking::merge(
                    request
                        .additional_params
                        .get_or_insert_with(|| serde_json::json!({})),
                    extra,
                );
            }
        }
        if request
            .additional_params
            .as_ref()
            .is_some_and(|v| v.as_object().is_some_and(|o| o.is_empty()))
        {
            request.additional_params = None;
        }
        Ok(request)
    }

    /// Output-token ceiling via the same metadata lookup `prepare` uses
    /// (settings override → discovered → catalog).
    pub fn max_output(&self) -> Option<u32> {
        let kind = self.kind?;
        let id = self.label.as_deref().unwrap_or("");
        crate::thinking::model_info_for_endpoint(
            self.catalog
                .unwrap_or_else(|| crate::thinking::catalog_key(kind)),
            self.endpoint.as_deref(),
            id,
            self.settings.as_ref(),
        )
        .max_output
    }
}

/// Every concrete rig model — provider-wired or script-mocked — already
/// satisfies the source contract; this blanket keeps call sites usable
/// with raw models in tests and equivalence checks.
impl<W, T> CompletionSource for rig_core::driver::Model<W, T>
where
    W: Wire<Op = Completion> + Clone + Send + Sync + 'static,
    T: Transport<W> + Clone + Send + Sync + 'static,
{
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse, ProviderError>> + Send {
        let inner = self.clone().erase();
        async move { inner.call(request).await }
    }

    fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ProviderError> {
        self.clone().erase().stream(request)
    }
}

impl CompletionSource for DynamicModel {
    fn completion(
        &self,
        request: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse, ProviderError>> + Send {
        let prepared = self.prepare(request);
        let inner = self.inner.clone();
        async move { inner.completion(prepared?).await }
    }

    fn stream(&self, request: CompletionRequest) -> Result<CompletionStream, ProviderError> {
        self.inner.stream(self.prepare(request)?)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProviderKind;
    use super::*;
    use crate::thinking::{Effort, ThinkingConfig};

    fn model(kind: ProviderKind, id: &str) -> DynamicModel {
        DynamicModel::wrap(
            Some(id),
            rig_core::test_utils::MockCompletionModel::from_stream_turns(Vec::<
                Vec<rig_core::test_utils::MockStreamEvent>,
            >::new()),
        )
        .with_kind(kind)
    }

    #[test]
    fn fallback_resolves_raw_preference_without_leaking_primary_dialect() {
        let mut request = crate::edge::to_request(
            &[crate::history::Message::user("hello")],
            &[],
            None,
            None,
            Some(8192),
        );
        crate::thinking::attach(&mut request, ThinkingConfig::Effort(Effort::Low));
        let primary = model(ProviderKind::Anthropic, "claude-sonnet-4-5")
            .prepare(request.clone())
            .unwrap();
        let fallback = model(ProviderKind::Openai, "gpt-5")
            .prepare(request)
            .unwrap();
        assert!(primary.additional_params.unwrap().get("thinking").is_some());
        let extra = fallback.additional_params.unwrap();
        assert_eq!(extra, serde_json::json!({"reasoning": {"effort": "low"}}));
    }

    #[test]
    fn auxiliary_calls_disable_default_on_thinking_and_keep_other_params() {
        let mut request = crate::edge::to_request(
            &[crate::history::Message::user("hello")],
            &[],
            None,
            None,
            None,
        );
        request.additional_params = Some(serde_json::json!({"top_p": 0.9}));
        let prepared = model(ProviderKind::Deepseek, "deepseek-reasoner")
            .prepare(request)
            .unwrap();
        assert_eq!(
            prepared.additional_params.unwrap(),
            serde_json::json!({"top_p": 0.9, "thinking": {"type": "disabled"}})
        );
    }

    #[test]
    fn tiny_output_cap_fails_before_the_provider_request() {
        let mut request = crate::edge::to_request(
            &[crate::history::Message::user("hello")],
            &[],
            None,
            None,
            Some(1024),
        );
        crate::thinking::attach(&mut request, ThinkingConfig::Budget(1024));
        assert!(
            model(ProviderKind::Anthropic, "claude-sonnet-4-5")
                .prepare(request)
                .is_err()
        );
    }

    #[test]
    fn bedrock_claude_supplies_output_cap_with_thinking() {
        let mut request = crate::edge::to_request(
            &[crate::history::Message::user("hello")],
            &[],
            None,
            None,
            None,
        );
        crate::thinking::attach(&mut request, ThinkingConfig::Budget(99_999));
        let prepared = model(ProviderKind::Bedrock, "us.anthropic.claude-sonnet-4-5")
            .prepare(request)
            .unwrap();
        assert_eq!(prepared.max_tokens, Some(8192));
        assert_eq!(
            prepared.additional_params.unwrap()["thinking"]["budget_tokens"],
            4096
        );
    }

    /// The manual escape hatch for a missing catalog entry —
    /// `model "id" { max_output_tokens = N }` — must survive to the wire,
    /// not just the catalog: the request cap and the thinking ceiling both
    /// come from the same settings lookup.
    #[test]
    fn bedrock_claude_honors_configured_max_output_tokens() {
        let config = crate::config::Config::parse(
            "provider \"aws\" {\n  kind = \"amazon-bedrock\"\n  \
             model \"us.anthropic.claude-sonnet-4-6\" { max_output_tokens = 24000 }\n}",
        )
        .unwrap();
        let provider = crate::providers::Provider::from_config_with(
            &config.providers["aws"],
            crate::providers::Timeouts::default(),
            &|_| Ok("unused".into()),
        )
        .unwrap();
        let model = provider
            .configured_model(&config.providers["aws"], "us.anthropic.claude-sonnet-4-6")
            .unwrap();
        let mut request = crate::edge::to_request(
            &[crate::history::Message::user("hello")],
            &[],
            None,
            None,
            None,
        );
        crate::thinking::attach(&mut request, ThinkingConfig::Effort(Effort::High));
        let prepared = model.prepare(request).unwrap();
        assert_eq!(
            prepared.max_tokens,
            Some(24_000),
            "the configured cap replaces the 8192 fallback"
        );
    }
}
