//! One agent turn: drives the shared run loop, rendering to the TUI seam.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc};

use crate::compaction::CompactionEngine;
use crate::config::Config;
use crate::history;
use crate::permissions::PermissionManager;
use crate::providers::Provider as ClientProvider;
use crate::run::{self, CancelToken, RunOutcome};
use crate::tools::Workspace;

use super::super::cards::{self, Files};
use super::SessionState;
use super::approval::{ApprovalGate, model_reviewer};
use super::usage_recorder::record_run_usage;
use crate::tui::provider::{AgentEvent, Status, ToolCallData};

use super::{Selection, report};

fn lock_sink<T>(sink: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Everything one agent turn needs, bundled so the turn's helpers avoid a
/// long parameter list.
pub(super) struct TurnCtx {
    pub(super) config: Arc<Config>,
    pub(super) workspace: Workspace,
    pub(super) instructions_text: String,
    pub(super) selection: Selection,
    pub(super) state: Arc<Mutex<SessionState>>,
    pub(super) files: Files,
    pub(super) cancel: CancelToken,
    pub(super) tx: mpsc::UnboundedSender<AgentEvent>,
    pub(super) permissions: Arc<PermissionManager>,
}

/// Maps run-loop events to TUI events for one model call. Owns the
/// streaming-progress flags the outcome rendering consults afterwards.
struct TurnRenderer {
    tx: mpsc::UnboundedSender<AgentEvent>,
    files: Files,
    context_length: Option<u32>,
    streamed_text: Arc<AtomicBool>,
    streamed_reasoning: Arc<AtomicBool>,
    tools_started: Arc<AtomicBool>,
}

impl TurnRenderer {
    fn new(
        tx: mpsc::UnboundedSender<AgentEvent>,
        files: Files,
        context_length: Option<u32>,
    ) -> Self {
        Self {
            tx,
            files,
            context_length,
            streamed_text: Arc::new(AtomicBool::new(false)),
            streamed_reasoning: Arc::new(AtomicBool::new(false)),
            tools_started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn map(&self, event: run::Event) {
        match event {
            run::Event::TextDelta(delta) => {
                self.streamed_text.store(true, Ordering::Relaxed);
                let _ = self.tx.send(AgentEvent::AssistantDelta(delta));
            }
            run::Event::ThinkingDelta(delta) => {
                self.streamed_reasoning.store(true, Ordering::Relaxed);
                let _ = self.tx.send(AgentEvent::ReasoningDelta(delta));
            }
            run::Event::ToolStart {
                id,
                name,
                arguments,
            } => {
                if !self.tools_started.swap(true, Ordering::Relaxed) {
                    let _ = self.tx.send(AgentEvent::StatusChanged(Status::Running));
                }
                let _ = self.tx.send(AgentEvent::ToolCall(ToolCallData {
                    id,
                    kind: cards::tool_head(&name, &arguments),
                    lines: Vec::new(),
                    awaiting_approval: false,
                }));
            }
            run::Event::ToolDone {
                id,
                name,
                arguments,
                result,
            } => {
                let done = cards::tool_done(id, &name, &arguments, &result);
                if let Some((path, status)) = done.touched {
                    self.files.lock().expect("files lock").insert(path, status);
                    let _ = self
                        .tx
                        .send(AgentEvent::FilesSet(cards::touched_files(&self.files)));
                }
                let _ = self.tx.send(AgentEvent::ToolCall(done.card));
            }
            run::Event::TurnComplete { usage, .. } => {
                let _ = self.tx.send(AgentEvent::TokenUsage(cards::usage_label(
                    usage.input_tokens + usage.output_tokens,
                    usage.input_tokens,
                    self.context_length,
                )));
            }
            // The nudge is visible in the next model call; nothing to show.
            // The remaining taxonomy variants carry no TUI rendering yet.
            run::Event::Nudge
            | run::Event::ToolPending { .. }
            | run::Event::ToolOutput { .. }
            | run::Event::ToolResultsSubmitted { .. }
            | run::Event::Done { .. }
            | run::Event::Info(_)
            | run::Event::Error(_)
            | run::Event::Retry { .. }
            | run::Event::AuthRequired { .. }
            | run::Event::AutoCompacting { .. }
            | run::Event::CompactionDone { .. }
            | run::Event::StagnationDetected { .. }
            | run::Event::AutoReviewStart { .. }
            | run::Event::AutoReviewDecision { .. }
            | run::Event::StreamClosed => {}
        }
    }

    /// Whether text or reasoning already streamed this call (the reply was
    /// shown live, so it must not be emitted again).
    fn streamed(&self) -> bool {
        self.streamed_text.load(Ordering::Relaxed)
            || self.streamed_reasoning.load(Ordering::Relaxed)
    }
}

/// Emit the model's reply unless streaming already showed it live.
fn emit_reply(tx: &mpsc::UnboundedSender<AgentEvent>, streamed: bool, reply: &str) {
    if streamed {
        let _ = tx.send(AgentEvent::AssistantEnd);
    } else if !reply.is_empty() {
        let _ = tx.send(AgentEvent::AssistantText(reply.to_owned()));
    }
}

async fn resolve_model(
    config: &Config,
    selection: &Selection,
) -> Result<crate::providers::DynamicModel, String> {
    let Some(provider_config) = config.providers.get(&selection.provider) else {
        return Err(format!("unknown provider {:?}", selection.provider));
    };
    let provider = ClientProvider::from_config(provider_config).map_err(report)?;
    provider.completion_model(&selection.model).map_err(report)
}

/// Run configured compaction stages whose context-fill threshold is crossed
/// before the history is sent to the model; commit effectiveness state only.
async fn compact_history(
    state: &Arc<Mutex<SessionState>>,
    config: &Config,
    model: &crate::providers::DynamicModel,
    history: &mut Vec<history::Message>,
    context_length: Option<u32>,
) {
    let shared = state.lock().await.compaction.clone();
    if let Some(mut compaction) = shared.lock().ok().map(|guard| guard.clone()) {
        CompactionEngine::new(config.compaction.clone())
            .with_buffer(config.compaction_buffer)
            .maybe_compact(&mut compaction, model, history, context_length)
            .await;
        if let Ok(mut guard) = shared.lock() {
            *guard = compaction;
        }
    }
}

/// What the continuation loop does after one outcome: keep looping, stop
/// and commit the history, or abort the turn leaving history untouched.
enum TurnFlow {
    Continue,
    Commit,
    Abort,
}

#[allow(clippy::too_many_arguments)]
async fn handle_outcome(
    outcome: RunOutcome,
    renderer: &TurnRenderer,
    history: &[history::Message],
    prompt: &mut String,
    continuations: &mut usize,
    state: &Arc<Mutex<SessionState>>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) -> TurnFlow {
    match outcome {
        RunOutcome::Cancelled => {
            let _ = tx.send(AgentEvent::AssistantEnd);
            let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
            TurnFlow::Abort
        }
        RunOutcome::Failed(message) => {
            if renderer.streamed() {
                let _ = tx.send(AgentEvent::AssistantEnd);
            }
            let _ = tx.send(AgentEvent::AssistantText(message));
            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
            TurnFlow::Abort
        }
        RunOutcome::MaxTurns => {
            // Keep the partial run: the follow-up prompt continues from
            // where the budget ran out instead of silently losing it.
            state.lock().await.history = history.to_vec();
            let _ = tx.send(AgentEvent::AssistantText(
                "Reached the turn limit. Send another message to continue.".into(),
            ));
            TurnFlow::Commit
        }
        RunOutcome::DoomStop => {
            // The doom-loop hard stop committed the sanitized partial run;
            // like MaxTurns, continue from the cut-off on the next message.
            state.lock().await.history = history.to_vec();
            let _ = tx.send(AgentEvent::AssistantText(
                "Stopped: the agent appeared stuck in a loop. Send another message to continue."
                    .into(),
            ));
            TurnFlow::Commit
        }
        RunOutcome::MaxTokens { reply } => {
            state.lock().await.history = history.to_vec();
            emit_reply(tx, renderer.streamed(), &reply);
            let _ = tx.send(AgentEvent::AssistantText(
                "The reply hit the output-token limit. Send another message to continue.".into(),
            ));
            TurnFlow::Commit
        }
        RunOutcome::Done { reply } => {
            if reply.is_empty() && *continuations > 0 {
                *continuations -= 1;
                *prompt = CONTINUE_AFTER_EMPTY.into();
                return TurnFlow::Continue;
            }
            if renderer.streamed() || !reply.is_empty() {
                emit_reply(tx, renderer.streamed(), &reply);
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

// A turn that ends with only reasoning (no reply, no tool calls) still
// carries real work; nudge the model to continue instead of stopping.
const MAX_EMPTY_CONTINUATIONS: usize = 2;
const CONTINUE_AFTER_EMPTY: &str = "Your last turn produced no visible reply and no tool \
     calls. Continue the task with your reply or the next tool call.";

pub(super) async fn run_turn(ctx: TurnCtx, text: String) {
    let TurnCtx {
        config,
        workspace,
        instructions_text,
        selection,
        state,
        files,
        cancel,
        tx,
        permissions,
    } = ctx;
    macro_rules! fail {
        ($message:expr) => {{
            let _ = tx.send(AgentEvent::AssistantText($message));
            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
            return;
        }};
    }

    state.lock().await.pending_approval = None;

    let model = match resolve_model(&config, &selection).await {
        Ok(model) => model,
        Err(message) => fail!(message),
    };

    let mut history = state.lock().await.history.clone();
    let dedup = state.lock().await.dedup.clone();
    let guardrails = state.lock().await.guardrails.clone();

    compact_history(
        &state,
        &config,
        &model,
        &mut history,
        selection.context_length,
    )
    .await;
    let compaction_ctx = run::CompactionCtx {
        state: state.lock().await.compaction.clone(),
        stages: config.compaction.clone(),
        buffer: config.compaction_buffer,
        context_length: selection.context_length,
    };

    let tools = workspace
        .register()
        .with_dedup(dedup)
        .with_guardrails(guardrails)
        .with_before(Arc::new(ApprovalGate::new(
            state.clone(),
            tx.clone(),
            cancel.clone(),
            permissions.clone(),
            Some(model_reviewer(model.clone())),
        )));
    let params = run::RunParams {
        preamble: Some(crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", workspace.root().display().to_string())
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &format!("{}{}", config.agent.preamble, instructions_text),
            &crate::prompt::ResolvedSlots::default(),
        )),
        temperature: config.agent.temperature,
        max_tokens: config.agent.max_tokens,
        max_turns: run::RunParams::UNBOUNDED,
        recency: None,
        compression: config.compression.clone(),
        max_continuation_turns: run::RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
        compaction: Some(compaction_ctx),
        reauth: config
            .providers
            .get(&selection.provider)
            .map(|provider_config| {
                crate::providers::reauth_hook(provider_config, &selection.model)
            }),
        model_spec: Some(format!("{}/{}", selection.provider, selection.model).into()),
        retry: run::RetryCtx::default(),
        fast: false,
    };

    let _ = tx.send(AgentEvent::StatusChanged(Status::Thinking));
    let mut prompt = text;
    let mut continuations = MAX_EMPTY_CONTINUATIONS;
    loop {
        let renderer = TurnRenderer::new(tx.clone(), files.clone(), selection.context_length);
        // The terminal Done's per-model ledger, captured from the event seam;
        // folded into the session and the cost ledger once the run returns.
        let done_by_model = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&done_by_model);
        let outcome = run::run(
            &model,
            &params,
            &tools,
            &mut history,
            &prompt,
            &cancel,
            &|event| {
                if let run::Event::Done { by_model, .. } = event {
                    lock_sink(&done_by_model).replace(by_model);
                    return;
                }
                renderer.map(event);
            },
        )
        .await;
        let by_model = lock_sink(&sink).take().unwrap_or_default();
        record_run_usage(&state, &tx, by_model, params.fast).await;

        match handle_outcome(
            outcome,
            &renderer,
            &history,
            &mut prompt,
            &mut continuations,
            &state,
            &tx,
        )
        .await
        {
            TurnFlow::Continue => continue,
            TurnFlow::Abort => return,
            TurnFlow::Commit => break,
        }
    }
    state.lock().await.history = history;
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
}
