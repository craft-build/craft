//! Crate-wide error type and its SNAFU context selectors.
//!
//! Every fallible operation reports an [`Error`] variant; callers add context
//! with `ResultExt::context`/`OptionExt::context` and the binary renders the
//! full chain via `#[snafu::report]` on `main`.

use std::path::PathBuf;

use snafu::Snafu;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    #[snafu(display("cannot determine home directory for agent configuration"))]
    HomeDirectory,

    #[snafu(display("reading {}", path.display()))]
    ReadConfig {
        path: PathBuf,
        source: std::io::Error,
    },

    #[snafu(display("loading {}", path.display()))]
    LoadConfig {
        path: PathBuf,
        #[snafu(source(from(Error, Box::new)))]
        source: Box<Error>,
    },

    #[snafu(display("invalid agent TOML"))]
    InvalidToml { source: toml::de::Error },

    #[snafu(display("provider {name:?}"))]
    InvalidProvider {
        name: String,
        #[snafu(source(from(Error, Box::new)))]
        source: Box<Error>,
    },

    #[snafu(display("base_url must be an absolute HTTP(S) URL"))]
    InvalidBaseUrl { source: url::ParseError },

    #[snafu(display("{reason}"))]
    Invalid { reason: String },

    #[snafu(display("credential environment variable {name:?} is not set or not Unicode"))]
    CredentialMissing {
        name: String,
        source: std::env::VarError,
    },

    #[snafu(display("credential environment variable {name:?} is empty"))]
    CredentialEmpty { name: String },

    #[snafu(display("creating {kind} provider"))]
    CreateProvider {
        kind: &'static str,
        #[snafu(source(from(Error, Box::new)))]
        source: Box<Error>,
    },

    #[snafu(display("selecting model {model:?} on {kind} provider"))]
    SelectModel {
        model: String,
        kind: &'static str,
        #[snafu(source(from(Error, Box::new)))]
        source: Box<Error>,
    },

    #[snafu(display("provider does not support completion models"))]
    NoCompletion,

    #[snafu(display("azure requires base_url or AZURE_ENDPOINT"))]
    AzureEndpointMissing,

    #[snafu(display("azure requires api_version or AZURE_API_VERSION"))]
    AzureApiVersionMissing,

    /// Rig provider client setup errors, kept behind an opaque boxed source so
    /// one variant covers every provider-specific error type.
    #[snafu(context(false))]
    #[snafu(display("{source}"))]
    ProviderClient {
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[snafu(display("summarization request failed"))]
    Summarize {
        source: rig::completion::CompletionError,
    },

    #[snafu(display("ACP connection failed"))]
    #[snafu(visibility(pub))] // used by the binary's `#[snafu::report]` main
    AcpConnection {
        source: agent_client_protocol::Error,
    },
}

/// Box an arbitrary Rig provider error into [`Error::ProviderClient`].
pub(crate) fn client_error(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::ProviderClient {
        source: Box::new(error),
    }
}
