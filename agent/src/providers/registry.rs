//! Registry macros: the `providers!` table is the single source of truth for
//! config kinds, concrete clients, construction, discovery, and completion
//! support. Rig expresses provider capabilities at compile time, so
//! unsupported listers must not be called.

use rig_core::client::{CompletionClient, ModelListingClient};
use rig_core::model::ModelList;
use rig_core::providers::*;
use serde::Deserialize;
use snafu::ResultExt;

use super::catalog::{CatalogModel, merge_catalog};
use super::dynamic::DynamicModel;
use super::openai_compat::list_openai_compatible_models;
use super::{
    Timeouts, base_url_env, build_azure, build_chatgpt, build_copilot, build_llamafile,
    build_ollama, credential, timeout_client,
};
use crate::config::ProviderConfig;
use crate::error::{
    CreateProviderSnafu, InvalidSnafu, NoCompletionSnafu, Result, SelectModelSnafu,
};

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
        DynamicModel::wrap(Some($model), $client.completion_model($model))
    };
    ($client:expr, $model:expr, no) => {{
        let _ = ($client, $model);
        NoCompletionSnafu.fail()?
    }};
}

macro_rules! build_client {
    ($config:expr, $client:ty, $env:literal, $timeouts:expr, $credential:expr) => {{
        let config = $config;
        let key = $credential(config.api_key_env.as_deref().unwrap_or($env))?;
        let mut builder = <$client>::builder()
            .api_key(key)
            .http_client(timeout_client($timeouts)?);
        let base_url = config.base_url.as_deref().map(str::to_owned).or_else(|| {
            std::env::var(base_url_env($env))
                .ok()
                .filter(|s| !s.is_empty())
        });
        if let Some(base_url) = base_url {
            builder = builder.base_url(base_url);
        }
        builder.build().map_err(crate::error::client_error)?
    }};
    ($config:expr, $client:ty, $factory:ident, $timeouts:expr, $credential:expr) => {
        $factory($config, $timeouts, $credential)?
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
    ($($variant:ident, $name:literal, $client:ty, $auth:tt, $listing:ident, $completion:ident;)+) => {
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
                            build_client!(config, $client, $auth, timeouts, credential)
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
                    Ok(match self {
                        $(Self::$variant(client) => completion_model!(client, model, $completion),)+
                    })
                })().with_context(|_| SelectModelSnafu {
                    model: model.to_string(),
                    kind: self.kind().as_str(),
                })
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
                    } else {
                        match self {
                            $(Self::$variant(client) => list_models!(client, $listing),)+
                        }
                    }
                } else {
                    ModelList::new(vec![])
                };
                Ok(merge_catalog(config, discovered))
            }
        }
    };
}

// Variant, config kind, client, auth, model listing, text completion.
providers! {
    Anthropic, "anthropic", anthropic::Client, "ANTHROPIC_API_KEY", yes, yes;
    Azure, "azure", azure::Client, build_azure, no, yes;
    Chatgpt, "chatgpt", chatgpt::Client, build_chatgpt, no, yes;
    Cohere, "cohere", cohere::Client, "COHERE_API_KEY", no, yes;
    Copilot, "copilot", copilot::Client, build_copilot, yes, yes;
    Deepseek, "deepseek", deepseek::Client, "DEEPSEEK_API_KEY", yes, yes;
    Doubleword, "doubleword", doubleword::Client, "DOUBLEWORD_API_KEY", no, yes;
    Gemini, "gemini", gemini::Client, "GEMINI_API_KEY", yes, yes;
    Groq, "groq", groq::Client, "GROQ_API_KEY", yes, yes;
    Huggingface, "huggingface", huggingface::Client, "HUGGINGFACE_API_KEY", no, yes;
    Hyperbolic, "hyperbolic", hyperbolic::Client, "HYPERBOLIC_API_KEY", no, yes;
    Llamafile, "llamafile", llamafile::Client, build_llamafile, no, yes;
    Minimax, "minimax", minimax::Client, "MINIMAX_API_KEY", yes, yes;
    Mira, "mira", mira::Client, "MIRA_API_KEY", yes, yes;
    Mistral, "mistral", mistral::Client, "MISTRAL_API_KEY", yes, yes;
    Moonshot, "moonshot", moonshot::Client, "MOONSHOT_API_KEY", yes, yes;
    Ollama, "ollama", ollama::Client, build_ollama, yes, yes;
    Openai, "openai", openai::Client, "OPENAI_API_KEY", yes, yes;
    OpenaiCompatible, "openai-compatible", openai::CompletionsClient, "OPENAI_API_KEY", yes, yes;
    Openrouter, "openrouter", openrouter::Client, "OPENROUTER_API_KEY", yes, yes;
    Perplexity, "perplexity", perplexity::Client, "PERPLEXITY_API_KEY", no, yes;
    Together, "together", together::Client, "TOGETHER_API_KEY", no, yes;
    Venice, "venice", venice::Client, "VENICE_API_KEY", yes, yes;
    Voyageai, "voyageai", voyageai::Client, "VOYAGEAI_API_KEY", no, no;
    Xai, "xai", xai::Client, "XAI_API_KEY", no, yes;
    Xiaomimimo, "xiaomimimo", xiaomimimo::Client, "XIAOMI_MIMO_API_KEY", yes, yes;
    Zai, "zai", zai::Client, "ZAI_API_KEY", no, yes;
}
