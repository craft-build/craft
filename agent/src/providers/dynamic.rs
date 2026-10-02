//! Type-erased completion models.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rig_core::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
};
use rig_core::streaming::StreamingCompletionResponse;

/// A type-erased completion model: clones cheaply, implements rig-core's
/// `CompletionModel` by forwarding to the provider's concrete model behind an
/// `Arc`. This is the only model type the rest of the crate sees.
#[derive(Clone)]
pub struct DynamicModel {
    label: Option<String>,
    inner: Arc<dyn ErasedModel>,
    kind: Option<super::ProviderKind>,
    settings: Option<crate::config::ModelConfig>,
}

type ModelFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, CompletionError>> + Send + 'a>>;

trait ErasedModel: Send + Sync + 'static {
    fn completion(&self, request: CompletionRequest) -> ModelFuture<'_, CompletionResponse>;
    fn stream(&self, request: CompletionRequest) -> ModelFuture<'_, StreamingCompletionResponse>;
}

impl<M: CompletionModel + Send + Sync + 'static> ErasedModel for M {
    fn completion(&self, request: CompletionRequest) -> ModelFuture<'_, CompletionResponse> {
        Box::pin(async move { CompletionModel::completion(self, request).await })
    }

    fn stream(&self, request: CompletionRequest) -> ModelFuture<'_, StreamingCompletionResponse> {
        Box::pin(async move { CompletionModel::stream(self, request).await })
    }
}

impl DynamicModel {
    pub(crate) fn wrap<M: CompletionModel + Send + Sync + 'static>(
        label: Option<&str>,
        model: M,
    ) -> Self {
        Self {
            label: label.map(str::to_owned),
            inner: Arc::new(model),
            kind: None,
            settings: None,
        }
    }

    pub(crate) fn with_kind(mut self, kind: super::ProviderKind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub(crate) fn with_settings(mut self, settings: Option<&crate::config::ModelConfig>) -> Self {
        self.settings = settings.cloned();
        self
    }

    /// Strip the private preference envelope, then resolve for this handle.
    /// Both completion and streaming use this, so auxiliary calls default off
    /// and fallbacks never inherit another provider's JSON dialect.
    fn prepare(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionRequest, CompletionError> {
        let thinking = crate::thinking::take(&mut request)
            .map_err(|e| CompletionError::RequestError(e.into()))?;
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
            let info = crate::thinking::model_info(kind, id, settings);
            if request.max_tokens.is_none()
                && (kind == super::ProviderKind::Anthropic
                    || (kind == super::ProviderKind::Bedrock && id.contains("claude")))
            {
                request.max_tokens = Some(u64::from(info.max_output.unwrap_or(8192)));
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
                return Err(CompletionError::RequestError(
                    "output cap is too small for the minimum thinking budget and an answer; increase agent.max_tokens".into()));
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

    /// The model/deployment ID this handle was built for, when known.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}

impl CompletionModel for DynamicModel {
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        self.inner.completion(self.prepare(request)?).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        self.inner.stream(self.prepare(request)?).await
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
}
