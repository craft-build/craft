//! Registry macros: the `providers!` table is the single source of truth for
//! config kinds, concrete clients, construction, discovery, and completion
//! support. 0.44 named OpenAI-dialect vendors share the `OpenAI` client
//! type; the table still keeps one variant per kind so per-kind wires,
//! listings, and constructors stay addressable.

use serde::Deserialize;
use snafu::ResultExt;

use super::catalog::{CatalogModel, merge_catalog};
use super::dynamic::DynamicModel;
use super::openai_compat::list_openai_compatible_models;
use super::{
    Timeouts, build_anthropic, build_azure, build_bedrock, build_chatgpt, build_cohere,
    build_copilot, build_deepseek, build_doubleword, build_gemini, build_groq, build_huggingface,
    build_hyperbolic, build_llamafile, build_minimax, build_mira, build_mistral, build_moonshot,
    build_ollama, build_openai, build_openai_compatible, build_openrouter, build_perplexity,
    build_together, build_venice, build_voyageai, build_xai, build_xiaomimimo, build_zai,
    credential,
};
use crate::config::ProviderConfig;
use crate::error::{
    CreateProviderSnafu, InvalidSnafu, NoCompletionSnafu, Result, SelectModelSnafu,
};
use rig_core::model::ModelList;
use rig_core::providers::{anthropic, cohere, copilot, gemini, ollama, openai, voyageai};

macro_rules! list_models {
    ($client:expr, yes) => {
        $client
            .list_models()
            .await
            .map_err(crate::error::client_error)?
    };
    ($client:expr, no) => {{
        let _ = $client;
        ModelList::new(vec![])
    }};
}

macro_rules! completion_model {
    ($client:expr, $model:expr, yes) => {
        DynamicModel::wrap(Some($model), $client.completion($model))
    };
    ($client:expr, $model:expr, chat) => {
        DynamicModel::wrap(Some($model), $client.chat($model))
    };
    ($client:expr, $model:expr, no) => {{
        let _ = ($client, $model);
        NoCompletionSnafu.fail()?
    }};
}

macro_rules! build_client {
    ($config:expr, $build:expr, $timeouts:expr, $credential:expr) => {
        $build($config, $timeouts, $credential)?
    };
}

// Expands to the default credential env var for a providers! table row:
// literal env names surface, factory-auth kinds (azure, chatgpt, …) have
// none at the kind level.
macro_rules! default_env {
    ($env:literal) => {
        Some($env)
    };
    ($factory:ident) => {
        None
    };
}

macro_rules! providers {
    ($($variant:ident, $name:literal, $client:ty, $auth:tt, $build:ident, $listing:ident, $completion:ident;)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
        pub enum ProviderKind {
            $(#[serde(rename = $name)] $variant,)+
        }

        /// Native clients, not a lowest-common-denominator protocol wrapper.
        pub enum Provider {
            $($variant($client),)+
        }

        impl ProviderKind {
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name,)+ }
            }

            /// Default credential env var for the kind, when the table row
            /// names one (`api_key_env` in the config still wins).
            pub fn api_key_env_default(self) -> Option<&'static str> {
                match self { $(Self::$variant => default_env!($auth),)+ }
            }
        }

        impl Provider {
            pub fn from_config(config: &ProviderConfig) -> Result<Self> {
                Self::from_config_with_timeouts(config, Timeouts::default())
            }

            /// Build with an explicit timeout policy (the shared HTTP client
            /// every configured provider runs on).
            pub fn from_config_with_timeouts(
                config: &ProviderConfig,
                timeouts: Timeouts,
            ) -> Result<Self> {
                Self::from_config_with(config, timeouts, &credential)
            }

            // Inject credential lookup for tests without mutating process-wide
            // environment variables in a multithreaded async test runner.
            pub(crate) fn from_config_with(
                config: &ProviderConfig,
                timeouts: Timeouts,
                credential: &dyn Fn(&str) -> Result<String>,
            ) -> Result<Self> {
                config.validate()?;
                (|| -> Result<Self> {
                    Ok(match config.kind {
                        $(ProviderKind::$variant => Self::$variant(
                            build_client!(config, $build, timeouts, credential)
                        ),)+
                    })
                })().with_context(|_| CreateProviderSnafu {
                    kind: config.kind.as_str(),
                })
            }

            pub fn kind(&self) -> ProviderKind {
                match self { $(Self::$variant(_) => ProviderKind::$variant,)+ }
            }

            /// Select an exact provider model/deployment ID without discovery.
            /// The concrete model is erased once behind [`DynamicModel`],
            /// retaining its native protocol.
            pub fn completion_model(&self, model: &str) -> Result<DynamicModel> {
                if model.trim().is_empty() {
                    return InvalidSnafu {
                        reason: "model ID must not be empty",
                    }
                    .fail();
                }
                (|| -> Result<DynamicModel> {
                    let handle = match self {
                        $(Self::$variant(client) => completion_model!(client, model, $completion),)+
                    };
                    Ok(handle.with_kind(self.kind()))
                })().with_context(|_| SelectModelSnafu {
                    model: model.to_string(),
                    kind: self.kind().as_str(),
                })
            }

            /// Build with explicit per-model overrides, preserving protocol kind.
            pub fn configured_model(&self, config: &ProviderConfig, model: &str) -> Result<DynamicModel> {
                let settings = config.models.get(model);
                let context_length = settings
                    .and_then(|s| s.context_length)
                    .or_else(|| {
                        crate::models_dev::metadata_for(
                            crate::thinking::provider_catalog_key(config),
                            model,
                        )
                        .map(|meta| meta.context)
                    });
                Ok(self.completion_model(model)?
                    .with_settings(settings)
                    .with_limits(context_length)
                    .with_discovery(config))
            }

            /// Providers without listing support return the configured models.
            /// Discovery errors propagate instead of masquerading as an empty
            /// catalog. Set `discover_models = false` for manual-only catalogs.
            pub async fn models(&self, config: &ProviderConfig) -> Result<Vec<CatalogModel>> {
                self.models_with(config, &credential).await
            }

            // Injectable credential lookup keeps environment access out of
            // multithreaded async tests.
            pub(crate) async fn models_with(
                &self,
                config: &ProviderConfig,
                credential: &(dyn Fn(&str) -> Result<String> + Send + Sync),
            ) -> Result<Vec<CatalogModel>> {
                config.validate()?;
                if config.kind != self.kind() {
                    return InvalidSnafu {
                        reason: "model config kind does not match the provider",
                    }
                    .fail();
                }
                let discovered = if config.discover_models {
                    if matches!(self, Self::OpenaiCompatible(_)) && config.base_url.is_some() {
                        list_openai_compatible_models(config, credential).await?
                    } else if matches!(self, Self::Bedrock(_)) {
                        // The rig-bedrock data-plane client has no listing;
                        // inference profiles come from the control-plane API.
                        super::bedrock::list_models(config).await?
                    } else {
                        match self {
                            $(Self::$variant(client) => list_models!(client, $listing),)+
                        }
                    }
                } else {
                    ModelList::new(vec![])
                };
                let mut catalog = merge_catalog(config, discovered);
                // The models.dev side-table (warmed by the caller before
                // discovery) fills whatever the listing and config left
                // `None`, so context windows and output limits — compaction
                // thresholds and metadata consumers — hold across surfaces.
                // Configured values win; unknown models keep what discovery
                // gave them.
                crate::models_dev::enrich_catalog(config.kind.as_str(), &mut catalog);
                Ok(catalog)
            }
        }
    };
}

// Variant, config kind, client, default auth env, factory, model listing,
// text completion. The `chat` route pins OpenAI-compatible endpoints to
// Chat Completions; every other vendor keeps its own native default.
providers! {
    Anthropic, "anthropic", anthropic::Anthropic, "ANTHROPIC_API_KEY", build_anthropic, yes, yes;
    Azure, "azure", openai::OpenAI, build_azure, build_azure, no, yes;
    Bedrock, "amazon-bedrock", rig_bedrock::client::BedrockRuntime, build_bedrock, build_bedrock, no, yes;
    Chatgpt, "chatgpt", openai::OpenAI, build_chatgpt, build_chatgpt, no, yes;
    Cohere, "cohere", cohere::Cohere, "COHERE_API_KEY", build_cohere, no, yes;
    Copilot, "copilot", copilot::Copilot, build_copilot, build_copilot, yes, yes;
    Deepseek, "deepseek", openai::OpenAI, "DEEPSEEK_API_KEY", build_deepseek, yes, yes;
    Doubleword, "doubleword", openai::OpenAI, "DOUBLEWORD_API_KEY", build_doubleword, no, yes;
    Gemini, "gemini", gemini::Gemini, "GEMINI_API_KEY", build_gemini, yes, yes;
    Groq, "groq", openai::OpenAI, "GROQ_API_KEY", build_groq, yes, yes;
    Huggingface, "huggingface", openai::OpenAI, "HUGGINGFACE_API_KEY", build_huggingface, no, yes;
    Hyperbolic, "hyperbolic", openai::OpenAI, "HYPERBOLIC_API_KEY", build_hyperbolic, no, yes;
    Llamafile, "llamafile", openai::OpenAI, build_llamafile, build_llamafile, no, yes;
    Minimax, "minimax", openai::OpenAI, "MINIMAX_API_KEY", build_minimax, yes, yes;
    Mira, "mira", openai::OpenAI, "MIRA_API_KEY", build_mira, yes, yes;
    Mistral, "mistral", openai::OpenAI, "MISTRAL_API_KEY", build_mistral, yes, yes;
    Moonshot, "moonshot", openai::OpenAI, "MOONSHOT_API_KEY", build_moonshot, yes, yes;
    Ollama, "ollama", ollama::Ollama, build_ollama, build_ollama, yes, yes;
    Openai, "openai", openai::OpenAI, "OPENAI_API_KEY", build_openai, yes, yes;
    OpenaiCompatible, "openai-compatible", openai::OpenAI, "OPENAI_API_KEY", build_openai_compatible, yes, chat;
    Openrouter, "openrouter", openai::OpenAI, "OPENROUTER_API_KEY", build_openrouter, yes, yes;
    Perplexity, "perplexity", openai::OpenAI, "PERPLEXITY_API_KEY", build_perplexity, no, yes;
    Together, "together", openai::OpenAI, "TOGETHER_API_KEY", build_together, no, yes;
    Venice, "venice", openai::OpenAI, "VENICE_API_KEY", build_venice, yes, yes;
    Voyageai, "voyageai", voyageai::VoyageAi, "VOYAGEAI_API_KEY", build_voyageai, no, no;
    Xai, "xai", openai::OpenAI, "XAI_API_KEY", build_xai, no, yes;
    Xiaomimimo, "xiaomimimo", openai::OpenAI, "XIAOMI_MIMO_API_KEY", build_xiaomimimo, yes, yes;
    Zai, "zai", openai::OpenAI, "ZAI_API_KEY", build_zai, no, yes;
}
