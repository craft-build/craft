//! Streaming aggregation over a provider model's stream.
//!
//! rig-core's `StreamingCompletionResponse` already assembles ragged
//! tool-argument fragments and multi-part text; this module consumes its
//! public events, forwards them as [`Event`]s (text/reasoning deltas, tool
//! start), and produces the completed [`history::Message`] for the assistant
//! turn plus its usage report.

use std::collections::HashMap;

use futures::StreamExt;
use rig_core::completion::{CompletionModel, CompletionRequest};
use rig_core::streaming::{StreamFinal, StreamedAssistantContent};

use crate::edge::{StreamedParts, assistant_from_stream, fold_streamed_event};
use crate::history;

use super::{CancelToken, Event};

/// The aggregated result of one model call.
#[derive(Debug, Clone)]
pub struct TurnOutput {
    pub assistant: history::Message,
    pub usage: history::Usage,
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
                    Ok(()) if *cancel_rx.borrow_and_update() => {
                        return Err(StreamFailure::Cancelled);
                    }
                    Ok(()) => {}
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
                            emit(Event::ReasoningDelta(reasoning.clone()));
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
                        }
                        _ => {}
                    }
                    fold_streamed_event(&mut parts, &event);
                }
            },
        }
    }
    let usage = usage.unwrap_or_else(|| usage_from_response(&stream));
    emit(Event::Usage(usage));
    let assistant = assistant_from_stream(stream.choice.as_ref(), &call_ids, &parts);
    Ok(TurnOutput { assistant, usage })
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
