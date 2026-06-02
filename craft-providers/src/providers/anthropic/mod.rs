//! Anthropic allows 4 cache breakpoints per request. We place them on: the last tool
//! definition, the system prompt, and the last block of the 2 most recent messages.

pub(crate) mod bedrock;
pub(crate) mod shared;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use flume::Sender;
use futures::io::{AsyncBufReadExt, BufReader};
use futures::TryStreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::io::StreamReader;
use tracing::debug;

use crate::model::Model;
use crate::provider::{BoxFuture, Provider};
use crate::{AgentError, Message, ProviderEvent, RequestOptions, StreamResponse};

use super::{KeyPool, MIME_JSON, lock_unpoison};

const API_VERSION: &str = "2023-06-01";
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const MODELS_URL: &str = "https://api.anthropic.com/v1/models?limit=1000";
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";

const ENV_VAR: &str = "ANTHROPIC_API_KEY";

pub(crate) use shared::models;

fn resolve_auth_from_key(key: &str) -> super::ResolvedAuth {
    super::ResolvedAuth {
        base_url: Some("https://api.anthropic.com/v1/messages".into()),
        headers: vec![("x-api-key".into(), key.to_string())],
    }
}

fn apply_fast_mode(body: &mut Value, fast: bool) {
    if !fast {
        return;
    }
    let betas = body
        .as_object_mut()
        .unwrap()
        .entry("anthropic_beta")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = betas.as_array_mut()
        && !arr.iter().any(|v| v.as_str() == Some(FAST_MODE_BETA))
    {
        arr.push(Value::String(FAST_MODE_BETA.to_string()));
    }
}

pub struct Anthropic {
    client: Client,
    auth: Arc<Mutex<super::ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    system_prefix: Option<String>,
    stream_timeout: Duration,
}

impl Anthropic {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::from_env(ENV_VAR)?;
        let resolved = resolve_auth_from_key(pool.current());
        debug!(keys = pool.len(), "using API key authentication");
        Ok(Self {
            client: super::http_client(timeouts)?,
            auth: Arc::new(Mutex::new(resolved)),
            key_pool: Some(pool),
            system_prefix: None,
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            client: super::http_client(timeouts)?,
            auth,
            key_pool: None,
            system_prefix: None,
            stream_timeout: timeouts.stream,
        })
    }

    pub(crate) fn with_system_prefix(mut self, prefix: Option<String>) -> Self {
        self.system_prefix = prefix;
        self
    }

    fn build_request(&self, method: &str, url: Option<&str>) -> reqwest::RequestBuilder {
        let auth = lock_unpoison(&self.auth);
        let url = url.unwrap_or_else(|| auth.base_url.as_deref().unwrap_or(MESSAGES_URL));
        let mut builder = self
            .client
            .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), url)
            .header("anthropic-version", API_VERSION);
        for (key, value) in &auth.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder
    }

    async fn do_stream_request(
        &self,
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
        fast: bool,
        long_context: bool,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(body)?;
        let mut req = self
            .build_request("POST", None)
            .header("content-type", MIME_JSON);
        let mut betas = Vec::new();
        if fast {
            betas.push(FAST_MODE_BETA);
        }
        if long_context {
            betas.push(shared::LONG_CONTEXT_BETA);
        }
        if !betas.is_empty() {
            req = req.header("anthropic-beta", betas.join(","));
        }
        let response = req.body(json_body).send().await?;
        let status = response.status().as_u16();

        if status == 200 {
            let stream = response.bytes_stream();
            let reader = StreamReader::new(
                stream.map_err(std::io::Error::other),
            );
            parse_sse(BufReader::new(reader.compat()), event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    async fn do_list_models(&self) -> Result<Vec<String>, AgentError> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;

        loop {
            let mut url = MODELS_URL.to_string();
            if let Some(cursor) = &after_id {
                url.push_str(&format!("&after_id={cursor}"));
            }

            let response = self.build_request("GET", Some(&url)).send().await?;
            if response.status().as_u16() != 200 {
                return Err(AgentError::from_response(response).await);
            }

            let body_text = response.text().await?;
            let page: ModelsPage = serde_json::from_str(&body_text)?;
            for m in page.data {
                // The API never tells us about `-1m`, so we mint it ourselves for
                // any model that reports a 1M window.
                if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                    models.push(format!("{}{}", m.id, shared::LONG_CONTEXT_SUFFIX));
                }
                models.push(m.id);
            }

            if !page.has_more {
                break;
            }
            after_id = page.last_id;
        }

        models.sort();
        Ok(models)
    }
}

impl Provider for Anthropic {
    fn stream_message<'a>(
        &'a self,
        model: &'a Model,
        messages: &'a [Message],
        system: &'a str,
        tools: &'a Value,
        event_tx: &'a Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&str>,
    ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
        Box::pin(async move {
            let fast = opts.fast && model.supports_fast();
            let thinking = opts.thinking;
            let system_blocks = if let Some(prefix) = &self.system_prefix {
                vec![
                    shared::SystemBlock {
                        r#type: "text",
                        text: prefix,
                        cache_control: None,
                    },
                    shared::SystemBlock {
                        r#type: "text",
                        text: system,
                        cache_control: Some(shared::EPHEMERAL),
                    },
                ]
            } else {
                vec![shared::SystemBlock {
                    r#type: "text",
                    text: system,
                    cache_control: Some(shared::EPHEMERAL),
                }]
            };

            let mut body = shared::build_request_body_with_system(
                model,
                messages,
                &system_blocks,
                tools,
                thinking,
            );
            body["model"] = json!(shared::strip_long_context(&model.id));
            body["stream"] = json!(true);
            apply_fast_mode(&mut body, fast);
            let long_context = model.id.ends_with(shared::LONG_CONTEXT_SUFFIX);

            debug!(model = %model.id, num_messages = messages.len(), ?thinking, fast, long_context, "sending API request");
            self.do_stream_request(&body, event_tx, fast, long_context)
                .await
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
        Box::pin(self.do_list_models())
    }

    fn reload_auth(&self) -> BoxFuture<'_, Result<(), AgentError>> {
        Box::pin(async {
            let pool = KeyPool::from_env(ENV_VAR)?;
            *lock_unpoison(&self.auth) = resolve_auth_from_key(pool.current());
            debug!("reloaded Anthropic auth from env");
            Ok(())
        })
    }

    fn rotate_key(&self) -> BoxFuture<'_, Result<bool, AgentError>> {
        Box::pin(async {
            Ok(self
                .key_pool
                .as_ref()
                .is_some_and(|p| p.rotate_auth(&self.auth, resolve_auth_from_key)))
        })
    }
}

#[derive(Deserialize)]
struct ModelInfo {
    id: String,
    #[serde(default)]
    max_input_tokens: u32,
}

#[derive(Deserialize)]
struct ModelsPage {
    data: Vec<ModelInfo>,
    has_more: bool,
    last_id: Option<String>,
}

pub(crate) async fn parse_sse(
    reader: impl futures::io::AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();
    let mut parser = shared::EventParser::new();
    let mut current_event = String::new();
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event = event_type.to_string();
            continue;
        }

        let data = match line.strip_prefix("data: ") {
            Some(d) => d,
            None => continue,
        };

        if parser
            .process(&current_event, data, event_tx)
            .await?
            .is_break()
        {
            break;
        }
    }

    Ok(parser.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, ProviderEvent, Role, StopReason, TokenUsage};
    use serde_json::{Value, json};
    use shared::build_wire_messages;
    use std::time::Duration;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

    #[tokio::test]
    async fn parse_sse_text_and_usage() {
        let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_creation_input_tokens\":5,\"cache_read_input_tokens\":8}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n";

        let (tx, rx) = flume::unbounded();
        let resp = parse_sse(futures::io::Cursor::new(sse_data), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert_eq!(
            resp.usage,
            TokenUsage {
                input: 42,
                output: 10,
                cache_creation: 5,
                cache_read: 8
            }
        );
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
        );
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));

        let mut deltas = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let ProviderEvent::TextDelta { text: t } = e {
                deltas.push(t);
            }
        }
        assert_eq!(deltas, vec!["Hello", " world"]);
    }

    #[tokio::test]
    async fn parse_sse_tool_use() {
        let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"bash\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" \\\"echo hi\\\"}\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n";

        let (tx, rx) = flume::unbounded();
        let resp =
            parse_sse(futures::io::Cursor::new(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

        let tools: Vec<_> = resp.message.tool_uses().collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].0, "tu_1");
        assert_eq!(tools[0].1, "bash");

        let starts: Vec<_> = rx
            .drain()
            .filter_map(|e| match e {
                ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![("tu_1".to_string(), "bash".to_string())]);
    }

    #[test]
    fn cache_control_placement() {
        let single = vec![Message::user("only".into())];
        let wire = build_wire_messages(&single);
        let json: Value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            json[0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        let multi = vec![
            Message::user("first".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "reply".into(),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "second".into(),
                    },
                ],
                ..Default::default()
            },
        ];
        let wire = build_wire_messages(&multi);
        let json: Value = serde_json::to_value(&wire).unwrap();

        assert!(json[0]["content"][0].get("cache_control").is_none());
        assert_eq!(
            json[1]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(json[2]["content"][0].get("cache_control").is_none());
        assert_eq!(
            json[2]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[tokio::test]
    async fn parse_sse_overloaded_error() {
        let input = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
        let (tx, _rx) = flume::unbounded();
        let err = parse_sse(futures::io::Cursor::new(input), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap_err();
        match err {
            AgentError::Api { status, message } => {
                assert_eq!(status, 529);
                assert_eq!(message, "Overloaded");
            }
            other => panic!("expected Api error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_sse_unparseable_error() {
        let input = b"event: error\ndata: not-json\n";
        let (tx, _rx) = flume::unbounded();
        let err = parse_sse(futures::io::Cursor::new(input), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap_err();
        match err {
            AgentError::Api { status, message } => {
                assert_eq!(status, 400);
                assert_eq!(message, "not-json");
            }
            other => panic!("expected Api error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_sse_malformed_tool_json_yields_empty_object() {
        let sse_data = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_2\",\"name\":\"read\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{broken\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n";

        let (tx, _rx) = flume::unbounded();
        let resp =
            parse_sse(futures::io::Cursor::new(sse_data.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
                .await
                .unwrap();

        let tools: Vec<_> = resp.message.tool_uses().collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1, "read");
        assert_eq!(*tools[0].2, Value::Object(Default::default()));
    }

    #[tokio::test]
    async fn parse_sse_thinking_blocks() {
        let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" think\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig123\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n";

        let (tx, rx) = flume::unbounded();
        let resp = parse_sse(futures::io::Cursor::new(sse_data), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert!(
            matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, signature }
                if thinking == "Let me think" && *signature == Some("sig123".to_string()))
        );
        assert!(
            matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello")
        );

        let thinking_deltas: Vec<_> = rx
            .drain()
            .filter_map(|e| match e {
                ProviderEvent::ThinkingDelta { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(thinking_deltas, vec!["Let me", " think"]);
    }

    #[tokio::test]
    async fn parse_sse_redacted_thinking() {
        let sse_data = b"\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque_data\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\
\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\"}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n";

        let (tx, _rx) = flume::unbounded();
        let resp = parse_sse(futures::io::Cursor::new(sse_data), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert!(
            matches!(&resp.message.content[0], ContentBlock::RedactedThinking { data } if data == "opaque_data")
        );
        assert!(
            matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hi")
        );
    }

    #[test]
    fn apply_fast_mode_adds_beta_when_fast() {
        let mut body = json!({"model": "claude-opus-4-8"});
        apply_fast_mode(&mut body, true);
        let betas = body["anthropic_beta"].as_array().unwrap();
        assert!(betas.iter().any(|v| v.as_str() == Some(FAST_MODE_BETA)));
    }

    #[test]
    fn apply_fast_mode_noop_when_not_fast() {
        let mut body = json!({"model": "claude-opus-4-8"});
        apply_fast_mode(&mut body, false);
        assert!(body.get("anthropic_beta").is_none());
    }

    #[test]
    fn apply_fast_mode_appends_to_existing_betas() {
        let mut body = json!({"anthropic_beta": ["existing-beta"]});
        apply_fast_mode(&mut body, true);
        let betas = body["anthropic_beta"].as_array().unwrap();
        assert_eq!(betas.len(), 2);
        assert!(betas.iter().any(|v| v.as_str() == Some("existing-beta")));
        assert!(betas.iter().any(|v| v.as_str() == Some(FAST_MODE_BETA)));
    }

    #[test]
    fn apply_fast_mode_idempotent() {
        let mut body = json!({});
        apply_fast_mode(&mut body, true);
        apply_fast_mode(&mut body, true);
        let betas = body["anthropic_beta"].as_array().unwrap();
        assert_eq!(betas.len(), 1);
    }

    #[test]
    fn long_context_spec_resolves_to_1m_window() {
        let model = Model::from_spec("anthropic/claude-opus-4-8-1m").unwrap();
        assert_eq!(model.id, "claude-opus-4-8-1m");
        assert_eq!(model.context_window, shared::LONG_CONTEXT_WINDOW);
        assert!(model.id.ends_with(shared::LONG_CONTEXT_SUFFIX));
        // The API has never heard of `-1m`, so strip it before sending.
        assert_eq!(shared::strip_long_context(&model.id), "claude-opus-4-8");
    }

    #[test]
    fn list_models_adds_1m_variant_from_max_input_tokens() {
        // The real /v1/models payload hides the 1M window in `max_input_tokens`.
        let page: ModelsPage = serde_json::from_str(
            r#"{
                "data": [
                    {"id": "claude-opus-4-8", "max_input_tokens": 1000000},
                    {"id": "claude-opus-4-5-20251101", "max_input_tokens": 200000}
                ],
                "has_more": false,
                "last_id": null
            }"#,
        )
        .unwrap();

        let mut models = Vec::new();
        for m in page.data {
            if m.max_input_tokens >= shared::LONG_CONTEXT_WINDOW {
                models.push(format!("{}{}", m.id, shared::LONG_CONTEXT_SUFFIX));
            }
            models.push(m.id);
        }
        models.sort();

        assert_eq!(
            models,
            vec![
                "claude-opus-4-5-20251101".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-opus-4-8-1m".to_string(),
            ]
        );
    }
}
