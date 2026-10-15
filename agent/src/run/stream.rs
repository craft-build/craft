//! Streaming aggregation over a provider model's stream.
//!
//! rig-core's `CompletionStream` already assembles ragged tool-argument
//! fragments and multi-part text; this module consumes its public events,
//! forwards them as [`Event`]s (text/reasoning deltas, tool start), and
//! produces the completed [`history::Message`] for the assistant turn plus
//! its usage report.

use std::collections::HashMap;

use crate::providers::CompletionSource;
use futures::StreamExt;
use rig_core::completion::message::AssistantContent;
use rig_core::completion::{CompletionRequest, FinishReason};
use rig_core::error::ProviderError;
use rig_core::streaming::{Item, StreamEvent};

use crate::edge::{StreamedParts, assistant_from_stream, fold_streamed_event};
use crate::history;

use super::{CancelToken, Event};

/// The aggregated result of one model call.
#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub assistant: history::Message,
    pub usage: history::Usage,
    /// Why the model stopped, when the provider reported it. `Length` means
    /// the reply was truncated at `max_tokens`.
    pub finish_reason: Option<FinishReason>,
}

/// How a provider stream ended without producing a turn.
#[derive(Debug, Clone)]
pub enum StreamFailure {
    /// The user cancelled mid-stream; `streamed` is the reply text that
    /// already reached the view, kept so history can agree with it.
    Cancelled {
        streamed: String,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}

impl StreamFailure {
    /// Classify a bare provider message (no typed error available).
    #[cfg(test)]
    pub(crate) fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = classify_string(&message);
        Self::Error { kind, message }
    }
}

/// Recovery taxonomy for provider stream errors, ported from the
/// reference's `AgentError::{is_retryable, should_rotate_key, should_abort}`
/// (`craft-providers/src/error.rs`). Most failures classify from the
/// preserved HTTP status, when one survived, and otherwise fall back to
/// message matching.
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

/// Classify a typed rig provider error: use the preserved HTTP status when
/// one exists (non-success statuses surface as `ProviderResponse`, in
/// transport-refusals and failed replies alike), otherwise match on the
/// display string.
pub(crate) fn classify_error(error: &ProviderError) -> ErrorKind {
    if matches!(error, ProviderError::Truncated) {
        // A stream cut before its terminal record is retryable transport
        // damage, not a short honest answer.
        return ErrorKind::Transport;
    }
    if let Some(status) = error.provider_response_status() {
        match status.as_u16() {
            401 => return ErrorKind::Auth,
            429 => return ErrorKind::RateLimited,
            500..=599 => return ErrorKind::Server,
            // 4xx bodies carry the real reason (overflow, content policy);
            // keep matching on the message instead of guessing Fatal.
            _ => {}
        }
    }
    if let ProviderError::Http(http) = error
        && let Some(status) = http.non_success_status()
    {
        match status.as_u16() {
            401 => return ErrorKind::Auth,
            429 => return ErrorKind::RateLimited,
            500..=599 => return ErrorKind::Server,
            _ => {}
        }
    }
    classify_string(&error.to_string())
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
    }
}

/// Run one model stream to completion.
pub(crate) async fn run_model_stream<M: CompletionSource + Clone>(
    model: &M,
    request: CompletionRequest,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> std::result::Result<TurnOutput, StreamFailure> {
    // Request-building failures (an unrepresentable wire body) surface
    // before any bytes hit the network; the transport handshake itself now
    // happens inside the polled loop below, where cancel outranks it.
    if cancel.cancelled() {
        return Err(StreamFailure::Cancelled {
            streamed: String::new(),
        });
    }
    let mut stream = model.stream(request).map_err(|e| failure_from_error(&e))?;
    // The reply text streamed so far; carried out on cancellation so the
    // committed history keeps what the user already saw.
    let mut streamed = String::new();
    let mut parts = StreamedParts::default();
    // provider tool-call id -> run-stable internal id recorded at call time.
    let mut call_ids: HashMap<String, String> = HashMap::new();
    let mut cancel_rx = cancel.subscribe();
    // A dropped CancelFlag makes `changed()` ready (with Err) on every poll;
    // polling it forever would busy-loop, so disable the branch once that
    // happens and let the stream drive the loop.
    let mut cancel_alive = true;
    loop {
        tokio::select! {
            biased;
            changed = cancel_rx.changed(), if cancel_alive => {
                match changed {
                    Ok(()) => return Err(StreamFailure::Cancelled { streamed }),
                    Err(_) => cancel_alive = false,
                }
            }
            item = stream.next() => match item {
                None => break,
                Some(Err(error)) => return Err(failure_from_error(&error)),
                Some(Ok(Item::Unknown(_))) => {}
                Some(Ok(Item::Event(event))) => {
                    match &event {
                        StreamEvent::Text { text, .. } => {
                            streamed.push_str(text);
                            emit(Event::TextDelta(text.clone()));
                        }
                        StreamEvent::Reasoning { text, .. } => {
                            emit(Event::ThinkingDelta(text.clone()));
                        }
                        StreamEvent::End {
                            content: AssistantContent::ToolCall(tool_call),
                            ..
                        } => {
                            // Stream ids are provider property (fresh on
                            // every attempt); mint a globally unique internal
                            // id so replayed history survives retries and
                            // cross-wire fallbacks. It must stay unique across
                            // streams too: a per-stream counter restarted at
                            // `call-0` on every model response, so history
                            // carried duplicate tool ids and the TUI merged a
                            // later call into an earlier call's card.
                            let internal_call_id =
                                format!("call-{}", crate::id::CraftId::generate());
                            let internal_call_id = call_ids
                                .entry(tool_call.id.to_string())
                                .or_insert(internal_call_id)
                                .clone();
                            emit(Event::ToolStart {
                                id: internal_call_id,
                                name: tool_call.function.name.to_string(),
                                arguments: tool_call.function.arguments_value(),
                            });
                        }
                        _ => {}
                    }
                    fold_streamed_event(&mut parts, &event);
                }
            },
        }
    }
    // The folded turn: usage, finish reason, and the assembled choice come
    // from the same response a blocking call would have returned.
    let response = match stream.finish().await {
        Ok(response) => response,
        Err(error) => return Err(failure_from_error(&error)),
    };
    let usage = usage_from_response(&response);
    let assistant = assistant_from_stream(&response.choice, Some(&response.origin), &call_ids, &parts);
    Ok(TurnOutput {
        assistant,
        usage,
        finish_reason: response.finish_reason(),
    })
}

/// Zero-valued counters are the documented sentinel for a missing report;
/// `total_tokens` is derived when the provider omitted it.
fn usage_from_response(response: &rig_core::completion::CompletionResponse) -> history::Usage {
    let rig_usage = &response.usage;
    let input = rig_usage.input_tokens.unwrap_or(0);
    let output = rig_usage.output_tokens.unwrap_or(0);
    let total = rig_usage
        .total_tokens
        .unwrap_or(input.saturating_add(output));
    history::Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: total,
    }
}

impl StreamFailure {
    pub(crate) fn is_overflow(&self) -> bool {
        matches!(self, Self::Error { kind, .. } if kind.is_overflow())
    }

    pub(crate) fn is_auth(&self) -> bool {
        matches!(self, Self::Error { kind, .. } if *kind == ErrorKind::Auth)
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
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    fn request() -> CompletionRequest {
        crate::edge::to_request(
            &[crate::history::Message::user("hi")],
            &[],
            None,
            None,
            None,
        )
    }

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

    /// A preserved provider status classifies without string matching, both
    /// from transport refusals and provider-shaped failures.
    #[test]
    fn classifies_typed_http_status() {
        for (status, expected) in [
            (429u16, ErrorKind::RateLimited),
            (500, ErrorKind::Server),
            (503, ErrorKind::Server),
            (400, ErrorKind::Fatal),
            (401, ErrorKind::Auth),
        ] {
            let error = ProviderError::from_http_response(
                http::StatusCode::from_u16(status).unwrap(),
                "boom",
            );
            assert_eq!(classify_error(&error), expected, "status {status}");
            assert_eq!(
                classify_error(&ProviderError::from_transport_error(
                    rig_core::http_client::Error::non_success_with_details(
                        http::StatusCode::from_u16(status).unwrap(),
                        http::HeaderMap::new(),
                        "boom".into(),
                    ),
                )),
                expected,
                "transport status {status}"
            );
        }
        // A truncated stream is retryable transport damage.
        assert_eq!(
            classify_error(&ProviderError::Truncated),
            ErrorKind::Transport
        );
    }

    /// A dropped `CancelFlag` must disable the cancel branch instead of
    /// busy-looping: the stream still completes and aggregates normally.
    #[tokio::test]
    async fn dropped_cancel_flag_lets_the_stream_complete() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final_with_total_tokens(3)),
        ]]);
        let (flag, cancel) = crate::run::cancel_channel();
        drop(flag);
        let output = run_model_stream(&model, request(), &cancel, &|_| {})
            .await
            .unwrap();
        assert_eq!(output.assistant.text(), "hello");
        assert_eq!(output.usage.total_tokens, 3);
        assert_eq!(output.finish_reason, None);
    }

    /// A `FinalResponse` finish's reason rides the turn output.
    #[tokio::test]
    async fn finish_reason_rides_the_turn_output() {
        let finish = {
            let mut finish =
                rig_core::test_utils::mock_final(rig_core::completion::Usage::default());
            finish.reason = Some(rig_core::completion::FinishReason::Length);
            finish
        };
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("trunc"),
            MockStreamEvent::FinalResponse(finish),
        ]]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let output = run_model_stream(&model, request(), &cancel, &|_| {})
            .await
            .unwrap();
        assert_eq!(
            output.finish_reason,
            Some(rig_core::completion::FinishReason::Length)
        );
    }

    /// A stream that ends without reporting usage yields the zero-valued
    /// sentinel when the provider sent none.
    #[tokio::test]
    async fn missing_usage_report_falls_back_to_zero() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hi"),
            MockStreamEvent::FinalResponse(rig_core::test_utils::mock_final(
                rig_core::completion::Usage::default(),
            )),
        ]]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let output = run_model_stream(&model, request(), &cancel, &|_| {})
            .await
            .unwrap();
        assert_eq!(output.assistant.text(), "hi");
        assert_eq!(output.usage.total_tokens, 0);
    }

    /// A stream that ends without the provider's terminal record is
    /// truncation-damaged transport, not a short honest answer: it
    /// classifies retryable and carries a message.
    #[tokio::test]
    async fn unterminated_stream_is_transport_failure() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![MockStreamEvent::text("hi")]]);
        let (_flag, cancel) = crate::run::cancel_channel();
        match run_model_stream(&model, request(), &cancel, &|_| {}).await {
            Err(StreamFailure::Error { kind, .. }) => {
                assert!(kind.is_retryable(), "{kind:?}");
            }
            other => panic!("expected transport failure, got {other:?}"),
        }
    }

    /// A model whose stream never resolves: the first poll is the window an
    /// Esc lands in during a slow provider handshake.
    #[derive(Clone)]
    struct HangingModel;

    impl CompletionSource for HangingModel {
        fn completion(
            &self,
            _request: CompletionRequest,
        ) -> impl Future<Output = Result<rig_core::completion::CompletionResponse, ProviderError>> + Send
        {
            std::future::pending()
        }

        fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<rig_core::streaming::CompletionStream, ProviderError> {
            Ok(rig_core::streaming::CompletionStream::relay(
                "hanging",
                Box::pin(futures::stream::pending()),
            ))
        }
    }

    /// An Esc that lands while the handshake is still being polled must
    /// settle the turn as a normal cancellation (partial history commit)
    /// instead of hanging until the interrupt grace abort drops the turn.
    #[tokio::test]
    async fn cancel_during_stream_establishment_settles_immediately() {
        let (flag, cancel) = crate::run::cancel_channel();
        let task = tokio::spawn(async move {
            run_model_stream(&HangingModel, request(), &cancel, &|_| {}).await
        });
        // Let the task park inside the handshake poll, then interrupt.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        flag.set(true);
        match task.await.unwrap() {
            Err(StreamFailure::Cancelled { .. }) => {}
            other => panic!("expected cancellation, got {other:?}"),
        }
    }

    /// A token already cancelled before the call must not enter the stream.
    #[tokio::test]
    async fn pre_cancelled_token_cancels_before_establishment() {
        let (flag, cancel) = crate::run::cancel_channel();
        flag.set(true);
        match run_model_stream(&HangingModel, request(), &cancel, &|_| {}).await {
            Err(StreamFailure::Cancelled { .. }) => {}
            other => panic!("expected cancellation, got {other:?}"),
        }
    }
}
