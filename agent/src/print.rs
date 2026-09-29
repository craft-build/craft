//! G.3 print mode (headless): `craft -p` with text or stream-json output.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged: `stream-json` is JSONL (`system/init`, `assistant`, `user`,
//! `system/api_retry`, then a final `result` line). Text output is always
//! the raw reply (`--verbose` events only surface in stream-json mode). We
//! adopt new fields when Claude Code adds them but never invent our own.
//!
//! Unlike the reference (whose print mode exits 0 even on agent errors),
//! [`emit`] reports agent failures so the caller can exit nonzero.

use std::io::Cursor;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use serde_json::{Value, json};

use crate::cli::OutputFormat;
use crate::error::{InvalidSnafu, Result};
use crate::headless::HeadlessHandle;
use crate::history::{ImageBlock, ImageMedia, Message, Usage};
use crate::id::SessionRef;
use crate::run::{DoneReason, Event};

const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn invalid(reason: impl Into<String>) -> crate::error::Error {
    InvalidSnafu {
        reason: reason.into(),
    }
    .build()
}

/// Attach `--image` paths to the prompt as vision content. Files are fully
/// decoded before base64 so a corrupt image fails here instead of poisoning
/// the message history (same rationale as the `view_image` tool).
pub fn load_images(paths: &[std::path::PathBuf]) -> Result<Vec<ImageBlock>> {
    paths.iter().map(|path| load_image(path)).collect()
}

fn load_image(path: &Path) -> Result<ImageBlock> {
    let bytes = std::fs::read(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    let format = image::guess_format(&bytes)
        .map_err(|e| invalid(format!("{} is not an image: {e}", path.display())))?;
    let media = media_of(format).ok_or_else(|| {
        invalid(format!(
            "unsupported image format for {}: only png, jpeg, gif, and webp can be attached",
            path.display()
        ))
    })?;
    image::ImageReader::with_format(Cursor::new(&bytes), format)
        .decode()
        .map_err(|e| invalid(format!("cannot decode {}: {e}", path.display())))?;
    Ok(ImageBlock {
        media_type: media,
        data: base64::engine::general_purpose::STANDARD.encode(&bytes),
        caption: path.display().to_string(),
    })
}

fn media_of(format: image::ImageFormat) -> Option<ImageMedia> {
    match format {
        image::ImageFormat::Png => Some(ImageMedia::Png),
        image::ImageFormat::Jpeg => Some(ImageMedia::Jpeg),
        image::ImageFormat::Gif => Some(ImageMedia::Gif),
        image::ImageFormat::WebP => Some(ImageMedia::Webp),
        _ => None,
    }
}

/// Claude Code stop-reason strings for our [`DoneReason`] taxonomy.
fn stop_reason(reason: DoneReason) -> &'static str {
    match reason {
        DoneReason::Stop => "end_turn",
        DoneReason::MaxTurns => "max_turns",
        DoneReason::MaxTokens => "max_tokens",
        DoneReason::Cancelled => "cancelled",
        DoneReason::Error => "error",
        DoneReason::DoomStop => "doom_stop",
    }
}

#[derive(Serialize)]
struct UsageValue {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

impl From<Usage> for UsageValue {
    fn from(usage: Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
        }
    }
}

#[derive(Serialize)]
struct PrintResult {
    #[serde(rename = "type")]
    result_type: &'static str,
    subtype: &'static str,
    is_error: bool,
    duration_ms: u128,
    num_turns: u32,
    result: String,
    stop_reason: Option<&'static str>,
    session_id: SessionRef,
    total_cost_usd: f64,
    usage: UsageValue,
}

#[derive(Serialize)]
struct InitEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    cwd: &'a str,
    session_id: &'a SessionRef,
    tools: &'a [String],
    model: &'a str,
}

#[derive(Serialize)]
struct AssistantEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: AssistantMessage<'a>,
    session_id: &'a SessionRef,
}

#[derive(Serialize)]
struct AssistantMessage<'a> {
    model: &'a str,
    role: &'static str,
    content: Vec<Value>,
    usage: UsageValue,
}

#[derive(Serialize)]
struct UserEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: UserMessage,
    session_id: &'a SessionRef,
}

#[derive(Serialize)]
struct UserMessage {
    role: &'static str,
    content: Value,
}

#[derive(Serialize)]
struct RetryEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    attempt: u32,
    retry_delay_ms: u64,
    error: &'a str,
    session_id: &'a SessionRef,
}

/// Stream-json line for a non-fatal system notice (C.12 advisor notes).
#[derive(Serialize)]
struct SystemEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    severity: &'a str,
    message: &'a str,
    session_id: &'a SessionRef,
}

/// Where the verbose/stream events go: JSONL lines, a collected array, or
/// nowhere (plain text without `--verbose`).
enum WireOut {
    None,
    Json(Vec<Value>),
    StreamJson,
}

impl WireOut {
    fn emit(&mut self, value: &impl Serialize) -> Result<()> {
        match self {
            Self::None => {}
            Self::StreamJson => println!(
                "{}",
                serde_json::to_string(value)
                    .map_err(|e| invalid(format!("serializing event: {e}")))?
            ),
            Self::Json(events) => events.push(
                serde_json::to_value(value)
                    .map_err(|e| invalid(format!("serializing event: {e}")))?,
            ),
        }
        Ok(())
    }
}

/// Folds the run's event stream into the print-mode outputs. `observe`
/// returns `true` on the terminal event (`Done` or `Error`).
struct PrintState {
    session_id: SessionRef,
    model: String,
    result_text: String,
    turn_text: String,
    turn_tools: Vec<Value>,
    num_turns: u32,
    usage: Usage,
    cost: Option<f64>,
    stop_reason: Option<&'static str>,
    error: Option<String>,
    out: WireOut,
}

impl PrintState {
    fn new(session_id: SessionRef, model: String, format: &OutputFormat, verbose: bool) -> Self {
        let out = match format {
            OutputFormat::StreamJson => WireOut::StreamJson,
            _ if verbose => WireOut::Json(Vec::new()),
            _ => WireOut::None,
        };
        Self {
            session_id,
            model,
            result_text: String::new(),
            turn_text: String::new(),
            turn_tools: Vec::new(),
            num_turns: 0,
            usage: Usage::default(),
            cost: None,
            stop_reason: None,
            error: None,
            out,
        }
    }

    fn init(&mut self, cwd: &str, tools: &[String]) -> Result<()> {
        self.out.emit(&InitEvent {
            event_type: "system",
            subtype: "init",
            cwd,
            session_id: &self.session_id,
            tools,
            model: &self.model,
        })
    }

    /// The assistant message content for the in-flight turn, rebuilt from
    /// the streamed deltas and tool calls (our `TurnComplete` carries usage
    /// only, not the message).
    fn turn_content(&mut self) -> Vec<Value> {
        let mut content = Vec::new();
        if !self.turn_text.is_empty() {
            content.push(json!({"type": "text", "text": self.turn_text}));
            self.turn_text.clear();
        }
        content.append(&mut self.turn_tools);
        content
    }

    fn observe(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::StreamClosed => {}
            Event::TextDelta(text) => {
                self.turn_text.push_str(&text);
                self.result_text.push_str(&text);
            }
            Event::ToolStart {
                id,
                name,
                arguments,
            } => self
                .turn_tools
                .push(json!({"type": "tool_use", "id": id, "name": name, "input": arguments})),
            Event::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                self.out.emit(&RetryEvent {
                    event_type: "system",
                    subtype: "api_retry",
                    attempt,
                    retry_delay_ms: delay_ms,
                    error: &message,
                    session_id: &self.session_id,
                })?;
            }
            Event::TurnComplete { usage, .. } => {
                let content = self.turn_content();
                self.out.emit(&AssistantEvent {
                    event_type: "assistant",
                    message: AssistantMessage {
                        model: &self.model,
                        role: "assistant",
                        content,
                        usage: usage.into(),
                    },
                    session_id: &self.session_id,
                })?;
            }
            Event::ToolResultsSubmitted { message } => {
                self.out.emit(&UserEvent {
                    event_type: "user",
                    message: UserMessage {
                        role: "user",
                        content: message_content(&message),
                    },
                    session_id: &self.session_id,
                })?;
            }
            Event::Done {
                usage,
                num_turns,
                reason,
                cost,
                ..
            } => {
                self.num_turns = num_turns;
                self.usage = usage;
                // The run's own ledger also counts compaction spend, which
                // summing the turns would miss.
                self.cost = cost.or(self.cost);
                self.stop_reason = Some(stop_reason(reason));
                return Ok(true);
            }
            Event::Error(message) => {
                self.error = Some(message.clone());
                self.result_text = message;
                return Ok(true);
            }
            Event::AdvisorNote { severity, message } => {
                self.out.emit(&SystemEvent {
                    event_type: "system",
                    subtype: "advisor_note",
                    severity: &severity,
                    message: &message,
                    session_id: &self.session_id,
                })?;
            }
            _ => {}
        }
        Ok(false)
    }

    /// Emit the terminal `result` and print the final output for `format`.
    /// Returns the agent error, if any, so the caller exits nonzero.
    fn finish(self, format: &OutputFormat, duration_ms: u128) -> Result<Option<String>> {
        let error = self.error.clone();
        let is_error = error.is_some();
        let result = PrintResult {
            result_type: "result",
            subtype: if is_error { "error" } else { "success" },
            is_error,
            duration_ms,
            num_turns: self.num_turns,
            result: self.result_text.clone(),
            stop_reason: self.stop_reason,
            session_id: self.session_id.clone(),
            // Zero on an unpriced model, which is what its turns reported too.
            total_cost_usd: self.cost.unwrap_or_default(),
            usage: self.usage.into(),
        };
        match format {
            // Reference semantics: text output is always the raw reply;
            // `--verbose` events only surface in stream-json mode (the
            // reference prints the transcript solely for its `json` format,
            // which our CLI does not expose).
            OutputFormat::Text => print!("{}", result.result),
            OutputFormat::StreamJson => {
                println!(
                    "{}",
                    serde_json::to_string(&result)
                        .map_err(|e| invalid(format!("serializing result: {e}")))?
                );
            }
        }
        Ok(error)
    }
}

/// Serialize the tool-results wave in our own history shape (tagged blocks,
/// not Claude Code's): the transcript is for debugging and tooling, and the
/// raw history encoding is the lossless form.
fn message_content(message: &Message) -> Value {
    let content = match message {
        Message::User { content } => serde_json::to_value(content),
        other => serde_json::to_value(other.text()),
    };
    content.unwrap_or_else(|_| json!([]))
}

/// Consume a headless run's events to completion and print the output for
/// `format`. Returns the agent error message, if any, so the caller can exit
/// nonzero (the reference's known bug, deliberately not ported).
pub async fn emit(
    handle: HeadlessHandle,
    format: &OutputFormat,
    verbose: bool,
    model: &str,
) -> Result<Option<String>> {
    let HeadlessHandle {
        events,
        tool_names,
        session_id,
        cwd,
        task,
        ..
    } = handle;
    let start = Instant::now();
    let mut events = events;
    let mut state = PrintState::new(session_id.clone(), model.to_string(), format, verbose);
    state.init(&cwd, &tool_names)?;
    while let Some(envelope) = events.next().await {
        if state.observe(envelope.event)? {
            break;
        }
    }
    let _ = tokio::time::timeout(AGENT_SHUTDOWN_TIMEOUT, task).await;
    state.finish(format, start.elapsed().as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{ToolResult, UserContent};
    use serde_json::Value;

    const PRINT_RESULT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "is_error",
        "num_turns",
        "result",
        "stop_reason",
        "session_id",
        "total_cost_usd",
        "usage",
        "duration_ms",
    ];
    const INIT_EVENT_FIELDS: &[&str] = &["type", "subtype", "cwd", "session_id", "tools", "model"];
    const RETRY_EVENT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "attempt",
        "retry_delay_ms",
        "error",
        "session_id",
    ];

    fn sid() -> SessionRef {
        SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap())
    }

    #[test]
    fn wire_format_required_fields() {
        let result = PrintResult {
            result_type: "result",
            subtype: "success",
            is_error: false,
            duration_ms: 1234,
            num_turns: 2,
            result: "done".into(),
            stop_reason: Some("end_turn"),
            session_id: sid(),
            total_cost_usd: 0.003,
            usage: Usage::default().into(),
        };
        let json: Value = serde_json::to_value(&result).unwrap();
        for field in PRINT_RESULT_FIELDS {
            assert!(json.get(field).is_some(), "PrintResult missing: {field}");
        }

        let init = InitEvent {
            event_type: "system",
            subtype: "init",
            cwd: "/tmp",
            session_id: &sid(),
            tools: &["bash".into(), "read".into()],
            model: "test-model",
        };
        let json: Value = serde_json::to_value(&init).unwrap();
        for field in INIT_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "InitEvent missing: {field}");
        }

        let retry = RetryEvent {
            event_type: "system",
            subtype: "api_retry",
            attempt: 2,
            retry_delay_ms: 3000,
            error: "rate_limit",
            session_id: &sid(),
        };
        let json: Value = serde_json::to_value(&retry).unwrap();
        for field in RETRY_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "RetryEvent missing: {field}");
        }
    }

    fn transcript() -> PrintState {
        PrintState::new(sid(), "mock/model".into(), &OutputFormat::Text, true)
    }

    #[test]
    fn stop_reasons_map_to_claude_code_names() {
        assert_eq!(stop_reason(DoneReason::Stop), "end_turn");
        assert_eq!(stop_reason(DoneReason::MaxTurns), "max_turns");
        assert_eq!(stop_reason(DoneReason::Cancelled), "cancelled");
        assert_eq!(stop_reason(DoneReason::DoomStop), "doom_stop");
    }

    #[test]
    fn verbose_transcript_collects_assistant_user_and_retry_events() {
        let mut state = transcript();
        state.init("/project", &["read".into()]).unwrap();

        state.observe(Event::TextDelta("fixing".into())).unwrap();
        state
            .observe(Event::ToolStart {
                id: "t1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.rs"}),
            })
            .unwrap();
        state
            .observe(Event::TurnComplete {
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                },
                context_size: 20,
            })
            .unwrap();
        state
            .observe(Event::ToolResultsSubmitted {
                message: Message::User {
                    content: vec![UserContent::ToolResult(ToolResult::text(
                        "t1", "read", "ok",
                    ))],
                },
            })
            .unwrap();
        state
            .observe(Event::Retry {
                attempt: 1,
                message: "rate_limit".into(),
                delay_ms: 500,
            })
            .unwrap();
        assert!(!state.observe(Event::TextDelta("done".into())).unwrap());
        let terminal = state
            .observe(Event::Done {
                usage: Usage {
                    input_tokens: 25,
                    output_tokens: 9,
                    total_tokens: 34,
                },
                context_size: 30,
                context_window: 0,
                num_turns: 2,
                reason: DoneReason::Stop,
                cost: Some(0.01),
                by_model: Default::default(),
            })
            .unwrap();
        assert!(terminal);

        let events = match std::mem::replace(&mut state.out, WireOut::None) {
            WireOut::Json(events) => events,
            _ => panic!("verbose text mode collects a JSON array"),
        };
        assert_eq!(events.len(), 4, "init, assistant, user, api_retry");
        assert_eq!(events[0]["type"], "system");
        assert_eq!(events[0]["subtype"], "init");

        let assistant = &events[1];
        assert_eq!(assistant["type"], "assistant");
        assert_eq!(assistant["message"]["model"], "mock/model");
        assert_eq!(assistant["message"]["content"][0]["type"], "text");
        assert_eq!(assistant["message"]["content"][1]["type"], "tool_use");
        assert_eq!(assistant["message"]["content"][1]["name"], "read");
        assert_eq!(assistant["message"]["usage"]["input_tokens"], 10);

        assert_eq!(events[2]["type"], "user");
        assert_eq!(events[2]["message"]["role"], "user");
        assert!(events[2]["message"]["content"].is_array());

        assert_eq!(events[3]["type"], "system");
        assert_eq!(events[3]["subtype"], "api_retry");

        assert_eq!(state.result_text, "fixingdone");
        assert_eq!(state.num_turns, 2);
        assert_eq!(state.stop_reason, Some("end_turn"));
    }

    #[test]
    fn error_event_is_terminal_and_replaces_result_text() {
        let mut state = transcript();
        assert!(
            state
                .observe(Event::Error("provider exploded".into()))
                .unwrap()
        );
        assert_eq!(state.error.as_deref(), Some("provider exploded"));
        assert_eq!(state.result_text, "provider exploded");
    }

    #[test]
    fn text_without_verbose_prints_only_the_reply() {
        let state = PrintState::new(sid(), "m".into(), &OutputFormat::Text, false);
        assert!(matches!(state.out, WireOut::None));
    }

    #[test]
    fn load_images_encodes_and_rejects_non_images() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("dot.png");
        image::DynamicImage::new_rgb8(1, 1)
            .save_with_format(&png, image::ImageFormat::Png)
            .unwrap();
        let txt = tmp.path().join("not.txt");
        std::fs::write(&txt, "nope").unwrap();

        let images = load_images(std::slice::from_ref(&png)).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, ImageMedia::Png);
        assert_eq!(images[0].caption, png.display().to_string());
        assert!(!images[0].data.is_empty());

        let err = load_images(&[txt]).unwrap_err();
        assert!(err.to_string().contains("not an image"));
    }
}
