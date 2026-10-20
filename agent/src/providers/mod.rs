//! Config-driven native Rig clients. Keep concrete clients available so future
//! inference, embeddings, tools, and streaming can use their full capabilities.
//! [`DynamicModel`] erases the concrete model type behind rig-core's
//! `DynModel` so the rest of the crate never names a provider type.

mod bedrock;
mod catalog;
mod dynamic;
mod openai_compat;
mod reauth;
mod registry;
pub mod usage_fetch;

pub use catalog::{CatalogEntry, CatalogModel};
pub use dynamic::{CompletionSource, DynamicModel};
pub use reauth::reauth_hook;
pub use registry::{Provider, ProviderKind};
pub use usage_fetch::{ModelUsageRow, ProviderUsage, UsageLimit};

use std::future::Future;
use std::time::Duration;

use rig_core::providers::{anthropic, chatgpt, cohere, copilot, gemini, ollama, openai, voyageai};
use snafu::{OptionExt, ResultExt};

use crate::config::ProviderConfig;
use crate::error::{
    AzureApiVersionMissingSnafu, AzureEndpointMissingSnafu, CredentialEmptySnafu,
    CredentialMissingSnafu, Result,
};

pub(crate) fn credential(name: &str) -> Result<String> {
    let value = std::env::var(name).context(CredentialMissingSnafu {
        name: name.to_string(),
    })?;
    if value.trim().is_empty() {
        return CredentialEmptySnafu {
            name: name.to_string(),
        }
        .fail();
    }
    Ok(value)
}

/// Shared timeout policy for every provider HTTP client, ported from the
/// reference `Timeouts` (craft-providers/src/providers/mod.rs): a quick
/// connect failure, a bounded overall stream budget, and a low-speed floor
/// for providers that support per-read timeouts (kept for parity with the
/// reference, which wires it per-provider rather than on the shared client).
/// Timeouts that fire surface as transport errors the retry machine
/// (`run/retry.rs`) classifies as `ErrorKind::Timeout` and retries patiently
/// up to `MAX_TIMEOUT_RETRIES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub connect: Duration,
    pub stream: Duration,
    pub low_speed: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            stream: Duration::from_secs(300),
            low_speed: Duration::from_secs(30),
        }
    }
}

/// The shared HTTP backend every configured provider is built on, wrapping
/// the same connect-timeout plus overall stream timeout policy in
/// rig-reqwest's transport newtype.
pub(crate) fn timeout_client(timeouts: Timeouts) -> Result<rig_reqwest::ReqwestClient> {
    let client = reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.stream)
        .build()
        .map_err(crate::error::client_error)?;
    Ok(rig_reqwest::ReqwestClient::from(client))
}

/// The base-URL env var convention `from_env` reads for a provider whose
/// key env is `api_env` (e.g. `ANTHROPIC_API_KEY` → `ANTHROPIC_BASE_URL`).
fn base_url_env(api_env: &str) -> String {
    api_env.replace("_API_KEY", "_BASE_URL")
}

/// The base-URL override for a provider: explicit config first, then the
/// `<NAME>_BASE_URL` environment convention `from_env` reads.
fn effective_base_url(config: &ProviderConfig, api_env: &str) -> Option<String> {
    config.base_url.as_deref().map(str::to_owned).or_else(|| {
        std::env::var(base_url_env(api_env))
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// Resolve the API key for a plain vendor row: the config's named
/// credential, or the vendor's default environment variable.
fn api_key(
    config: &ProviderConfig,
    credential: &dyn Fn(&str) -> Result<String>,
    env_default: &'static str,
) -> Result<String> {
    credential(config.api_key_env.as_deref().unwrap_or(env_default))
}

/// Every OpenAI-dialect vendor shares one construction: the dialect's config
/// with the resolved key and optional base-URL override, connected to the
/// shared timeout transport.
fn openai_dialect(
    dialect: &openai::wire::Dialect,
    key: String,
    base_url: Option<String>,
    timeouts: Timeouts,
) -> Result<openai::OpenAI> {
    let mut config = openai::OpenAIConfig::with_key(dialect, key);
    if let Some(base_url) = base_url {
        config = config.with_base_url(base_url);
    }
    Ok(config.connect(timeout_client(timeouts)?))
}

macro_rules! openai_vendor_factory {
    ($(#[$doc:meta])* $name:ident, $client:ident, $env:literal, $dialect:path) => {
        $(#[$doc])*
        pub(crate) fn $name(
            config: &ProviderConfig,
            timeouts: Timeouts,
            credential: &dyn Fn(&str) -> Result<String>,
        ) -> Result<openai::$client> {
            openai_dialect(
                &$dialect,
                api_key(config, credential, $env)?,
                effective_base_url(config, $env),
                timeouts,
            )
        }
    };
}

openai_vendor_factory!(
    build_deepseek,
    OpenAI,
    "DEEPSEEK_API_KEY",
    openai::wire::DEEPSEEK
);
openai_vendor_factory!(
    build_doubleword,
    OpenAI,
    "DOUBLEWORD_API_KEY",
    openai::wire::DOUBLEWORD
);
openai_vendor_factory!(build_groq, OpenAI, "GROQ_API_KEY", openai::wire::GROQ);
openai_vendor_factory!(
    build_huggingface,
    OpenAI,
    "HUGGINGFACE_API_KEY",
    openai::wire::HUGGINGFACE
);
openai_vendor_factory!(
    build_hyperbolic,
    OpenAI,
    "HYPERBOLIC_API_KEY",
    openai::wire::HYPERBOLIC
);
openai_vendor_factory!(
    build_minimax,
    OpenAI,
    "MINIMAX_API_KEY",
    openai::wire::MINIMAX
);
openai_vendor_factory!(build_mira, OpenAI, "MIRA_API_KEY", openai::wire::MIRA);
openai_vendor_factory!(
    build_mistral,
    OpenAI,
    "MISTRAL_API_KEY",
    openai::wire::MISTRAL
);
openai_vendor_factory!(
    build_moonshot,
    OpenAI,
    "MOONSHOT_API_KEY",
    openai::wire::MOONSHOT
);
openai_vendor_factory!(
    build_openrouter,
    OpenAI,
    "OPENROUTER_API_KEY",
    openai::wire::OPENROUTER
);
openai_vendor_factory!(
    build_perplexity,
    OpenAI,
    "PERPLEXITY_API_KEY",
    openai::wire::PERPLEXITY
);
openai_vendor_factory!(
    build_together,
    OpenAI,
    "TOGETHER_API_KEY",
    openai::wire::TOGETHER
);
openai_vendor_factory!(build_venice, OpenAI, "VENICE_API_KEY", openai::wire::VENICE);
openai_vendor_factory!(
    build_xai,
    OpenAI,
    "XAI_API_KEY",
    rig_core::providers::xai::DIALECT
);
openai_vendor_factory!(
    build_xiaomimimo,
    OpenAI,
    "XIAOMI_MIMO_API_KEY",
    openai::wire::XIAOMIMIMO
);
openai_vendor_factory!(build_zai, OpenAI, "ZAI_API_KEY", openai::wire::ZAI);

pub(crate) fn build_openai(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<openai::OpenAI> {
    openai_dialect(
        &openai::wire::OPENAI,
        api_key(config, credential, "OPENAI_API_KEY")?,
        effective_base_url(config, "OPENAI_API_KEY"),
        timeouts,
    )
}

/// Third-party OpenAI-compatible endpoints: the OpenAI wire vocabulary on
/// the Chat Completions route, pointed at the configured base URL.
pub(crate) fn build_openai_compatible(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<openai::OpenAI> {
    let key = match &config.api_key_env {
        Some(name) => credential(name)?,
        None => std::env::var("OPENAI_API_KEY").unwrap_or_default(),
    };
    openai_dialect(
        &openai::wire::OPENAI,
        key,
        effective_base_url(config, "OPENAI_API_KEY"),
        timeouts,
    )
}

/// Legacy llamafile deployments continue to work over the llama.cpp
/// OpenAI-compatible wire (the llamafile module was folded into llamacpp).
pub(crate) fn build_llamafile(
    config: &ProviderConfig,
    timeouts: Timeouts,
    _: &dyn Fn(&str) -> Result<String>,
) -> Result<openai::OpenAI> {
    openai_dialect(
        &openai::wire::LLAMACPP,
        String::new(),
        config
            .base_url
            .clone()
            .or_else(|| std::env::var("LLAMAFILE_API_BASE_URL").ok()),
        timeouts,
    )
}

pub(crate) fn build_anthropic(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<anthropic::Anthropic> {
    let mut provider =
        anthropic::AnthropicConfig::new(api_key(config, credential, "ANTHROPIC_API_KEY")?);
    if let Some(base_url) = effective_base_url(config, "ANTHROPIC_API_KEY") {
        provider = provider.with_base_url(base_url);
    }
    Ok(provider.connect(timeout_client(timeouts)?))
}

pub(crate) fn build_cohere(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<cohere::Cohere> {
    let mut provider = cohere::CohereConfig::new(api_key(config, credential, "COHERE_API_KEY")?);
    if let Some(base_url) = effective_base_url(config, "COHERE_API_KEY") {
        provider = provider.with_base_url(base_url);
    }
    Ok(provider.connect(timeout_client(timeouts)?))
}

pub(crate) fn build_gemini(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<gemini::Gemini> {
    let mut provider = gemini::GeminiConfig::new(api_key(config, credential, "GEMINI_API_KEY")?);
    if let Some(base_url) = effective_base_url(config, "GEMINI_API_KEY") {
        provider = provider.with_base_url(base_url);
    }
    Ok(provider.connect(timeout_client(timeouts)?))
}

/// Embedding and rerank only: no completion models come from Voyage AI.
pub(crate) fn build_voyageai(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<voyageai::VoyageAi> {
    let mut provider =
        voyageai::VoyageAiConfig::new(api_key(config, credential, "VOYAGEAI_API_KEY")?);
    if let Some(base_url) = effective_base_url(config, "VOYAGEAI_API_KEY") {
        provider = provider.with_base_url(base_url);
    }
    Ok(provider.connect(timeout_client(timeouts)?))
}

pub(crate) fn build_ollama(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<ollama::Ollama> {
    let key = match &config.api_key_env {
        Some(name) => credential(name)?,
        None => std::env::var("OLLAMA_API_KEY").unwrap_or_default(),
    };
    let mut provider = ollama::OllamaConfig::new();
    if let Some(base_url) = config
        .base_url
        .clone()
        .or_else(|| std::env::var("OLLAMA_API_BASE_URL").ok())
    {
        provider = provider.with_base_url(base_url);
    }
    if !key.is_empty() {
        provider = provider.with_api_key(key);
    }
    Ok(provider.connect(timeout_client(timeouts)?))
}

pub(crate) fn build_azure(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<openai::OpenAI> {
    // Key auth when the config names one or AZURE_API_KEY is set; otherwise
    // the deployment takes an Entra ID bearer token (AZURE_TOKEN).
    let (secret, token_auth) = if let Some(name) = &config.api_key_env {
        (credential(name)?, false)
    } else if let Ok(key) = std::env::var("AZURE_API_KEY") {
        (key, false)
    } else {
        (credential("AZURE_TOKEN")?, true)
    };
    let endpoint = config
        .base_url
        .clone()
        .or_else(|| std::env::var("AZURE_ENDPOINT").ok())
        .context(AzureEndpointMissingSnafu)?;
    let version = config
        .api_version
        .clone()
        .or_else(|| std::env::var("AZURE_API_VERSION").ok())
        .context(AzureApiVersionMissingSnafu)?;
    let mut provider =
        openai::OpenAIConfig::with_key(&openai::wire::AZURE, secret).with_base_url(endpoint);
    if token_auth {
        provider.auth = openai::wire::Auth::Bearer;
    }
    Ok(provider
        .with_api_version(version)
        .connect(timeout_client(timeouts)?))
}

/// Bedrock has no craft-side credential or base URL: the AWS SDK's default
/// chain (environment credentials, `~/.aws` profiles including SSO and
/// assume-role, web identity, container and IMDS roles) supplies both
/// credentials and region through one craft-owned, cached SDK config —
/// with a fast-failing IMDS probe so non-EC2 machines neither stall nor
/// wait through multi-second SDK defaults (see [`crate::providers::bedrock`]).
/// `region`/`profile` in the provider config override the region and name
/// the shared-config profile (e.g. an assume-role or SSO account that is
/// not the default profile); `AWS_PROFILE` flows through the chain
/// natively. A missing region fails here with the fixes spelled out
/// instead of an opaque "dispatch failure" on the first request. The AWS
/// SDK owns its transport, so craft's `Timeouts` map onto the SDK's
/// connect/stall timeouts rather than the shared reqwest policy.
pub(crate) fn build_bedrock(
    config: &ProviderConfig,
    timeouts: Timeouts,
    _: &dyn Fn(&str) -> Result<String>,
) -> Result<rig_bedrock::client::BedrockRuntime> {
    let sdk_config = bedrock::sdk_config_blocking(config, timeouts)?;
    Ok(rig_bedrock::client::BedrockRuntime::from(
        aws_sdk_bedrockruntime::Client::new(&sdk_config),
    ))
}

/// The OAuth and GitHub-token device flows are async; providers are still
/// built synchronously. On the multi-threaded app runtime the flow parks the
/// calling thread onto the reactor; outside any runtime a private
/// current-thread runtime runs it to completion. Test credential injection
/// never takes these branches.
fn block_on_auth<F: Future>(future: F) -> Result<F::Output> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Ok(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(crate::error::client_error)?
            .block_on(future)),
    }
}

pub(crate) fn build_chatgpt(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<openai::OpenAI> {
    let mut provider = if let Some(name) = &config.api_key_env {
        openai::OpenAIConfig::with_key(&chatgpt::DIALECT, credential(name)?)
    } else if let Ok(access_token) = std::env::var("CHATGPT_ACCESS_TOKEN") {
        let mut provider = openai::OpenAIConfig::with_key(&chatgpt::DIALECT, access_token);
        if let Ok(account_id) = std::env::var("CHATGPT_ACCOUNT_ID") {
            provider.account_id = Some(account_id);
        }
        provider
    } else {
        let authenticator = chatgpt::auth::Authenticator::new(
            chatgpt::auth::AuthSource::OAuth,
            None,
            chatgpt::auth::DeviceCodeHandler::default(),
            true,
        );
        let mut provider = openai::OpenAIConfig::with_key(&chatgpt::DIALECT, String::new());
        if let Some(base_url) = &config.base_url {
            provider = provider.with_base_url(base_url.clone());
        }
        let client = provider.connect(timeout_client(timeouts)?);
        return block_on_auth(async { client.authenticate(&authenticator).await })?
            .map_err(crate::error::client_error);
    };
    if let Some(account_id) = &config.account_id {
        provider.account_id = Some(account_id.clone());
    }
    if let Some(base_url) = &config.base_url {
        provider = provider.with_base_url(base_url.clone());
    }
    Ok(provider.connect(timeout_client(timeouts)?))
}

pub(crate) fn build_copilot(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<copilot::Copilot> {
    use copilot::auth::{AuthSource, Authenticator, DeviceCodeHandler};

    // Exchanged session keys are used as-is.
    if let Some(key) = config
        .api_key_env
        .as_deref()
        .map(credential)
        .transpose()?
        .or_else(|| first_env(&["GITHUB_COPILOT_API_KEY", "COPILOT_API_KEY"]))
    {
        let mut provider = copilot::CopilotConfig::new(key);
        if let Some(base_url) = &config.base_url {
            provider = provider.with_base_url(base_url.clone());
        }
        return Ok(provider.connect(timeout_client(timeouts)?));
    }

    // GitHub tokens need the exchange; OAuth runs the cached device flow.
    let source = if let Some(token) = first_env(&["COPILOT_GITHUB_ACCESS_TOKEN", "GITHUB_TOKEN"]) {
        AuthSource::GitHubAccessToken(token)
    } else {
        AuthSource::OAuth
    };
    let (access_token_file, api_key_file) = copilot::auth::default_token_dir()
        .map(|dir| {
            (
                Some(dir.join("access-token")),
                Some(dir.join("api-key.json")),
            )
        })
        .unwrap_or((None, None));
    let authenticator = Authenticator::new(
        source,
        access_token_file,
        api_key_file,
        DeviceCodeHandler::default(),
        true,
    );
    let mut provider = copilot::CopilotConfig::new("");
    if let Some(base_url) = &config.base_url {
        provider = provider.with_base_url(base_url.clone());
    }
    let client = provider.connect(timeout_client(timeouts)?);
    block_on_auth(async { client.authenticate(&authenticator).await })?
        .map_err(crate::error::client_error)
}

fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|s| !s.trim().is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use rig_core::model::{ModelInfo as Model, ModelList};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    fn config(kind: ProviderKind, base_url: &str) -> ProviderConfig {
        // Bedrock takes neither credentials nor a base URL: the AWS default
        // credential chain owns both. It does need a resolvable region —
        // the fixture pins one so provider construction stays offline
        // (no IMDS probing, no region guard).
        let auth = if matches!(kind, ProviderKind::Llamafile | ProviderKind::Bedrock) {
            ""
        } else {
            "api_key_env = \"CRAFT_TEST_KEY\"\n"
        };
        let azure = if kind == ProviderKind::Azure {
            "api_version = \"2024-10-21\"\n"
        } else {
            ""
        };
        let base = if kind == ProviderKind::Bedrock {
            String::new()
        } else {
            format!("  base_url = \"{base_url}\"\n")
        };
        let region = if kind == ProviderKind::Bedrock {
            "  region = \"us-east-1\"\n"
        } else {
            ""
        };
        Config::parse(&format!(
            "provider \"test\" {{\n  kind = \"{}\"\n{region}{base}{auth}{azure}  model \"manual\" {{ name = \"Manual model\" }}\n}}",
            kind.as_str(),
        ))
        .unwrap()
        .providers
        .remove("test")
        .unwrap()
    }

    fn build(config: &ProviderConfig) -> Provider {
        Provider::from_config_with(config, Timeouts::default(), &|name| {
            assert_eq!(name, "CRAFT_TEST_KEY");
            Ok("test-key".into())
        })
        .unwrap()
    }

    #[test]
    fn timeout_defaults_match_the_reference_policy() {
        let timeouts = Timeouts::default();
        assert_eq!(timeouts.connect, Duration::from_secs(10));
        assert_eq!(timeouts.stream, Duration::from_secs(300));
        assert_eq!(timeouts.low_speed, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn stream_timeout_bounds_a_stalled_provider() {
        // Accepts the request but never responds, so only the client-side
        // stream timeout can end the call.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stall = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer);
            thread::sleep(Duration::from_secs(5));
        });
        let config = config(ProviderKind::OpenaiCompatible, &format!("{base}/v1"));
        let provider = Provider::from_config_with(
            &config,
            Timeouts {
                stream: Duration::from_millis(300),
                ..Timeouts::default()
            },
            &|_| Ok("test-key".into()),
        )
        .unwrap();
        let model = provider.completion_model("manual").unwrap();
        let request = crate::edge::to_request(
            &[crate::history::Message::user("hi")],
            &[],
            None,
            None,
            None,
        );
        // The response future is polled lazily by the stream, so drain it to
        // surface the client-side timeout.
        use futures::StreamExt;
        let started = Instant::now();
        let mut stream = crate::providers::CompletionSource::stream(&model, request)
            .expect("the request is representable");
        let error = loop {
            match stream.next().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => continue,
                None => panic!("stalled provider stream unexpectedly ended"),
            }
        };
        assert!(started.elapsed() < Duration::from_secs(3), "{started:?}");
        let mut chain = String::new();
        let mut source: Option<&dyn std::error::Error> = Some(&error);
        while let Some(error) = source {
            chain.push_str(&error.to_string());
            source = error.source();
        }
        // Rig's error wrapper drops the reqwest source chain, so the proof is
        // behavioral: the server stalls for 5s and only the 300ms stream
        // budget can end the call (a connect failure would say "refused").
        let chain = chain.to_lowercase();
        assert!(!chain.contains("refused"), "{chain}");
        stall.join().unwrap();
    }

    #[test]
    fn base_url_env_follows_the_key_env_convention() {
        assert_eq!(base_url_env("ANTHROPIC_API_KEY"), "ANTHROPIC_BASE_URL");
        assert_eq!(base_url_env("OPENAI_API_KEY"), "OPENAI_BASE_URL");
    }

    #[tokio::test]
    async fn env_only_config_also_runs_on_the_timeout_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stall = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0; 4096];
            let _ = stream.read(&mut buffer);
            thread::sleep(Duration::from_secs(5));
        });
        // No api_key_env/base_url in config: the key and base URL both come
        // from the environment, and the timeout policy must still apply.
        let config = Config::parse("provider \"test\" { kind = \"openai\" }")
            .unwrap()
            .providers
            .remove("test")
            .unwrap();
        unsafe { std::env::set_var("OPENAI_BASE_URL", format!("{base}/v1")) };
        let provider = Provider::from_config_with(
            &config,
            Timeouts {
                stream: Duration::from_millis(300),
                ..Timeouts::default()
            },
            &|_| Ok("test-key".into()),
        )
        .unwrap();
        unsafe { std::env::remove_var("OPENAI_BASE_URL") };
        let model = provider.completion_model("manual").unwrap();
        let request = crate::edge::to_request(
            &[crate::history::Message::user("hi")],
            &[],
            None,
            None,
            None,
        );
        use futures::StreamExt;
        let started = Instant::now();
        let mut stream = crate::providers::CompletionSource::stream(&model, request)
            .expect("the request is representable");
        let error = loop {
            match stream.next().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => continue,
                None => panic!("stalled provider stream unexpectedly ended"),
            }
        };
        assert!(started.elapsed() < Duration::from_secs(3), "{started:?}");
        assert!(!error.to_string().to_lowercase().contains("refused"));
        stall.join().unwrap();
    }

    #[test]
    fn merges_partial_overrides_and_manual_models() {
        let config = Config::parse(
            "provider \"x\" {\n  kind = \"openai\"\n  model \"existing\" { max_output_tokens = 2048 }\n  model \"new\" { name = \"Manual model\" }\n}",
        )
        .unwrap();
        let mut existing = Model::new("existing", "Discovered name");
        existing.context_length = Some(8192);
        let merged = catalog::merge_catalog(
            &config.providers["x"],
            ModelList::new(vec![existing, Model::from_id("untouched")]),
        );
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].name.as_deref(), Some("Discovered name"));
        assert_eq!(merged[0].context_length, Some(8192));
        assert_eq!(merged[0].max_output_tokens, Some(2048));
        assert_eq!(merged[1].id, "new");
        assert_eq!(merged[2].id, "untouched");
    }

    #[tokio::test]
    async fn constructs_every_kind_and_supports_manual_catalogs_without_network() {
        let _dir = tempfile::tempdir().unwrap();
        for &kind in ProviderKind::ALL {
            let mut config = config(kind, "http://127.0.0.1:1");
            config.discover_models = false;
            let provider = build(&config);
            assert_eq!(provider.kind(), kind);
            let models = provider
                .models_with(&config, &|_| Ok("test-key".into()))
                .await
                .unwrap();
            assert_eq!(models.len(), 1, "{}", kind.as_str());
            if kind != ProviderKind::Voyageai {
                let model = provider.completion_model("unlisted-model").unwrap();
                assert_eq!(model.label(), Some("unlisted-model"));
            }
        }
    }

    #[tokio::test]
    async fn unsupported_discovery_uses_configured_models() {
        // Bedrock now discovers through the AWS control-plane API; without
        // credentials that surfaces as an error instead of an empty list,
        // and discover_models = false restores the configured-only catalog.
        let mut parsed = Config::parse(
            "provider \"test\" { kind = \"amazon-bedrock\"\n  region = \"us-east-1\"\n  model \"m\" { name = \"Manual\" }\n  discover_models = false }",
        )
        .unwrap();
        let bedrock = parsed.providers.remove("test").unwrap();
        assert_eq!(
            build(&bedrock)
                .models_with(&bedrock, &|_| Ok("test-key".into()))
                .await
                .unwrap()
                .len(),
            1
        );
        for kind in [ProviderKind::Voyageai] {
            let config = config(kind, "http://127.0.0.1:1");
            assert!(config.discover_models);
            assert_eq!(
                build(&config)
                    .models_with(&config, &|_| Ok("test-key".into()))
                    .await
                    .unwrap()
                    .len(),
                1,
                "{}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn missing_credentials_are_reported_only_when_building() {
        let config = Config::parse(
            "provider \"test\" { kind = \"openai\"\n  api_key_env = \"CRAFT_TEST_KEY\" }",
        )
        .unwrap();
        let result =
            Provider::from_config_with(&config.providers["test"], Timeouts::default(), &|name| {
                crate::error::InvalidSnafu {
                    reason: format!("missing {name}"),
                }
                .fail()
            });
        let error = result.err().unwrap();
        let report = snafu::Report::from_error(error).to_string();
        assert!(report.contains("missing CRAFT_TEST_KEY"), "{report}");
        assert!(report.contains("creating openai provider"), "{report}");
    }

    /// One-request local HTTP server with bounded accept/read times. Captures the
    /// complete request so tests verify both the protocol and custom URL prefix.
    fn server(status: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let content_type = if body.starts_with("data:") || body.starts_with("event:") {
            "text/event-stream"
        } else {
            "application/json"
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        );
        let task = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "no request received");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let size = stream.read(&mut buffer).unwrap();
                assert_ne!(size, 0, "incomplete request");
                request.extend_from_slice(&buffer[..size]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .map(|value| value.parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (base, task)
    }

    #[tokio::test]
    async fn thinking_controls_reach_each_native_http_protocol() {
        use crate::providers::CompletionSource;
        use crate::thinking::{Effort, ReasoningOption, ThinkingConfig};
        fn assert_fragment(body: &serde_json::Value, expected: &serde_json::Value) {
            if let Some(object) = expected.as_object() {
                for (key, value) in object {
                    assert_fragment(&body[key], value);
                }
            } else {
                assert_eq!(body, expected);
            }
        }
        let cases = [
            (
                ProviderKind::OpenaiCompatible,
                "manual",
                ThinkingConfig::Effort(Effort::Low),
                serde_json::json!({"reasoning_effort": "low"}),
            ),
            (
                ProviderKind::Openai,
                "gpt-5.2",
                ThinkingConfig::Effort(Effort::High),
                serde_json::json!({"reasoning": {"effort": "low"}}),
            ),
            (
                ProviderKind::Xai,
                "grok-4.6",
                ThinkingConfig::Effort(Effort::Low),
                serde_json::json!({"reasoning": {"effort": "low"}}),
            ),
            (
                ProviderKind::Openrouter,
                "manual",
                ThinkingConfig::Effort(Effort::Low),
                serde_json::json!({"reasoning": {"effort": "low"}}),
            ),
            (
                ProviderKind::Anthropic,
                "claude-sonnet-4-5",
                ThinkingConfig::Budget(99_999),
                serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}),
            ),
            (
                ProviderKind::Anthropic,
                "claude-opus-4-7",
                ThinkingConfig::Effort(Effort::Low),
                serde_json::json!({"thinking": {"type": "adaptive", "display": "summarized"},
                    "output_config": {"effort": "low"}}),
            ),
            (
                ProviderKind::Gemini,
                "gemini-3-flash-preview",
                ThinkingConfig::Effort(Effort::Low),
                serde_json::json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "low", "includeThoughts": true}}}),
            ),
            (
                ProviderKind::Ollama,
                "qwen3",
                ThinkingConfig::Off,
                serde_json::json!({"reasoning_effort": "none"}),
            ),
            (
                ProviderKind::Deepseek,
                "manual",
                ThinkingConfig::Off,
                serde_json::json!({"thinking": {"type": "disabled"}}),
            ),
            (
                ProviderKind::Zai,
                "manual",
                ThinkingConfig::Off,
                serde_json::json!({"thinking": {"type": "disabled"}}),
            ),
        ];
        for (kind, id, thinking, expected) in cases {
            // A rejecting local endpoint is enough to capture the complete
            // outbound body; these tests never spend tokens or use credentials.
            let (base, server) =
                server("400 Bad Request", r#"{"error":{"message":"capture only"}}"#);
            let mut config = config(kind, &base);
            config.models.insert(
                id.into(),
                crate::config::ModelConfig {
                    supports_thinking: Some(true),
                    reasoning_options: Some(vec![ReasoningOption {
                        kind: "effort".into(),
                        values: vec!["none".into(), "low".into(), "xhigh".into()],
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            );
            let model = build(&config).configured_model(&config, id).unwrap();
            let mut request = crate::edge::to_request(
                &[crate::history::Message::user("hello")],
                &[],
                None,
                Some(0.2),
                Some(4096),
            );
            crate::thinking::attach(&mut request, thinking);
            // Some rig providers create a lazy stream; drive it so the HTTP
            // request is sent and the intentional rejection is observed.
            if let Ok(mut stream) = model.stream(request) {
                use futures::StreamExt;
                while let Some(item) = stream.next().await {
                    if item.is_err() {
                        break;
                    }
                }
            }
            let captured = server.join().unwrap();
            let (_, body) = captured.split_once("\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert!(
                !body.to_string().contains("__craft_thinking"),
                "{kind:?}: {body}"
            );
            assert_fragment(&body, &expected);
        }
    }

    #[tokio::test]
    async fn custom_openai_discovers_and_merges_models() {
        let (base, task) = server("200 OK", r#"{"data":[{"id":"discovered"}]}"#);
        let config = config(ProviderKind::OpenaiCompatible, &format!("{base}/custom/v1"));
        let models = build(&config)
            .models_with(&config, &|_| Ok("test-key".into()))
            .await
            .unwrap();
        assert_eq!(models.len(), 2);
        let request = task.join().unwrap();
        assert!(request.starts_with("GET /custom/v1/models "));
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer test-key")
        );
    }

    #[tokio::test]
    async fn openai_compatible_discovery_preserves_listing_metadata() {
        let (base, task) = server(
            "200 OK",
            r#"{"data":[
                {"id":"rich","name":"Rich","description":"a dense model","owned_by":"acme",
                 "created":1,"context_length":524288,"max_output_length":65536},
                {"id":"plain"}
            ]}"#,
        );
        let config = config(ProviderKind::OpenaiCompatible, &format!("{base}/v1"));
        let models = build(&config)
            .models_with(&config, &|_| Ok("test-key".into()))
            .await
            .unwrap();
        let rich = models.iter().find(|model| model.id == "rich").unwrap();
        assert_eq!(rich.context_length, Some(524288));
        assert_eq!(rich.description.as_deref(), Some("a dense model"));
        assert_eq!(rich.max_output_tokens, Some(65536));
        let plain = models.iter().find(|model| model.id == "plain").unwrap();
        assert_eq!(plain.context_length, None);
        task.join().unwrap();
    }

    #[tokio::test]
    async fn custom_anthropic_discovers_models() {
        let (base, task) = server(
            "200 OK",
            r#"{"data":[{"id":"discovered","display_name":"Discovered"}],"has_more":false}"#,
        );
        let config = config(ProviderKind::Anthropic, &format!("{base}/custom"));
        assert_eq!(build(&config).models(&config).await.unwrap().len(), 2);
        let request = task.join().unwrap();
        assert!(request.starts_with("GET /custom/v1/models "));
        assert!(request.to_lowercase().contains("x-api-key: test-key"));
    }

    #[tokio::test]
    async fn discovery_errors_are_not_hidden_by_manual_models() {
        let (base, task) = server("401 Unauthorized", r#"{"error":"unauthorized"}"#);
        let config = config(ProviderKind::OpenaiCompatible, &base);
        assert!(
            build(&config)
                .models_with(&config, &|_| Ok("test-key".into()))
                .await
                .is_err()
        );
        task.join().unwrap();
    }

    #[tokio::test]
    async fn custom_openai_can_perform_inference() {
        // The driver streams, so the server must speak SSE.
        let (base, task) = server(
            "200 OK",
            "data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"manual\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"manual\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\ndata: [DONE]\n\n",
        );
        let config = config(ProviderKind::OpenaiCompatible, &format!("{base}/v1"));
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
        let model = build(&config).completion_model("manual").unwrap();
        let tools = workspace.register();
        let (_, cancel) = crate::run::cancel_channel();
        let mut history = Vec::new();
        let outcome = crate::run::run(
            &model,
            &crate::run::RunParams::default(),
            &tools,
            &mut history,
            "hi",
            &cancel,
            &|_| {},
        )
        .await;
        match outcome {
            crate::run::RunOutcome::Done { reply } => assert_eq!(reply, "hello"),
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(history.len(), 2);
        let request = task.join().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions "));
        assert!(request.contains("\"model\":\"manual\""));
    }

    #[tokio::test]
    async fn custom_anthropic_can_perform_inference() {
        // The driver streams, so the server must speak SSE.
        let (base, task) = server(
            "200 OK",
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"test\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"manual\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let config = config(ProviderKind::Anthropic, &format!("{base}/custom"));
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
        let model = build(&config).completion_model("manual").unwrap();
        let tools = workspace.register();
        let (_, cancel) = crate::run::cancel_channel();
        let mut history = Vec::new();
        let params = crate::run::RunParams {
            max_tokens: Some(32),
            ..crate::run::RunParams::default()
        };
        let outcome = crate::run::run(
            &model,
            &params,
            &tools,
            &mut history,
            "hi",
            &cancel,
            &|_| {},
        )
        .await;
        match outcome {
            crate::run::RunOutcome::Done { reply } => assert_eq!(reply, "hello"),
            other => panic!("unexpected outcome: {other:?}"),
        }
        let request = task.join().unwrap();
        assert!(request.starts_with("POST /custom/v1/messages "));
        assert!(request.contains("\"model\":\"manual\""));
    }
}
