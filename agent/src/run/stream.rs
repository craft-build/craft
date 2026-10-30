//! Streaming one model call over animist's [`InferenceExchange`].
//!
//! The exchange owns transport work and bounded partial output; this
//! module drives it against craft's cancellation, forwards provider
//! events as [`Event`]s (text/reasoning deltas, tool starts), and
//! produces the completed [`history::Message`] for the assistant turn
//! plus its usage report.

use std::collections::HashMap;

use animist::provider::{ExchangeEvent, InferenceExchange, ProviderError, ProviderEvent};
use animist::provider::{RequestOptions, StopReason, Tool};

use crate::edge;
use crate::history;
use crate::provider::ModelHandle;

use super::{CancelToken, Event};

/// The aggregated result of one model call.
#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub assistant: history::Message,
    pub usage: history::Usage,
    /// Why the model stopped, when the provider reported it.
    /// [`StopReason::MaxTokens`] means the reply was truncated at the
    /// output limit; [`StopReason::ContextWindowExceeded`] is converted to
    /// an overflow failure instead.
    pub finish_reason: Option<StopReason>,
}

/// How a provider stream ended without producing a turn.
#[derive(Debug, Clone)]
pub enum StreamFailure {
    /// The user cancelled mid-stream; `streamed` is the reply text that
    /// already reached the view, kept so history can agree with it.
    Cancelled { streamed: String },
    Error {
        kind: ErrorKind,
        message: String,
        /// The provider already retried this failure internally (Bedrock
        /// 429/503 backoff), so same-model backoff must not stack on it;
        /// recovery goes straight to the fallback chain.
        provider_retried: bool,
    },
}

impl StreamFailure {
    /// Classify a provider error into the recovery taxonomy.
    #[cfg(test)]
    pub(crate) fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = classify_string(&message);
        Self::Error {
            kind,
            message,
            provider_retried: false,
        }
    }
}

/// Recovery taxonomy for provider stream errors, ported from the
/// reference's `AgentError::{is_retryable, should_rotate_key, should_abort}`.
/// Most failures classify from the preserved HTTP status, when one
/// survived, and otherwise fall back to message matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    /// 429: retried with backoff; the first remedy is key rotation.
    RateLimited,
    /// 5xx / overloaded: retried with backoff.
    Server,
    /// Connection-level transport failure: retried with backoff.
    Transport,
    /// Timed out: retried with backoff, capped at
    /// [`crate::run::retry::MAX_TIMEOUT_RETRIES`].
    Timeout,
    /// 401: authentication failed. Never retried here; the run loop's
    /// reauth wait (E.10) owns it.
    Auth,
    /// Prompt exceeded the context window: never retried here; the run
    /// loop's overflow recovery (C.5) owns it.
    Overflow,
    /// Content policy / billing: abort immediately, retrying cannot help.
    Abort,
    /// Everything else: surface the failure.
    Fatal,
}

impl ErrorKind {
    pub(crate) fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::Server | Self::Transport | Self::Timeout
        )
    }

    /// 429 alone: the reference also lists 401/403 in `should_rotate_key`,
    /// but rotation is only reachable inside the `is_retryable` branch of
    /// `stream_with_retry`, so those statuses never rotate there either.
    pub(crate) fn should_rotate_key(self) -> bool {
        self == Self::RateLimited
    }

    pub(crate) fn is_overflow(self) -> bool {
        self == Self::Overflow
    }

    /// Human summary for [`Event::Retry`], after the reference's
    /// `AgentError::retry_message`.
    pub(crate) fn retry_message(self, message: &str) -> String {
        match self {
            Self::RateLimited => "Rate limited".into(),
            Self::Server => "Provider server error".into(),
            Self::Transport => "Connection error".into(),
            Self::Timeout => "Stream timed out".into(),
            _ => message.to_owned(),
        }
    }
}

/// Classify a typed provider error: the preserved HTTP status first (Bedrock
/// errors through their source), credential failures as auth, transport
/// timeouts as timeouts, and everything else by message.
pub(crate) fn classify_error(error: &ProviderError) -> ErrorKind {
    match error {
        ProviderError::Http { status: 429, .. } => return ErrorKind::RateLimited,
        ProviderError::Http { status: 401, .. } => return ErrorKind::Auth,
        ProviderError::Http {
            status: 500..=599, ..
        } => return ErrorKind::Server,
        // 4xx bodies carry the real reason (overflow, content policy);
        // keep matching on the message instead of guessing Fatal.
        ProviderError::Http { body, .. } => {
            if crate::compaction::llm::is_context_overflow_body(body) {
                return ErrorKind::Overflow;
            }
        }
        ProviderError::Transport { source } if source.is_timeout() => return ErrorKind::Timeout,
        ProviderError::Transport { .. } => return ErrorKind::Transport,
        ProviderError::Credentials { .. }
        | ProviderError::CredentialEnvironment { .. }
        | ProviderError::EmptyApiKey { .. } => return ErrorKind::Auth,
        ProviderError::Bedrock { source } => return classify_bedrock(source),
        _ => {}
    }
    classify_string(&error.to_string())
}

/// Bedrock failures: animist already retried 429/503 internally (bounded
/// by `max_attempts`, `Retry-After`-aware), so those surface as
/// provider-retried; the rest classify through the same taxonomy.
fn classify_bedrock(source: &animist::provider::bedrock::BedrockError) -> ErrorKind {
    use animist::provider::bedrock::BedrockError;
    let (status, body): (u16, &[u8]) = match source {
        BedrockError::Http { status, body, .. } => (*status, body.as_slice()),
        BedrockError::ErrorBodyTooLarge {
            status,
            body_prefix,
            ..
        } => (*status, body_prefix),
        BedrockError::Credentials { .. } | BedrockError::MissingCredentialsProvider { .. } => {
            return ErrorKind::Auth;
        }
        BedrockError::Transport { source } if source.is_timeout() => return ErrorKind::Timeout,
        BedrockError::Transport { .. } => return ErrorKind::Transport,
        other => return classify_string(&other.to_string()),
    };
    match status {
        429 => ErrorKind::RateLimited,
        401 => ErrorKind::Auth,
        500..=599 => ErrorKind::Server,
        _ => {
            let body = String::from_utf8_lossy(body);
            if crate::compaction::llm::is_context_overflow_body(&body) {
                ErrorKind::Overflow
            } else {
                classify_string(&body)
            }
        }
    }
}

/// Whether animist's provider already spent its own retries on this error:
/// Bedrock's 429/503 handling, and nothing else.
fn provider_retried(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::Bedrock {
            source: animist::provider::bedrock::BedrockError::Http {
                status: 429 | 503,
                ..
            }
        }
    )
}

fn classify_string(message: &str) -> ErrorKind {
    // The single shared context-overflow classifier lives in compaction
    // (its superset also rejects oversized *output* caps, which stream
    // errors for prompt overflow cannot be mistaken for either).
    if crate::compaction::llm::is_context_overflow_body(message) {
        return ErrorKind::Overflow;
    }
    let message = message.to_ascii_lowercase();
    // Reference `AgentError::should_abort`.
    if ["content policy", "content_policy", "billing"]
        .iter()
        .any(|n| message.contains(n))
    {
        return ErrorKind::Abort;
    }
    if message.contains("timeout") || message.contains("timed out") {
        return ErrorKind::Timeout;
    }
    if message.contains("unauthorized") || message.contains("401") {
        return ErrorKind::Auth;
    }
    if message.contains("rate limit")
        || message.contains("rate_limit")
        || message.contains("429")
        || message.contains("too many requests")
    {
        return ErrorKind::RateLimited;
    }
    if message.contains("overloaded")
        || message.contains("503")
        || message.contains("server error")
        || ["500", "502", "504"].iter().any(|n| message.contains(n))
    {
        return ErrorKind::Server;
    }
    if [
        "connection",
        "network",
        "broken pipe",
        "connection reset",
        "error sending request",
    ]
    .iter()
    .any(|n| message.contains(n))
    {
        return ErrorKind::Transport;
    }
    ErrorKind::Fatal
}

fn failure_from_error(error: &ProviderError) -> StreamFailure {
    StreamFailure::Error {
        kind: classify_error(error),
        message: error.to_string(),
        provider_retried: provider_retried(error),
    }
}

/// One model call, already shaped for the provider: the mapped history,
/// the system preamble, the tool declarations, and the request options.
#[derive(Clone)]
pub struct ProviderRequest {
    pub messages: Vec<animist::history::Message>,
    pub system: String,
    pub tools: Vec<Tool>,
    pub options: RequestOptions,
}

/// Build the per-request options from run params and the model handle:
/// thinking preference, sampling, the output cap (with the Claude default
/// and its warning), and the system-message policy the target protocol
/// needs.
pub(crate) fn request_options(
    handle: &ModelHandle,
    thinking: crate::thinking::ThinkingConfig,
    fast: bool,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
) -> RequestOptions {
    let mut options = RequestOptions::builder()
        .thinking(thinking.to_animist())
        .fast(fast)
        .build();
    // bon makes the optional members' setters take the inner type, so the
    // `None` cases simply leave the fields unset.
    if let Some(cap) = max_tokens {
        options.max_output_tokens = Some(cap.min(u64::from(u32::MAX)) as u32);
    }
    options.temperature = temperature;
    if handle.moves_system_to_preamble() {
        options.system_message_policy = animist::provider::SystemMessagePolicy::MoveToPreamble;
    }
    options
}

/// Run one model stream to completion.
pub(crate) async fn run_model_stream(
    handle: &ModelHandle,
    request: ProviderRequest,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> std::result::Result<TurnOutput, StreamFailure> {
    if cancel.cancelled() {
        return Err(StreamFailure::Cancelled {
            streamed: String::new(),
        });
    }
    let ProviderRequest {
        messages,
        system,
        tools,
        options,
    } = request;
    edge::assert_replay_valid(&messages, &tools);
    let model = handle.model.clone();
    let mut exchange = InferenceExchange::new(move |event_tx| {
        let model = model.clone();
        async move {
            model
                .provider
                .stream_message(&model, &messages, &system, &tools, &event_tx, options)
                .await
        }
    });
    // The reply text streamed so far; carried out on cancellation so the
    // committed history keeps what the user already saw.
    let mut streamed = String::new();
    // provider tool-call id -> run-stable internal id recorded at call time.
    let mut call_ids: HashMap<String, String> = HashMap::new();
    let mut cancel_rx = cancel.subscribe();
    // A dropped CancelFlag makes `changed()` ready (with Err) on every poll;
    // polling it forever would busy-loop, so disable the branch once that
    // happens and let the exchange drive the loop.
    let mut cancel_alive = true;
    let response = loop {
        tokio::select! {
            biased;
            changed = cancel_rx.changed(), if cancel_alive => {
                match changed {
                    Ok(()) => {
                        let partial = exchange.cancel();
                        return Err(StreamFailure::Cancelled {
                            streamed: partial.display_text.clone(),
                        });
                    }
                    Err(_) => cancel_alive = false,
                }
            }
            event = exchange.next() => {
                let Some(event) = event else {
                    return Err(StreamFailure::Cancelled { streamed });
                };
                match event {
                    ExchangeEvent::Provider(event) => match &event {
                        ProviderEvent::TextDelta { text } => {
                            streamed.push_str(text);
                            emit(Event::TextDelta(text.clone()));
                        }
                        ProviderEvent::ThinkingDelta { text } => {
                            emit(Event::ThinkingDelta(text.clone()));
                        }
                        ProviderEvent::ToolUseStart { id, name } => {
                            // Stream ids are provider property (fresh on
                            // every attempt); mint a globally unique
                            // internal id so replayed history survives
                            // retries and cross-wire fallbacks. It must
                            // stay unique across streams: a per-stream
                            // counter restarted at `call-0` left duplicate
                            // tool ids in history and the TUI merged a
                            // later call into an earlier call's card.
                            let internal_id = call_ids
                                .entry(id.clone())
                                .or_insert_with(|| format!("call-{}", crate::id::CraftId::generate()))
                                .clone();
                            emit(Event::ToolPending {
                                id: internal_id,
                                name: name.clone(),
                            });
                        }
                        ProviderEvent::ContentComplete {
                            block: block @ animist::history::ContentBlock::ToolUse { id, input, .. },
                        } => {
                            // Emit at completion: the arguments are whole,
                            // matching the timing surfaces expect.
                            if let Some(internal_id) = call_ids.get(id) {
                                let animist::history::ContentBlock::ToolUse { name, .. } = block
                                else {
                                    unreachable!("matched on ToolUse");
                                };
                                emit(Event::ToolStart {
                                    id: internal_id.clone(),
                                    name: name.clone(),
                                    arguments: input.clone(),
                                });
                            }
                        }
                        _ => {}
                    },
                    ExchangeEvent::Finished(Ok(response)) => break response,
                    ExchangeEvent::Finished(Err(error)) => {
                        return Err(failure_from_error(&error));
                    }
                }
            }
        }
    };
    if response.stop_reason == Some(StopReason::ContextWindowExceeded) {
        return Err(StreamFailure::Error {
            kind: ErrorKind::Overflow,
            message: "the prompt exceeded the model's context window".to_owned(),
            provider_retried: false,
        });
    }
    let usage = history::Usage {
        input_tokens: u64::from(response.usage.input + response.usage.cache_read),
        output_tokens: u64::from(response.usage.output),
        total_tokens: u64::from(
            response.usage.input
                + response.usage.output
                + response.usage.cache_read
                + response.usage.cache_creation,
        ),
    };
    let assistant = edge::assistant_from_animist(&response.message, &call_ids);
    Ok(TurnOutput {
        assistant,
        usage,
        finish_reason: response.stop_reason,
    })
}

impl StreamFailure {
    pub(crate) fn is_overflow(&self) -> bool {
        matches!(
            self,
            Self::Error { kind, .. } if kind.is_overflow()
        )
    }

    pub(crate) fn is_auth(&self) -> bool {
        matches!(
            self,
            Self::Error { kind, .. } if *kind == ErrorKind::Auth
        )
    }

    pub(crate) fn message(&self) -> Option<&str> {
        match self {
            Self::Error { message, .. } => Some(message),
            Self::Cancelled { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_provider_overflow_messages() {
        // Cases from the former local classifier, re-expressed against the
        // shared superset in `compaction::llm` (the full table lives there).
        for message in [
            "Error: prompt is too long: 210744 tokens > 200000 maximum",
            "This model's maximum context length is 4096 tokens. However, you requested ...",
            "The input token count (524389) exceeds the maximum number of tokens allowed (1048576)",
            "conversation exceeds the model context window",
        ] {
            assert!(
                crate::compaction::llm::is_context_overflow_body(message),
                "should match: {message}"
            );
            assert!(StreamFailure::error(message).is_overflow());
        }
    }

    #[test]
    fn non_overflow_failures_do_not_match() {
        for message in [
            "rate limit exceeded, retry after 30s",
            "invalid api key",
            "connection closed before response",
            // The shared classifier rejects output-cap complaints: retrying
            // with a shorter prompt cannot fix a `max_tokens` bid too high.
            "input length and `max_tokens` exceed context limit: 10922 + 8192 > 8192",
        ] {
            assert!(
                !crate::compaction::llm::is_context_overflow_body(message),
                "must not match: {message}"
            );
            assert!(!StreamFailure::error(message).is_overflow());
        }
        assert!(
            !StreamFailure::Cancelled {
                streamed: String::new()
            }
            .is_overflow()
        );
    }

    /// The recovery taxonomy mirrors the reference's `AgentError`
    /// classification: retryable, rotate-worthy, abort, fatal.
    #[test]
    fn classifies_error_kinds() {
        let cases = [
            (
                "rate limit exceeded, retry after 30s",
                ErrorKind::RateLimited,
            ),
            ("HTTP 429", ErrorKind::RateLimited),
            ("Too many requests", ErrorKind::RateLimited),
            ("provider is overloaded", ErrorKind::Server),
            ("server error (503)", ErrorKind::Server),
            ("request timed out", ErrorKind::Timeout),
            ("operation timeout", ErrorKind::Timeout),
            (
                "error sending request: connection reset by peer",
                ErrorKind::Transport,
            ),
            ("network unreachable", ErrorKind::Transport),
            (
                "your credit balance is too low (billing hard limit)",
                ErrorKind::Abort,
            ),
            ("request rejected by content policy", ErrorKind::Abort),
            ("invalid api key", ErrorKind::Fatal),
            ("401 unauthorized: invalid credentials", ErrorKind::Auth),
            ("Unauthorized", ErrorKind::Auth),
            ("HTTP status 401", ErrorKind::Auth),
        ];
        for (message, expected) in cases {
            let StreamFailure::Error { kind, .. } = StreamFailure::error(message) else {
                panic!("must classify: {message}");
            };
            assert_eq!(kind, expected, "message: {message}");
        }
    }

    /// Auth failures are never retried here; the run loop's reauth wait
    /// (E.10) owns them.
    #[test]
    fn auth_errors_are_never_retryable_and_match_is_auth() {
        assert!(!ErrorKind::Auth.is_retryable());
        assert!(!ErrorKind::Auth.should_rotate_key());
        assert!(!ErrorKind::Auth.is_overflow());
        assert!(StreamFailure::error("401 unauthorized").is_auth());
        assert!(!StreamFailure::error("rate limit exceeded").is_auth());
        assert!(
            !StreamFailure::Cancelled {
                streamed: String::new()
            }
            .is_auth()
        );
    }

    #[test]
    fn only_transient_kinds_are_retryable_and_rotating() {
        for kind in [
            ErrorKind::RateLimited,
            ErrorKind::Server,
            ErrorKind::Transport,
            ErrorKind::Timeout,
        ] {
            assert!(kind.is_retryable(), "{kind:?}");
        }
        for kind in [ErrorKind::Overflow, ErrorKind::Abort, ErrorKind::Fatal] {
            assert!(!kind.is_retryable(), "{kind:?}");
            assert!(!kind.should_rotate_key(), "{kind:?}");
        }
        // 429 is the only status that both retries and rotates.
        assert!(ErrorKind::RateLimited.should_rotate_key());
        for kind in [ErrorKind::Server, ErrorKind::Transport, ErrorKind::Timeout] {
            assert!(!kind.should_rotate_key(), "{kind:?}");
        }
    }

    /// A typed HTTP status classifies without string matching, including
    /// Bedrock statuses surfacing through their source, and Bedrock
    /// 429/503 mark themselves provider-retried.
    #[test]
    fn classifies_typed_http_status() {
        for (status, expected) in [
            (429u16, ErrorKind::RateLimited),
            (500, ErrorKind::Server),
            (503, ErrorKind::Server),
            (400, ErrorKind::Fatal),
            (401, ErrorKind::Auth),
        ] {
            let error = ProviderError::Http {
                status,
                body: "boom".to_owned(),
                body_truncated: false,
                backtrace: snafu::GenerateImplicitData::generate(),
            };
            assert_eq!(classify_error(&error), expected, "status {status}");
        }
        let overflow = ProviderError::Http {
            status: 400,
            body: "This model's maximum context length is 200000 tokens.".to_owned(),
            body_truncated: false,
            backtrace: snafu::GenerateImplicitData::generate(),
        };
        assert!(failure_from_error(&overflow).is_overflow());

        let bedrock_retried = ProviderError::Bedrock {
            source: animist::provider::bedrock::BedrockError::Http {
                status: 429,
                request_id: None,
                error_type: None,
                body: Vec::new(),
                backtrace: snafu::GenerateImplicitData::generate(),
            },
        };
        let StreamFailure::Error {
            kind,
            provider_retried,
            ..
        } = failure_from_error(&bedrock_retried)
        else {
            panic!("error failure");
        };
        assert_eq!(kind, ErrorKind::RateLimited);
        assert!(provider_retried, "animist already spent its retries");

        let plain = ProviderError::Http {
            status: 429,
            body: String::new(),
            body_truncated: false,
            backtrace: snafu::GenerateImplicitData::generate(),
        };
        let StreamFailure::Error {
            provider_retried, ..
        } = failure_from_error(&plain)
        else {
            panic!("error failure");
        };
        assert!(!provider_retried, "only Bedrock retries internally");
    }

    /// A stream that ends without reporting usage yields the zero-valued
    /// sentinel when the provider sent none.
    #[tokio::test]
    async fn scripted_text_turn_streams_and_aggregates() {
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        provider.push(crate::testing::text_turn("hello"));
        let handle = ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("m", 1000, 100, false, false),
        ));
        let request = ProviderRequest {
            messages: crate::edge::to_animist(&[crate::history::Message::user("hi")], false),
            system: String::new(),
            tools: Vec::new(),
            options: RequestOptions::default(),
        };
        let (flag, cancel) = crate::run::cancel_channel();
        drop(flag);
        let output = run_model_stream(&handle, request, &cancel, &|_| {})
            .await
            .unwrap();
        assert_eq!(output.assistant.text(), "hello");
        assert_eq!(output.usage.total_tokens, 12);
        assert_eq!(output.finish_reason, Some(StopReason::EndTurn));
    }

    /// Cancellation mid-stream keeps the partial text the view already
    /// showed, from the exchange's retained partial.
    #[tokio::test]
    async fn cancelled_stream_carries_the_partial_text() {
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        provider.push(crate::testing::text_turn("partial").blocked());
        let handle = ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("m", 1000, 100, false, false),
        ));
        let request = ProviderRequest {
            messages: crate::edge::to_animist(&[crate::history::Message::user("hi")], false),
            system: String::new(),
            tools: Vec::new(),
            options: RequestOptions::default(),
        };
        let (flag, cancel) = crate::run::cancel_channel();
        let task =
            tokio::spawn(async move { run_model_stream(&handle, request, &cancel, &|_| {}).await });
        while provider.requests().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        flag.set(true);
        match task.await.unwrap() {
            Err(StreamFailure::Cancelled { streamed }) => assert_eq!(streamed, "partial"),
            other => panic!("expected cancellation, got {other:?}"),
        }
    }
}
