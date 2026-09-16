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

/// How a model stream ended without producing a turn.
#[derive(Debug, Clone)]
pub enum StreamFailure {
    Cancelled,
    Error(String),
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
        .map_err(|e| StreamFailure::Error(e.to_string()))?;
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
                    Ok(()) => return Err(StreamFailure::Cancelled),
                    Err(_) => cancel_alive = false,
                }
            }
            item = stream.next() => match item {
                None => break,
                Some(Err(error)) => return Err(StreamFailure::Error(error.to_string())),
                Some(Ok(event)) => {
                    match &event {
                        StreamedAssistantContent::Text(delta) => {
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
        matches!(self, Self::Error(message) if is_context_overflow(message))
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
            assert!(StreamFailure::Error(message.into()).is_overflow());
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
            assert!(!StreamFailure::Error(message.into()).is_overflow());
        }
        assert!(!StreamFailure::Cancelled.is_overflow());
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
