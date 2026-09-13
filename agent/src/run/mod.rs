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
pub mod guardrails;
mod nudge;
mod read_lifecycle;
mod recency;
mod stream;

pub use dedup::{SharedDedupCache, ToolDedupCache, shared_cache};
pub use dispatch::{
    AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome, ToolDispatch,
};
pub use guardrails::{SharedGuardrails, shared_guardrails};
pub use recency::{RecencyCtx, RecencyFacts, RecencySource, attach_recency_tail};
pub use stream::TurnOutput;

use std::sync::Arc;

use rig_core::completion::{CompletionModel, FinishReason};
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
    /// The model returned an empty reply after tool calls and was nudged
    /// to continue.
    Nudge,
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
    /// Bound on automatic continuations of truncated (`max_tokens`) replies.
    pub max_continuation_turns: usize,
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
            .field("max_continuation_turns", &self.max_continuation_turns)
            .finish()
    }
}

impl RunParams {
    pub const UNBOUNDED: usize = usize::MAX;

    /// Reference default (`DEFAULT_MAX_CONTINUATION_TURNS`).
    pub const DEFAULT_MAX_CONTINUATION_TURNS: usize = 3;

    pub fn new(preamble: Option<String>) -> Self {
        Self {
            preamble,
            temperature: None,
            max_tokens: None,
            max_turns: Self::UNBOUNDED,
            recency: None,
            compression: CompressionConfig::default(),
            max_continuation_turns: Self::DEFAULT_MAX_CONTINUATION_TURNS,
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
            max_continuation_turns: Self::DEFAULT_MAX_CONTINUATION_TURNS,
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
    /// Every continuation of a truncated (`max_tokens`) reply was spent and
    /// the model still stopped on the output limit. The turn's messages
    /// (including the truncated tail) were committed, like `Done`.
    MaxTokens { reply: String },
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
    let outcome = run_inner(model, params, tools, history, prompt, cancel, emit).await;
    // Clean (and cancelled) run ends close the capture session onto the
    // `/undo` stack; a failed run leaves it for the next attempt to merge
    // into, mirroring the reference's commit points.
    if !matches!(outcome, RunOutcome::Failed(_))
        && let Some(snapshots) = tools.snapshots()
    {
        snapshots.commit();
    }
    outcome
}

async fn run_inner<M: CompletionModel + Clone>(
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
    // Nudge budget for this run; real progress (tool results) resets it.
    let mut nudges: u32 = 0;
    // Continuations spent on truncated (`max_tokens`) replies.
    let mut continuations: usize = 0;
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
            // A truncated reply continues: the model's cut-off message is
            // already in `turn`, so the next request resumes from it.
            let truncated = output.finish_reason == Some(FinishReason::Length);
            if let Some(outcome) = handle_terminal_reply(
                history,
                &mut turn,
                &full,
                &reply,
                truncated,
                &mut nudges,
                &mut turns,
                params,
                &mut continuations,
                emit,
            ) {
                return outcome;
            }
            continue;
        }
        if let Some(outcome) = dispatch_tool_calls(tools, &mut turn, tool_calls, cancel, emit).await
        {
            return outcome;
        }
        turns += 1;
        nudges = 0;
        if turns >= params.max_turns {
            return commit_partial(history, &mut turn);
        }
    }
}

/// Commit a partial turn and end the run at its budget: sanitized so
/// dangling tool calls replay cleanly on the next request.
fn commit_partial(history: &mut Vec<Message>, turn: &mut Vec<Message>) -> RunOutcome {
    sanitize_partial(turn);
    history.append(turn);
    RunOutcome::MaxTurns
}

/// Handle an assistant turn with no tool calls. Continues truncated and
/// empty replies while their budgets allow; otherwise commits history and
/// returns the run outcome. `None` means "keep looping".
fn handle_terminal_reply(
    history: &mut Vec<Message>,
    turn: &mut Vec<Message>,
    full: &[Message],
    reply: &str,
    truncated: bool,
    nudges: &mut u32,
    turns: &mut usize,
    params: &RunParams,
    continuations: &mut usize,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Option<RunOutcome> {
    if truncated && *continuations < params.max_continuation_turns {
        *continuations += 1;
        *turns += 1;
        if *turns >= params.max_turns {
            return Some(commit_partial(history, turn));
        }
        return None;
    }
    // A truncated reply is never "empty-and-stalled": even a
    // zero-visible-text truncation keeps its cut-off message and
    // ends MaxTokens, so the nudge path below never swallows it.
    if reply.trim().is_empty() && !truncated {
        // The marker takes the silent reply's place in history.
        turn.pop();
        // `full` is the exact view the model just saw (its wire-only
        // rewrites are shape-preserving); the trailing marker+nudge
        // pairs this run already pushed are skipped by count.
        let nudge = *nudges < nudge::MAX_NUDGES
            && nudge::has_recent_tool_results(
                full,
                nudge::RECENT_TOOL_WINDOW,
                2 * *nudges as usize,
            );
        nudge::stall_turn(turn, nudge);
        if nudge {
            *nudges += 1;
            emit(Event::Nudge);
            *turns += 1;
            if *turns >= params.max_turns {
                return Some(commit_partial(history, turn));
            }
            return None;
        }
    }
    history.append(turn);
    Some(if truncated {
        RunOutcome::MaxTokens {
            reply: reply.to_owned(),
        }
    } else {
        RunOutcome::Done {
            reply: reply.to_owned(),
        }
    })
}

/// Execute the turn's tool calls, appending their results to `turn`.
/// Returns `Some(outcome)` when the run must stop (cancel or dispatch
/// failure); `None` means the loop continues.
async fn dispatch_tool_calls(
    tools: &ToolDispatch,
    turn: &mut Vec<Message>,
    calls: Vec<history::ToolCall>,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Option<RunOutcome> {
    for call in calls {
        if cancel.cancelled() {
            return Some(RunOutcome::Cancelled);
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
            Ok(DispatchOutcome::Stopped(_)) => return Some(RunOutcome::Cancelled),
            Err(unknown) => return Some(RunOutcome::Failed(unknown)),
        }
    }
    None
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
    let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
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
                        answered.insert(result.call.clone());
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
    async fn snapshots_commit_on_done_and_undo_restores_files() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event(
                    "t1",
                    "write",
                    serde_json::json!({"path":"f.txt","content":"changed\n"}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "original\n").unwrap();
        let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
        let snapshots = workspace.snapshots().clone();
        let tools = workspace.register();
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &tools,
            &mut history,
            "write it",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "changed\n"
        );
        assert_eq!(snapshots.undo_depth(), 1, "session committed on Done");
        let message = snapshots.rollback().await.unwrap();
        assert!(message.contains("rolled back"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "original\n"
        );
    }

    fn length_final(total_tokens: u64) -> MockStreamEvent {
        use rig_core::streaming::StreamFinal;
        MockStreamEvent::FinalResponse(
            StreamFinal::new(
                "mock",
                rig_core::completion::Usage {
                    total_tokens,
                    ..Default::default()
                },
            )
            .with_finish_reason(rig_core::completion::FinishReason::Length),
        )
    }

    #[tokio::test]
    async fn truncated_reply_continues_then_finishes() {
        // Truncated, truncated, then a clean stop: the run continues the
        // truncated replies and ends Done with the final text.
        let (model, _turns) = stream_turns(vec![
            vec![MockStreamEvent::text("part one "), length_final(1)],
            vec![MockStreamEvent::text("part two "), length_final(1)],
            vec![
                MockStreamEvent::text("part three"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &ToolDispatch::default(),
            &mut history,
            "tell me a long story",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "part three"),
            "{outcome:?}"
        );
        assert_eq!(model.request_count(), 3);
        // prompt + three assistant messages, all committed
        assert_eq!(history.len(), 4);
        assert_eq!(history[1].text(), "part one ");
    }

    #[tokio::test]
    async fn truncation_gives_up_after_the_continuation_budget() {
        // Every reply truncates: after `max_continuation_turns` continuations
        // the run ends MaxTokens with the truncated tail committed.
        let (model, _turns) = stream_turns(vec![
            vec![MockStreamEvent::text("a"), length_final(1)],
            vec![MockStreamEvent::text("b"), length_final(1)],
            vec![MockStreamEvent::text("c"), length_final(1)],
            vec![MockStreamEvent::text("d"), length_final(1)],
        ]);
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &ToolDispatch::default(),
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(
            matches!(outcome, RunOutcome::MaxTokens { ref reply } if reply == "d"),
            "{outcome:?}"
        );
        // 1 initial + 3 continuations (the default budget).
        assert_eq!(model.request_count(), 4);
        assert_eq!(history.len(), 5, "truncated tail is committed");
    }

    #[tokio::test]
    async fn continuation_budget_is_configurable_to_zero() {
        let (model, _turns) = stream_turns(vec![vec![
            MockStreamEvent::text("cut off"),
            length_final(1),
        ]]);
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            max_continuation_turns: 0,
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
        assert!(matches!(outcome, RunOutcome::MaxTokens { .. }));
        assert_eq!(model.request_count(), 1);
    }

    #[tokio::test]
    async fn continuations_respect_the_turn_budget() {
        // One clean turn budget, a truncated first reply: the continuation
        // counts against `max_turns` and the run ends MaxTurns sanitized.
        let (model, _turns) = stream_turns(vec![
            vec![MockStreamEvent::text("a"), length_final(1)],
            vec![MockStreamEvent::text("b"), length_final(1)],
        ]);
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            max_turns: 1,
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
        assert!(matches!(outcome, RunOutcome::MaxTurns));
        assert_eq!(model.request_count(), 1);
        assert_eq!(history.last().unwrap().text(), END_MARKER);
    }

    #[tokio::test]
    async fn empty_truncated_reply_is_committed_as_max_tokens() {
        // A truncation with zero visible text is not an empty-and-stalled
        // turn: the cut-off (empty) assistant message is committed and the
        // run ends MaxTokens instead of firing the nudge path.
        let (model, _turns) = stream_turns(vec![vec![length_final(1)]]);
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            max_continuation_turns: 0,
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
        assert!(matches!(outcome, RunOutcome::MaxTokens { .. }));
        assert_eq!(model.request_count(), 1, "no nudge was sent");
        assert_eq!(history.len(), 2);
        // prompt + the truncated assistant message (empty, not replaced by
        // the empty-response marker)
        assert_ne!(history[1].text(), nudge::EMPTY_RESPONSE_MARKER);
    }

    #[tokio::test]
    async fn guardrails_warn_then_block_repeated_failures() {
        // Five identical failing reads: the counters warn at 2 and block at
        // 4, so the third call carries the warning and the fifth is blocked.
        let read = || tool_event("t", "read", serde_json::json!({"path": "missing"}));
        let (model, _turns) = stream_turns(vec![
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let tools = crate::tools::Workspace::new(dir.path())
            .unwrap()
            .register()
            .with_guardrails(crate::run::shared_guardrails());
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
        assert_eq!(results.len(), 5);
        assert!(!results[0].contains("guardrail"));
        assert!(
            results[2].starts_with("[guardrail]"),
            "warn at the third call: {}",
            results[2]
        );
        assert!(
            results[4].contains("blocked by guardrails"),
            "block at the fifth call: {}",
            results[4]
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
    async fn empty_reply_after_tool_call_is_nudged_to_continue() {
        // Tool call, then a completely empty reply, then recovery.
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path":"f"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![MockStreamEvent::final_response_with_total_tokens(1)],
            vec![
                MockStreamEvent::text("all better now"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "body\n").unwrap();
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
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "all better now"),
            "nudged model must recover: {outcome:?}"
        );
        // prompt, tool call, result, empty marker, nudge prompt, final reply
        assert_eq!(history.len(), 6);
        assert_eq!(history[3].text(), nudge::EMPTY_RESPONSE_MARKER);
        assert!(
            history[4].text().contains("returned an empty response"),
            "nudge prompt must be committed: {}",
            history[4].text()
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, Event::Nudge)),
            "a nudge event must be emitted"
        );
        // The nudged retry carried the marker and the prompt.
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        let sent = crate::edge::rig_to_own(&requests[2].chat_history);
        assert_eq!(sent[3].text(), nudge::EMPTY_RESPONSE_MARKER);
        assert!(sent[4].text().contains("returned an empty response"));
    }

    #[tokio::test]
    async fn empty_reply_without_recent_tool_results_ends_the_run() {
        let (model, _turns) = stream_turns(vec![vec![
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let outcome = run(
            &model,
            &RunParams::default(),
            &ToolDispatch::default(),
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply.is_empty()));
        // prompt + empty marker; no nudge prompt follows
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].text(), nudge::EMPTY_RESPONSE_MARKER);
        assert_eq!(model.request_count(), 1);
    }

    #[tokio::test]
    async fn empty_reply_counts_against_the_turn_budget() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path":"f"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![MockStreamEvent::final_response_with_total_tokens(1)],
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), "body\n").unwrap();
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
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        assert!(matches!(outcome, RunOutcome::MaxTurns));
        assert_eq!(model.request_count(), 2, "nudged retry still ran");
        // The sanitized tail closes with the end marker.
        assert_eq!(history.last().unwrap().text(), END_MARKER);
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
