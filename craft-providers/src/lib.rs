pub(crate) mod error;
pub mod manifest;
pub mod model;
pub mod model_registry;
pub mod provider;
pub(crate) mod providers;
pub mod retry;
pub mod roles;
pub(crate) mod types;

pub use error::AgentError;
pub use model::{
    FastPricing, Model, ModelEntry, ModelError, ModelFamily, ModelInfo, ModelPricing, ModelTier,
    ThinkingSupport, TokenUsage, add_cost, format_tokens,
};
pub use providers::Timeouts;
pub use providers::copilot::auth as copilot_auth;
pub use providers::dynamic;
pub use providers::openai::auth as openai_auth;
pub use providers::opencode::{
    ProviderData, catalog_provider, catalog_providers, catalog_providers_if_available, warm_catalog,
};
pub use providers::xai::auth as xai_auth;
pub use types::{
    ContentBlock, EMPTY_RESPONSE_MARKER, Effort, EffortDialect, IMAGE_OMITTED_NOTE, ImageMediaType,
    ImageSource, Message, MessageKind, ProviderEvent, ProviderUsage, RequestOptions, Role,
    StopReason, StreamResponse, ThinkingConfig, UsageLimit, adapt_images_for_model, dialect,
};
