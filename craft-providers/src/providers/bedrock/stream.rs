//! Drives a Bedrock `ConverseStream` event receiver to completion, assembling a
//! [`StreamResponse`] and forwarding live deltas via `event_tx`.
//!
//! The loop itself lives in `mod.rs::stream_message` because the SDK's
//! `EventReceiver` type is in a private module and cannot be named in a
//! signature. This module owns the pure, testable per-event processor plus a
//! generic driver exercised by unit tests via [`InMemoryStream`].

use aws_sdk_bedrockruntime::types::ConverseStreamOutput;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use base64::Engine;
use flume::Sender;
use serde_json::Value;
use tracing::{debug, warn};

use crate::AgentError;
use crate::model::TokenUsage;
use crate::providers::bedrock::map_stop_reason;
use crate::types::{ContentBlock, ProviderEvent, StopReason};

/// Per-block buffer for streaming tool-use input JSON, indexed by block.
#[derive(Default)]
pub(crate) struct BlockState {
    pub(crate) tool_input_json: String,
}

/// Type alias for the raw message the SDK attaches to stream errors.
pub(crate) type RawMessage = aws_smithy_types::event_stream::RawMessage;

/// Error returned by a [`ConverseStream`] receiver.
#[cfg(all(test, feature = "bedrock"))]
pub(crate) type StreamError =
    SdkError<aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError, RawMessage>;

/// Abstraction over the SDK `EventReceiver` so the loop is testable without the
/// smithy eventstream codec: tests supply [`InMemoryStream`].
#[cfg(all(test, feature = "bedrock"))]
pub(crate) trait ConverseStream: Send {
    async fn recv(&mut self) -> Result<Option<ConverseStreamOutput>, StreamError>;
}

/// Drives any [`ConverseStream`] to completion. The real provider drives the
/// SDK receiver inline (its type is in a private module); tests use this with
/// [`InMemoryStream`].
#[cfg(all(test, feature = "bedrock"))]
pub(crate) async fn run_converse_stream<S: ConverseStream + Unpin>(
    mut stream: S,
    event_tx: &Sender<ProviderEvent>,
) -> Result<crate::types::StreamResponse, AgentError> {
    use crate::types::{Message, Role, StreamResponse};
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut block_states: Vec<BlockState> = Vec::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason: Option<StopReason> = None;

    loop {
        let event = match stream.recv().await {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(err) => return Err(map_recv_error(err)),
        };
        process_event(
            &event,
            &mut content_blocks,
            &mut block_states,
            &mut usage,
            &mut stop_reason,
            event_tx,
        )
        .await?;
        if stop_reason.is_some() {
            break;
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

/// Pure per-event processor, extracted from the loop so it can be unit-tested
/// with synthetic AWS events (no smithy codec required).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_event(
    event: &ConverseStreamOutput,
    content_blocks: &mut Vec<ContentBlock>,
    block_states: &mut Vec<BlockState>,
    usage: &mut TokenUsage,
    stop_reason: &mut Option<StopReason>,
    event_tx: &Sender<ProviderEvent>,
) -> Result<(), AgentError> {
    match event {
        ConverseStreamOutput::MessageStart(_) => {}
        ConverseStreamOutput::ContentBlockStart(ev) => {
            let idx = ev.content_block_index() as usize;
            ensure_block_slots(content_blocks, block_states, idx);
            if let Some(start) = ev.start()
                && let aws_sdk_bedrockruntime::types::ContentBlockStart::ToolUse(tu) = start
            {
                let id = tu.tool_use_id().to_string();
                let name = tu.name().to_string();
                event_tx
                    .send_async(ProviderEvent::ToolUseStart {
                        id: id.clone(),
                        name: name.clone(),
                    })
                    .await?;
                content_blocks[idx] = ContentBlock::ToolUse {
                    id,
                    name,
                    input: Value::Null,
                };
                return Ok(());
            }
            content_blocks[idx] = ContentBlock::Text {
                text: String::new(),
            };
        }
        ConverseStreamOutput::ContentBlockDelta(ev) => {
            let idx = ev.content_block_index() as usize;
            let Some(delta) = ev.delta() else {
                return Ok(());
            };
            use aws_sdk_bedrockruntime::types::ContentBlockDelta as D;
            match delta {
                D::Text(text) => {
                    if !text.is_empty() {
                        ensure_block_slots(content_blocks, block_states, idx);
                        if !matches!(content_blocks.get(idx), Some(ContentBlock::Text { .. })) {
                            content_blocks[idx] = ContentBlock::Text {
                                text: String::new(),
                            };
                        }
                        if let Some(ContentBlock::Text { text: t }) = content_blocks.get_mut(idx) {
                            t.push_str(text);
                        }
                        event_tx
                            .send_async(ProviderEvent::TextDelta {
                                text: text.to_string(),
                            })
                            .await?;
                    }
                }
                D::ToolUse(tu) => {
                    ensure_block_slots(content_blocks, block_states, idx);
                    if let Some(state) = block_states.get_mut(idx) {
                        state.tool_input_json.push_str(tu.input());
                    }
                }
                D::ReasoningContent(rc) => {
                    use aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta as R;
                    match rc {
                        R::Text(text) => {
                            ensure_reasoning_slot(content_blocks, block_states, idx);
                            if !text.is_empty() {
                                if let Some(ContentBlock::Thinking { thinking: t, .. }) =
                                    content_blocks.get_mut(idx)
                                {
                                    t.push_str(text);
                                }
                                event_tx
                                    .send_async(ProviderEvent::ThinkingDelta {
                                        text: text.to_string(),
                                    })
                                    .await?;
                            }
                        }
                        R::Signature(sig) => {
                            ensure_reasoning_slot(content_blocks, block_states, idx);
                            if let Some(ContentBlock::Thinking { signature, .. }) =
                                content_blocks.get_mut(idx)
                            {
                                *signature = Some(sig.to_string());
                            }
                        }
                        R::RedactedContent(blob) => {
                            ensure_reasoning_slot(content_blocks, block_states, idx);
                            let data =
                                base64::engine::general_purpose::STANDARD.encode(blob.as_ref());
                            if let Some(slot) = content_blocks.get_mut(idx) {
                                *slot = ContentBlock::RedactedThinking { data };
                            }
                        }
                        _ => debug!("ignoring unrecognized reasoning delta"),
                    }
                }
                _ => debug!("ignoring non-text/tool/reasoning delta"),
            }
        }
        ConverseStreamOutput::ContentBlockStop(ev) => {
            let idx = ev.content_block_index() as usize;
            if let Some(ContentBlock::ToolUse { input, .. }) = content_blocks.get_mut(idx) {
                let raw = block_states
                    .get_mut(idx)
                    .map(|s| std::mem::take(&mut s.tool_input_json))
                    .unwrap_or_default();
                *input = parse_tool_json(&raw);
            }
        }
        ConverseStreamOutput::Metadata(meta) => {
            if let Some(u) = meta.usage() {
                usage.input = u.input_tokens().max(0) as u32;
                usage.output = u.output_tokens().max(0) as u32;
                usage.cache_read = u
                    .cache_read_input_tokens()
                    .map(|n| n.max(0) as u32)
                    .unwrap_or(0);
                usage.cache_creation = u
                    .cache_write_input_tokens()
                    .map(|n| n.max(0) as u32)
                    .unwrap_or(0);
            }
        }
        ConverseStreamOutput::MessageStop(ev) => {
            *stop_reason = Some(map_stop_reason(ev.stop_reason()));
        }
        _ => debug!("ignoring unknown ConverseStreamOutput variant"),
    }
    Ok(())
}

fn ensure_block_slots(blocks: &mut Vec<ContentBlock>, states: &mut Vec<BlockState>, idx: usize) {
    if blocks.len() <= idx {
        blocks.resize(
            idx + 1,
            ContentBlock::Text {
                text: String::new(),
            },
        );
    }
    if states.len() <= idx {
        states.resize_with(idx + 1, BlockState::default);
    }
}

/// Promotes an existing placeholder block at `idx` to a `Thinking` block if it
/// is still an empty Text, so reasoning deltas accumulate correctly.
fn ensure_reasoning_slot(blocks: &mut Vec<ContentBlock>, states: &mut Vec<BlockState>, idx: usize) {
    ensure_block_slots(blocks, states, idx);
    if matches!(blocks[idx], ContentBlock::Text { ref text } if text.is_empty()) {
        blocks[idx] = ContentBlock::Thinking {
            thinking: String::new(),
            signature: None,
        };
    }
}

fn parse_tool_json(raw: &str) -> Value {
    if raw.is_empty() {
        return Value::Object(Default::default());
    }
    match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, json = %raw, "malformed tool-use JSON, falling back to {{}}");
            Value::Object(Default::default())
        }
    }
}

/// Stream `recv()` errors carry `RawMessage` (no HTTP status); map via the
/// service error's `code()` when available.
pub(crate) fn map_recv_error<E>(err: SdkError<E, RawMessage>) -> AgentError
where
    E: aws_smithy_types::error::metadata::ProvideErrorMetadata,
{
    let code = err.code().unwrap_or("");
    let message = err
        .message()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{err}"));
    let status = crate::providers::bedrock::status_for_code(code).unwrap_or(500);
    warn!(code, status, message = %message, "bedrock stream error");
    AgentError::Api { status, message }
}

/// Test-only stream backed by an in-memory `Vec` of events.
#[cfg(all(test, feature = "bedrock"))]
pub(crate) struct InMemoryStream {
    events: std::vec::IntoIter<ConverseStreamOutput>,
}

#[cfg(all(test, feature = "bedrock"))]
impl InMemoryStream {
    pub(crate) fn new(events: Vec<ConverseStreamOutput>) -> Self {
        Self {
            events: events.into_iter(),
        }
    }
}

#[cfg(all(test, feature = "bedrock"))]
impl ConverseStream for InMemoryStream {
    async fn recv(&mut self) -> Result<Option<ConverseStreamOutput>, StreamError> {
        Ok(self.events.next())
    }
}

#[cfg(all(test, feature = "bedrock"))]
mod tests {
    use super::*;
    use crate::types::{Role, StopReason, StreamResponse};
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStart, ContentBlockStartEvent,
        ContentBlockStopEvent, ConverseStreamMetadataEvent, ConverseStreamOutput, MessageStopEvent,
        ReasoningContentBlockDelta, StopReason as AwsStopReason, TokenUsage as AwsTokenUsage,
        ToolUseBlockDelta, ToolUseBlockStart,
    };

    async fn drive(events: Vec<ConverseStreamOutput>) -> StreamResponse {
        let (event_tx, _event_rx) = flume::unbounded();
        run_converse_stream(InMemoryStream::new(events), &event_tx)
            .await
            .unwrap()
    }

    async fn drive_with_events_collected(
        events: Vec<ConverseStreamOutput>,
    ) -> (StreamResponse, Vec<ProviderEvent>) {
        let (event_tx, event_rx) = flume::unbounded();
        let resp = run_converse_stream(InMemoryStream::new(events), &event_tx)
            .await
            .unwrap();
        let emitted: Vec<ProviderEvent> = event_rx.drain().collect();
        (resp, emitted)
    }

    #[test]
    fn parse_tool_json_empty_returns_empty_object() {
        assert_eq!(parse_tool_json(""), Value::Object(Default::default()));
    }

    #[test]
    fn parse_tool_json_valid_passes_through() {
        let v = parse_tool_json(r#"{"command":"ls"}"#);
        assert_eq!(v["command"], "ls");
    }

    #[test]
    fn parse_tool_json_malformed_falls_back_to_empty_object() {
        assert_eq!(
            parse_tool_json("{not json"),
            Value::Object(Default::default())
        );
    }

    #[tokio::test]
    async fn empty_stream_yields_empty_assistant_message() {
        let resp = drive(vec![]).await;
        assert!(matches!(resp.message.role, Role::Assistant));
        assert!(resp.message.content.is_empty());
        assert_eq!(resp.stop_reason, None);
    }

    #[tokio::test]
    async fn text_delta_assembles_into_text_block_and_emits_event() {
        let events = vec![
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::Text("Hel".to_string()))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::Text("lo".to_string()))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::MessageStop(
                MessageStopEvent::builder()
                    .stop_reason(AwsStopReason::EndTurn)
                    .build()
                    .unwrap(),
            ),
        ];
        let (resp, emitted) = drive_with_events_collected(events).await;
        assert_eq!(resp.message.content.len(), 1);
        match &resp.message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        let text_deltas: Vec<String> = emitted
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text_deltas, vec!["Hel".to_string(), "lo".to_string()]);
    }

    #[tokio::test]
    async fn tool_use_assembles_input_json_across_deltas() {
        let events = vec![
            ConverseStreamOutput::ContentBlockStart(
                ContentBlockStartEvent::builder()
                    .content_block_index(0)
                    .start(ContentBlockStart::ToolUse(
                        ToolUseBlockStart::builder()
                            .tool_use_id("tu_1")
                            .name("bash")
                            .build()
                            .unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ToolUse(
                        ToolUseBlockDelta::builder()
                            .input("{\"cmd\":".to_string())
                            .build()
                            .unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ToolUse(
                        ToolUseBlockDelta::builder()
                            .input("\"ls\"}".to_string())
                            .build()
                            .unwrap(),
                    ))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::ContentBlockStop(
                ContentBlockStopEvent::builder()
                    .content_block_index(0)
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::MessageStop(
                MessageStopEvent::builder()
                    .stop_reason(AwsStopReason::ToolUse)
                    .build()
                    .unwrap(),
            ),
        ];
        let (resp, emitted) = drive_with_events_collected(events).await;
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
        match &resp.message.content[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "bash");
                assert_eq!(input["cmd"], "ls");
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
        assert!(matches!(
            emitted[0],
            ProviderEvent::ToolUseStart { ref id, ref name } if id == "tu_1" && name == "bash"
        ));
    }

    #[tokio::test]
    async fn reasoning_text_and_signature_assemble_into_thinking_block() {
        let events = vec![
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ReasoningContent(
                        ReasoningContentBlockDelta::Text("so ".to_string()),
                    ))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ReasoningContent(
                        ReasoningContentBlockDelta::Text("careful".to_string()),
                    ))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::ContentBlockDelta(
                ContentBlockDeltaEvent::builder()
                    .content_block_index(0)
                    .delta(ContentBlockDelta::ReasoningContent(
                        ReasoningContentBlockDelta::Signature("sig-abc".to_string()),
                    ))
                    .build()
                    .unwrap(),
            ),
            ConverseStreamOutput::MessageStop(
                MessageStopEvent::builder()
                    .stop_reason(AwsStopReason::EndTurn)
                    .build()
                    .unwrap(),
            ),
        ];
        let (resp, emitted) = drive_with_events_collected(events).await;
        match &resp.message.content[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "so careful");
                assert_eq!(signature.as_deref(), Some("sig-abc"));
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
        let thinking_deltas: Vec<String> = emitted
            .iter()
            .filter_map(|e| match e {
                ProviderEvent::ThinkingDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking_deltas,
            vec!["so ".to_string(), "careful".to_string()]
        );
    }

    #[tokio::test]
    async fn metadata_populates_usage() {
        let events = vec![
            ConverseStreamOutput::Metadata(
                ConverseStreamMetadataEvent::builder()
                    .usage(
                        AwsTokenUsage::builder()
                            .input_tokens(120)
                            .output_tokens(8)
                            .total_tokens(128)
                            .cache_read_input_tokens(40)
                            .cache_write_input_tokens(12)
                            .build()
                            .unwrap(),
                    )
                    .build(),
            ),
            ConverseStreamOutput::MessageStop(
                MessageStopEvent::builder()
                    .stop_reason(AwsStopReason::EndTurn)
                    .build()
                    .unwrap(),
            ),
        ];
        let (resp, _) = drive_with_events_collected(events).await;
        assert_eq!(resp.usage.input, 120);
        assert_eq!(resp.usage.output, 8);
        assert_eq!(resp.usage.cache_read, 40);
        assert_eq!(resp.usage.cache_creation, 12);
    }
}
