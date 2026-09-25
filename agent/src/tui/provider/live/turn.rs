//! One agent turn: drives the shared run loop, rendering to the TUI seam.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc};

use crate::compaction::CompactionEngine;
use crate::config::Config;
use crate::history;
use crate::permissions::PermissionManager;
use crate::providers::Provider as ClientProvider;
use crate::run::{self, AgentMode, CancelToken, RunOutcome};
use crate::tools::Workspace;

use super::super::cards::{self, Files};
use super::SessionState;
use super::approval::{ApprovalGate, model_reviewer};
use super::usage_recorder::record_run_usage;
use crate::tui::provider::{AgentEvent, Status, Tone, ToolCallData};

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
    pub(super) mode: AgentMode,
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
            run::Event::Retry {
                attempt,
                message,
                delay_ms,
            } => self.notice(
                Tone::Warning,
                format!(
                    "retrying (attempt {attempt}): {message} — next in {:?}",
                    std::time::Duration::from_millis(delay_ms)
                ),
            ),
            run::Event::AuthRequired { attempt, .. } => self.notice(
                Tone::Warning,
                format!(
                    "authentication failed (attempt {attempt}); waiting for re-authentication…"
                ),
            ),
            run::Event::AutoCompacting {
                context_size,
                context_window,
            } => self.notice(
                Tone::Info,
                format!(
                    "auto-compacting context ({})…",
                    usage_label_size(context_size, context_window)
                ),
            ),
            run::Event::CompactionDone {
                context_size_before,
                context_size_after,
                ..
            } => self.notice(
                Tone::Success,
                format!(
                    "context compacted {} → {} tokens",
                    fmt_tokens(context_size_before),
                    fmt_tokens(context_size_after)
                ),
            ),
            run::Event::StagnationDetected { .. } => self.notice(
                Tone::Danger,
                "agent looks stuck in a loop; asking it to summarize and stop.".into(),
            ),
            run::Event::Info(text) => self.notice(Tone::Neutral, text),
            // The nudge is visible in the next model call; nothing to show.
            // Live-call rendering of ToolPending/ToolOutput/
            // ToolResultsSubmitted is Phase 8 tool work; Done is consumed
            // by the caller; Error rides RunOutcome::Failed; auto-review
            // renders through its tool card (approval.rs).
            run::Event::Nudge
            | run::Event::ToolPending { .. }
            | run::Event::ToolOutput { .. }
            | run::Event::ToolResultsSubmitted { .. }
            | run::Event::Done { .. }
            | run::Event::Error(_)
            | run::Event::AutoReviewStart { .. }
            | run::Event::AutoReviewDecision { .. }
            | run::Event::StreamClosed => {}
        }
    }

    fn notice(&self, tone: Tone, text: String) {
        let _ = self.tx.send(AgentEvent::Notice { tone, text });
    }

    /// Whether text or reasoning already streamed this call (the reply was
    /// shown live, so it must not be emitted again).
    fn streamed(&self) -> bool {
        self.streamed_text.load(Ordering::Relaxed)
            || self.streamed_reasoning.load(Ordering::Relaxed)
    }
}

/// Human token count for notices (44.8K-style, matching `cards::usage_label`).
fn fmt_tokens(tokens: u64) -> String {
    cards::usage_label(tokens, tokens, None)
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
/// Posts a notice when a stage actually ran; silence remains the default.
async fn compact_history(
    state: &Arc<Mutex<SessionState>>,
    config: &Config,
    model: &crate::providers::DynamicModel,
    history: &mut Vec<history::Message>,
    context_length: Option<u32>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let shared = state.lock().await.compaction.clone();
    if let Some(mut compaction) = shared.lock().ok().map(|guard| guard.clone()) {
        let before = compaction
            .estimator
            .scale(crate::compaction::estimate_tokens(history));
        let ran = CompactionEngine::new(config.compaction.clone())
            .with_buffer(config.compaction_buffer)
            .maybe_compact(&mut compaction, model, history, context_length)
            .await;
        if let Ok(mut guard) = shared.lock() {
            *guard = compaction;
        }
        if ran {
            let after = shared
                .lock()
                .map(|guard| {
                    guard
                        .estimator
                        .scale(crate::compaction::estimate_tokens(history))
                })
                .unwrap_or_default();
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Info,
                text: format!(
                    "context compacted {} → {} tokens",
                    fmt_tokens(before),
                    fmt_tokens(after)
                ),
            });
        }
    }
}

/// `/compact`: force every armed compaction stage over the session history,
/// announcing the run and reporting through notices; also refreshes the token
/// label. An empty or already-compact history reports a neutral no-op (silence
/// would read as a lost command).
pub(super) async fn compact_now(
    config: &Config,
    selection: &Selection,
    state: &Arc<Mutex<SessionState>>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let model = match resolve_model(config, selection).await {
        Ok(model) => model,
        Err(message) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: message,
            });
            return;
        }
    };
    let shared = state.lock().await.compaction.clone();
    let Some(mut compaction) = shared.lock().ok().map(|guard| guard.clone()) else {
        return;
    };
    let mut session = state.lock().await;
    let before = compaction
        .estimator
        .scale(crate::compaction::estimate_tokens(&session.history));
    // `force_compact` can await an LLM summary before it returns; announce the
    // run so the pause is not mistaken for a lost command.
    let _ = tx.send(AgentEvent::Notice {
        tone: Tone::Info,
        text: format!(
            "compacting context ({})…",
            usage_label_size(before, u64::from(selection.context_length.unwrap_or(0)))
        ),
    });
    let ran = CompactionEngine::new(config.compaction.clone())
        .with_buffer(config.compaction_buffer)
        .force_compact(
            &mut compaction,
            &model,
            &mut session.history,
            selection.context_length,
        )
        .await;
    let after = compaction
        .estimator
        .scale(crate::compaction::estimate_tokens(&session.history));
    if ran {
        let messages = session.history.clone();
        if let Some(store) = &mut session.store {
            store.record_turn(
                &messages,
                format!("{}/{}", selection.provider, selection.model),
            );
        }
    }
    drop(session);
    if let Ok(mut guard) = shared.lock() {
        *guard = compaction;
    }
    if ran {
        let _ = tx.send(AgentEvent::Notice {
            tone: Tone::Success,
            text: format!(
                "context compacted {} → {} tokens",
                fmt_tokens(before),
                fmt_tokens(after)
            ),
        });
        let _ = tx.send(AgentEvent::TokenUsage(cards::usage_label(
            after,
            after,
            selection.context_length,
        )));
    } else {
        let _ = tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "context already compact".into(),
        });
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
        mode,
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
    // The plan file is the one write target allowed outside the workspace;
    // cleared again on every Build-mode turn.
    workspace.set_plan_path(mode.plan_path().map(|p| p.to_path_buf()));

    compact_history(
        &state,
        &config,
        &model,
        &mut history,
        selection.context_length,
        &tx,
    )
    .await;
    let compaction_ctx = run::CompactionCtx {
        state: state.lock().await.compaction.clone(),
        stages: config.compaction.clone(),
        buffer: config.compaction_buffer,
        context_length: selection.context_length,
    };

    let tools = workspace
        .register_with_mode(mode.clone())
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
            mode.plan_path(),
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
    {
        let mut session = state.lock().await;
        session.history = history;
        // The committed turn is persisted, the same seam headless uses, so
        // `/sessions` can list and reload this conversation. (The clone is
        // sequenced before the store borrow: field splits do not apply
        // through the mutex guard's deref.)
        let messages = session.history.clone();
        if let Some(store) = &mut session.store {
            store.record_turn(
                &messages,
                params.model_spec.as_deref().unwrap_or("unknown").to_owned(),
            );
        }
    }
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn renderer() -> (TurnRenderer, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (TurnRenderer::new(tx, Files::default(), Some(1_000_000)), rx)
    }

    fn notice(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> (Tone, String) {
        match rx.try_recv().expect("a notice was sent") {
            AgentEvent::Notice { tone, text } => (tone, text),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn retry_maps_to_a_warning_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(run::Event::Retry {
            attempt: 2,
            message: "stream reset".into(),
            delay_ms: 1500,
        });
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Warning);
        assert!(
            text.contains("attempt 2") && text.contains("stream reset") && text.contains("1.5s"),
            "{text}"
        );
    }

    #[test]
    fn auth_required_maps_to_a_warning_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(run::Event::AuthRequired {
            attempt: 1,
            message: "401".into(),
        });
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
        renderer.map(run::Event::AutoCompacting {
            context_size: 50_000,
            context_window: 1_000_000,
        });
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Info);
        assert!(text.starts_with("auto-compacting context ("), "{text}");
    }

    #[test]
    fn compaction_done_maps_to_a_success_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(run::Event::CompactionDone {
            context_size_before: 50_000,
            context_size_after: 10_000,
            context_window: 1_000_000,
        });
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Success);
        assert_eq!(text, "context compacted 50.0K → 10.0K tokens");
    }

    #[test]
    fn stagnation_maps_to_a_danger_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(run::Event::StagnationDetected { similarity: 0.68 });
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Danger);
        assert!(text.contains("stuck in a loop"), "{text}");
    }

    #[test]
    fn info_maps_to_a_neutral_notice() {
        let (renderer, mut rx) = renderer();
        renderer.map(run::Event::Info(
            "guardrail blocked read: no progress".into(),
        ));
        let (tone, text) = notice(&mut rx);
        assert_eq!(tone, Tone::Neutral);
        assert!(text.contains("guardrail blocked read"), "{text}");
    }
}
