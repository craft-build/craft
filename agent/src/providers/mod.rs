//! Config-driven native Rig clients. Keep concrete clients available so future
//! inference, embeddings, tools, and streaming can use their full capabilities.
//! [`DynamicModel`] erases the concrete model type behind rig-core's
//! `CompletionModel` so the rest of the crate never names a provider type.

mod catalog;
mod dynamic;
mod openai_compat;
mod reauth;
mod registry;
pub mod usage_fetch;

pub use catalog::{CatalogEntry, CatalogModel};
pub use dynamic::DynamicModel;
pub use reauth::reauth_hook;
pub use registry::{Provider, ProviderKind};
pub use usage_fetch::{ModelUsageRow, ProviderUsage, UsageLimit};

use std::time::Duration;

use rig_core::providers::*;
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

/// The shared HTTP backend every configured provider is built on, matching the
/// reference `http_client`: connect timeout plus an overall stream timeout.
pub(crate) fn timeout_client(timeouts: Timeouts) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(timeouts.connect)
        .timeout(timeouts.stream)
        .build()
        .map_err(crate::error::client_error)
}

/// The base-URL env var rig's `from_env` reads for a provider whose key env
/// is `api_env` (e.g. `ANTHROPIC_API_KEY` → `ANTHROPIC_BASE_URL`).
pub(crate) fn base_url_env(api_env: &str) -> String {
    api_env.replace("_API_KEY", "_BASE_URL")
}

pub(crate) fn build_llamafile(
    config: &ProviderConfig,
    timeouts: Timeouts,
    _: &dyn Fn(&str) -> Result<String>,
) -> Result<llamafile::Client> {
    let mut builder = llamafile::Client::builder()
        .api_key(rig_core::client::Nothing)
        .http_client(timeout_client(timeouts)?);
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    } else if let Ok(base) = std::env::var("LLAMAFILE_API_BASE_URL") {
        builder = builder.base_url(base);
    }
    builder.build().map_err(crate::error::client_error)
}

pub(crate) fn build_ollama(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<ollama::Client> {
    let key = match &config.api_key_env {
        Some(name) => credential(name)?,
        None => std::env::var("OLLAMA_API_KEY").unwrap_or_default(),
    };
    let mut builder = ollama::Client::builder()
        .api_key(key)
        .http_client(timeout_client(timeouts)?);
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    } else if let Ok(base) = std::env::var("OLLAMA_API_BASE_URL") {
        builder = builder.base_url(base);
    }
    builder.build().map_err(crate::error::client_error)
}

pub(crate) fn build_azure(
    config: &ProviderConfig,
    timeouts: Timeouts,
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
        .context(AzureEndpointMissingSnafu)?;
    let version = config
        .api_version
        .clone()
        .or_else(|| std::env::var("AZURE_API_VERSION").ok())
        .context(AzureApiVersionMissingSnafu)?;
    azure::Client::builder()
        .api_key(auth)
        .azure_endpoint(endpoint)
        .api_version(&version)
        .http_client(timeout_client(timeouts)?)
        .build()
        .map_err(crate::error::client_error)
}

pub(crate) fn build_chatgpt(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<chatgpt::Client> {
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
    let mut builder = chatgpt::Client::builder()
        .api_key(auth)
        .http_client(timeout_client(timeouts)?);
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    }
    builder.build().map_err(crate::error::client_error)
}

pub(crate) fn build_copilot(
    config: &ProviderConfig,
    timeouts: Timeouts,
    credential: &dyn Fn(&str) -> Result<String>,
) -> Result<copilot::Client> {
    let http = || timeout_client(timeouts).map_err(crate::error::client_error);
    let mut builder = copilot::Client::builder();
    if let Some(base) = &config.base_url {
        builder = builder.base_url(base);
    }
    if let Some(name) = &config.api_key_env {
        builder
            .api_key(credential(name)?)
            .http_client(http()?)
            .build()
    } else if let Some(key) = first_env(&["GITHUB_COPILOT_API_KEY", "COPILOT_API_KEY"]) {
        builder.api_key(key).http_client(http()?).build()
    } else if let Some(token) = first_env(&["COPILOT_GITHUB_ACCESS_TOKEN", "GITHUB_TOKEN"]) {
        builder
            .github_access_token(token)
            .http_client(http()?)
            .build()
    } else {
        builder.oauth().http_client(http()?).build()
    }
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
    use rig_core::model::{Model, ModelList};
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
        let error = match rig_core::completion::CompletionModel::stream(&model, request).await {
            Err(error) => error,
            Ok(mut stream) => loop {
                match stream.next().await {
                    Some(Err(error)) => break error,
                    Some(Ok(_)) => continue,
                    None => panic!("stalled provider stream unexpectedly ended"),
                }
            },
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
        let config = Config::parse("[providers.test]\nkind = 'openai'\n")
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
        let error = match rig_core::completion::CompletionModel::stream(&model, request).await {
            Err(error) => error,
            Ok(mut stream) => loop {
                match stream.next().await {
                    Some(Err(error)) => break error,
                    Some(Ok(_)) => continue,
                    None => panic!("stalled provider stream unexpectedly ended"),
                }
            },
        };
        assert!(started.elapsed() < Duration::from_secs(3), "{started:?}");
        assert!(!error.to_string().to_lowercase().contains("refused"));
        stall.join().unwrap();
    }

    #[test]
    fn merges_partial_overrides_and_manual_models() {
        let config = Config::parse(
            "[providers.x]\nkind = 'openai'\n\
             [providers.x.models.existing]\nmax_output_tokens = 2048\n\
             [providers.x.models.new]\nname = 'Manual model'",
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
        let config = config(ProviderKind::Voyageai, "http://127.0.0.1:1");
        assert!(config.discover_models);
        assert_eq!(
            build(&config)
                .models_with(&config, &|_| Ok("test-key".into()))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn missing_credentials_are_reported_only_when_building() {
        let config =
            Config::parse("[providers.test]\nkind = 'openai'\napi_key_env = 'CRAFT_TEST_KEY'")
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
