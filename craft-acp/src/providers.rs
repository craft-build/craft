//! Config-driven native Rig clients. Keep concrete clients available so future
//! inference, embeddings, tools, and streaming can use their full capabilities.

use anyhow::{Context, Result, bail};
use rig::{
    agent::ModelHandle,
    client::{CompletionClient, ModelListingClient, ProviderClient},
    model::ModelList,
    providers::*,
};
use serde::Deserialize;

use crate::config::ProviderConfig;

fn credential(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| {
        format!("credential environment variable {name:?} is not set or not Unicode")
    })?;
    if value.trim().is_empty() {
        bail!("credential environment variable {name:?} is empty");
    }
    Ok(value)
}

// The table below is the single source of truth for config kinds, concrete
// clients, construction, discovery, and completion support. Rig expresses provider
// capabilities at compile time, so unsupported listers must not be called.
macro_rules! list_models {
    ($client:expr, yes) => {
        $client.list_models().await?
    };
    ($client:expr, no) => {{
        let _ = $client;
        ModelList::new(vec![])
    }};
}

macro_rules! completion_model {
    ($client:expr, $model:expr, yes) => {
        ModelHandle::named($model, $client.completion_model($model))
    };
    ($client:expr, $model:expr, no) => {{
        let _ = ($client, $model);
        bail!("provider does not support completion models");
    }};
}

macro_rules! build_client {
    ($config:expr, $client:ty, $env:literal, $credential:expr) => {{
        let config = $config;
        if config.api_key_env.is_none() && config.base_url.is_none() {
            <$client>::from_env()?
        } else {
            let key = $credential(config.api_key_env.as_deref().unwrap_or($env))?;
            let mut builder = <$client>::builder().api_key(key);
            if let Some(base_url) = &config.base_url {
                builder = builder.base_url(base_url);
            }
            builder.build()?
        }
    }};
    ($config:expr, $client:ty, $factory:ident, $credential:expr) => {
        $factory($config, $credential)?
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
        }

        impl Provider {
            pub fn from_config(config: &ProviderConfig) -> Result<Self> {
                Self::from_config_with(config, &credential)
            }

            // Inject credential lookup for tests without mutating process-wide
            // environment variables in a multithreaded async test runner.
            fn from_config_with(
                config: &ProviderConfig,
                credential: &dyn Fn(&str) -> Result<String>,
            ) -> Result<Self> {
                config.validate()?;
                (|| -> Result<Self> {
                    Ok(match config.kind {
                        $(ProviderKind::$variant => Self::$variant(
                            build_client!(config, $client, $auth, credential)
                        ),)+
                    })
                })().with_context(|| format!("creating {} provider", config.kind.as_str()))
            }

            pub fn kind(&self) -> ProviderKind {
                match self { $(Self::$variant(_) => ProviderKind::$variant,)+ }
            }

            /// Select an exact provider model/deployment ID without discovery.
            /// Rig erases the concrete model once, retaining its native protocol.
            pub fn completion_model(&self, model: &str) -> Result<ModelHandle> {
                if model.trim().is_empty() {
                    bail!("model ID must not be empty");
                }
                (|| -> Result<ModelHandle> {
                    Ok(match self {
                        $(Self::$variant(client) => completion_model!(client, model, $completion),)+
                    })
                })().with_context(|| format!(
                    "selecting model {model:?} on {} provider", self.kind().as_str()
                ))
            }

            /// Providers without listing support return the configured models.
            /// Discovery errors propagate instead of masquerading as an empty
            /// catalog. Set `discover_models = false` for manual-only catalogs.
            pub async fn models(&self, config: &ProviderConfig) -> Result<ModelList> {
                config.validate()?;
                if config.kind != self.kind() {
                    bail!("model config kind does not match the provider");
                }
                let discovered = if config.discover_models {
                    match self {
                        $(Self::$variant(client) => list_models!(client, $listing),)+
                    }
                } else {
                    ModelList::new(vec![])
                };
                Ok(config.merge_models(discovered))
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
    Voyageai, "voyageai", voyageai::Client, "VOYAGE_API_KEY", no, no;
    Xai, "xai", xai::Client, "XAI_API_KEY", no, yes;
    Xiaomimimo, "xiaomimimo", xiaomimimo::Client, "XIAOMI_MIMO_API_KEY", yes, yes;
    Zai, "zai", zai::Client, "ZAI_API_KEY", no, yes;
}

fn build_llamafile(
    config: &ProviderConfig,
    _: &dyn Fn(&str) -> Result<String>,
) -> Result<llamafile::Client> {
    let mut builder = llamafile::Client::builder().api_key(rig::client::Nothing);
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    } else if let Ok(base) = std::env::var("LLAMAFILE_API_BASE_URL") {
        builder = builder.base_url(base);
    }
    Ok(builder.build()?)
}

fn build_ollama(
    config: &ProviderConfig,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<ollama::Client> {
    if config.api_key_env.is_none() && config.base_url.is_none() {
        return Ok(ollama::Client::from_env()?);
    }
    let key = match &config.api_key_env {
        Some(name) => credential(name)?,
        None => std::env::var("OLLAMA_API_KEY").unwrap_or_default(),
    };
    let mut builder = ollama::Client::builder().api_key(key);
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    } else if let Ok(base) = std::env::var("OLLAMA_API_BASE_URL") {
        builder = builder.base_url(base);
    }
    Ok(builder.build()?)
}

fn build_azure(
    config: &ProviderConfig,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<azure::Client> {
    let auth = if let Some(name) = &config.api_key_env {
        azure::AzureOpenAIAuth::ApiKey(credential(name)?)
    } else if let Ok(key) = std::env::var("AZURE_API_KEY") {
        azure::AzureOpenAIAuth::ApiKey(key)
    } else {
        azure::AzureOpenAIAuth::Token(credential("AZURE_TOKEN")?)
    };
    let endpoint = config
        .base_url
        .clone()
        .or_else(|| std::env::var("AZURE_ENDPOINT").ok())
        .context("azure requires base_url or AZURE_ENDPOINT")?;
    let version = config
        .api_version
        .clone()
        .or_else(|| std::env::var("AZURE_API_VERSION").ok())
        .context("azure requires api_version or AZURE_API_VERSION")?;
    Ok(azure::Client::builder()
        .api_key(auth)
        .azure_endpoint(endpoint)
        .api_version(&version)
        .build()?)
}

fn build_chatgpt(
    config: &ProviderConfig,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<chatgpt::Client> {
    if config.api_key_env.is_none() && config.base_url.is_none() {
        return Ok(chatgpt::Client::from_env()?);
    }
    let auth = if let Some(name) = &config.api_key_env {
        chatgpt::ChatGPTAuth::AccessToken {
            access_token: credential(name)?,
            account_id: config.account_id.clone(),
        }
    } else if let Ok(access_token) = std::env::var("CHATGPT_ACCESS_TOKEN") {
        chatgpt::ChatGPTAuth::AccessToken {
            access_token,
            account_id: std::env::var("CHATGPT_ACCOUNT_ID").ok(),
        }
    } else {
        chatgpt::ChatGPTAuth::OAuth
    };
    let mut builder = chatgpt::Client::builder().api_key(auth);
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    }
    Ok(builder.build()?)
}

fn build_copilot(
    config: &ProviderConfig,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<copilot::Client> {
    if config.api_key_env.is_none() && config.base_url.is_none() {
        return Ok(copilot::Client::from_env()?);
    }
    let mut builder = copilot::Client::builder();
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    }
    if let Some(name) = &config.api_key_env {
        Ok(builder.api_key(credential(name)?).build()?)
    } else if let Some(key) = first_env(&["GITHUB_COPILOT_API_KEY", "COPILOT_API_KEY"]) {
        Ok(builder.api_key(key).build()?)
    } else if let Some(token) = first_env(&["COPILOT_GITHUB_ACCESS_TOKEN", "GITHUB_TOKEN"]) {
        Ok(builder.github_access_token(token).build()?)
    } else {
        Ok(builder.oauth().build()?)
    }
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    fn config(kind: ProviderKind, base_url: &str) -> ProviderConfig {
        let auth = if kind == ProviderKind::Llamafile {
            ""
        } else {
            "api_key_env = 'CRAFT_TEST_KEY'\n"
        };
        let azure = if kind == ProviderKind::Azure {
            "api_version = '2024-10-21'\n"
        } else {
            ""
        };
        Config::parse(&format!(
            "[providers.test]\nkind = '{}'\nbase_url = '{base_url}'\n{auth}{azure}\
             [providers.test.models.manual]\nname = 'Manual model'\n",
            kind.as_str(),
        ))
        .unwrap()
        .providers
        .remove("test")
        .unwrap()
    }

    fn build(config: &ProviderConfig) -> Provider {
        Provider::from_config_with(config, &|name| {
            assert_eq!(name, "CRAFT_TEST_KEY");
            Ok("test-key".into())
        })
        .unwrap()
    }

    #[tokio::test]
    async fn constructs_every_kind_and_supports_manual_catalogs_without_network() {
        for &kind in ProviderKind::ALL {
            let mut config = config(kind, "http://127.0.0.1:1");
            config.discover_models = false;
            let provider = build(&config);
            assert_eq!(provider.kind(), kind);
            let models = provider.models(&config).await.unwrap();
            assert_eq!(models.len(), 1, "{}", kind.as_str());
            if kind != ProviderKind::Voyageai {
                let agent = crate::agent::build(
                    &provider,
                    "unlisted-model",
                    &crate::config::AgentConfig::default(),
                )
                .unwrap();
                assert_eq!(agent.model_handle().label(), Some("unlisted-model"));
            }
        }
    }

    #[tokio::test]
    async fn unsupported_discovery_uses_configured_models() {
        let config = config(ProviderKind::Voyageai, "http://127.0.0.1:1");
        assert!(config.discover_models);
        assert_eq!(build(&config).models(&config).await.unwrap().len(), 1);
    }

    #[test]
    fn missing_credentials_are_reported_only_when_building() {
        let config =
            Config::parse("[providers.test]\nkind = 'openai'\napi_key_env = 'CRAFT_TEST_KEY'")
                .unwrap();
        let result =
            Provider::from_config_with(&config.providers["test"], &|name| bail!("missing {name}"));
        let error = result.err().unwrap();
        assert!(format!("{error:#}").contains("missing CRAFT_TEST_KEY"));
        assert!(format!("{error:#}").contains("creating openai provider"));
    }

    /// One-request local HTTP server with bounded accept/read times. Captures the
    /// complete request so tests verify both the protocol and custom URL prefix.
    fn server(status: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
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
    async fn custom_openai_discovers_and_merges_models() {
        let (base, task) = server("200 OK", r#"{"data":[{"id":"discovered"}]}"#);
        let config = config(ProviderKind::OpenaiCompatible, &format!("{base}/custom/v1"));
        let models = build(&config).models(&config).await.unwrap();
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
        assert!(build(&config).models(&config).await.is_err());
        task.join().unwrap();
    }

    #[tokio::test]
    async fn custom_openai_can_perform_inference() {
        let (base, task) = server(
            "200 OK",
            r#"{
            "id":"test","object":"chat.completion","created":0,"model":"manual",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        }"#,
        );
        let config = config(ProviderKind::OpenaiCompatible, &format!("{base}/v1"));
        let agent = crate::agent::build(
            &build(&config),
            "manual",
            &crate::config::AgentConfig::default(),
        )
        .unwrap();
        let response = agent.runner("hi").run().await.unwrap();
        assert_eq!(response.output(), "hello");
        assert_eq!(response.requests(), 1);
        let request = task.join().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions "));
        assert!(request.contains("\"model\":\"manual\""));
    }

    #[tokio::test]
    async fn custom_anthropic_can_perform_inference() {
        let (base, task) = server(
            "200 OK",
            r#"{
            "id":"test","type":"message","role":"assistant","model":"manual",
            "content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,
            "usage":{"input_tokens":1,"output_tokens":1}
        }"#,
        );
        let config = config(ProviderKind::Anthropic, &format!("{base}/custom"));
        let settings = crate::config::AgentConfig {
            max_tokens: Some(32),
            ..crate::config::AgentConfig::default()
        };
        let agent = crate::agent::build(&build(&config), "manual", &settings).unwrap();
        let response = agent.runner("hi").run().await.unwrap();
        assert_eq!(response.output(), "hello");
        assert_eq!(response.requests(), 1);
        let request = task.join().unwrap();
        assert!(request.starts_with("POST /custom/v1/messages "));
        assert!(request.contains("\"model\":\"manual\""));
    }
}
