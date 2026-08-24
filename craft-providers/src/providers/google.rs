use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use flume::Sender;
use futures::TryStreamExt;
use futures::io::{AsyncBufReadExt, BufReader};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::io::StreamReader;
use tracing::warn;

use craft_storage::id::{CraftId, SessionRef};

use crate::model::{Model, ModelEntry, ModelFamily, ModelPricing, ModelTier};
use crate::provider::Provider;
use crate::{
    AgentError, ContentBlock, Message, ProviderEvent, RequestOptions, Role, StopReason,
    StreamResponse, ThinkingConfig, TokenUsage,
};

use super::{KeyPool, MIME_JSON, ResolvedAuth, http_client, lock_unpoison, next_sse_line};

const BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
const ENV_VAR: &str = "GEMINI_API_KEY";
const FLASH_MAX_THINKING: u32 = 24_576;
const PRO_MAX_THINKING: u32 = 32_768;

/// The generic per-model max, capped by Google's documented `thinkingBudget`
/// hard limits per family.
fn max_thinking(model: &Model) -> u32 {
    let cap = if model.id.contains("flash") {
        FLASH_MAX_THINKING
    } else {
        PRO_MAX_THINKING
    };
    model.max_thinking_budget().map_or(cap, |m| m.min(cap))
}

inventory::submit!(craft_config::providers::BuiltInProvider {
    slug: "google",
    display_name: "Google",
    protocol: craft_config::providers::Protocol::Google,
    default_base_url: BASE_URL,
    default_api_key_env: ENV_VAR,
    default_model: "google/gemini-2.5-pro",
    plans: None,
    login_url: Some("https://aistudio.google.com/apikey"),
    needs_url: false,
});

pub(crate) const fn models() -> &'static [ModelEntry] {
    &[
        ModelEntry {
            prefixes: &["gemini-2.5-pro"],
            tier: ModelTier::Strong,
            family: ModelFamily::Gemini,
            default: true,
            pricing: ModelPricing {
                input: 1.25,
                output: 5.00,
                cache_write: 0.00,
                cache_read: 0.31,
                fast: None,
            },
            max_output_tokens: Some(65_536),
            context_window: 1_048_576,
            supports_vision: true,
        },
        ModelEntry {
            prefixes: &["gemini-2.5-flash"],
            tier: ModelTier::Medium,
            family: ModelFamily::Gemini,
            default: true,
            pricing: ModelPricing {
                input: 0.15,
                output: 0.60,
                cache_write: 0.00,
                cache_read: 0.04,
                fast: None,
            },
            max_output_tokens: Some(65_536),
            context_window: 1_048_576,
            supports_vision: true,
        },
        ModelEntry {
            prefixes: &["gemini-2.0-flash-lite"],
            tier: ModelTier::Weak,
            family: ModelFamily::Gemini,
            default: true,
            pricing: ModelPricing {
                input: 0.075,
                output: 0.30,
                cache_write: 0.00,
                cache_read: 0.01,
                fast: None,
            },
            max_output_tokens: Some(65_536),
            context_window: 1_048_576,
            supports_vision: true,
        },
    ]
}

fn resolve_google_base_url() -> Option<String> {
    let config = craft_config::providers::ProvidersConfig::load();
    craft_config::providers::resolve_base_url("google", config.get("google"))
}

fn resolve_auth_from_key(key: &str, base_url: Option<String>) -> ResolvedAuth {
    ResolvedAuth {
        base_url,
        headers: vec![("x-goog-api-key".into(), key.to_string())],
    }
}

pub struct Google {
    client: Client,
    auth: Arc<Mutex<ResolvedAuth>>,
    key_pool: Option<KeyPool>,
    stream_timeout: Duration,
    /// Env / `providers.toml` / inventory default, resolved once at construction.
    resolved_base_url: Option<String>,
}

impl Google {
    pub fn new(timeouts: super::Timeouts) -> Result<Self, AgentError> {
        let pool = KeyPool::resolve("google", ENV_VAR)?;
        let resolved_base_url = resolve_google_base_url();
        let resolved = resolve_auth_from_key(pool.current(), resolved_base_url.clone());
        Ok(Self {
            client: http_client(timeouts)?,
            auth: Arc::new(Mutex::new(resolved)),
            key_pool: Some(pool),
            stream_timeout: timeouts.stream,
            resolved_base_url,
        })
    }

    pub(crate) fn with_auth(
        auth: Arc<Mutex<super::ResolvedAuth>>,
        timeouts: super::Timeouts,
    ) -> Result<Self, AgentError> {
        let resolved_base_url = auth.lock().unwrap().base_url.clone();
        Ok(Self {
            client: http_client(timeouts)?,
            auth,
            key_pool: None,
            stream_timeout: timeouts.stream,
            resolved_base_url,
        })
    }

    fn build_request(&self, method: &str, url: &str) -> reqwest::RequestBuilder {
        let auth = lock_unpoison(&self.auth);
        auth.configure_request(
            self.client
                .request(method.parse().unwrap(), url)
                .header("user-agent", super::user_agent()),
        )
    }

    fn api_key(&self) -> String {
        let auth = lock_unpoison(&self.auth);
        auth.headers
            .iter()
            .find(|(k, _)| k == "x-goog-api-key")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn stream_url(&self, model_id: &str) -> String {
        let base = {
            let auth = lock_unpoison(&self.auth);
            auth.base_url.as_deref().unwrap_or(BASE_URL).to_string()
        };
        let encoded = super::urlenc(model_id);
        format!("{base}/models/{encoded}:streamGenerateContent?alt=sse")
    }

    fn models_url(&self) -> String {
        let base = {
            let auth = lock_unpoison(&self.auth);
            auth.base_url.as_deref().unwrap_or(BASE_URL).to_string()
        };
        let key = self.api_key();
        format!("{base}/models?key={}&pageSize=1000", super::urlenc(&key))
    }

    fn build_body(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        thinking: ThinkingConfig,
    ) -> Value {
        let mut body = json!({
            "contents": convert_messages(messages),
        });

        if !system.is_empty() {
            body["systemInstruction"] = json!({"parts": [{"text": system}]});
        }

        thinking.apply_google_thinking(&mut body, max_thinking(model));

        if let Some(max_output) = model.max_output_tokens {
            body["generationConfig"]["maxOutputTokens"] = json!(max_output);
        }

        let tool_decls = convert_tools(tools);
        if !tool_defs_empty(&tool_decls) {
            body["tools"] = json!([{"functionDeclarations": tool_decls}]);
        }

        body
    }

    async fn do_stream(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        thinking: ThinkingConfig,
    ) -> Result<StreamResponse, AgentError> {
        let body = self.build_body(model, messages, system, tools, thinking);
        let url = self.stream_url(&model.id);
        let json_body = serde_json::to_vec(&body)?;

        let response = self
            .build_request("POST", &url)
            .header("content-type", MIME_JSON)
            .body(json_body)
            .send()
            .await?;
        let status = response.status().as_u16();

        if status == 200 {
            let stream = response.bytes_stream();
            let reader = StreamReader::new(stream.map_err(std::io::Error::other));
            let reader = BufReader::new(reader.compat());
            parse_sse(reader, event_tx, self.stream_timeout).await
        } else {
            Err(AgentError::from_response(response).await)
        }
    }
}

#[async_trait]
impl Provider for Google {
    async fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &Value,
        event_tx: &Sender<ProviderEvent>,
        opts: RequestOptions,
        _session_id: Option<&SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        self.do_stream(model, messages, system, tools, event_tx, opts.thinking)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        let request = self.build_request("GET", &self.models_url());
        let response = request.send().await?;
        if response.status().as_u16() != 200 {
            return Err(AgentError::from_response(response).await);
        }
        let body_text = response.text().await?;
        let models_response: ModelsListResponse = serde_json::from_str(&body_text)?;
        let mut ids: Vec<String> = models_response
            .models
            .into_iter()
            .filter(|m| {
                m.supported_generation_methods
                    .iter()
                    .any(|m| m == "generateContent")
            })
            .map(|m| {
                m.name
                    .strip_prefix("models/")
                    .map(String::from)
                    .unwrap_or(m.name)
            })
            .collect();
        ids.sort();
        Ok(ids)
    }

    async fn reload_auth(&self) -> Result<(), AgentError> {
        let pool = KeyPool::resolve("google", ENV_VAR)?;
        *lock_unpoison(&self.auth) =
            resolve_auth_from_key(pool.current(), self.resolved_base_url.clone());
        Ok(())
    }

    async fn rotate_key(&self) -> Result<bool, AgentError> {
        let base_url = self.resolved_base_url.clone();
        Ok(self.key_pool.as_ref().is_some_and(|p| {
            p.rotate_auth(&self.auth, |key| {
                resolve_auth_from_key(key, base_url.clone())
            })
        }))
    }
}

fn convert_messages(messages: &[Message]) -> Vec<Value> {
    let tool_names: std::collections::HashMap<&str, &str> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();

    let mut out: Vec<Value> = Vec::new();

    for msg in messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "model",
        };

        let mut parts: Vec<Value> = Vec::new();

        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    parts.push(json!({"text": text}));
                }
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    let mut part = json!({"text": thinking, "thought": true});
                    if let Some(sig) = signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                ContentBlock::RedactedThinking { .. } => {}
                ContentBlock::ToolUse {
                    id: _,
                    name,
                    input,
                    thought_signature,
                } => {
                    let mut part = json!({
                        "functionCall": {
                            "name": name,
                            "args": input,
                        }
                    });
                    if let Some(sig) = thought_signature {
                        part["thoughtSignature"] = json!(sig);
                    }
                    parts.push(part);
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    images,
                    is_error,
                } => {
                    let parsed = serde_json::from_str::<Value>(content);
                    let mut response_val = match parsed {
                        Ok(Value::Object(map)) => Value::Object(map),
                        Ok(other) => json!({"result": other}),
                        Err(_) => json!({"result": content}),
                    };
                    if *is_error {
                        response_val = json!({"error": response_val});
                    }
                    let name = tool_names
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("unknown");
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": response_val,
                        }
                    }));
                    for source in images {
                        parts.push(json!({
                            "inlineData": {
                                "mimeType": source.media_type.as_mime(),
                                "data": source.data,
                            }
                        }));
                    }
                }
                ContentBlock::Image { source } => {
                    parts.push(json!({
                        "inlineData": {
                            "mimeType": source.media_type.as_mime(),
                            "data": source.data,
                        }
                    }));
                }
            }
        }

        if !parts.is_empty() {
            out.push(json!({"role": role, "parts": parts}));
        }
    }

    out
}

fn convert_tools(tools: &Value) -> Vec<Value> {
    let Some(arr) = tools.as_array() else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let description = t.get("description")?.as_str().unwrap_or("");
            let parameters = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "name": name,
                "description": description,
                "parameters": strip_additional_properties(parameters),
            }))
        })
        .collect()
}

fn strip_additional_properties(value: Value) -> Value {
    match value {
        Value::Object(mut map) => {
            map.remove("additionalProperties");
            map.values_mut()
                .for_each(|v| *v = strip_additional_properties(std::mem::take(v)));
            Value::Object(map)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(strip_additional_properties).collect())
        }
        other => other,
    }
}

fn tool_defs_empty(tool_decls: &[Value]) -> bool {
    tool_decls.is_empty()
}

// --- SSE response types ---

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseCandidate {
    content: Option<SseContent>,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseContent {
    parts: Option<Vec<SsePart>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SsePart {
    text: Option<String>,
    thought: Option<bool>,
    thought_signature: Option<String>,
    function_call: Option<SseFunctionCall>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseFunctionCall {
    name: String,
    args: Option<Value>,
    #[serde(default)]
    thought_signature: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseUsageMetadata {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
    #[serde(default)]
    cached_content_token_count: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SseResponse {
    candidates: Option<Vec<SseCandidate>>,
    usage_metadata: Option<SseUsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelsListResponse {
    models: Vec<ModelInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelInfo {
    name: String,
    #[serde(default)]
    supported_generation_methods: Vec<String>,
}

/// Append `text` to the last block when it is already a `Text`, else push a new
/// `Text` block. Gemini streams a single assistant message as many small text
/// parts across SSE chunks; coalescing keeps them as one content block so
/// session restore renders one block instead of one per delta.
fn push_or_extend_text(blocks: &mut Vec<ContentBlock>, text: String) {
    if let Some(ContentBlock::Text { text: prev }) = blocks.last_mut() {
        prev.push_str(&text);
    } else {
        blocks.push(ContentBlock::Text { text });
    }
}

/// Same as `push_or_extend_text` for thinking parts. The `thoughtSignature`
/// arrives on the final thinking delta, so a later signature overwrites an
/// earlier one; a `Some` on an earlier delta is preserved if the last is None.
fn push_or_extend_thinking(
    blocks: &mut Vec<ContentBlock>,
    text: String,
    signature: Option<String>,
) {
    if let Some(ContentBlock::Thinking {
        thinking: prev,
        signature: prev_sig,
    }) = blocks.last_mut()
    {
        prev.push_str(&text);
        if signature.is_some() {
            *prev_sig = signature;
        }
    } else {
        blocks.push(ContentBlock::Thinking {
            thinking: text,
            signature,
        });
    }
}

async fn parse_sse(
    reader: impl futures::io::AsyncBufRead + Unpin,
    event_tx: &Sender<ProviderEvent>,
    stream_timeout: Duration,
) -> Result<StreamResponse, AgentError> {
    let mut lines = reader.lines();

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;
    let mut deadline = Instant::now() + stream_timeout;

    while let Some(line) = next_sse_line(&mut lines, &mut deadline, stream_timeout).await? {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.strip_prefix(' ').unwrap_or(d),
            _ => continue,
        };

        let chunk: SseResponse = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "failed to parse Gemini SSE chunk");
                continue;
            }
        };

        if let Some(meta) = chunk.usage_metadata {
            usage.input = meta.prompt_token_count;
            usage.output = meta.candidates_token_count;
            if let Some(cached) = meta.cached_content_token_count {
                usage.cache_read = cached;
            }
        }

        let Some(candidates) = chunk.candidates else {
            continue;
        };

        for candidate in candidates {
            if let Some(reason) = candidate.finish_reason {
                stop_reason = stop_reason.or(Some(StopReason::from_google(&reason)));
            }

            let Some(content) = candidate.content else {
                continue;
            };
            let Some(parts) = content.parts else {
                continue;
            };

            for part in parts {
                if let Some(func_call) = part.function_call {
                    let id = format!("call_{}_{}", func_call.name, CraftId::generate());
                    let input = func_call.args.unwrap_or_default();
                    let thought_signature = func_call.thought_signature.or(part.thought_signature);
                    event_tx
                        .send_async(ProviderEvent::ToolUseStart {
                            id: id.clone(),
                            name: func_call.name.clone(),
                        })
                        .await?;
                    content_blocks.push(ContentBlock::ToolUse {
                        id,
                        name: func_call.name,
                        input,
                        thought_signature,
                    });
                    stop_reason = Some(StopReason::ToolUse);
                } else if let Some(text) = part.text {
                    if part.thought.unwrap_or(false) {
                        if !text.is_empty() {
                            event_tx
                                .send_async(ProviderEvent::ThinkingDelta { text: text.clone() })
                                .await?;
                        }
                        push_or_extend_thinking(&mut content_blocks, text, part.thought_signature);
                    } else if !text.is_empty() {
                        // TODO: preserve part.thought_signature if ContentBlock::Text adds signature support.
                        event_tx
                            .send_async(ProviderEvent::TextDelta { text: text.clone() })
                            .await?;
                        push_or_extend_text(&mut content_blocks, text);
                    }
                }
            }
        }
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
    use std::sync::Arc;
    use test_case::test_case;

    const GEMINI_API_KEY: &str = "test-key";

    fn test_auth() -> Arc<Mutex<ResolvedAuth>> {
        Arc::new(Mutex::new(ResolvedAuth {
            base_url: None,
            headers: vec![("x-goog-api-key".into(), GEMINI_API_KEY.into())],
        }))
    }

    fn test_timeouts() -> super::super::Timeouts {
        super::super::Timeouts {
            connect: Duration::from_secs(5),
            low_speed: Duration::from_secs(30),
            stream: Duration::from_secs(300),
        }
    }

    fn test_model() -> Model {
        Model {
            id: "gemini-2.5-flash".into(),
            provider: Arc::<str>::from("google"),
            tier: ModelTier::Medium,
            family: ModelFamily::Gemini,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: Some(true),
            pricing: ModelPricing::default(),
            discovered_free: false,
            max_output_tokens: Some(8192),
            context_window: 1_048_576,
            thinking_fields: None,
        }
    }

    #[test]
    fn google_build_body_basic() {
        let google = Google::with_auth(test_auth(), test_timeouts()).unwrap();
        let model = test_model();
        let messages = vec![Message::user("hello".into())];
        let body = google.build_body(
            &model,
            &messages,
            "be helpful",
            &json!([]),
            ThinkingConfig::Off,
        );

        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be helpful");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn google_build_body_thinking_adaptive() {
        let google = Google::with_auth(test_auth(), test_timeouts()).unwrap();
        let messages = vec![Message::user("think".into())];
        let body = google.build_body(
            &test_model(),
            &messages,
            "",
            &json!([]),
            ThinkingConfig::Adaptive,
        );

        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn google_build_body_thinking_budget() {
        let google = Google::with_auth(test_auth(), test_timeouts()).unwrap();
        let messages = vec![Message::user("think hard".into())];
        let body = google.build_body(
            &test_model(),
            &messages,
            "",
            &json!([]),
            ThinkingConfig::Budget(8192),
        );

        // Clamped to the model's max thinking budget (half of 8192 output tokens).
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            4096
        );
    }

    #[test_case("STOP", StopReason::EndTurn ; "stop")]
    #[test_case("MAX_TOKENS", StopReason::MaxTokens ; "max_tokens")]
    #[test_case("SAFETY", StopReason::EndTurn ; "safety")]
    #[test_case("RECITATION", StopReason::EndTurn ; "recitation")]
    #[test_case("unknown", StopReason::EndTurn ; "unknown")]
    fn stop_reason_from_google(input: &str, expected: StopReason) {
        assert_eq!(StopReason::from_google(input), expected);
    }

    #[test]
    fn convert_messages_user_and_assistant() {
        let messages = vec![
            Message::user("hello".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "hi there".into(),
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["role"], "user");
        assert_eq!(result[0]["parts"][0]["text"], "hello");
        assert_eq!(result[1]["role"], "model");
        assert_eq!(result[1]["parts"][0]["text"], "hi there");
    }

    #[test]
    fn convert_messages_thinking_block() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: Some("sig123".into()),
            }],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["text"], "hmm");
        assert_eq!(result[0]["parts"][0]["thought"], true);
        assert_eq!(result[0]["parts"][0]["thoughtSignature"], "sig123");
    }

    #[test]
    fn convert_messages_tool_use_and_result() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call_1",
                    "read_file",
                    json!({"path": "/tmp/a"}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "file contents".into(),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["name"],
            "read_file"
        );
    }

    #[test_case("not json at all", json!({"result": "not json at all"}) ; "non_json_wraps_string")]
    #[test_case(r#""a json string""#, json!({"result": "a json string"}) ; "json_scalar_wraps")]
    #[test_case("42", json!({"result": 42}) ; "json_number_wraps")]
    #[test_case(r#"{"out": "ok"}"#, json!({"out": "ok"}) ; "json_object_passes_through")]
    fn convert_messages_tool_result_response_is_always_struct(content: &str, expected: Value) {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("call_1", "read", json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: content.into(),
                    images: vec![],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["response"],
            expected
        );
    }

    #[test]
    fn convert_messages_tool_result_error_wraps_response() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use("call_1", "read", json!({}))],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "boom".into(),
                    images: vec![],
                    is_error: true,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        assert_eq!(
            result[1]["parts"][0]["functionResponse"]["response"],
            json!({"error": {"result": "boom"}})
        );
    }

    #[test]
    fn convert_messages_tool_use_preserves_thought_signature() {
        const SIG: &str = "sig-abc";
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: json!({"path": "/tmp/a"}),
                thought_signature: Some(SIG.into()),
            }],
            ..Default::default()
        }];
        let result = convert_messages(&messages);
        assert_eq!(result[0]["parts"][0]["functionCall"]["name"], "read_file");
        assert_eq!(result[0]["parts"][0]["thoughtSignature"], SIG);
        assert!(
            result[0]["parts"][0]["functionCall"]
                .get("thoughtSignature")
                .is_none()
        );
    }

    #[test]
    fn convert_tools_basic() {
        let tools = json!([{
            "name": "bash",
            "description": "run a command",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "additionalProperties": false},
                    "opts": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"verbose": {"type": "boolean"}}
                    }
                },
                "additionalProperties": false
            }
        }]);
        let result = convert_tools(&tools);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["name"], "bash");
        assert_eq!(result[0]["description"], "run a command");
        assert!(result[0]["parameters"]["properties"]["cmd"].is_object());
        assert!(
            result[0]["parameters"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            result[0]["parameters"]["properties"]["cmd"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            result[0]["parameters"]["properties"]["opts"]
                .get("additionalProperties")
                .is_none()
        );
    }

    #[test]
    fn strip_additional_properties_recursive() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "inner": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"x": {"type": "number", "additionalProperties": false}}
                },
                "list": {
                    "type": "array",
                    "items": {"type": "string", "additionalProperties": false}
                }
            }
        });
        let cleaned = strip_additional_properties(schema);
        assert!(cleaned.get("additionalProperties").is_none());
        assert!(
            cleaned["properties"]["inner"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            cleaned["properties"]["inner"]["properties"]["x"]
                .get("additionalProperties")
                .is_none()
        );
        assert!(
            cleaned["properties"]["list"]["items"]
                .get("additionalProperties")
                .is_none()
        );
    }

    #[test]
    fn models_list_has_defaults() {
        let models = models();
        assert!(!models.is_empty());
        for entry in models {
            assert!(!entry.prefixes.is_empty());
            assert!(entry.max_output_tokens.is_some_and(|t| t > 0));
            assert!(entry.context_window >= entry.max_output_tokens.unwrap());
        }
    }

    fn mock_sse(data: &'static [u8]) -> futures::io::Cursor<&'static [u8]> {
        futures::io::Cursor::new(data)
    }

    #[tokio::test]
    async fn parse_sse_plain_text() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":10}}\n\n";
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(result.usage.input, 5);
        assert_eq!(result.usage.output, 10);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Text { text } if text == "hello"
        ));
    }

    #[tokio::test]
    async fn parse_sse_thinking_part() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"thinking...\",\"thought\":true,\"thoughtSignature\":\"sig1\"}]}},{\"content\":{\"parts\":[{\"text\":\"answer\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":20}}\n\n";
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Thinking { thinking, signature } if thinking == "thinking..." && signature.as_deref() == Some("sig1")
        ));
        assert!(matches!(
            &result.message.content[1],
            ContentBlock::Text { text } if text == "answer"
        ));
    }

    #[tokio::test]
    async fn parse_sse_tool_call() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"bash\",\"args\":{\"cmd\":\"ls\"}}}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":15}}\n\n";
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(result.stop_reason, Some(StopReason::ToolUse));
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::ToolUse { name, .. } if name == "bash"
        ));
    }

    #[tokio::test]
    async fn parse_sse_coalesces_text_deltas_into_one_block() {
        let event = |text: &str| {
            format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\"}}]}}}}]}}\n\n",
                text
            )
        };
        let mut data = String::new();
        data.push_str(&event("Hello, "));
        data.push_str(&event("world."));
        let data = Box::leak(data.into_boxed_str());
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data.as_bytes()), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(
            result.message.content.len(),
            1,
            "expected one coalesced text block"
        );
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Text { text } if text == "Hello, world."
        ));
    }

    #[tokio::test]
    async fn parse_sse_coalesces_thinking_deltas_keeps_last_signature() {
        let event = |text: &str, sig: Option<&str>| match sig {
            Some(s) => format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\",\"thought\":true,\"thoughtSignature\":\"{}\"}}]}}}}]}}\n\n",
                text, s
            ),
            None => format!(
                "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{}\",\"thought\":true}}]}}}}]}}\n\n",
                text
            ),
        };
        let mut data = String::new();
        data.push_str(&event("reasoning... ", None));
        data.push_str(&event("more.", Some("sig-final")));
        let data = Box::leak(data.into_boxed_str());
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data.as_bytes()), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(result.message.content.len(), 1);
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::Thinking { thinking, signature }
                if thinking == "reasoning... more." && signature.as_deref() == Some("sig-final")
        ));
    }

    #[tokio::test]
    async fn parse_sse_parallel_same_name_tool_calls_get_unique_ids() {
        let part = r#"{"functionCall":{"name":"bash","args":{"cmd":"ls"}}}"#;
        let payload = format!(r#"{{"candidates":[{{"content":{{"parts":[{part},{part}]}}}}]}}"#,);
        let data = Box::leak(format!("data: {payload}\n\n").into_boxed_str());
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data.as_bytes()), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        let ids: Vec<&str> = result
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "expected two tool calls");
        assert_ne!(ids[0], ids[1], "ids must be unique for parallel calls");
    }

    #[tokio::test]
    async fn parse_sse_cached_tokens() {
        let data = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":50}}\n\n";
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(result.usage.input, 100);
        assert_eq!(result.usage.output, 10);
        assert_eq!(result.usage.cache_read, 50);
    }

    #[tokio::test]
    async fn parse_sse_tool_call_captures_part_level_thought_signature() {
        parse_sse_tool_call_signature(
            r#"{"functionCall":{"name":"bash","args":{"cmd":"ls"}},"thoughtSignature":"CvcQAdHtim/pKv/c0ClPFkYA=="}"#,
        )
        .await;
    }

    #[tokio::test]
    async fn parse_sse_tool_call_captures_nested_thought_signature() {
        parse_sse_tool_call_signature(
            r#"{"functionCall":{"name":"bash","args":{"cmd":"ls"},"thoughtSignature":"CvcQAdHtim/pKv/c0ClPFkYA=="}}"#,
        )
        .await;
    }

    async fn parse_sse_tool_call_signature(part_json: &str) {
        const SIG: &str = "CvcQAdHtim/pKv/c0ClPFkYA==";
        let payload = format!(
            r#"{{"candidates":[{{"content":{{"parts":[{part_json}]}},"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":5,"candidatesTokenCount":15}}}}"#,
        );
        let data = Box::leak(format!("data: {payload}\n\n").into_boxed_str());
        let (tx, _rx) = flume::unbounded();
        let result = parse_sse(mock_sse(data.as_bytes()), &tx, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(matches!(
            &result.message.content[0],
            ContentBlock::ToolUse { thought_signature: Some(s), .. } if s == SIG
        ));
    }

    #[test]
    fn convert_messages_tool_result_with_image() {
        use crate::{ImageMediaType, ImageSource};
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    "call_1",
                    "browser_screenshot",
                    json!({"url": "https://example.com"}),
                )],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "screenshot of https://example.com".into(),
                    images: vec![ImageSource::new(ImageMediaType::Png, Arc::from("aGVsbG8="))],
                    is_error: false,
                }],
                ..Default::default()
            },
        ];
        let result = convert_messages(&messages);
        let parts = &result[1]["parts"];
        assert_eq!(parts[0]["functionResponse"]["name"], "browser_screenshot");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "aGVsbG8=");
    }
}
