//! One agent turn: drives the conversation's run, rendering to the TUI seam
//! through the session-long forwarder.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc};

use crate::config::Config;
use animist::agent::AgentEvent as RuntimeEvent;
use animist::provider::ProviderEvent;

use crate::harness::AgentMode;
use crate::harness::{self, CraftEvent, Event, EventStream};
use crate::history;

use super::super::cards::{self, Files};
use super::usage_recorder::record_run_usage;
use crate::tui::provider::{AgentEvent, PlanItem, Status, Tone, ToolCallData};

use super::{Selection, SessionKit, SessionState, report};

/// How long a finished run waits for the forwarder to process its terminal
/// Done before reading the streamed flags anyway: a dead forwarder must not
/// hang the turn (and with it the whole session) past recovery.
const FORWARDER_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Tools that spawn a subagent through the `SpawnSubagent` seam; their
/// finished call settles the spawned subagent's task chat.
const SUBAGENT_TOOLS: &[&str] = &["task", "review"];

/// Everything one agent turn needs, bundled so the turn's helpers avoid a
/// long parameter list.
pub(super) struct TurnCtx {
    pub(super) kit: SessionKit,
    pub(super) state: Arc<Mutex<SessionState>>,
    pub(super) mode: AgentMode,
}

/// Turn-level signals shared with the session forwarder: the streamed-reply
/// flags (so the outcome rendering knows what the user already saw) and the
/// generation counter the forwarder bumps once a run's terminal Done — and
/// every event before it — has been forwarded.
#[derive(Clone, Debug)]
pub(super) struct RunSignals {
    streamed_text: Arc<AtomicBool>,
    streamed_reasoning: Arc<AtomicBool>,
    tools_started: Arc<AtomicBool>,
    done: Arc<tokio::sync::watch::Sender<u64>>,
}

impl RunSignals {
    pub(super) fn new() -> Self {
        let (done, _) = tokio::sync::watch::channel(0);
        Self {
            streamed_text: Arc::new(AtomicBool::new(false)),
            streamed_reasoning: Arc::new(AtomicBool::new(false)),
            tools_started: Arc::new(AtomicBool::new(false)),
            done: Arc::new(done),
        }
    }

    /// Clear the per-run flags before a run starts.
    fn reset(&self) {
        self.streamed_text.store(false, Ordering::Relaxed);
        self.streamed_reasoning.store(false, Ordering::Relaxed);
        self.tools_started.store(false, Ordering::Relaxed);
    }

    /// Whether text or reasoning already streamed this run (the reply was
    /// shown live, so it must not be emitted again).
    fn streamed(&self) -> bool {
        self.streamed_text.load(Ordering::Relaxed)
            || self.streamed_reasoning.load(Ordering::Relaxed)
    }

    /// The current terminal-Done generation.
    fn current(&self) -> u64 {
        *self.done.borrow()
    }

    /// Wait until the forwarder has processed the terminal Done of the run
    /// that started at `previous`. Bounded: a missing Done (a forwarder that
    /// died mid-run) degrades to stale flags, never a stuck turn.
    async fn settle(&self, previous: u64) {
        let mut rx = self.done.subscribe();
        let settled = tokio::time::timeout(FORWARDER_SETTLE_TIMEOUT, async {
            while *rx.borrow() <= previous {
                if rx.changed().await.is_err() {
                    // The forwarder is gone; nothing further will arrive.
                    return;
                }
            }
        })
        .await;
        if settled.is_err() {
            tracing::warn!("session forwarder did not settle after the run");
        }
    }

    /// Publish that a run's terminal Done was forwarded.
    fn bump(&self) {
        self.done.send_modify(|generation| *generation += 1);
    }
}

/// Maps conversation events to TUI events for the session's lifetime; the
/// forwarder task drives it. Owns the streaming-progress flags the outcome
/// rendering consults after each run.
struct TurnRenderer {
    tx: mpsc::UnboundedSender<AgentEvent>,
    files: Files,
    selection: Arc<arc_swap::ArcSwap<Selection>>,
    signals: RunSignals,
}

impl TurnRenderer {
    fn new(
        tx: mpsc::UnboundedSender<AgentEvent>,
        files: Files,
        selection: Arc<arc_swap::ArcSwap<Selection>>,
    ) -> Self {
        Self {
            tx,
            files,
            selection,
            signals: RunSignals::new(),
        }
    }

    fn map(&self, event: Event) {
        match event {
            Event::Agent(RuntimeEvent::Provider {
                event: ProviderEvent::TextDelta { text },
                ..
            }) => {
                self.signals.streamed_text.store(true, Ordering::Relaxed);
                let _ = self.tx.send(AgentEvent::AssistantDelta(text));
            }
            Event::Agent(RuntimeEvent::Provider {
                event: ProviderEvent::ThinkingDelta { text },
                ..
            }) => {
                self.signals
                    .streamed_reasoning
                    .store(true, Ordering::Relaxed);
                let _ = self.tx.send(AgentEvent::ReasoningDelta(text));
            }
            Event::Agent(RuntimeEvent::AttemptInterrupted { .. }) => {
                // An abandoned attempt's streamed text is superseded, never
                // concatenated into the next attempt's reply. The streamed
                // flags stay set: the user did see that text, so the
                // outcome renderer must not re-emit it.
                let _ = self.tx.send(AgentEvent::AssistantEnd);
            }
            Event::Agent(RuntimeEvent::ToolStarted { call, .. }) => {
                if !self.signals.tools_started.swap(true, Ordering::Relaxed) {
                    let _ = self.tx.send(AgentEvent::StatusChanged(Status::Running));
                }
                let _ = self.tx.send(AgentEvent::ToolCall(ToolCallData {
                    id: call.id,
                    kind: cards::tool_head(&call.name, &call.input),
                    lines: Vec::new(),
                    awaiting_approval: false,
                    image: None,
                }));
            }
            Event::Agent(RuntimeEvent::ToolFinished { call, output }) => {
                // A finished seam tool call settles its subagent's chat:
                // is_error is the most specific ending it sees.
                if SUBAGENT_TOOLS.contains(&call.name.as_str()) {
                    let _ = self.tx.send(AgentEvent::SubagentFinished {
                        tool_use_id: call.id.clone(),
                        is_error: output.is_error,
                    });
                }
                let result = tool_result_block(&call, &output);
                let done = cards::tool_done(call.id.clone(), &call.name, &call.input, &result);
                if !done.touched.is_empty() {
                    // Poison-tolerant: a poisoned mutex must not leave
                    // the UI stuck "Running".
                    let mut sink = self.files.lock().unwrap_or_else(|e| e.into_inner());
                    for (path, status) in &done.touched {
                        sink.insert(path.clone(), *status);
                    }
                    drop(sink);
                    let _ = self
                        .tx
                        .send(AgentEvent::FilesSet(cards::touched_files(&self.files)));
                }
                let _ = self.tx.send(AgentEvent::ToolCall(done.card));
                if call.name == "todo_write" {
                    let _ = self.tx.send(AgentEvent::PlanSet(todo_plan(&call.input)));
                }
            }
            Event::Agent(RuntimeEvent::StepFinished { usage, .. }) => {
                let in_tokens = u64::from(usage.input) + u64::from(usage.cache_read);
                let context_length = self.selection.load().context_length;
                let _ = self.tx.send(AgentEvent::TokenUsage(cards::usage_label(
                    in_tokens + u64::from(usage.output),
                    in_tokens,
                    context_length,
                )));
            }
            Event::Craft(CraftEvent::Retry {
                attempt,
                message,
                delay_ms,
            }) => self.notice(
                Tone::Warning,
                format!(
                    "retrying (attempt {attempt}): {message} — next in {:?}",
                    std::time::Duration::from_millis(delay_ms)
                ),
            ),
            Event::Craft(CraftEvent::AuthRequired { attempt, .. }) => self.notice(
                Tone::Warning,
                format!(
                    "authentication failed (attempt {attempt}); waiting for re-authentication…"
                ),
            ),
            Event::Craft(CraftEvent::AutoCompacting {
                context_size,
                context_window,
            }) => self.notice(
                Tone::Info,
                format!(
                    "auto-compacting context ({})…",
                    usage_label_size(context_size, context_window)
                ),
            ),
            Event::Craft(CraftEvent::CompactionDone {
                context_size_before,
                context_size_after,
                ..
            }) => self.notice(
                Tone::Success,
                format!(
                    "context compacted {} → {} tokens",
                    fmt_tokens(context_size_before),
                    fmt_tokens(context_size_after)
                ),
            ),
            Event::Craft(CraftEvent::StagnationDetected { .. }) => self.notice(
                Tone::Danger,
                "agent looks stuck in a loop; asking it to summarize and stop.".into(),
            ),
            Event::Craft(CraftEvent::Info(text)) => self.notice(Tone::Neutral, text),
            Event::Craft(CraftEvent::PersistenceFailed(message)) => self.notice(
                Tone::Warning,
                format!("session checkpoint failed: {message}"),
            ),
            Event::Craft(CraftEvent::AdvisorNote { severity, message }) => self.notice(
                advisor_tone(&severity),
                format!("advisor [{severity}]: {message}"),
            ),
            Event::Craft(CraftEvent::AutoReviewStart { id, .. }) => {
                // The in-progress line under the tool card; the verdict
                // event replaces it in place by sharing the call's id.
                let _ = self.tx.send(AgentEvent::AutoReview {
                    id,
                    tone: Tone::Neutral,
                    text: "auto-review: reviewing…".into(),
                });
            }
            Event::Craft(CraftEvent::AutoReviewDecision {
                id,
                verdict,
                risk,
                rationale,
                ..
            }) => {
                let (tone, text) = if verdict == "failed" {
                    (
                        Tone::Danger,
                        format!("auto-review failed closed: {rationale}"),
                    )
                } else {
                    (
                        if verdict == "allow" {
                            Tone::Success
                        } else {
                            Tone::Warning
                        },
                        format!("auto-review {verdict}: {risk} — {rationale}"),
                    )
                };
                let _ = self.tx.send(AgentEvent::AutoReview { id, tone, text });
            }
            // Nothing to render: Done is consumed by the forwarder; Error
            // rides TurnOutcome::Failed; the nudge precedes a continuation
            // the reply rendering covers; the remaining agent events are
            // substrate the TUI has no surface for.
            Event::Craft(CraftEvent::Done { .. })
            | Event::Craft(CraftEvent::Error(_))
            | Event::Craft(CraftEvent::Nudge)
            | Event::Agent(_) => {}
            // Child events arrive wrapped from the subagent launcher's
            // forwarder; route the renderable ones into the subagent's
            // task chat, tagged with the spawning call's id.
            Event::Craft(CraftEvent::Subagent {
                tool_use_id,
                description,
                event,
            }) => {
                if let Some(inner) = subagent_event(*event) {
                    let _ = self.tx.send(AgentEvent::Subagent {
                        tool_use_id,
                        description,
                        event: Box::new(inner),
                    });
                }
            }
        }
    }

    fn notice(&self, tone: Tone, text: String) {
        let _ = self.tx.send(AgentEvent::Notice { tone, text });
    }
}

/// The model-facing result block of a finished tool call, in the shape the
/// card renderer reads (same bytes the model saw).
fn tool_result_block(
    call: &animist::agent::tool::ToolCall,
    output: &animist::agent::tool::ToolOutput,
) -> history::ContentBlock {
    history::ContentBlock::ToolResult {
        tool_use_id: call.id.clone(),
        name: Some(call.name.clone()),
        content: output.content.clone(),
        is_error: output.is_error,
        loaded_tools: output.loaded_tools.clone(),
    }
}

/// Tone for an advisor note by severity: blockers shout, concerns
/// warn, nits stay neutral.
fn advisor_tone(severity: &str) -> Tone {
    match severity {
        "blocker" => Tone::Danger,
        "concern" => Tone::Warning,
        _ => Tone::Neutral,
    }
}

/// Convert one forwarded child event into the `AgentEvent` its task chat
/// renders; anything the chat has no surface for is dropped.
/// Tool cards complete from `ToolFinished` (child streaming output is
/// filtered upstream).
fn subagent_event(event: Event) -> Option<AgentEvent> {
    match event {
        Event::Agent(RuntimeEvent::Provider {
            event: ProviderEvent::TextDelta { text },
            ..
        }) => Some(AgentEvent::AssistantDelta(text)),
        Event::Agent(RuntimeEvent::Provider {
            event: ProviderEvent::ThinkingDelta { text },
            ..
        }) => Some(AgentEvent::ReasoningDelta(text)),
        Event::Agent(RuntimeEvent::ToolStarted { call, .. }) => {
            Some(AgentEvent::ToolCall(ToolCallData {
                id: call.id,
                kind: cards::tool_head(&call.name, &call.input),
                lines: Vec::new(),
                awaiting_approval: false,
                image: None,
            }))
        }
        Event::Agent(RuntimeEvent::ToolFinished { call, output }) => Some(AgentEvent::ToolCall(
            cards::tool_done(
                call.id.clone(),
                &call.name,
                &call.input,
                &tool_result_block(&call, &output),
            )
            .card,
        )),
        Event::Craft(CraftEvent::Info(text)) => Some(AgentEvent::Notice {
            tone: Tone::Neutral,
            text,
        }),
        Event::Craft(CraftEvent::AdvisorNote { severity, message }) => Some(AgentEvent::Notice {
            tone: advisor_tone(&severity),
            text: format!("advisor [{severity}]: {message}"),
        }),
        Event::Craft(CraftEvent::Retry {
            attempt, message, ..
        }) => Some(AgentEvent::Notice {
            tone: Tone::Warning,
            text: format!("retrying (attempt {attempt}): {message}"),
        }),
        _ => None,
    }
}

/// Human token count for notices (44.8K-style, matching `cards::usage_label`).
fn fmt_tokens(tokens: u64) -> String {
    cards::usage_label(tokens, tokens, None)
}

/// Sidebar plan items for a `todo_write` call: the arguments carry the full
/// replacement list, so the panel mirrors the todo store; children are
/// indented under their parent.
fn todo_plan(arguments: &serde_json::Value) -> Vec<PlanItem> {
    let todos = arguments
        .get("todos")
        .cloned()
        .and_then(|v| serde_json::from_value::<Vec<crate::tools::Todo>>(v).ok())
        .unwrap_or_default();
    crate::tools::flatten_todos(&todos)
        .into_iter()
        .map(|(todo, depth)| PlanItem {
            label: format!("{}{} {}", "  ".repeat(depth), todo.id, todo.content),
            done: matches!(todo.status.as_str(), "completed" | "cancelled"),
            active: todo.status == "in_progress",
        })
        .collect()
}

/// `size/window (pct)` for the auto-compacting notice.
fn usage_label_size(size: u64, window: u64) -> String {
    cards::usage_label(size, size, u32::try_from(window).ok())
}

/// Emit the model's reply unless streaming already showed it live.
fn emit_reply(tx: &mpsc::UnboundedSender<AgentEvent>, streamed: bool, reply: &str) {
    if streamed {
        let _ = tx.send(AgentEvent::AssistantEnd);
    } else if !reply.is_empty() {
        let _ = tx.send(AgentEvent::AssistantText(reply.to_owned()));
    }
}

pub(super) async fn resolve_model(
    config: &Config,
    selection: &Selection,
) -> Result<crate::provider::ModelHandle, String> {
    let Some(provider_config) = config.providers.get(&selection.provider) else {
        return Err(format!("unknown provider {:?}", selection.provider));
    };
    let catalog = crate::provider::load_catalog().await;
    let provider = crate::provider::ProviderHandle::from_config(provider_config, Some(&catalog))
        .await
        .map_err(report)?;
    provider
        .model(provider_config, Some(&catalog), &selection.model)
        .await
        .map_err(report)
}

/// Forward one conversation's events to the TUI for the conversation's
/// lifetime: render through [`TurnRenderer`], fold each run's terminal Done
/// into the usage ledgers, and publish the generation the turn settles on.
/// Exits when the conversation (and with it the sink) is dropped. Returns
/// the signals the session's turns settle on.
pub(super) fn start_forwarder(
    mut stream: EventStream,
    state: Arc<Mutex<SessionState>>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    files: Files,
    selection: Arc<arc_swap::ArcSwap<Selection>>,
) -> RunSignals {
    let renderer = TurnRenderer::new(tx, files, selection);
    let signals = renderer.signals.clone();
    let task_signals = signals.clone();
    tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            match event {
                Event::Craft(CraftEvent::Done { by_model, .. }) => {
                    let persister = state.lock().await.conversation.persister();
                    record_run_usage(&state, &renderer.tx, by_model.clone(), false).await;
                    if let Some(persister) = persister {
                        let mut guard = persister.lock().await;
                        guard.store_mut().record_cost(&by_model, false);
                    }
                    task_signals.bump();
                }
                other => renderer.map(other),
            }
        }
    });
    signals
}

/// What the continuation loop does after one outcome: keep looping or stop.
/// The conversation commits its own history now — the driver owns every
/// terminal marker — so outcomes only render.
enum TurnFlow {
    Continue,
    Commit,
    Abort,
}

fn handle_outcome(
    outcome: harness::TurnOutcome,
    signals: &RunSignals,
    prompt: &mut String,
    continuations: &mut usize,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> TurnFlow {
    match outcome {
        harness::TurnOutcome::Cancelled => {
            // The driver committed the interrupted attempt's prefix and the
            // cancel marker; close the bubble and settle like a done turn.
            let _ = tx.send(AgentEvent::AssistantEnd);
            TurnFlow::Commit
        }
        harness::TurnOutcome::Failed(message) => {
            // The failure is recorded in the conversation (a `[Run failed]`
            // observation); the turn's tail must not follow an aborted run.
            if signals.streamed() {
                let _ = tx.send(AgentEvent::AssistantEnd);
            }
            let _ = tx.send(AgentEvent::AssistantText(message));
            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
            TurnFlow::Abort
        }
        harness::TurnOutcome::MaxTurns => {
            // The partial run (with its end marker) committed, so the
            // follow-up prompt continues from where the budget ran out.
            let _ = tx.send(AgentEvent::AssistantText(
                "Reached the turn limit. Send another message to continue.".into(),
            ));
            TurnFlow::Commit
        }
        harness::TurnOutcome::DoomStop => {
            // The partial run (with its end marker) committed, so the
            // follow-up prompt continues from the cut-off.
            let _ = tx.send(AgentEvent::AssistantText(
                "Stopped: the agent appeared stuck in a loop. Send another message to continue."
                    .into(),
            ));
            TurnFlow::Commit
        }
        harness::TurnOutcome::MaxTokens { reply } => {
            emit_reply(tx, signals.streamed(), &reply);
            let _ = tx.send(AgentEvent::AssistantText(
                "The reply hit the output-token limit. Send another message to continue.".into(),
            ));
            TurnFlow::Commit
        }
        harness::TurnOutcome::Done { reply } => {
            if reply.is_empty() && *continuations > 0 {
                *continuations -= 1;
                *prompt = CONTINUE_AFTER_EMPTY.into();
                return TurnFlow::Continue;
            }
            if signals.streamed() || !reply.is_empty() {
                emit_reply(tx, signals.streamed(), &reply);
            } else {
                let _ = tx.send(AgentEvent::AssistantText(
                    "The model returned an empty response. Send another message to continue."
                        .into(),
                ));
            }
            TurnFlow::Commit
        }
    }
}

// A turn ending with only reasoning (no reply, no tool calls) still
// carries real work; nudge the model to continue instead of stopping.
const MAX_EMPTY_CONTINUATIONS: usize = 2;
const CONTINUE_AFTER_EMPTY: &str = "Your last turn produced no visible reply and no tool \
     calls. Continue the task with your reply or the next tool call.";

/// How long a turn waits for MCP servers to settle: slow initializes
/// covered, a hung server cannot pin the turn.
const MCP_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) async fn run_turn(ctx: TurnCtx, text: String, images: Vec<history::ContentBlock>) {
    let TurnCtx { kit, state, mode } = ctx;
    let tx = kit.evt_tx.clone();
    macro_rules! fail {
        ($message:expr) => {{
            let _ = tx.send(AgentEvent::AssistantText($message));
            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
            return;
        }};
    }

    // The turn's cancellation goes into the cell the gates, the MCP tools,
    // and the subagent launcher read; a new turn replaces it wholesale.
    // Stored before model resolution so an Esc during the (network-bound)
    // resolve cancels this turn, not the previous one's stale cell.
    let turn_cancel = animist::agent::Cancellation::default();
    kit.cancel_cell.store(Arc::new(turn_cancel.clone()));
    let selection = Arc::unwrap_or_clone(kit.selection.load_full());
    let model = match resolve_model(&kit.config, &selection).await {
        Ok(model) => model,
        Err(message) => fail!(message),
    };

    let conversation = state.lock().await.conversation.clone();
    // A model switch since the conversation was built swaps it between
    // runs, keeping the history, the layer state, and the event stream.
    // The spec, not the id alone: two providers can share a model id.
    if conversation.model_spec() != format!("{}/{}", selection.provider, model.model.id)
        && let Err(error) = conversation.set_model(model.clone())
    {
        fail!(report(error));
    }

    {
        let mut session = state.lock().await;
        let effective = crate::thinking::reconcile_for(session.thinking, Some(&model.model.info));
        if effective != session.thinking {
            session.thinking = effective;
            conversation.update_settings(|settings| settings.thinking = effective);
            if let Some(persister) = conversation.persister() {
                persister.lock().await.store_mut().set_thinking(effective);
            }
            let _ = tx.send(AgentEvent::ThinkingChanged(effective));
        }
    }
    // Per-turn mode (Tab toggle, plan implementation) and the plan-file
    // exception: the one write target allowed outside the workspace,
    // session-scoped so the Build-mode turn that implements the plan keeps
    // access to it (also after a context clear).
    conversation.update_settings(|settings| settings.mode = mode.clone());
    if let Some(plan) = mode.plan_path() {
        kit.workspace.set_plan_path(Some(plan.to_path_buf()));
    }
    // The first prompt must not ship without MCP tools, but a hung
    // server cannot block forever — the gate times out and the turn
    // proceeds with whatever has landed. Esc skips the wait (and the
    // warning): the run's own cancel check settles the turn immediately.
    if let Some(mcp) = kit.workspace.mcp() {
        let gate = tokio::time::timeout(MCP_READY_TIMEOUT, mcp.ready());
        tokio::select! {
            biased;
            () = turn_cancel.cancelled() => {}
            ready = gate => {
                if ready.is_err() {
                    tracing::warn!("MCP servers not ready after {MCP_READY_TIMEOUT:?}");
                    let _ = tx.send(AgentEvent::Notice {
                        tone: Tone::Warning,
                        text: format!(
                            "MCP servers still connecting after {MCP_READY_TIMEOUT:?}; \
                             continuing without their tools"
                        ),
                    });
                }
            }
        }
        // Feed MCP tool annotations to the permission engine;
        // sync is generation-keyed, so steady state is a no-op.
        kit.permissions.sync_mcp_annotations(&mcp);
    }

    let _ = tx.send(AgentEvent::StatusChanged(Status::Thinking));
    let mut prompt = text;
    let mut continuations = MAX_EMPTY_CONTINUATIONS;
    loop {
        let signals = state.lock().await.signals.clone();
        signals.reset();
        let generation = signals.current();
        let outcome = harness::run_turn(&conversation, &prompt, &images, turn_cancel.clone()).await;
        // The terminal Done rides the forwarder; settle before reading the
        // streamed flags so the reply is not emitted twice.
        signals.settle(generation).await;
        match handle_outcome(outcome, &signals, &mut prompt, &mut continuations, &tx) {
            TurnFlow::Continue => continue,
            TurnFlow::Abort => return,
            TurnFlow::Commit => break,
        }
    }
    // Detached, best-effort memory extraction over the committed
    // conversation; never blocks or fails the turn.
    let messages = conversation
        .history()
        .snapshot()
        .await
        .entries()
        .iter()
        .filter_map(animist::history::HistoryEntry::current)
        .map(|stored| stored.message().clone())
        .collect();
    crate::knowledge_memory::spawn_extraction(
        conversation.model(),
        messages,
        kit.workspace.root().to_path_buf(),
        kit.config.agent.memory_extraction,
    );
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer() -> (TurnRenderer, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            TurnRenderer::new(
                tx,
                Files::default(),
                Arc::new(arc_swap::ArcSwap::from_pointee(Selection {
                    provider: "mock".into(),
                    model: "model".into(),
                    context_length: Some(1_000_000),
                })),
            ),
            rx,
        )
    }

    fn notice(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> (Tone, String) {
        match rx.try_recv().expect("a notice was sent") {
            AgentEvent::Notice { tone, text } => (tone, text),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn auto_review_events_render_as_review_lines() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::AutoReviewStart {
            id: "t1".into(),
            tool: "write".into(),
            scopes: vec!["/w/a.rs".into()],
        }));
        let AgentEvent::AutoReview { id, tone, text } = rx.try_recv().expect("line") else {
            panic!("expected the in-progress AutoReview line");
        };
        assert_eq!(id, "t1");
        assert_eq!(tone, Tone::Neutral);
        assert_eq!(text, "auto-review: reviewing…");

        renderer.map(Event::Craft(CraftEvent::AutoReviewDecision {
            id: "t1".into(),
            tool: "write".into(),
            scopes: vec!["/w/a.rs".into()],
            verdict: "allow".into(),
            risk: "low".into(),
            rationale: "in-project edit".into(),
        }));
        let AgentEvent::AutoReview { id, tone, text } = rx.try_recv().expect("verdict") else {
            panic!("expected the verdict line");
        };
        assert_eq!(id, "t1", "the verdict replaces the in-progress line");
        assert_eq!(tone, Tone::Success);
        assert_eq!(text, "auto-review allow: low — in-project edit");

        // A failed review renders the fail-closed line.
        renderer.map(Event::Craft(CraftEvent::AutoReviewDecision {
            id: "t2".into(),
            tool: "write".into(),
            scopes: vec!["/w/a.rs".into()],
            verdict: "failed".into(),
            risk: String::new(),
            rationale: "timed out".into(),
        }));
        let AgentEvent::AutoReview { id, tone, text } = rx.try_recv().expect("failure") else {
            panic!("expected the failure line");
        };
        assert_eq!(id, "t2");
        assert_eq!(tone, Tone::Danger);
        assert_eq!(text, "auto-review failed closed: timed out");
    }

    fn tool_finished_event(id: &str, name: &str, is_error: bool) -> Event {
        let builder =
            animist::agent::tool::ToolOutput::builder().content(history::ToolContents(vec![
                history::ToolContent::Text { text: "ok".into() },
            ]));
        let output = if is_error {
            builder.is_error(true).build()
        } else {
            builder.build()
        };
        Event::Agent(RuntimeEvent::ToolFinished {
            call: animist::agent::tool::ToolCall {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
            },
            output,
        })
    }

    /// Both `SpawnSubagent` seam tools settle their subagent's chat when
    /// the call finishes; a plain tool must not.
    #[test]
    fn finished_seam_tools_settle_their_subagent_chats() {
        for name in SUBAGENT_TOOLS {
            let (renderer, mut rx) = renderer();
            renderer.map(tool_finished_event("t1", name, false));
            let AgentEvent::SubagentFinished {
                tool_use_id,
                is_error,
            } = rx.try_recv().expect("subagent finished")
            else {
                panic!("expected SubagentFinished for {name}");
            };
            assert_eq!(tool_use_id, "t1");
            assert!(!is_error);
            assert!(
                matches!(rx.try_recv(), Ok(AgentEvent::ToolCall(_))),
                "the done card still renders"
            );
        }

        let (renderer, mut rx) = renderer();
        renderer.map(tool_finished_event("t9", "bash", true));
        assert!(
            matches!(rx.try_recv(), Ok(AgentEvent::ToolCall(_))),
            "non-seam tools emit no SubagentFinished"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn todo_write_done_emits_plan_set() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Agent(RuntimeEvent::ToolFinished {
            call: animist::agent::tool::ToolCall {
                id: "t1".into(),
                name: "todo_write".into(),
                input: serde_json::json!({"todos": [
                    {"id": "T1", "parent": null, "content": "Map the seam", "status": "completed"},
                    {"id": "T1.1", "parent": "T1", "content": "Fix the renderer", "status": "in_progress"},
                    {"id": "T2", "parent": null, "content": "Add tests", "status": "pending"},
                ]}),
            },
            output: animist::agent::tool::ToolOutput::builder()
                .content(history::ToolContents(vec![history::ToolContent::Text {
                    text: "ok".into(),
                }]))
                .build(),
        }));
        // Tool card first, then the plan snapshot.
        assert!(matches!(rx.try_recv(), Ok(AgentEvent::ToolCall(_))));
        let AgentEvent::PlanSet(plan) = rx.try_recv().expect("plan was sent") else {
            panic!("expected PlanSet");
        };
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].label, "T1 Map the seam");
        assert!(plan[0].done && !plan[0].active);
        assert_eq!(plan[1].label, "  T1.1 Fix the renderer");
        assert!(!plan[1].done && plan[1].active);
        assert!(!plan[2].done && !plan[2].active);
    }

    #[test]
    fn retry_maps_to_a_warning_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::Retry {
            attempt: 2,
            message: "stream reset".into(),
            delay_ms: 1500,
        }));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Warning);
        assert!(
            text.contains("attempt 2") && text.contains("stream reset") && text.contains("1.5s"),
            "{text}"
        );
    }

    /// An abandoned attempt closes the streaming buffer so its text stands
    /// superseded, separate from the next attempt's reply.
    #[test]
    fn attempt_interrupted_closes_the_streaming_buffer() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Agent(RuntimeEvent::Provider {
            run_id: 0,
            step: 0,
            attempt: 0,
            event: ProviderEvent::TextDelta {
                text: "abandoned prefix".into(),
            },
        }));
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentEvent::AssistantDelta(text)) if text == "abandoned prefix"
        ));
        renderer.map(Event::Agent(RuntimeEvent::AttemptInterrupted {
            run_id: 0,
            step: 0,
            attempt: 0,
        }));
        assert!(
            matches!(rx.try_recv(), Ok(AgentEvent::AssistantEnd)),
            "the abandoned buffer closes before the next attempt streams"
        );
        renderer.map(Event::Agent(RuntimeEvent::Provider {
            run_id: 0,
            step: 0,
            attempt: 1,
            event: ProviderEvent::TextDelta {
                text: "fresh start".into(),
            },
        }));
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentEvent::AssistantDelta(text)) if text == "fresh start"
        ));
    }

    #[test]
    fn auth_required_maps_to_a_warning_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::AuthRequired {
            attempt: 1,
            message: "401".into(),
        }));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Warning);
        assert!(
            text.contains("attempt 1") && text.contains("re-authentication"),
            "{text}"
        );
    }

    #[test]
    fn auto_compacting_maps_to_an_info_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::AutoCompacting {
            context_size: 50_000,
            context_window: 1_000_000,
        }));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Info);
        assert!(text.starts_with("auto-compacting context ("), "{text}");
    }

    #[test]
    fn compaction_done_maps_to_a_success_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::CompactionDone {
            context_size_before: 50_000,
            context_size_after: 10_000,
            context_window: 1_000_000,
        }));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Success);
        assert_eq!(text, "context compacted 50.0K → 10.0K tokens");
    }

    #[test]
    fn stagnation_maps_to_a_danger_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::StagnationDetected {
            similarity: 0.68,
        }));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Danger);
        assert!(text.contains("stuck in a loop"), "{text}");
    }

    #[test]
    fn info_maps_to_a_neutral_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Craft(CraftEvent::Info(
            "guardrail blocked read: no progress".into(),
        )));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Neutral);
        assert!(text.contains("guardrail blocked read"), "{text}");
    }

    /// A dedup `Respond` never fires `ToolStarted`: the card renders from
    /// `ToolFinished` alone, so a served-from-cache call still completes
    /// its card instead of hanging "running".
    #[test]
    fn tool_finished_without_tool_started_still_renders_the_done_card() {
        let (renderer, mut rx) = renderer();
        renderer.map(Event::Agent(RuntimeEvent::ToolFinished {
            call: animist::agent::tool::ToolCall {
                id: "cache-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
            },
            output: animist::agent::tool::ToolOutput::builder()
                .content(history::ToolContents(vec![history::ToolContent::Text {
                    text: "file body".into(),
                }]))
                .build(),
        }));
        let event = rx.try_recv().expect("the done card renders");
        let AgentEvent::ToolCall(card) = event else {
            panic!("expected a tool card, got {event:?}");
        };
        assert_eq!(card.id, "cache-1");
        assert!(!card.awaiting_approval);
        assert!(
            !card.lines.is_empty(),
            "the card body carries the result text"
        );
    }

    #[tokio::test]
    async fn settle_returns_once_the_forwarder_bumps_the_generation() {
        let signals = RunSignals::new();
        let before = signals.current();
        let waiter = signals.clone();
        let settled = tokio::spawn(async move { waiter.settle(before).await });
        // Not settled until the bump lands…
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!settled.is_finished());
        signals.bump();
        settled.await.expect("settle completes");
        assert_eq!(signals.current(), before + 1);
    }

    #[test]
    fn empty_done_continues_and_decrements_the_budget() {
        let (renderer, _rx) = renderer();
        let (tx, _out) = mpsc::unbounded_channel();
        let mut prompt = String::new();
        let mut continuations = MAX_EMPTY_CONTINUATIONS;
        let flow = handle_outcome(
            harness::TurnOutcome::Done {
                reply: String::new(),
            },
            &renderer.signals,
            &mut prompt,
            &mut continuations,
            &tx,
        );
        assert!(matches!(flow, TurnFlow::Continue));
        assert_eq!(continuations, MAX_EMPTY_CONTINUATIONS - 1);
        assert!(!prompt.is_empty(), "the continuation prompt was staged");
    }

    #[test]
    fn failed_aborts_without_the_done_tail() {
        let (renderer, _) = renderer();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut prompt = String::new();
        let mut continuations = MAX_EMPTY_CONTINUATIONS;
        let flow = handle_outcome(
            harness::TurnOutcome::Failed("provider exploded".into()),
            &renderer.signals,
            &mut prompt,
            &mut continuations,
            &tx,
        );
        assert!(matches!(flow, TurnFlow::Abort));
        let event = rx.try_recv().expect("the failure text was sent");
        assert!(matches!(event, AgentEvent::AssistantText(ref t) if t == "provider exploded"));
        assert!(matches!(
            rx.try_recv(),
            Ok(AgentEvent::StatusChanged(Status::Failed))
        ));
    }
}
