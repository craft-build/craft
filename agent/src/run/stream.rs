//! Streaming aggregation over a provider model's stream.
//!
//! rig-core's `StreamingCompletionResponse` already assembles ragged
//! tool-argument fragments and multi-part text; this module consumes its
//! public events, forwards them as [`Event`]s (text/reasoning deltas, tool
//! start), and produces the completed [`history::Message`] for the assistant
//! turn plus its usage report.

use std::collections::HashMap;

use futures::StreamExt;
use rig_core::completion::{CompletionModel, CompletionRequest, FinishReason};
use rig_core::streaming::{StreamFinal, StreamedAssistantContent};

use crate::edge::{StreamedParts, assistant_from_stream, fold_streamed_event};
use crate::history;
use rig_core::completion::CompletionError;

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
/// (`craft-providers/src/error.rs`). rig surfaces most failures as strings
/// or `HttpError`, so classification starts from the HTTP status when one
/// survived and falls back to message matching.
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

/// Classify a typed rig completion error: use the preserved HTTP status when
/// one exists (non-success statuses surface as `InvalidStatusCodeWithMessage`),
/// otherwise match on the display string.
pub(crate) fn classify_error(error: &CompletionError) -> ErrorKind {
    if let CompletionError::HttpError(http) = error
        && let rig_core::http_client::Error::InvalidStatusCodeWithMessage(status, _) = http
    {
        match status.as_u16() {
            429 => return ErrorKind::RateLimited,
            500..=599 => return ErrorKind::Server,
            // 4xx bodies carry the real reason (overflow, content policy);
            // keep matching on the message instead of guessing Fatal.
            _ => {}
        }
    }
    classify_string(&error.to_string())
}

fn classify_string(message: &str) -> ErrorKind {
    if is_context_overflow(message) {
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

fn failure_from_error(error: &CompletionError) -> StreamFailure {
    StreamFailure::Error {
        kind: classify_error(error),
        message: error.to_string(),
    }
}

/// Run one model stream to completion.
pub(crate) async fn run_model_stream<M: CompletionModel + Clone>(
    model: &M,
    request: CompletionRequest,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> std::result::Result<TurnOutput, StreamFailure> {
    let mut stream = model
        .stream(request)
        .await
        .map_err(|e| failure_from_error(&e))?;
    // The reply text streamed so far; carried out on cancellation so the
    // committed history keeps what the user already saw.
    let mut streamed = String::new();
    let mut parts = StreamedParts::default();
    // provider tool-call id -> run-stable internal id recorded at call time.
    let mut call_ids: HashMap<String, String> = HashMap::new();
    let mut usage: Option<history::Usage> = None;
    let mut finish_reason: Option<FinishReason> = None;
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
                Some(Ok(event)) => {
                    match &event {
                        StreamedAssistantContent::Text(delta) => {
                            streamed.push_str(&delta.text);
                            emit(Event::TextDelta(delta.text.clone()));
                        }
                        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                            emit(Event::ThinkingDelta(reasoning.clone()));
                        }
                        StreamedAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id,
                        } => {
                            call_ids
                                .entry(tool_call.id.to_string())
                                .or_insert_with(|| internal_call_id.clone());
                            emit(Event::ToolStart {
                                id: internal_call_id.clone(),
                                name: tool_call.function.name.clone(),
                                arguments: tool_call.function.arguments.clone(),
                            });
                        }
                        StreamedAssistantContent::Final(final_) => {
                            usage = Some(usage_from_final(final_));
                            finish_reason = final_.finish_reason.clone();
                        }
                        _ => {}
                    }
                    fold_streamed_event(&mut parts, &event);
                }
            },
        }
    }
    let usage = usage.unwrap_or_else(|| usage_from_response(&stream));
    let assistant = assistant_from_stream(stream.choice.as_ref(), &call_ids, &parts);
    Ok(TurnOutput {
        assistant,
        usage,
        finish_reason,
    })
}

fn usage_from_final(final_: &StreamFinal) -> history::Usage {
    history::Usage {
        input_tokens: final_.usage.input_tokens,
        output_tokens: final_.usage.output_tokens,
        total_tokens: final_.usage.total_tokens,
    }
}

/// Zero-valued usage is the documented sentinel for a missing report.
fn usage_from_response(
    response: &rig_core::streaming::StreamingCompletionResponse,
) -> history::Usage {
    let usage = response.usage();
    history::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
    }
}

/// Whether a provider stream error means the prompt exceeded the context
/// window (ported from the reference's `AgentError::is_overflow`; ours has
/// only the provider's message string to classify). Matches the phrasings of
/// the major providers; deliberately excludes rate limits and output
/// truncation, which surface as `FinishReason::Length`, not stream errors.
pub(crate) fn is_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "prompt is too long",                                 // Anthropic
        "maximum context length",                             // OpenAI
        "context window",                                     // generic
        "input length and `max_tokens` exceed context limit", // Anthropic variant
        "exceeds the maximum number of tokens",               // Gemini
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

impl StreamFailure {
    pub(crate) fn is_overflow(&self) -> bool {
        matches!(self, Self::Error { kind, .. } if kind.is_overflow())
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
        crate::edge::to_request(&[], &[], None, None, None)
    }

    #[test]
    fn classifies_provider_overflow_messages() {
        for message in [
            "Error: prompt is too long: 210744 tokens > 200000 maximum",
            "This model's maximum context length is 4096 tokens. However, you requested ...",
            "input length and `max_tokens` exceed context limit: 10922 + 8192 > 8192",
            "The input token count (524389) exceeds the maximum number of tokens allowed (1048576)",
            "conversation exceeds the model context window",
        ] {
            assert!(is_context_overflow(message), "should match: {message}");
            assert!(StreamFailure::error(message).is_overflow());
        }
    }

    #[test]
    fn non_overflow_failures_do_not_match() {
        for message in [
            "rate limit exceeded, retry after 30s",
            "invalid api key",
            "connection closed before response",
            "maximum tokens per request is 128000 for this tier",
        ] {
            assert!(!is_context_overflow(message), "must not match: {message}");
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
        ];
        for (message, expected) in cases {
            let StreamFailure::Error { kind, .. } = StreamFailure::error(message) else {
                panic!("must classify: {message}");
            };
            assert_eq!(kind, expected, "message: {message}");
        }
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

    /// A typed `HttpError` with a preserved status classifies without string
    /// matching.
    #[test]
    fn classifies_typed_http_status() {
        for (status, expected) in [
            (429u16, ErrorKind::RateLimited),
            (500, ErrorKind::Server),
            (503, ErrorKind::Server),
            (400, ErrorKind::Fatal),
            (401, ErrorKind::Fatal),
        ] {
            let error = CompletionError::HttpError(
                rig_core::http_client::Error::InvalidStatusCodeWithMessage(
                    http::StatusCode::from_u16(status).unwrap(),
                    "boom".into(),
                ),
            );
            assert_eq!(classify_error(&error), expected, "status {status}");
        }
    }

    /// A dropped `CancelFlag` must disable the cancel branch instead of
    /// busy-looping: the stream still completes and aggregates normally.
    #[tokio::test]
    async fn dropped_cancel_flag_lets_the_stream_complete() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::final_response_with_total_tokens(3),
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

    /// A `Final` event's finish reason rides the turn output.
    #[tokio::test]
    async fn finish_reason_rides_the_turn_output() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("trunc"),
            MockStreamEvent::FinalResponse(
                rig_core::streaming::StreamFinal::new("mock", rig_core::completion::Usage::new())
                    .with_finish_reason(rig_core::completion::FinishReason::Length),
            ),
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

    /// A stream without a `Final` usage event falls back to the response's
    /// usage report (zero-valued sentinel when the provider sent none).
    #[tokio::test]
    async fn missing_final_event_falls_back_to_response_usage() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![MockStreamEvent::text("hi")]]);
        let (_flag, cancel) = crate::run::cancel_channel();
        let output = run_model_stream(&model, request(), &cancel, &|_| {})
            .await
            .unwrap();
        assert_eq!(output.assistant.text(), "hi");
        assert_eq!(output.usage.total_tokens, 0);
    }
}
