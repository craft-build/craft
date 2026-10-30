//! Scripted animist provider for tests: replayable attempts, recorded
//! requests, and turn/error script helpers.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use animist::{
    flume::Sender,
    history::{ContentBlock, Message, MessageKind, Role},
    jiff,
    provider::{
        Config, Features, Model, ModelInfo, Pricing, Provider, ProviderError, ProviderEvent,
        ProviderImpl, RequestOptions, StopReason, StreamResponse, Thinking, Tier, TokenUsage, Tool,
    },
};
use async_trait::async_trait;
use snafu::GenerateImplicitData;
use tokio::sync::watch;

/// One scripted inference attempt: streamed events, then the final outcome.
#[derive(Debug)]
pub struct Attempt {
    events: Vec<ProviderEvent>,
    outcome: animist::provider::Result<StreamResponse>,
    blocked: bool,
}

impl Attempt {
    /// Holds the attempt after its events stream until `ScriptedProvider::release`.
    pub fn blocked(mut self) -> Self {
        self.blocked = true;
        self
    }

    fn new(events: Vec<ProviderEvent>, outcome: animist::provider::Result<StreamResponse>) -> Self {
        Self {
            events,
            outcome,
            blocked: false,
        }
    }
}

/// Tool call spec for [`tool_turn`]: `(id, name, input)`.
pub type ToolCallSpec = (String, String, serde_json::Value);

/// One inference request exactly as the scripted provider received it.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub system: String,
    pub tools: Vec<Tool>,
    pub options: RequestOptions,
}

#[derive(Debug)]
struct Shared {
    attempts: Mutex<VecDeque<Attempt>>,
    requests: Mutex<Vec<RecordedRequest>>,
    models: Vec<ModelInfo>,
    refreshes: AtomicUsize,
    reloads: AtomicUsize,
    release: watch::Sender<bool>,
}

/// Replays scripted attempts and records every request it receives.
///
/// Limits inherited by every script: `release` is one-shot and opens every
/// gate for the rest of the script, attempts never emit a model-ownership
/// `ModelProviderMismatch`, and blocked attempts keep the events they already
/// streamed as the interrupted partial.
///
/// Clone the handle before wrapping it in a [`Model`]; every clone shares one
/// script queue, one recording log, and one release gate.
#[derive(Debug, Clone)]
pub struct ScriptedProvider {
    shared: Arc<Shared>,
}

impl ScriptedProvider {
    /// Provider whose `list_models` returns `models`.
    pub fn new(models: Vec<ModelInfo>) -> Self {
        Self {
            shared: Arc::new(Shared {
                attempts: Mutex::default(),
                requests: Mutex::default(),
                models,
                refreshes: AtomicUsize::new(0),
                reloads: AtomicUsize::new(0),
                release: watch::channel(false).0,
            }),
        }
    }

    /// Queues one attempt; attempts replay in the order pushed.
    pub fn push(&self, attempt: Attempt) -> &Self {
        self.shared
            .attempts
            .lock()
            .expect("script queue unpoisoned")
            .push_back(attempt);
        self
    }

    /// Requests received so far, in order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.shared
            .requests
            .lock()
            .expect("request log unpoisoned")
            .clone()
    }

    /// Completed `refresh_auth` calls.
    pub fn refresh_count(&self) -> usize {
        self.shared.refreshes.load(Ordering::Relaxed)
    }

    /// Completed `reload_auth` calls.
    pub fn reload_count(&self) -> usize {
        self.shared.reloads.load(Ordering::Relaxed)
    }

    /// Releases every blocked attempt, now and for the rest of the script.
    pub fn release(&self) {
        self.shared.release.send_replace(true);
    }
}

#[async_trait]
impl ProviderImpl for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn stream_message(
        &self,
        model: &Model,
        messages: &[Message],
        system: &str,
        tools: &[Tool],
        events: &Sender<ProviderEvent>,
        options: RequestOptions,
    ) -> animist::provider::Result<StreamResponse> {
        self.shared
            .requests
            .lock()
            .expect("request log unpoisoned")
            .push(RecordedRequest {
                model: model.id.clone(),
                messages: messages.to_vec(),
                system: system.to_owned(),
                tools: tools.to_vec(),
                options,
            });
        let attempt = self
            .shared
            .attempts
            .lock()
            .expect("script queue unpoisoned")
            .pop_front()
            .unwrap_or_else(|| panic!("scripted provider has no attempt left to replay"));
        for event in &attempt.events {
            events
                .send_async(event.clone())
                .await
                .expect("agent keeps its event channel open during an attempt");
        }
        if attempt.blocked {
            let mut released = self.shared.release.subscribe();
            while !*released.borrow_and_update() {
                released
                    .changed()
                    .await
                    .expect("release sender lives in the provider state");
            }
        }
        attempt.outcome
    }

    async fn list_models(&self) -> animist::provider::Result<Vec<ModelInfo>> {
        Ok(self.shared.models.clone())
    }

    async fn refresh_auth(&self) -> animist::provider::Result<()> {
        self.shared.refreshes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn reload_auth(&self) -> animist::provider::Result<()> {
        self.shared.reloads.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Wraps a clone of `provider` in an animist [`Model`] named by `info`'s first id.
pub fn scripted_model(provider: &ScriptedProvider, info: ModelInfo) -> Model {
    let id = info
        .ids
        .first()
        .cloned()
        .unwrap_or_else(|| "scripted".to_owned());
    Model::builder()
        .id(id)
        .provider(Arc::new(Provider::new(provider.clone())))
        .info(info)
        .build()
}

/// Full-capability `ModelInfo` for one canonical id, with `thinking` support
/// and `vision` toggled as given.
pub fn model_info(
    id: &str,
    context_window: u32,
    max_output: u32,
    thinking: bool,
    vision: bool,
) -> ModelInfo {
    let features = Features::builder()
        .tool_examples(false)
        .thinking(
            Thinking::builder()
                .supported(thinking)
                .required(false)
                .efforts(Vec::new())
                .build(),
        )
        .vision(vision)
        .deferred_tools(false)
        .build();
    ModelInfo::builder()
        .ids(vec![id.to_owned()])
        .tier(Tier::Medium)
        .features(features)
        .pricing(Pricing::default())
        .config(
            Config::builder()
                .context_window(context_window)
                .max_output_tokens(max_output)
                .build(),
        )
        .build()
}

/// An `ModelInfo` row with optional limits, for catalog rows exercising an
/// unknown window or output cap.
pub fn model_info_with(
    id: &str,
    context_window: Option<u32>,
    max_output: Option<u32>,
) -> ModelInfo {
    let mut config = Config::builder().build();
    config.context_window = context_window;
    config.max_output_tokens = max_output;
    ModelInfo::builder()
        .ids(vec![id.to_owned()])
        .tier(Tier::Medium)
        .features(
            Features::builder()
                .tool_examples(false)
                .thinking(
                    Thinking::builder()
                        .supported(false)
                        .required(false)
                        .efforts(Vec::new())
                        .build(),
                )
                .vision(false)
                .deferred_tools(false)
                .build(),
        )
        .pricing(Pricing::default())
        .config(config)
        .build()
}

/// Map recorded wire messages back onto craft history, the test-side
/// inverse of [`crate::edge::to_animist`]: assertions compare what a
/// request carried against the committed history. Assistant messages reuse
/// the production mapping (tool-call ids stay as the wire recorded them).
pub fn replay_history(messages: &[Message]) -> Vec<crate::history::Message> {
    use crate::history::{ToolResult, ToolResultContent, UserContent};
    use animist::history::{ContentBlock, ImageSource, Role, ToolContent};

    let image_block = |source: &ImageSource| match source {
        ImageSource::Base64 { media_type, data } => crate::history::ImageBlock {
            media_type: match media_type.as_str() {
                "image/jpeg" => crate::history::ImageMedia::Jpeg,
                "image/gif" => crate::history::ImageMedia::Gif,
                "image/webp" => crate::history::ImageMedia::Webp,
                _ => crate::history::ImageMedia::Png,
            },
            data: data.clone(),
            caption: "[image]".to_owned(),
        },
        ImageSource::Url { url } => crate::history::ImageBlock {
            media_type: crate::history::ImageMedia::Png,
            data: url.clone(),
            caption: "[image]".to_owned(),
        },
    };

    messages
        .iter()
        .map(|message| match message.role {
            Role::System => crate::history::Message::System {
                content: message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            },
            Role::User => crate::history::Message::User {
                content: message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(UserContent::text(text.clone())),
                        ContentBlock::Image { source } => {
                            Some(UserContent::Image(image_block(source)))
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            name,
                            content,
                            is_error,
                            ..
                        } => Some(UserContent::ToolResult(ToolResult {
                            call: tool_use_id.clone(),
                            name: name.clone().unwrap_or_default(),
                            content: content
                                .0
                                .iter()
                                .map(|item| match item {
                                    ToolContent::Text { text } => {
                                        ToolResultContent::Text(crate::history::Text {
                                            text: text.clone(),
                                        })
                                    }
                                    ToolContent::Image { source } => {
                                        ToolResultContent::Image(image_block(source))
                                    }
                                })
                                .collect(),
                            is_error: *is_error,
                        })),
                        _ => None,
                    })
                    .collect(),
            },
            Role::Assistant => {
                crate::edge::assistant_from_animist(message, &std::collections::HashMap::new())
            }
        })
        .collect()
}

fn assistant(content: Vec<ContentBlock>) -> Message {
    Message::builder()
        .role(Role::Assistant)
        .content(content)
        .kind(MessageKind::Turn)
        .build()
}

fn usage() -> TokenUsage {
    TokenUsage::builder()
        .input(10_u32)
        .output(2_u32)
        .cache_creation(0_u32)
        .cache_read(0_u32)
        .build()
}

/// Cumulative-usage checkpoint matching the attempt's final usage.
fn usage_checkpoint() -> ProviderEvent {
    ProviderEvent::Usage { usage: usage() }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "Ok and error attempts share one type"
)]
#[expect(
    clippy::result_large_err,
    reason = "ProviderError is animist's public error type and cannot be boxed here"
)]
fn response(
    content: Vec<ContentBlock>,
    stop: StopReason,
) -> animist::provider::Result<StreamResponse> {
    Ok(StreamResponse::builder()
        .message(assistant(content))
        .usage(usage())
        .stop_reason(stop)
        .input_transformations(Vec::new())
        .build())
}

/// One assistant text response that ends the turn.
pub fn text_turn(text: &str) -> Attempt {
    Attempt::new(
        vec![
            ProviderEvent::TextDelta {
                text: text.to_owned(),
            },
            ProviderEvent::ContentComplete {
                block: ContentBlock::Text {
                    text: text.to_owned(),
                },
            },
            usage_checkpoint(),
        ],
        response(
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            StopReason::EndTurn,
        ),
    )
}

/// One assistant tool-call response, in the order given.
pub fn tool_turn(calls: Vec<ToolCallSpec>) -> Attempt {
    let mut events = Vec::new();
    let mut content = Vec::new();
    for (id, name, input) in calls {
        events.push(ProviderEvent::ToolUseStart {
            id: id.clone(),
            name: name.clone(),
        });
        let block = ContentBlock::ToolUse {
            id,
            name,
            input,
            thought_signature: None,
        };
        events.push(ProviderEvent::ContentComplete {
            block: block.clone(),
        });
        content.push(block);
    }
    events.push(usage_checkpoint());
    Attempt::new(events, response(content, StopReason::ToolUse))
}

/// One reasoning-then-text response carrying an optional thinking signature.
pub fn thinking_turn(thinking: &str, text: &str, signature: Option<String>) -> Attempt {
    Attempt::new(
        vec![
            ProviderEvent::ThinkingDelta {
                text: thinking.to_owned(),
            },
            ProviderEvent::ContentComplete {
                block: ContentBlock::Thinking {
                    thinking: thinking.to_owned(),
                    signature: signature.clone(),
                },
            },
            ProviderEvent::TextDelta {
                text: text.to_owned(),
            },
            ProviderEvent::ContentComplete {
                block: ContentBlock::Text {
                    text: text.to_owned(),
                },
            },
            usage_checkpoint(),
        ],
        response(
            vec![
                ContentBlock::Thinking {
                    thinking: thinking.to_owned(),
                    signature,
                },
                ContentBlock::Text {
                    text: text.to_owned(),
                },
            ],
            StopReason::EndTurn,
        ),
    )
}

/// One text response the output token limit truncated.
pub fn max_tokens_turn(text: &str) -> Attempt {
    Attempt::new(
        vec![
            ProviderEvent::TextDelta {
                text: text.to_owned(),
            },
            ProviderEvent::ContentComplete {
                block: ContentBlock::Text {
                    text: text.to_owned(),
                },
            },
            usage_checkpoint(),
        ],
        response(
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            StopReason::MaxTokens,
        ),
    )
}

/// One failed attempt: the provider answered HTTP `status` with `body`.
pub fn http_error(status: u16, body: &str) -> Attempt {
    Attempt::new(
        Vec::new(),
        Err(ProviderError::Http {
            status,
            body: body.to_owned(),
            body_truncated: false,
            backtrace: snafu::Backtrace::generate(),
        }),
    )
}

/// One failed attempt: the transport timed out.
///
/// Connects to a loopback listener that never answers, so nothing leaves the
/// machine.
pub async fn transport_timeout() -> Attempt {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener binds");
    let address = listener.local_addr().expect("listener reports its address");
    let error = animist::reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .no_proxy()
        .build()
        .expect("test client builds")
        .get(format!("http://{address}/"))
        .send()
        .await
        .expect_err("an unanswered request always times out");
    assert!(error.is_timeout(), "expected a timeout error, got {error}");
    Attempt::new(Vec::new(), Err(ProviderError::Transport { source: error }))
}

/// One response reporting the prompt exceeded the context window.
pub fn context_overflow() -> Attempt {
    Attempt::new(
        Vec::new(),
        response(Vec::new(), StopReason::ContextWindowExceeded),
    )
}

/// The HTTP-400 form of [`context_overflow`]: an overflow-shaped error body.
pub fn context_overflow_http() -> Attempt {
    http_error(
        400,
        "{\"error\":{\"message\":\"This model's maximum context length is 200000 tokens. \
         However, your messages resulted in 210744 tokens.\",\"type\":\"invalid_request_error\",\
         \"code\":\"context_length_exceeded\"}}",
    )
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use animist::{
        agent::{
            self,
            tool::{ToolContext, ToolImpl, ToolRegistry},
        },
        history::{ContentBlock, Message, MessageKind, Role, StoredMessage},
        provider::{RequestOptions, StopReason},
    };
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    fn input(text: &str) -> StoredMessage {
        StoredMessage::builder()
            .timestamp(jiff::Timestamp::now())
            .message(
                Message::builder()
                    .role(Role::User)
                    .content(vec![ContentBlock::Text {
                        text: text.to_owned(),
                    }])
                    .kind(MessageKind::Turn)
                    .build(),
            )
            .build()
    }

    async fn wait_for_requests(provider: &ScriptedProvider, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while provider.requests().len() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scripted attempts start within the timeout");
    }

    #[derive(Deserialize)]
    struct EchoArgs {
        text: String,
    }

    struct Echo(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl ToolImpl for Echo {
        type Args = EchoArgs;
        type Output = String;
        type Error = Infallible;

        fn definition(&self) -> Tool {
            Tool::builder()
                .name("echo")
                .description("Returns its text argument.")
                .input_schema(json!({
                    "type": "object",
                    "properties": {"text": {"type": "string"}},
                    "required": ["text"]
                }))
                .build()
        }

        async fn call(
            &self,
            _context: &ToolContext<'_>,
            args: EchoArgs,
        ) -> Result<String, Infallible> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(args.text)
        }
    }

    #[test]
    fn test_turn_helpers_build_expected_responses() {
        let thinking = thinking_turn("because", "the answer", Some("sig".to_owned()));
        let thinking_response = thinking.outcome.expect("thinking attempt succeeds");
        let thinking_content = thinking_response.message.content;
        assert_eq!(thinking_content.len(), 2);
        assert!(thinking_content[0].is_thinking());
        assert_eq!(thinking_response.stop_reason, Some(StopReason::EndTurn));

        let max_tokens = max_tokens_turn("cut short");
        assert_eq!(
            max_tokens
                .outcome
                .expect("max-tokens attempt succeeds")
                .stop_reason,
            Some(StopReason::MaxTokens)
        );

        let overflow = context_overflow();
        assert_eq!(
            overflow
                .outcome
                .expect("overflow attempt succeeds")
                .stop_reason,
            Some(StopReason::ContextWindowExceeded)
        );

        let tool = tool_turn(vec![(
            "call-1".to_owned(),
            "echo".to_owned(),
            json!({"text": "x"}),
        )]);
        assert!(tool.events.iter().any(
            |event| matches!(event, ProviderEvent::ToolUseStart { name, .. } if name == "echo")
        ));
    }

    #[tokio::test]
    async fn test_error_helpers_build_provider_errors() {
        let http = http_error(429, "slow down");
        match http.outcome {
            Err(ProviderError::Http { status: 429, .. }) => {}
            other => panic!("expected an HTTP 429 error, got {other:?}"),
        }

        let timeout = transport_timeout().await;
        match timeout.outcome {
            Err(ProviderError::Transport { source }) => {
                assert!(source.is_timeout(), "transport error must be a timeout");
            }
            other => panic!("expected a transport error, got {other:?}"),
        }

        let overflow = context_overflow_http();
        match overflow.outcome {
            Err(ProviderError::Http { status, body, .. }) => {
                assert_eq!(status, 400);
                assert!(
                    crate::compaction::llm::is_context_overflow_body(&body),
                    "body must read as context overflow: {body}"
                );
            }
            other => panic!("expected an HTTP overflow error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_scripted_provider_lists_models_and_counts_auth_calls() {
        let info = model_info("listed", 1_000, 100, false, false);
        let provider = ScriptedProvider::new(vec![info.clone()]);
        let listed = ProviderImpl::list_models(&provider)
            .await
            .expect("scripted listing succeeds");
        assert_eq!(listed, vec![info]);

        ProviderImpl::refresh_auth(&provider)
            .await
            .expect("scripted refresh succeeds");
        ProviderImpl::reload_auth(&provider)
            .await
            .expect("scripted reload succeeds");
        assert_eq!(provider.refresh_count(), 1);
        assert_eq!(provider.reload_count(), 1);
    }

    #[tokio::test]
    async fn test_agent_text_turn_run_completes() {
        let info = model_info("test-model", 200_000, 8_192, true, true);
        let provider = ScriptedProvider::new(vec![info.clone()]);
        provider.push(text_turn("all done"));
        let agent = agent::Agent::builder()
            .model(scripted_model(&provider, info))
            .build();
        let report = agent
            .run(input("hello"), agent::RunOptions::builder().build())
            .await
            .expect("text run succeeds");
        assert_eq!(report.status, agent::RunStatus::Completed);
        let response = report.response.expect("completed run stores a response");
        assert_eq!(response.message().role, Role::Assistant);
        assert!(matches!(
            response.message().content.as_slice(),
            [ContentBlock::Text { text }] if text == "all done"
        ));

        let requests = provider.requests();
        let request = &requests[0];
        assert_eq!(requests.len(), 1);
        assert_eq!(request.model, "test-model");
        assert_eq!(request.messages.len(), 1);
        assert!(request.tools.is_empty());
        assert_eq!(request.options, RequestOptions::default());
    }

    #[tokio::test]
    async fn test_agent_tool_turn_registered_tool_executes() {
        let info = model_info("test-model", 200_000, 8_192, true, true);
        let provider = ScriptedProvider::new(vec![info.clone()]);
        provider.push(tool_turn(vec![(
            "call-1".to_owned(),
            "echo".to_owned(),
            json!({"text": "from the tool"}),
        )]));
        provider.push(text_turn("done after the tool"));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolRegistry::default();
        tools
            .register(Echo(Arc::clone(&calls)))
            .expect("unique tool registers");
        let agent = agent::Agent::builder()
            .model(scripted_model(&provider, info))
            .tools(tools)
            .build();
        let report = agent
            .run(input("use the tool"), agent::RunOptions::builder().build())
            .await
            .expect("tool run succeeds");
        assert_eq!(report.status, agent::RunStatus::Completed);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools.len(), 1);
        let second = &requests[1];
        assert_eq!(second.messages.len(), 3);
        assert!(
            matches!(
                second.messages.last(),
                Some(Message {
                    role: Role::User,
                    content,
                    ..
                }) if matches!(
                    content.as_slice(),
                    [ContentBlock::ToolResult {
                        tool_use_id, is_error, ..
                    }] if tool_use_id == "call-1" && !is_error
                )
            ),
            "second request ends with the echo result: {:?}",
            second.messages
        );
    }

    #[tokio::test]
    async fn test_agent_blocked_attempt_release_completes() {
        let info = model_info("test-model", 200_000, 8_192, true, true);
        let provider = ScriptedProvider::new(vec![info.clone()]);
        provider.push(text_turn("after the gate").blocked());
        let agent = agent::Agent::builder()
            .model(scripted_model(&provider, info))
            .build();
        let task = tokio::spawn(async move {
            agent
                .run(input("hello"), agent::RunOptions::builder().build())
                .await
        });
        wait_for_requests(&provider, 1).await;
        provider.release();
        let report = task
            .await
            .expect("run task joins")
            .expect("released run succeeds");
        assert_eq!(report.status, agent::RunStatus::Completed);
    }

    #[tokio::test]
    async fn test_agent_blocked_attempt_cancelled_keeps_scripted_partial() {
        let info = model_info("test-model", 200_000, 8_192, true, true);
        let provider = ScriptedProvider::new(vec![info.clone()]);
        provider.push(text_turn("partial").blocked());
        let cancellation = agent::Cancellation::default();
        let agent = agent::Agent::builder()
            .model(scripted_model(&provider, info))
            .build();
        let task = tokio::spawn({
            let cancellation = cancellation.clone();
            async move {
                agent
                    .run(
                        input("hello"),
                        agent::RunOptions::builder()
                            .cancellation(cancellation)
                            .build(),
                    )
                    .await
            }
        });
        wait_for_requests(&provider, 1).await;
        cancellation.cancel();
        let report = task
            .await
            .expect("run task joins")
            .expect("cancelled run still reports");
        assert_eq!(report.status, agent::RunStatus::Cancelled);
        let attempts = report.interrupted_attempts;
        assert_eq!(attempts.len(), 1);
        let interrupted = &attempts[0];
        assert!(interrupted.cancelled);
        assert_eq!(interrupted.provider, "scripted");
        assert_eq!(interrupted.model, "test-model");
        assert_eq!(interrupted.partial.display_text, "partial");
        assert_eq!(
            interrupted.partial.usage,
            Some(usage()),
            "the usage checkpoint survives cancellation as the partial's usage"
        );
    }
}
