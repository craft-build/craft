//! The crate-owned multi-turn agent loop.
//!
//! Per turn: build the request through the provider [`crate::edge`], stream
//! the model call ([`stream`]), dispatch its tool calls ([`dispatch`]), append
//! the results, and continue while the model emits tool calls — bounded by
//! `max_turns`. History is committed to the caller only on success: failed
//! and cancelled turns leave it untouched; a run that hits the turn budget
//! commits its sanitized partial history with an end marker so the next
//! prompt continues from where the budget ran out.

pub mod dedup;
pub mod dispatch;
mod read_lifecycle;
mod recency;
mod stream;

pub use dedup::{SharedDedupCache, ToolDedupCache, shared_cache};
pub use dispatch::{
    AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome, ToolDispatch,
};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use stream::TurnOutput;

use std::sync::Arc;

use rig_core::completion::CompletionModel;
use tokio::sync::watch;

use crate::compression::{self, CompressionConfig};
use crate::edge;
use crate::history::{self, Message};

/// Events emitted as the run progresses; consumed by the TUI and ACP surfaces.
#[derive(Clone, Debug)]
pub enum Event {
    /// A streamed chunk of the assistant reply.
    TextDelta(String),
    /// A streamed chunk of the model's reasoning.
    ReasoningDelta(String),
    /// The model issued a tool call.
    ToolStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// A tool call finished (ran, failed, or was skipped).
    ToolDone {
        id: String,
        name: String,
        arguments: serde_json::Value,
        result: history::ToolResult,
    },
    /// Token usage reported for one model call.
    Usage(history::Usage),
}

/// Cancellation shared between a surface and its run: set the flag, and the
/// run stops at the next stream/dispatch/turn boundary.
#[derive(Clone)]
pub struct CancelToken {
    rx: watch::Receiver<bool>,
}

/// The setting half of a [`CancelToken`].
#[derive(Clone)]
pub struct CancelFlag {
    tx: watch::Sender<bool>,
}

/// Create a cancellation pair, initially not cancelled.
pub fn cancel_channel() -> (CancelFlag, CancelToken) {
    let (tx, rx) = watch::channel(false);
    (CancelFlag { tx }, CancelToken { rx })
}

impl CancelToken {
    pub fn cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<bool> {
        self.rx.clone()
    }
}

impl CancelFlag {
    /// Set or clear the flag. `true` cancels every run sharing the token.
    pub fn set(&self, cancelled: bool) {
        let _ = self.tx.send(cancelled);
    }

    /// A token sharing this flag's state.
    pub fn token(&self) -> CancelToken {
        CancelToken {
            rx: self.tx.subscribe(),
        }
    }
}

/// Request-level settings for one run.
#[derive(Clone)]
pub struct RunParams {
    /// System instructions; `None` or empty omits the preamble.
    pub preamble: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    /// Bound on model calls per run. Interactive runs use [`RunParams::UNBOUNDED`].
    pub max_turns: usize,
    /// Per-turn volatile facts, appended to the last user message of each
    /// request (and never committed to history).
    pub recency: Option<Arc<dyn RecencySource>>,
    /// Tool-output pre-compression applied to the request view only;
    /// history and events always keep the raw results.
    pub compression: CompressionConfig,
}

impl std::fmt::Debug for RunParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunParams")
            .field("preamble", &self.preamble)
            .field("temperature", &self.temperature)
            .field("max_tokens", &self.max_tokens)
            .field("max_turns", &self.max_turns)
            .field("recency", &self.recency.as_ref().map(|_| "<source>"))
            .field("compression", &self.compression)
            .finish()
    }
}

impl RunParams {
    pub const UNBOUNDED: usize = usize::MAX;

    pub fn new(preamble: Option<String>) -> Self {
        Self {
            preamble,
            temperature: None,
            max_tokens: None,
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
        }
    }
}

impl Default for RunParams {
    fn default() -> Self {
        Self {
            preamble: None,
            temperature: None,
            max_tokens: None,
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// The model finished without pending tool calls. `reply` is the final
    /// assistant text; the turn's messages were committed to history.
    Done { reply: String },
    /// The turn budget ran out. The sanitized partial history (with an end
    /// marker) was committed; the next prompt continues from there.
    MaxTurns,
    /// The run was cancelled; history is not committed.
    Cancelled,
    /// The run failed; history is not committed.
    Failed(String),
}

/// Marker appended when a run is cut short, so the model knows the turn ended.
pub(crate) const END_MARKER: &str = "[The turn ended here; the run was cut short.]";

/// Drive one multi-turn run. `history` is the caller-owned conversation; the
/// prompt is appended as the turn's first user message. `emit` receives every
/// event as it happens (it must not block).
pub async fn run<M: CompletionModel + Clone>(
    model: &M,
    params: &RunParams,
    tools: &ToolDispatch,
    history: &mut Vec<Message>,
    prompt: &str,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> RunOutcome {
    let definitions = tools.definitions();
    let mut turn = vec![Message::user(prompt)];
    let mut turns = 0;
    loop {
        if cancel.cancelled() {
            return RunOutcome::Cancelled;
        }
        let mut full = history.clone();
        full.extend(turn.iter().cloned());
        // Volatile recency facts ride only this request; `full` is discarded.
        if let Some(source) = &params.recency
            && let Some(with_tail) = recency::recency_view(source, &full, turns as u32)
        {
            full = with_tail;
        }
        read_lifecycle::apply_to_request(&mut full, tools.compression_store());
        compress_request_view(&mut full, &params.compression);
        let request = edge::to_request(
            &full,
            &definitions,
            params.preamble.as_deref(),
            params.temperature,
            params.max_tokens,
        );
        let output = match stream::run_model_stream(model, request, cancel, emit).await {
            Ok(output) => output,
            Err(stream::StreamFailure::Cancelled) => return RunOutcome::Cancelled,
            Err(stream::StreamFailure::Error(message)) => return RunOutcome::Failed(message),
        };
        let Message::Assistant { content } = &output.assistant else {
            return RunOutcome::Failed("model produced a non-assistant message".into());
        };
        let tool_calls: Vec<history::ToolCall> = content
            .iter()
            .filter_map(|block| match block {
                history::AssistantContent::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        turn.push(output.assistant);
        if tool_calls.is_empty() {
            let reply = turn.last().expect("assistant pushed").text();
            history.append(&mut turn);
            return RunOutcome::Done { reply };
        }
        for call in tool_calls {
            if cancel.cancelled() {
                return RunOutcome::Cancelled;
            }
            match tools.execute(call.clone()).await {
                Ok(DispatchOutcome::Ran(result)) | Ok(DispatchOutcome::Skipped(result)) => {
                    turn.push(Message::User {
                        content: vec![history::UserContent::ToolResult(result.clone())],
                    });
                    emit(Event::ToolDone {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        arguments: call.function.arguments.clone(),
                        result,
                    });
                }
                Ok(DispatchOutcome::Stopped(_)) => return RunOutcome::Cancelled,
                Err(unknown) => return RunOutcome::Failed(unknown),
            }
        }
        turns += 1;
        if turns >= params.max_turns {
            sanitize_partial(&mut turn);
            history.append(&mut turn);
            return RunOutcome::MaxTurns;
        }
    }
}

/// Rewrite tool-result texts in the request copy through pre-compression.
/// Only the wire view is affected: `turn` and `history` keep raw results.
/// Request-time compression is unconditional (per the reference);
/// `protect_recent_tool_outputs` is a compaction-stage knob, not ours.
fn compress_request_view(full: &mut [Message], config: &CompressionConfig) {
    for message in full {
        let Message::User { content } = message else {
            continue;
        };
        for block in content {
            if let history::UserContent::ToolResult(result) = block {
                for item in &mut result.content {
                    if let history::ToolResultContent::Text(text) = item {
                        text.text = compression::compress_for_llm(&text.text, config);
                    }
                }
            }
        }
    }
}

/// Close dangling tool calls and append the end marker, so the committed
/// partial history replays cleanly on the next request.
pub(crate) fn sanitize_partial(turn: &mut Vec<Message>) {
    let mut dangling: Vec<history::ToolCall> = Vec::new();
    let mut answered: Vec<String> = Vec::new();
    for message in turn.iter() {
        match message {
            Message::Assistant { content } => {
                for block in content {
                    if let history::AssistantContent::ToolCall(call) = block {
                        dangling.push(call.clone());
                    }
                }
            }
            Message::User { content } => {
                for block in content {
                    if let history::UserContent::ToolResult(result) = block {
                        answered.push(result.call.clone());
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    let open: Vec<history::ToolCall> = dangling
        .into_iter()
        .filter(|call| !answered.contains(&call.id))
        .collect();
    if !open.is_empty() {
        let content = open
            .iter()
            .map(|call| {
                history::UserContent::ToolResult(history::ToolResult::text(
                    call.id.clone(),
                    call.function.name.clone(),
                    "skipped: the turn ended before this call ran",
                ))
            })
            .collect();
        turn.push(Message::User { content });
    }
    turn.push(Message::user(END_MARKER));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::UserContent;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    fn stream_turns(
        turns: Vec<Vec<MockStreamEvent>>,
    ) -> (MockCompletionModel, Vec<Vec<MockStreamEvent>>) {
        let model = MockCompletionModel::from_stream_turns(turns.clone());
        (model, turns)
    }

    fn tool_event(id: &str, name: &str, args: serde_json::Value) -> MockStreamEvent {
        MockStreamEvent::tool_call(id, name, args)
    }

    #[tokio::test]
    async fn multi_turn_tool_round_trip_commits_history() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path":"file.txt"})),
                MockStreamEvent::final_response_with_total_tokens(3),
            ],
            vec![
                MockStreamEvent::text("all done"),
                MockStreamEvent::final_response_with_total_tokens(5),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "read the file",
            &cancel,
            &|event| events.lock().unwrap().push(event),
        )
        .await;
        let guard = events.lock().unwrap();
        assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "all done"));
        // user prompt, assistant tool call, tool result, final assistant text
        assert_eq!(history.len(), 4);
        // The call id is the run-stable internal id the stream minted, not
        // the scripted "t1"; the result must answer whatever id was recorded.
        let Message::Assistant { content } = &history[1] else {
            panic!("assistant tool-call message");
        };
        let history::AssistantContent::ToolCall(call) = &content[0] else {
            panic!("tool-call block");
        };
        assert_eq!(call.function.name, "read");
        assert!(matches!(&history[2],
            Message::User { content } if matches!(&content[0], UserContent::ToolResult(r)
                if r.call == call.id && r.name == "read")));
        // The result the model sees on the next call is literal text.
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let replayed = crate::edge::rig_to_own(&requests[1].chat_history);
        assert_eq!(replayed[2], history[2]);
        // Events: tool start, tool done, usage per call.
        assert!(
            guard
                .iter()
                .any(|e| matches!(e, Event::ToolStart { name, .. } if name == "read"))
        );
        assert!(
            guard
                .iter()
                .any(|e| matches!(e, Event::ToolDone { result, .. } if !result.is_error))
        );
        assert_eq!(
            guard
                .iter()
                .filter(|e| matches!(e, Event::Usage(_)))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn failed_dispatch_leaves_history_uncommitted() {
        let (model, _turns) = stream_turns(vec![vec![
            tool_event("t1", "no_such_tool", serde_json::json!({})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let dir = tempfile::tempdir().unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let mut history = vec![Message::user("earlier")];
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Failed(message) if message.contains("unknown tool")));
        assert_eq!(history.len(), 1, "failed run must not commit");
    }

    #[tokio::test]
    async fn max_turns_bound_commits_sanitized_partial_history() {
        let turns: Vec<Vec<MockStreamEvent>> = (0..3)
            .map(|i| {
                vec![
                    tool_event(&format!("t{i}"), "read", serde_json::json!({"path":"f"})),
                    MockStreamEvent::final_response_with_total_tokens(1),
                ]
            })
            .collect();
        let (model, _t) = stream_turns(turns);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "x\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            max_turns: 2,
            ..RunParams::default()
        };
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &params,
            &tools,
            &mut history,
            "loop",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::MaxTurns));
        // Prompt + 2x(assistant+result) + end marker; the third call never ran.
        assert_eq!(model.request_count(), 2);
        assert_eq!(history.len(), 6);
        assert_eq!(history.last().unwrap().text(), END_MARKER);
        // No dangling tool calls: every call has its result.
        let calls: Vec<String> = history
            .iter()
            .flat_map(|m| match m {
                Message::Assistant { content } => content
                    .iter()
                    .filter_map(|b| match b {
                        history::AssistantContent::ToolCall(c) => Some(c.id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        let answered: Vec<String> = history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::ToolResult(r) => Some(r.call.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert!(calls.iter().all(|id| answered.contains(id)));
    }

    #[tokio::test]
    async fn cancel_mid_tool_skips_execution_and_commits_nothing() {
        // The before-execution hook denies the call by stopping the run,
        // standing in for a cancellation arriving at the dispatch boundary.
        struct StopAll;
        impl BeforeExecute for StopAll {
            fn decide(&self, _call: history::ToolCall) -> BoxFuture<Decision> {
                Box::pin(async { Decision::Stop("cancelled".into()) })
            }
        }
        let (model, _turns) = stream_turns(vec![vec![
            tool_event("t1", "read", serde_json::json!({"path":"f"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "secret\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_before(std::sync::Arc::new(StopAll));
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Cancelled));
        assert!(history.is_empty(), "cancelled run must not commit");
    }

    #[tokio::test]
    async fn skip_decision_reports_reason_to_model() {
        struct Deny;
        impl BeforeExecute for Deny {
            fn decide(&self, _call: history::ToolCall) -> BoxFuture<Decision> {
                Box::pin(async { Decision::Skip("not allowed".into()) })
            }
        }
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path":"f"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("okay"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_before(std::sync::Arc::new(Deny));
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "okay"));
        let history::Message::User { content } = &history[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        assert_eq!(result.content[0].to_text(), "not allowed");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn after_execute_transform_rewrites_results() {
        struct Upper;
        impl AfterExecute for Upper {
            fn transform(
                &self,
                _call: history::ToolCall,
                mut result: history::ToolResult,
            ) -> BoxFuture<history::ToolResult> {
                Box::pin(async move {
                    for item in &mut result.content {
                        if let history::ToolResultContent::Text(text) = item {
                            text.text = text.text.to_uppercase();
                        }
                    }
                    result
                })
            }
        }
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path":"f"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("ok"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "body\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_after(std::sync::Arc::new(Upper));
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let _ = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        let history::Message::User { content } = &history[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        assert_eq!(result.content[0].to_text(), "1: BODY");
    }

    #[tokio::test]
    async fn argument_fragments_accumulate_into_one_call() {
        // rig-core's assembler owns fragment folding; the driver must surface
        // exactly one ToolStart and one assistant tool-call block per call,
        // with the arguments parsed as JSON.
        let (model, _turns) = stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "call-1",
                    "read",
                    serde_json::json!({"path":"f","limit":1}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "a\nb\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|event| events.lock().unwrap().push(event),
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        let starts = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::ToolStart { .. }))
            .count();
        assert_eq!(starts, 1);
        let history::Message::Assistant { content } = &history[1] else {
            panic!("assistant message");
        };
        let tool_calls: Vec<_> = content
            .iter()
            .filter_map(|b| match b {
                history::AssistantContent::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "read");
        assert_eq!(
            tool_calls[0].function.arguments,
            serde_json::json!({"path": "f", "limit": 1})
        );
    }

    #[tokio::test]
    async fn dedup_replays_identical_read_calls() {
        // Two identical read calls in one run: the second answers from cache.
        let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
        let (model, _turns) = stream_turns(vec![
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "body\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_dedup(crate::run::shared_cache());
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        let results: Vec<&history::ToolResult> = history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::ToolResult(r) => Some(r),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content[0].to_text(), "1: body");
        assert!(
            results[1].content[0]
                .to_text()
                .starts_with("[cached] 1: body"),
            "second identical read must replay from cache"
        );
    }

    #[tokio::test]
    async fn dedup_invalidated_by_write_to_same_path() {
        let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
        let write = || {
            tool_event(
                "t",
                "write",
                serde_json::json!({"path": "f", "content": "new\n"}),
            )
        };
        let (model, _turns) = stream_turns(vec![
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![
                write(),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "old\n").unwrap();
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_dedup(crate::run::shared_cache());
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let _ = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        let results: Vec<String> = history
            .iter()
            .flat_map(|m| match m {
                Message::User { content } => content
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::ToolResult(r) => Some(r.content[0].to_text()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0], "1: old");
        assert!(!results[1].contains("cached"), "writes are never cached");
        assert_eq!(
            results[2], "1: new",
            "post-write read must re-execute against fresh contents"
        );
    }

    #[tokio::test]
    async fn recency_tail_rides_the_request_but_not_history() {
        struct Turns;
        impl crate::run::RecencySource for Turns {
            fn collect(&self, ctx: &crate::run::RecencyCtx) -> crate::run::RecencyFacts {
                let mut facts = crate::run::RecencyFacts::new();
                facts.push(format!("turn {}", ctx.turn));
                facts
            }
        }
        let (model, _turns) = stream_turns(vec![vec![
            MockStreamEvent::text("ok"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            recency: Some(std::sync::Arc::new(Turns)),
            ..RunParams::default()
        };
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &params,
            &ToolDispatch::default(),
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        let requests = model.requests();
        let sent = crate::edge::rig_to_own(&requests[0].chat_history);
        assert!(
            sent[0].text().contains("<turn-context>\n\nturn 0"),
            "tail must ride the request"
        );
        assert!(
            !history[0].text().contains("turn-context"),
            "tail must not be committed to history"
        );
    }

    #[tokio::test]
    async fn dedup_replays_through_after_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // The cache stores the raw result; a replay must re-run the
        // per-call AfterExecute transform instead of replaying its output.
        struct Counting {
            count: std::sync::Arc<AtomicUsize>,
        }
        impl AfterExecute for Counting {
            fn transform(
                &self,
                _call: history::ToolCall,
                result: history::ToolResult,
            ) -> BoxFuture<history::ToolResult> {
                let count = self.count.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::Relaxed);
                    result
                })
            }
        }
        let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
        let (model, _turns) = stream_turns(vec![
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "body\n").unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_dedup(crate::run::shared_cache())
            .with_after(std::sync::Arc::new(Counting {
                count: count.clone(),
            }));
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let _ = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert_eq!(
            count.load(Ordering::Relaxed),
            2,
            "after hook must run on both the execution and the replay"
        );
        assert!(history.len() >= 5);
        let replayed = &history[history.len() - 2];
        let Message::User { content } = replayed else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block");
        };
        assert!(result.content[0].to_text().starts_with("[cached]"));
    }

    #[tokio::test]
    async fn empty_history_and_prompt_round_trip() {
        let (model, _turns) = stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &ToolDispatch::default(),
            &mut history,
            "hi",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "hello"));
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn read_lifecycle_marks_request_view_only() {
        // read → write the same file, with enough later read turns that the
        // early read falls outside the working-set lookback. The model's
        // later requests must see the stale marker; history keeps the raw text.
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path": "f.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event(
                    "t2",
                    "write",
                    serde_json::json!({"path": "f.txt", "content": "changed\n"}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t3", "read", serde_json::json!({"path": "g.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t4", "read", serde_json::json!({"path": "h.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t5", "read", serde_json::json!({"path": "i.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t6", "read", serde_json::json!({"path": "j.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let long: String = (1..=40)
            .map(|i| format!("original line {i} of the file\n"))
            .collect();
        for name in ["f.txt", "g.txt", "h.txt", "i.txt", "j.txt"] {
            std::fs::write(dir.path().join(name), &long).unwrap();
        }
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        // Committed history keeps the raw read.
        let Message::User { content } = &history[2] else {
            panic!("first read result message");
        };
        let UserContent::ToolResult(raw) = &content[0] else {
            panic!("tool result block");
        };
        assert!(raw.content[0].to_text().contains("40: original line 40"));
        // The model's last request carried the stale marker for that read.
        let requests = model.requests();
        let sent = crate::edge::rig_to_own(&requests.last().unwrap().chat_history);
        let Message::User { content } = &sent[2] else {
            panic!("tool result message on the wire");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block on the wire");
        };
        let wire_text = result.content[0].to_text();
        assert!(
            wire_text.starts_with("[Stale read: "),
            "expected a stale marker, got: {wire_text}"
        );
        assert!(wire_text.contains("Re-read the file"));
    }

    #[tokio::test]
    async fn compression_trims_request_view_but_history_keeps_raw() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path": "big.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=100)
            .map(|i| format!("filler line number {i} for size\n"))
            .collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "go",
            &cancel,
            &|event| events.lock().unwrap().push(event),
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        // Committed history and the ToolDone event keep the raw result.
        let Message::User { content } = &history[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(raw) = &content[0] else {
            panic!("tool result block");
        };
        let raw_text = raw.content[0].to_text();
        assert!(raw_text.len() >= compression::MIN_COMPRESS_LEN);
        assert!(!raw_text.contains("lines omitted"));
        assert!(events.lock().unwrap().iter().any(
            |e| matches!(e, Event::ToolDone { result, .. } if !result.content[0]
                .to_text()
                .contains("lines omitted"))
        ));
        // The model's second request carried the compressed form.
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let sent = crate::edge::rig_to_own(&requests[1].chat_history);
        let Message::User { content } = &sent[2] else {
            panic!("tool result message on the wire");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block on the wire");
        };
        let wire_text = result.content[0].to_text();
        assert!(
            wire_text.contains("lines omitted"),
            "model must see the compressed form"
        );
        assert!(wire_text.len() < raw_text.len());
    }

    #[tokio::test]
    async fn compression_disabled_sends_raw_output() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path": "big.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=100)
            .map(|i| format!("filler line number {i} for size\n"))
            .collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            compression: CompressionConfig {
                enabled: false,
                ..CompressionConfig::default()
            },
            ..RunParams::default()
        };
        let mut history = Vec::new();
        let _ = run(
            &model,
            &params,
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        let Message::User { content } = &history[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(raw) = &content[0] else {
            panic!("tool result block");
        };
        let requests = model.requests();
        let sent = crate::edge::rig_to_own(&requests[1].chat_history);
        let Message::User { content } = &sent[2] else {
            panic!("tool result message on the wire");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block on the wire");
        };
        assert_eq!(result.content[0].to_text(), raw.content[0].to_text());
    }
}
