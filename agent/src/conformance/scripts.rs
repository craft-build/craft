//! Protocol script builders: the response bodies each dialect's parser
//! accepts, expressed as one function per turn shape. Shapes mirror
//! animist v0.7.0's protocol parsers (its SSE parser reads `data:` lines
//! only; `event:` lines are cosmetic).

use serde_json::{Value, json};

/// Turn-wide usage counters the scripts report.
#[derive(Clone, Copy)]
pub(super) struct Usage {
    pub input: u64,
    pub output: u64,
}

impl Usage {
    pub(super) fn new(input: u64, output: u64) -> Self {
        Self { input, output }
    }

    fn anthropic(&self) -> Value {
        json!({"input_tokens": self.input, "output_tokens": self.output})
    }

    fn openai(&self) -> Value {
        json!({"prompt_tokens": self.input, "completion_tokens": self.output})
    }

    fn responses(&self) -> Value {
        json!({"input_tokens": self.input, "output_tokens": self.output})
    }

    fn converse(&self) -> Value {
        json!({"inputTokens": self.input, "outputTokens": self.output})
    }
}

// --- Anthropic Messages (and Bedrock's anthropic_messages protocol) ---------------

/// SSE text carrying `events` as `data:` frames (`event:` names included
/// for fidelity; the parser ignores them).
pub(super) fn anthropic_sse(events: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for event in events {
        let kind = event["type"].as_str().unwrap_or("unknown");
        out.push_str(&format!("event: {kind}\r\ndata: {event}\r\n\r\n"));
    }
    out.into_bytes()
}

/// A complete assistant text turn.
pub(super) fn anthropic_text(text: &str, usage: Usage) -> Vec<u8> {
    anthropic_sse(&[
        json!({"type": "message_start", "message": {"role": "assistant", "content": [],
              "usage": usage.anthropic()}}),
        json!({"type": "content_block_start", "index": 0,
              "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0,
              "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
              "delta": {"stop_reason": "end_turn"},
              "usage": {"output_tokens": usage.output}}),
        json!({"type": "message_stop"}),
    ])
}

/// A thinking block followed by a text reply.
pub(super) fn anthropic_thinking(thinking: &str, text: &str, usage: Usage) -> Vec<u8> {
    anthropic_sse(&[
        json!({"type": "message_start", "message": {"role": "assistant", "content": [],
              "usage": usage.anthropic()}}),
        json!({"type": "content_block_start", "index": 0,
              "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0,
              "delta": {"type": "thinking_delta", "thinking": thinking}}),
        json!({"type": "content_block_delta", "index": 0,
              "delta": {"type": "signature_delta", "signature": "sig-1"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1,
              "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1,
              "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta",
              "delta": {"stop_reason": "end_turn"},
              "usage": {"output_tokens": usage.output}}),
        json!({"type": "message_stop"}),
    ])
}

/// One tool call (`input` is the arguments object).
pub(super) fn anthropic_tool_call(id: &str, name: &str, input: Value, usage: Usage) -> Vec<u8> {
    anthropic_sse(&[
        json!({"type": "message_start", "message": {"role": "assistant", "content": [],
              "usage": usage.anthropic()}}),
        json!({"type": "content_block_start", "index": 0,
              "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        json!({"type": "content_block_delta", "index": 0,
              "delta": {"type": "input_json_delta", "partial_json": input.to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta",
              "delta": {"stop_reason": "tool_use"},
              "usage": {"output_tokens": usage.output}}),
        json!({"type": "message_stop"}),
    ])
}

// --- OpenAI Chat Completions (and every chat-completions dialect kind) --------------

fn chat_sse(chunks: &[Value], usage: Option<&Value>) -> Vec<u8> {
    let mut out = String::new();
    for chunk in chunks {
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    if let Some(usage) = usage {
        // A trailing usage-only chunk rides before [DONE].
        out.push_str(&format!("data: {usage}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out.into_bytes()
}

/// A complete assistant text turn (`usage` rides a trailing chunk).
pub(super) fn chat_text(text: &str, usage: Usage) -> Vec<u8> {
    let chunks = vec![json!({"choices": [{"index": 0, "delta": {"content": text}}]})];
    let trailer = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                         "usage": usage.openai()});
    chat_sse(&chunks, Some(&trailer))
}

/// A reasoning turn: `reasoning_content` deltas then the reply text.
pub(super) fn chat_thinking(thinking: &str, text: &str, usage: Usage) -> Vec<u8> {
    let chunks = vec![
        json!({"choices": [{"index": 0, "delta": {"reasoning_content": thinking}}]}),
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
    ];
    let trailer = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                         "usage": usage.openai()});
    chat_sse(&chunks, Some(&trailer))
}

/// One tool call (`arguments` is the JSON string of the arguments object).
pub(super) fn chat_tool_call(id: &str, name: &str, arguments: &str, usage: Usage) -> Vec<u8> {
    let chunks = vec![
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "id": id, "type": "function",
            "function": {"name": name, "arguments": ""}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0, "function": {"arguments": arguments}}]}}]}),
    ];
    let trailer = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                         "usage": usage.openai()});
    chat_sse(&chunks, Some(&trailer))
}

// --- OpenAI Responses ---------------------------------------------------------------

fn responses_sse(events: &[Value]) -> Vec<u8> {
    let mut out = String::new();
    for event in events {
        out.push_str(&format!("data: {event}\n\n"));
    }
    out.into_bytes()
}

pub(super) fn responses_text(text: &str, usage: Usage) -> Vec<u8> {
    responses_sse(&[
        json!({"type": "response.output_text.delta", "delta": text}),
        json!({"type": "response.completed", "response": {
            "status": "completed",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": text}]}],
            "usage": usage.responses()}}),
    ])
}

pub(super) fn responses_thinking(thinking: &str, text: &str, usage: Usage) -> Vec<u8> {
    responses_sse(&[
        json!({"type": "response.reasoning_summary_text.delta", "delta": thinking}),
        json!({"type": "response.output_text.delta", "delta": text}),
        json!({"type": "response.completed", "response": {
            "status": "completed",
            "output": [{"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                        "content": [{"type": "output_text", "text": text}]}],
            "usage": usage.responses()}}),
    ])
}

pub(super) fn responses_tool_call(id: &str, name: &str, arguments: &str, usage: Usage) -> Vec<u8> {
    responses_sse(&[
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "function_call", "id": "fc_1", "call_id": id,
                        "name": name, "arguments": ""}}),
        json!({"type": "response.function_call_arguments.delta",
               "item_id": "fc_1", "delta": arguments}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"type": "function_call", "id": "fc_1", "call_id": id,
                        "name": name, "arguments": arguments, "status": "completed"}}),
        json!({"type": "response.completed", "response": {
            "status": "completed",
            "output": [{"type": "function_call", "id": "fc_1", "call_id": id,
                        "name": name, "arguments": arguments, "status": "completed"}],
            "usage": usage.responses()}}),
    ])
}

// --- Bedrock Converse (AWS eventstream framing) --------------------------------------

/// One AWS eventstream frame: prelude (total length, headers length,
/// prelude CRC32), headers, payload, message CRC32. Header values are
/// strings (the only kind the mock needs).
fn eventstream_frame(headers: &[(&str, &str)], payload: &Value) -> Vec<u8> {
    let mut encoded_headers = Vec::new();
    for (name, value) in headers {
        encoded_headers.push(name.len() as u8);
        encoded_headers.extend_from_slice(name.as_bytes());
        encoded_headers.push(7); // string value
        encoded_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        encoded_headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(payload).expect("eventstream payload serializes");
    let total = 16 + encoded_headers.len() + payload.len();
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&(total as u32).to_be_bytes());
    frame.extend_from_slice(&(encoded_headers.len() as u32).to_be_bytes());
    let prelude_crc = crc32(&frame);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(&encoded_headers);
    frame.extend_from_slice(&payload);
    let message_crc = crc32(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

/// CRC-32 (IEEE 802.3, reflected), as AWS eventstream requires.
fn crc32(data: &[u8]) -> u32 {
    // Standard bitwise implementation; the payloads are small.
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn converse_event(event_type: &str, payload: Value) -> Vec<u8> {
    eventstream_frame(
        &[
            (":message-type", "event"),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ],
        &payload,
    )
}

/// A complete Converse text turn: start, one text block, stop, metadata.
pub(super) fn converse_text(text: &str, usage: Usage) -> Vec<u8> {
    let mut body = converse_event("messageStart", json!({"role": "assistant"}));
    // No contentBlockStart for text blocks: animist v0.7.0's Converse
    // parser rejects a text-open start event (its `Start` shape covers
    // toolUse only), so real-Bedrock streams that emit one — and parsers
    // that grow to require it — are an animist followup, not this mock's
    // dialect. Recorded as animist followup F12.
    body.extend_from_slice(&converse_event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0, "delta": {"text": text}}),
    ));
    body.extend_from_slice(&converse_event(
        "contentBlockStop",
        json!({"contentBlockIndex": 0}),
    ));
    body.extend_from_slice(&converse_event(
        "messageStop",
        json!({"stopReason": "end_turn"}),
    ));
    body.extend_from_slice(&converse_event(
        "metadata",
        json!({"usage": usage.converse()}),
    ));
    body
}

/// One Converse tool call (`input` is the arguments object).
pub(super) fn converse_tool_call(id: &str, name: &str, input: Value, usage: Usage) -> Vec<u8> {
    let mut body = converse_event("messageStart", json!({"role": "assistant"}));
    body.extend_from_slice(&converse_event(
        "contentBlockStart",
        json!({"contentBlockIndex": 0,
               "start": {"toolUse": {"toolUseId": id, "name": name}}}),
    ));
    body.extend_from_slice(&converse_event(
        "contentBlockDelta",
        json!({"contentBlockIndex": 0,
               "delta": {"toolUse": {"input": input.to_string()}}}),
    ));
    body.extend_from_slice(&converse_event(
        "contentBlockStop",
        json!({"contentBlockIndex": 0}),
    ));
    body.extend_from_slice(&converse_event(
        "messageStop",
        json!({"stopReason": "tool_use"}),
    ));
    body.extend_from_slice(&converse_event(
        "metadata",
        json!({"usage": usage.converse()}),
    ));
    body
}

// --- Shared helpers ------------------------------------------------------------------

/// A real 1×1 PNG the image suites read through the `view_image` tool,
/// encoded by the same crate that decodes it.
pub(super) fn tiny_png() -> Vec<u8> {
    use image::ImageEncoder as _;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[0u8, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("1x1 png encodes");
    bytes.into_inner()
}
