use std::time::{Duration, Instant};

use flume::Sender;
use futures::TryStreamExt;
use futures::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::io::StreamReader;
use tracing::{debug, warn};

use super::{MIME_JSON, ResolvedAuth};
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse, TokenUsage,
};

const STREAM_DONE: &str = "[DONE]";

pub(crate) struct OpenAiCompatConfig {
    pub slug: &'static str,
    pub api_key_env: &'static str,
    pub base_url: &'static str,
    pub max_tokens_field: &'static str,
    pub include_stream_usage: bool,
    pub provider_name: &'static str,
}

pub(crate) struct OpenAiCompatProvider {
    client: Client,
    config: &'static OpenAiCompatConfig,
    stream_timeout: Duration,
    /// Env / `providers.toml` override, resolved once at construction. The
    /// static compat default stays the last resort because it can be more
    /// specific than the inventory one (`http://localhost:11434/v1` vs the
    /// bare ollama host). Request-time `auth.base_url` still wins (custom,
    /// local, dynamic).
    resolved_base_url: Option<String>,
}

impl OpenAiCompatProvider {
    pub fn new(
        config: &'static OpenAiCompatConfig,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        let resolved_base_url = if config.slug.is_empty() {
            None
        } else {
            let providers = craft_config::providers::ProvidersConfig::load();
            craft_config::providers::configured_base_url(config.slug, providers.get(config.slug))
        };
        Ok(Self {
            client: super::http_client(timeouts)?,
            config,
            stream_timeout: timeouts.stream,
            resolved_base_url,
        })
    }

    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    pub(crate) fn config(&self) -> &'static OpenAiCompatConfig {
        self.config
    }

    pub(crate) fn stream_timeout(&self) -> Duration {
        self.stream_timeout
    }

    pub(crate) async fn get_text(
        &self,
        auth: &ResolvedAuth,
        url: &str,
    ) -> Result<String, AgentError> {
        let request = auth.configure_request(
            self.client
                .get(url)
                .header("user-agent", super::user_agent()),
        );
        let response = request.send().await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }
        Ok(response.text().await?)
    }

    pub(crate) async fn post_text(
        &self,
        auth: &ResolvedAuth,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<String, AgentError> {
        let request = auth.configure_request(
            self.client
                .post(url)
                .header("content-type", content_type)
                .header("user-agent", super::user_agent()),
        );
        let response = request.body(body).send().await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }
        Ok(response.text().await?)
    }

    pub fn build_body(
        &self,
        model: &crate::model::Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
    ) -> Value {
        let wire_messages = convert_messages(messages, system);
        let wire_tools = convert_tools(tools);

        let mut body = json!({
            "model": model.id,
            "messages": wire_messages,
            "stream": true,
        });
        if let Some(max_output) = model.max_output_tokens {
            body[self.config.max_tokens_field] = json!(max_output);
        }
        if self.config.include_stream_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
        if wire_tools.as_array().is_some_and(|a| !a.is_empty()) {
            body["tools"] = wire_tools;
        }
        body
    }

    /// Effective base URL: an auth-supplied value (dynamic/custom providers)
    /// wins, then the construction-time env / `providers.toml` override, then
    /// the static compat default.
    fn base_url(&self, auth: &ResolvedAuth) -> String {
        if let Some(explicit) = auth.base_url.as_deref() {
            return explicit.to_string();
        }
        self.resolved_base_url
            .clone()
            .unwrap_or_else(|| self.config.base_url.to_string())
    }

    fn build_request(
        &self,
        method: &str,
        path: &str,
        auth: &ResolvedAuth,
    ) -> reqwest::RequestBuilder {
        let base = self.base_url(auth);
        auth.configure_request(
            self.client
                .request(method.parse().unwrap(), format!("{base}{path}"))
                .header("user-agent", super::user_agent()),
        )
    }

    pub async fn do_stream(
        &self,
        model: &crate::model::Model,
        extra_headers: &[(&str, &str)],
        body: &Value,
        event_tx: &Sender<ProviderEvent>,
        auth: &ResolvedAuth,
    ) -> Result<StreamResponse, AgentError> {
        let json_body = serde_json::to_vec(body)?;
        let mut request = self
            .build_request("POST", "/chat/completions", auth)
            .header("content-type", MIME_JSON);
        for &(key, value) in extra_headers {
            request = request.header(key, value);
        }

        debug!(
            model = %model.id,
            provider = self.config.provider_name,
            "sending API request"
        );

        let response = request.body(json_body).send().await?;
        let status = response.status().as_u16();

        if status == 200 {
            let stream = response.bytes_stream();
            let reader = StreamReader::new(stream.map_err(std::io::Error::other));
            parse_sse(
                BufReader::new(reader.compat()),
                event_tx,
                self.stream_timeout,
            )
            .await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }

    pub async fn fetch_and_parse_models(
        &self,
        auth: &ResolvedAuth,
        parse_fn: impl Fn(&Value) -> Option<crate::model::ModelInfo>,
    ) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        let base = self.base_url(auth);
        let url = format!("{base}/models");
        let body_text = self.get_text(auth, &url).await?;
        let body: Value = serde_json::from_str(&body_text)?;
        let mut models: Vec<crate::model::ModelInfo> = body["data"]
            .as_array()
            .map(|arr| arr.iter().filter_map(parse_fn).collect())
            .unwrap_or_default();
        models.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(models)
    }

    fn default_model_parser(m: &Value) -> Option<crate::model::ModelInfo> {
        let id = m["id"].as_str()?.to_string();
        let mut info = crate::model::ModelInfo::new(id);
        if let Some(n) = m.get("context_window").and_then(|v| v.as_u64()) {
            info.context_window = Some(n as u32);
        } else if let Some(n) = m.get("context_length").and_then(|v| v.as_u64()) {
            info.context_window = Some(n as u32);
        } else if let Some(n) = m.get("max_input_tokens").and_then(|v| v.as_u64()) {
            info.context_window = Some(n as u32);
        } else if let Some(n) = m.get("max_context_length").and_then(|v| v.as_u64()) {
            info.context_window = Some(n as u32);
        }
        if let Some(n) = m.get("max_output_tokens").and_then(|v| v.as_u64()) {
            info.max_output_tokens = Some(n as u32);
        } else if let Some(n) = m.get("max_tokens").and_then(|v| v.as_u64()) {
            info.max_output_tokens = Some(n as u32);
        }
        Some(info)
    }

    pub async fn do_list_models_with_info(
        &self,
        auth: &ResolvedAuth,
    ) -> Result<Vec<crate::model::ModelInfo>, AgentError> {
        self.fetch_and_parse_models(auth, Self::default_model_parser)
            .await
    }

    pub async fn do_list_models(&self, auth: &ResolvedAuth) -> Result<Vec<String>, AgentError> {
        let infos = self.do_list_models_with_info(auth).await?;
        Ok(infos.into_iter().map(|i| i.id).collect())
    }
}

pub fn convert_messages(messages: &[Message], system: &str) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];

    for msg in messages {
        match msg.role {
            Role::User => {
                let mut tool_results = Vec::new();
                let mut text_parts: Vec<&str> = Vec::new();
                let mut image_parts = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => text_parts.push(text.as_str()),
                        ContentBlock::Image { source } => {
                            image_parts.push(json!({
                                "type": "image_url",
                                "image_url": { "url": source.to_data_url() }
                            }));
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            images,
                            ..
                        } => {
                            tool_results.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                            if !images.is_empty() {
                                let mut parts = Vec::with_capacity(images.len() + 1);
                                parts.push(json!({
                                    "type": "text",
                                    "text": format!("{content}\n[image result of {tool_use_id}]")
                                }));
                                for source in images {
                                    parts.push(json!({
                                        "type": "image_url",
                                        "image_url": { "url": source.to_data_url() }
                                    }));
                                }
                                tool_results.push(json!({
                                    "role": "user",
                                    "content": parts,
                                }));
                            }
                        }
                        ContentBlock::ToolUse { .. }
                        | ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                if !image_parts.is_empty() {
                    let mut parts = image_parts;
                    if !text_parts.is_empty() {
                        parts.push(json!({"type": "text", "text": text_parts.join("\n")}));
                    }
                    out.push(json!({"role": "user", "content": parts}));
                } else if !text_parts.is_empty() {
                    out.push(json!({"role": "user", "content": text_parts.join("\n")}));
                }
                out.extend(tool_results);
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut reasoning_text = String::new();
                let mut tool_calls = Vec::new();

                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::Thinking { thinking, .. } => {
                            reasoning_text.push_str(thinking);
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": input.to_string(),
                                }
                            }));
                        }
                        ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. }
                        | ContentBlock::RedactedThinking { .. } => {}
                    }
                }

                if !text.is_empty() || !tool_calls.is_empty() || !reasoning_text.is_empty() {
                    // Always emit string `content` (""): some OpenAI-compatible
                    // backends (e.g. Cloudflare Workers AI gpt-oss) reject
                    // omitted/null content on assistant tool-call messages.
                    let mut msg_obj = json!({"role": "assistant", "content": text});
                    if !reasoning_text.is_empty() {
                        msg_obj["reasoning_content"] = Value::String(reasoning_text);
                    }
                    if !tool_calls.is_empty() {
                        msg_obj["tool_calls"] = Value::Array(tool_calls);
                    }
                    out.push(msg_obj);
                }
            }
        }
    }

    out
}

pub fn convert_tools(anthropic_tools: &Value) -> Value {
    let Some(tools) = anthropic_tools.as_array() else {
        return json!([]);
    };

    Value::Array(
        tools
            .iter()
            .filter_map(|t| {
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": t.get("name")?,
                        "description": t.get("description")?,
                        "parameters": t.get("input_schema")?,
                    }
                }))
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ChunkDelta {
    content: Option<ContentDelta>,
    #[serde(alias = "reasoning")]
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum ContentDelta {
    Array(Vec<ContentDeltaPart>),
    String(String),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ContentDeltaPart {
    Text { text: String },
    Thinking { thinking: Vec<ThinkingDelta> },
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum ThinkingDelta {
    Block(ThinkingDeltaBlock),
    String(String),
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ThinkingDeltaBlock {
    Text { text: String },
}

#[derive(Deserialize)]
struct ChunkChoice {
    #[serde(alias = "message")]
    delta: Option<ChunkDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u32,
}

#[derive(Deserialize)]
struct ChunkUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    prompt_tokens_details: Option<PromptTokensDetails>,
    /// DeepSeek reports cache hits here instead of `prompt_tokens_details`.
    #[serde(default)]
    prompt_cache_hit_tokens: u32,
}

#[derive(Deserialize)]
struct SseChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    usage: Option<ChunkUsage>,
}

struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

pub async fn parse_sse(
    reader: impl AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();

    let mut text = String::new();
    let mut reasoning_text = String::new();
    let mut tool_accumulators: Vec<ToolAccumulator> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut is_first_content = true;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = super::next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None => continue,
        };

        if data == STREAM_DONE {
            break;
        }

        if data.contains("\"error\"")
            && let Ok(ev) = serde_json::from_str::<super::SseErrorPayload>(data)
        {
            warn!(error_type = %ev.error.r#type, message = %ev.error.message, "SSE error in stream");
            return Err(ev.into_agent_error());
        }

        let chunk: SseChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to parse SSE chunk");
                continue;
            }
        };

        if let Some(u) = chunk.usage {
            let cached = u
                .prompt_tokens_details
                .map_or(0, |d| d.cached_tokens)
                .max(u.prompt_cache_hit_tokens);
            usage = TokenUsage {
                input: u.prompt_tokens.saturating_sub(cached),
                output: u.completion_tokens,
                cache_read: cached,
                cache_creation: 0,
            };
        }

        let Some(choice) = chunk.choices.into_iter().next() else {
            continue;
        };

        if let Some(reason) = choice.finish_reason {
            stop_reason = Some(StopReason::from_openai(&reason));
        }

        let Some(delta) = choice.delta else {
            continue;
        };

        if let Some(reasoning) = delta.reasoning_content
            && !reasoning.is_empty()
        {
            reasoning_text.push_str(&reasoning);
            event_tx
                .send_async(ProviderEvent::ThinkingDelta { text: reasoning })
                .await?;
        }

        match delta.content {
            Some(ContentDelta::String(content_str)) if !content_str.is_empty() => {
                let content = if is_first_content {
                    is_first_content = false;
                    content_str.trim_start().to_string()
                } else {
                    content_str
                };

                if !content.is_empty() {
                    text.push_str(&content);
                    event_tx
                        .send_async(ProviderEvent::TextDelta { text: content })
                        .await?;
                }
            }
            Some(ContentDelta::Array(content_array)) => {
                for part in content_array {
                    match part {
                        ContentDeltaPart::Thinking { thinking } => {
                            for thinking_block in thinking {
                                let content = match thinking_block {
                                    ThinkingDelta::Block(ThinkingDeltaBlock::Text {
                                        text: content_str,
                                    }) => content_str,
                                    ThinkingDelta::String(content_str) => content_str,
                                };

                                if content.is_empty() {
                                    continue;
                                }

                                reasoning_text.push_str(&content);
                                event_tx
                                    .send_async(ProviderEvent::ThinkingDelta { text: content })
                                    .await?;
                            }
                        }
                        ContentDeltaPart::Text { text: content_str } => {
                            let content = if is_first_content {
                                is_first_content = false;
                                content_str.trim_start().to_string()
                            } else {
                                content_str
                            };

                            if !content.is_empty() {
                                text.push_str(&content);
                                event_tx
                                    .send_async(ProviderEvent::TextDelta { text: content })
                                    .await?;
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        if let Some(tc_deltas) = delta.tool_calls {
            for tc in tc_deltas {
                while tool_accumulators.len() <= tc.index {
                    tool_accumulators.push(ToolAccumulator {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                }
                let acc = &mut tool_accumulators[tc.index];
                let was_unnamed = acc.name.is_empty();
                if let Some(id) = tc.id {
                    acc.id = id;
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        acc.name = name;
                    }
                    if let Some(args) = func.arguments {
                        acc.arguments.push_str(&args);
                    }
                }
                if was_unnamed && !acc.name.is_empty() {
                    event_tx
                        .send_async(ProviderEvent::ToolUseStart {
                            id: acc.id.clone(),
                            name: acc.name.clone(),
                        })
                        .await?;
                }
            }
        }
    }

    let mut content_blocks: Vec<ContentBlock> = Vec::new();

    if !reasoning_text.is_empty() {
        content_blocks.push(ContentBlock::Thinking {
            thinking: reasoning_text,
            signature: None,
        });
    }

    if !text.is_empty() {
        content_blocks.push(ContentBlock::Text { text });
    }

    for (idx, acc) in tool_accumulators.into_iter().enumerate() {
        let input: Value = match serde_json::from_str(&acc.arguments) {
            Ok(v) => {
                debug!(tool = %acc.name, json = %acc.arguments, "tool input JSON");
                v
            }
            Err(e) => {
                warn!(error = %e, tool = %acc.name, json = %acc.arguments, "malformed tool JSON, falling back to {{}}");
                Value::Object(Default::default())
            }
        };
        let id = if acc.id.is_empty() {
            warn!(raw_name = %acc.name, raw_args = %acc.arguments, "provider sent empty tool_use id; substituting placeholder");
            format!("craft_unnamed_{idx}")
        } else {
            acc.id
        };
        let name = if acc.name.is_empty() {
            warn!(%id, raw_args = %acc.arguments, "provider sent empty tool_use name; substituting placeholder");
            "craft_unknown_tool".to_owned()
        } else {
            acc.name
        };
        content_blocks.push(ContentBlock::ToolUse { id, name, input });
    }

    Ok(StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: content_blocks,
            ..Default::default()
        },
        usage,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::Cursor;

    const TEST_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

    #[tokio::test]
    async fn parse_sse_text_and_usage() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_tokens_details\":{\"cached_tokens\":40}}}\n\
\n\
data: [DONE]\n";

        let (tx, rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert_eq!(resp.usage.input, 60);
        assert_eq!(resp.usage.cache_read, 40);
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        assert!(
            matches!(&resp.message.content[0], ContentBlock::Text { text } if text == "Hello world")
        );
        assert!(!resp.message.has_tool_calls());

        let mut deltas = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let ProviderEvent::TextDelta { text } = e {
                deltas.push(text);
            }
        }
        assert_eq!(deltas, vec!["Hello", " world"]);
    }

    #[tokio::test]
    async fn parse_sse_deepseek_cache_hit_tokens() {
        let sse = "\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"prompt_cache_hit_tokens\":80,\"prompt_cache_miss_tokens\":20}}\n\
\n\
data: [DONE]\n";

        let (tx, _rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert_eq!(resp.usage.input, 20);
        assert_eq!(resp.usage.cache_read, 80);
        assert_eq!(resp.usage.output, 10);
    }

    #[tokio::test]
    async fn parse_sse_reasoning_and_content() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Let me think\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"...\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\
\n\
data: [DONE]\n";

        let (tx, rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert!(
            matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think...")
        );
        assert!(matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello"));

        let mut thinking = Vec::new();
        let mut text_deltas = Vec::new();
        while let Ok(e) = rx.try_recv() {
            match e {
                ProviderEvent::ThinkingDelta { text } => thinking.push(text),
                ProviderEvent::TextDelta { text } => text_deltas.push(text),
                ProviderEvent::ToolUseStart { .. } => {}
                ProviderEvent::PromptProgress { .. } => {}
            }
        }
        assert_eq!(thinking, vec!["Let me think", "..."]);
        assert_eq!(text_deltas, vec!["Hello"]);
    }

    #[tokio::test]
    async fn parse_sse_reasoning_alias() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"reasoning\":\"Let me think\"}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"reasoning\":\"...\"}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\
\n\
data: [DONE]\n";

        let (tx, _rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert!(
            matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think...")
        );
    }

    #[test]
    fn convert_messages_structure() {
        let messages = vec![
            Message::user("hello".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "thinking...".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tc_1".to_string(),
                        name: "bash".to_string(),
                        input: json!({"command": "ls"}),
                    },
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_1".to_string(),
                    content: "file.txt".to_string(),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];

        let wire = convert_messages(&messages, "be helpful");

        assert_eq!(wire[0]["role"], "system");
        assert_eq!(wire[0]["content"], "be helpful");
        assert_eq!(wire[1]["role"], "user");
        assert_eq!(wire[1]["content"], "hello");
        assert_eq!(wire[2]["role"], "assistant");
        assert_eq!(wire[2]["content"], "thinking...");
        assert_eq!(wire[2]["tool_calls"][0]["id"], "tc_1");
        assert_eq!(wire[2]["tool_calls"][0]["type"], "function");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(wire[3]["role"], "tool");
        assert_eq!(wire[3]["tool_call_id"], "tc_1");
        assert_eq!(wire[3]["content"], "file.txt");
    }

    #[test]
    fn convert_tools_structure() {
        let anthropic = json!([{
            "name": "bash",
            "description": "Run a command",
            "input_schema": {
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            }
        }]);

        let openai = convert_tools(&anthropic);
        let tool = &openai[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "bash");
        assert_eq!(tool["function"]["description"], "Run a command");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[tokio::test]
    async fn parse_sse_multiple_parallel_tool_calls() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"command\\\": \\\"ls\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"path\\\": \\\"/tmp\\\"}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3}}\n\
\n\
data: [DONE]\n";

        let (tx, rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        let tools: Vec<_> = resp.message.tool_uses().collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].0, "c1");
        assert_eq!(tools[0].1, "bash");
        assert_eq!(tools[0].2["command"], "ls");
        assert_eq!(tools[1].0, "c2");
        assert_eq!(tools[1].1, "read");
        assert_eq!(tools[1].2["path"], "/tmp");
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));

        let starts: Vec<_> = rx
            .drain()
            .filter_map(|e| match e {
                ProviderEvent::ToolUseStart { id, name } => Some((id, name)),
                _ => None,
            })
            .collect();
        assert_eq!(
            starts,
            vec![("c1".into(), "bash".into()), ("c2".into(), "read".into()),]
        );
    }

    #[tokio::test]
    async fn parse_sse_error_payload_returns_err() {
        let sse = "\
data: {\"error\":{\"message\":\"Server overloaded\",\"type\":\"overloaded_error\"}}\n";

        let (tx, _rx) = flume::unbounded();
        let err = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap_err();

        match err {
            AgentError::Api { status, message } => {
                assert_eq!(status, 529);
                assert_eq!(message, "Server overloaded");
            }
            other => panic!("expected Api error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn parse_sse_empty_tool_id_and_name_get_placeholders() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"tool_calls\\\":[{\\\"tool\\\":\\\"read\\\"}]}\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\
\n\
data: [DONE]\n";

        let (tx, _rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        let tools: Vec<_> = resp.message.tool_uses().collect();
        assert_eq!(tools.len(), 1);
        assert!(!tools[0].0.is_empty(), "id must be non-empty for Bedrock");
        assert!(!tools[0].1.is_empty(), "name must be non-empty for Bedrock");
    }

    #[tokio::test]
    async fn parse_sse_malformed_tool_json_yields_empty_object() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"bash\",\"arguments\":\"\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{broken\"}}]}}]}\n\
\n\
data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\
\n\
data: [DONE]\n";

        let (tx, _rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        let tools: Vec<_> = resp.message.tool_uses().collect();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].1, "bash");
        assert_eq!(*tools[0].2, Value::Object(Default::default()));
    }

    #[test]
    fn convert_messages_user_with_image() {
        use crate::types::{ImageMediaType, ImageSource};
        use std::sync::Arc;
        let source = ImageSource::new(ImageMediaType::Png, Arc::from("abc123"));
        let msgs = vec![Message::user_with_images("describe".into(), vec![source])];
        let result = convert_messages(&msgs, "system");
        let user = &result[1];
        let content = user["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image_url");
        assert!(
            content[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "describe");
    }

    #[test]
    fn convert_messages_user_text_only_stays_string() {
        let msgs = vec![Message::user("hello".into())];
        let result = convert_messages(&msgs, "system");
        assert!(result[1]["content"].is_string());
    }

    #[test]
    fn convert_messages_assistant_with_reasoning() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "Let me think...".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "Hello".into(),
                },
            ],
            ..Default::default()
        }];
        let wire = convert_messages(&messages, "");
        let asst = &wire[1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["content"], "Hello");
        assert_eq!(asst["reasoning_content"], "Let me think...");
    }

    #[test]
    fn convert_messages_assistant_reasoning_only() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "Just thinking...".into(),
                signature: None,
            }],
            ..Default::default()
        }];
        let wire = convert_messages(&messages, "");
        let asst = &wire[1];
        assert_eq!(asst["role"], "assistant");
        assert_eq!(asst["reasoning_content"], "Just thinking...");
        assert_eq!(asst["content"], "");
    }

    #[test]
    fn convert_messages_assistant_tool_calls_only_has_content() {
        let messages = vec![
            Message::user("list files".to_string()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tc_1".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command": "ls"}),
                }],
                ..Default::default()
            },
        ];

        let wire = convert_messages(&messages, "be helpful");

        assert_eq!(wire[2]["role"], "assistant");
        // `content` must be a present string ("") even with only tool_calls;
        // strict OpenAI-compatible backends reject null/omitted content.
        assert_eq!(wire[2]["content"], "");
        assert_eq!(wire[2]["tool_calls"][0]["function"]["name"], "bash");
    }

    #[tokio::test]
    async fn parse_sse_empty_stream() {
        let sse = "data: [DONE]\n";
        let (tx, _rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();
        assert!(resp.message.content.is_empty());
        assert_eq!(resp.usage, TokenUsage::default());
        assert_eq!(resp.stop_reason, None);
    }

    #[tokio::test]
    async fn parse_sse_content_as_array_with_thinking() {
        let sse = "\
data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"Let me think\"}]}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"thinking\",\"thinking\":[{\"type\":\"text\",\"text\":\"...\"}]}]}}]}\n\
\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
\n\
data: [DONE]\n";

        let (tx, rx) = flume::unbounded();
        let resp = parse_sse(Cursor::new(sse.as_bytes()), &tx, TEST_STREAM_TIMEOUT)
            .await
            .unwrap();

        assert!(
            matches!(&resp.message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "Let me think..."),
            "{:?}",
            resp.message.content[0],
        );
        assert!(matches!(&resp.message.content[1], ContentBlock::Text { text } if text == "Hello"));

        let mut thinking_deltas = Vec::new();
        let mut text_deltas = Vec::new();
        while let Ok(e) = rx.try_recv() {
            match e {
                ProviderEvent::ThinkingDelta { text } => thinking_deltas.push(text),
                ProviderEvent::TextDelta { text } => text_deltas.push(text),
                _ => {}
            }
        }

        assert_eq!(text_deltas, vec!["Hello"]);
        assert_eq!(thinking_deltas, vec!["Let me think", "..."]);
    }

    #[test]
    fn convert_messages_tool_result_with_image_splits_user_message() {
        use crate::{ImageMediaType, ImageSource};
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tc_img".to_string(),
                    name: "browser_screenshot".to_string(),
                    input: json!({"url": "https://example.com"}),
                }],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tc_img".to_string(),
                    content: "screenshot of https://example.com".to_string(),
                    images: vec![ImageSource::new(
                        ImageMediaType::Png,
                        std::sync::Arc::from("aGVsbG8="),
                    )],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let wire = convert_messages(&messages, "be helpful");

        let tool_msg = &wire[2];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["tool_call_id"], "tc_img");
        assert_eq!(tool_msg["content"], "screenshot of https://example.com");

        let user_img_msg = &wire[3];
        assert_eq!(user_img_msg["role"], "user");
        let parts = user_img_msg["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert!(
            parts[0]["text"]
                .as_str()
                .unwrap()
                .contains("[image result of tc_img]")
        );
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
    }
}
