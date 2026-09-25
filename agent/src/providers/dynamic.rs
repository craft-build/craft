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
        }
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
        self.inner.completion(request).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        self.inner.stream(request).await
    }
}
