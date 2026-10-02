//! The session command loop: dispatches [`Command`]s, drives turns and
//! approvals, and streams [`AgentEvent`]s back to the UI.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::AbortHandle;

use crate::permissions::PermissionAnswer;
use crate::run;

use super::approval::decide;
use super::cards::Files;
use super::mcp_request;
use super::question::answer_question;
use super::resume::{
    handle_load_session, handle_resume_latest, handle_set_draft, load_session, persist_on_exit,
    resume_latest,
};
use super::turn::{self, TurnCtx, run_turn};
use super::{AgentEvent, Command, Provider, Status, Tone, UsageFetchState};
use super::{CraftProvider, LoopCtx, Selection, SessionState, report};

impl CraftProvider {
    /// The session's command loop: receives [`Command`]s, drives turns and
    /// approvals, and streams [`AgentEvent`]s back. Spawned by `start`.
    async fn spawn_command_loop(
        self,
        mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
        evt_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    ) {
        let mut selection = self.selection;
        let cwd = self.workspace.root().display().to_string();
        let state_dir = crate::storage::StateDir::resolve().ok();
        let state = Arc::new(Mutex::new(
            SessionState::linked()
                .with_thinking(
                    self.config
                        .always_thinking
                        .or(self.config.agent.thinking)
                        .unwrap_or_default(),
                )
                .with_store(state_dir.as_ref(), &cwd, &LoopCtx::model_spec(&selection)),
        ));
        let files: Files = Files::default();
        let (cancel_flag, _) = run::cancel_channel();
        let subagent_cancels = Arc::new(run::cancel::CancelMap::new());
        let mut current_turn: Option<AbortHandle> = None;
        // Bang-mode bookkeeping lives in `ctx.shell` (ShellState).
        // Quota-fetch generation: shared with the spawned fetches so a slow
        // stale answer never overwrites a newer one (rapid Ctrl+R).
        let usage_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let shell: Arc<std::sync::Mutex<crate::tui::shell::ShellState>> = Arc::default();
        // Signals the loop when a running turn finishes (see LoopCtx).
        let wake = Arc::new(Notify::new());
        let permissions = self.permissions;
        let workspace = self.workspace;
        let snapshots = workspace.snapshots().clone();
        let instructions_text = self.instructions.text;
        let config = self.config;
        let catalogs = self.catalogs;
        let ctx = LoopCtx {
            state,
            files,
            cancel_flag,
            subagent_cancels,
            shell,
            permissions,
            config,
            workspace,
            instructions_text,
            catalogs,
            snapshots,
            state_dir: state_dir.clone(),
            cwd,
            evt_tx: evt_tx.clone(),
            wake: wake.clone(),
        };
        // Messages submitted while a turn is still running queue here and
        // are sent, in order, when that turn settles (never aborting it).
        let mut pending_messages: VecDeque<PendingMessage> = VecDeque::new();
        // Phase 4: server-initiated MCP requests (elicitation) arrive on
        // their own channel; each is answered on its own task so a parked
        // form never blocks the command loop.
        let mut server_requests = ctx
            .workspace
            .mcp()
            .and_then(|handle| handle.take_server_requests());

        let _ = evt_tx.send(AgentEvent::SessionInfo {
            cwd: self.cwd_label,
            branch: self.branch,
        });
        for note in self.notes {
            let _ = evt_tx.send(AgentEvent::AssistantText(note));
        }
        let (models, current) = ctx.catalog_choices(&selection);
        let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
        let _ = evt_tx.send(AgentEvent::ThinkingChanged(ctx.state.lock().await.thinking));
        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
        let _ = evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));

        if let Some(id) = &self.resume_session {
            load_session(
                &ctx.state,
                &ctx.files,
                id,
                ctx.state_dir.as_ref(),
                &ctx.cwd,
                &LoopCtx::model_spec(&selection),
                &ctx.evt_tx,
            )
            .await;
        } else if self.resume_latest {
            resume_latest(
                &ctx.state,
                &ctx.files,
                ctx.state_dir.as_ref(),
                &ctx.cwd,
                &LoopCtx::model_spec(&selection),
                &ctx.evt_tx,
            )
            .await;
        }

        let thinking = ctx.state.lock().await.thinking;
        handle_set_thinking(&ctx, &selection, thinking).await;

        loop {
            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
                _ = wake.notified() => {
                    // A turn settled: drop a finished handle and send the
                    // next queued message, if any.
                    maybe_send_next(&ctx, &selection, &mut current_turn, &mut pending_messages)
                        .await;
                    continue;
                }
                req = async { server_requests.as_mut().expect("polled only when present").recv().await },
                    if server_requests.is_some() => {
                    match req {
                        Some(request) => {
                            mcp_request::spawn_server_request(
                                mcp_request::ServerRequestCtx {
                                    state: ctx.state.clone(),
                                    evt_tx: ctx.evt_tx.clone(),
                                },
                                request,
                            );
                        }
                        // Every session dropped its sender (shutdown); stop
                        // polling the closed channel.
                        None => server_requests = None,
                    }
                    continue;
                }
            };
            // A turn that finished on its own leaves its handle behind;
            // drop it so the turn-running guards (undo/compact/load)
            // don't refuse forever after the first completed turn, then
            // send the next queued message if any.
            maybe_send_next(&ctx, &selection, &mut current_turn, &mut pending_messages).await;
            match cmd {
                Command::SetThinking(thinking) => {
                    handle_set_thinking(&ctx, &selection, thinking).await
                }
                Command::SendMessage(text, mode, images) => {
                    handle_send_message(
                        &ctx,
                        &selection,
                        &mut current_turn,
                        &mut pending_messages,
                        text,
                        mode,
                        images,
                    )
                    .await
                }
                Command::Shell { command, visible } => handle_shell(&ctx, command, visible),
                Command::RunMcpPrompt {
                    qualified,
                    arguments,
                    mode,
                } => {
                    handle_mcp_prompt(
                        &ctx,
                        &selection,
                        &mut current_turn,
                        &mut pending_messages,
                        qualified,
                        arguments,
                        mode,
                    )
                    .await
                }
                Command::Approve { id, always } => {
                    let answer = if always {
                        PermissionAnswer::AllowAlwaysLocal
                    } else {
                        PermissionAnswer::AllowSession
                    };
                    decide(&ctx.state, id, answer).await
                }
                Command::Reject { id, always } => {
                    let answer = if always {
                        PermissionAnswer::DenyAlwaysLocal
                    } else {
                        PermissionAnswer::Deny
                    };
                    decide(&ctx.state, id, answer).await
                }
                Command::AnswerPermission { id, answer } => {
                    decide(&ctx.state, id, answer).await;
                }
                Command::AnswerQuestion { id, answer } => {
                    answer_question(&ctx.state, id, answer).await;
                }
                Command::ToggleAutoReview => {
                    let on = ctx.permissions.toggle_auto_review();
                    let _ = ctx.evt_tx.send(AgentEvent::AssistantText(format!(
                        "auto-review {}.",
                        if on { "on" } else { "off" }
                    )));
                }
                Command::ArgosyMemory { query } => handle_argosy_memory(&ctx, query).await,
                Command::GetUsage => {
                    let rows = ctx.state.lock().await.usage.rows();
                    let _ = ctx.evt_tx.send(AgentEvent::UsageSnapshot(rows));
                }
                Command::FetchUsage => handle_fetch_usage(&ctx, &selection, &usage_gen).await,
                Command::Compact => handle_compact(&ctx, &selection, &current_turn).await,
                Command::LoadSession { id } => {
                    handle_load_session(&ctx, &selection, &current_turn, id).await
                }
                Command::ResumeLatest => {
                    handle_resume_latest(&ctx, &selection, &current_turn).await
                }
                Command::SetDraft(draft) => handle_set_draft(&ctx, draft).await,
                Command::Interrupt => {
                    handle_interrupt(&ctx, &current_turn);
                    pending_messages.clear();
                }
                Command::CancelSubagent { tool_use_id } => {
                    // Marks the id so children spawned later under it die
                    // too; the parent turn keeps running.
                    ctx.subagent_cancels.cancel_or_precancel(tool_use_id);
                }
                Command::Clear => {
                    handle_clear(&ctx, &selection, &mut current_turn).await;
                    pending_messages.clear();
                }
                Command::Reset => {
                    handle_reset(&ctx, &selection, &mut current_turn).await;
                    pending_messages.clear();
                }
                Command::Undo => handle_undo(&ctx, &current_turn).await,
                Command::SelectModel { provider, model } => {
                    handle_select_model(&ctx, &mut selection, provider, model).await
                }
            }
        }
        // The UI dropped its command half: the session is over. Persist
        // queued bang-mode results (they never got a next turn to ride)
        // and flush a soft checkpointed draft that never hit its write
        // window, so a keystroke from a second ago still reaches disk.
        persist_on_exit(&ctx, &selection).await;
        // Drain outstanding memory extractions before the loop task ends.
        crate::knowledge_memory::wait_for_pending(std::time::Duration::from_secs(15)).await;
        // B.11: the UI dropped its command half, so no turn can follow; tear
        // the MCP servers down before the loop task ends (bounded by the
        // manager's own shutdown timeout).
        if let Some(handle) = ctx.workspace.mcp() {
            handle.shutdown().await;
        }
    }
}

/// A message submitted while a turn is running, waiting for that turn to
/// settle before it is sent.
pub(super) type PendingMessage = (
    String,
    crate::run::AgentMode,
    Vec<crate::history::ImageBlock>,
);

/// `Command::SendMessage`: send now, or queue behind a still-running turn
/// (submitting never aborts the in-flight turn). Queued messages are sent
/// in order when the turn settles (`maybe_send_next`).
pub(super) async fn handle_send_message(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
    text: String,
    mode: crate::run::AgentMode,
    images: Vec<crate::history::ImageBlock>,
) {
    if text.trim().is_empty() {
        return;
    }
    if current_turn.as_ref().is_some_and(|h| !h.is_finished()) {
        pending.push_back((text, mode, images));
        return;
    }
    *current_turn = None;
    // Bang-mode results queue until the next turn: pushing them into the
    // history directly would race the running turn's whole-history commit.
    drain_shell_results(&ctx.shell, &ctx.state).await;
    *current_turn = Some(start_turn(ctx, selection, text, mode, images));
}

/// `Command::RunMcpPrompt` (`/server:name`): render the prompt on its
/// server, then send the rendered messages as a normal user turn (queueing
/// behind a running turn like any other submit). Render failures deny with
/// a notice instead of a turn.
async fn handle_mcp_prompt(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
    qualified: String,
    arguments: std::collections::HashMap<String, String>,
    mode: crate::run::AgentMode,
) {
    let Some(handle) = ctx.workspace.mcp() else {
        notice(ctx, Tone::Danger, "no MCP servers are running").await;
        return;
    };
    match handle.get_prompt(&qualified, &arguments).await {
        Ok(messages) => {
            let text = render_prompt_messages(&messages);
            if text.trim().is_empty() {
                notice(
                    ctx,
                    Tone::Warning,
                    format!("prompt {qualified} rendered no text"),
                )
                .await;
                return;
            }
            handle_send_message(
                ctx,
                selection,
                current_turn,
                pending,
                text,
                mode,
                Vec::new(),
            )
            .await;
        }
        Err(err) => {
            notice(
                ctx,
                Tone::Danger,
                format!("prompt {qualified} failed: {err}"),
            )
            .await;
        }
    }
}

/// Flatten rendered prompt messages into one prompt: user messages are the
/// prompt body; other roles keep their text so context the server prepends
/// (e.g. an assistant example) is not silently dropped.
fn render_prompt_messages(messages: &[crate::mcp::session::PromptMessage]) -> String {
    let mut out = Vec::new();
    for message in messages {
        if let Some(text) = message.text.as_deref().filter(|t| !t.trim().is_empty()) {
            if message.role == "user" {
                out.push(text.to_string());
            } else {
                out.push(format!("[{}]\n{text}", message.role));
            }
        }
    }
    out.join("\n\n")
}

async fn notice(ctx: &LoopCtx, tone: Tone, text: impl Into<String>) {
    let _ = ctx.evt_tx.send(AgentEvent::Notice {
        tone,
        text: text.into(),
    });
}

/// Drop a settled turn's handle and, if messages queued behind it, send
/// the next one. No-op while a turn is still running or none is.
pub(super) async fn maybe_send_next(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
) {
    if !current_turn.as_ref().is_some_and(AbortHandle::is_finished) {
        return;
    }
    *current_turn = None;
    if let Some((text, mode, images)) = pending.pop_front() {
        drain_shell_results(&ctx.shell, &ctx.state).await;
        *current_turn = Some(start_turn(ctx, selection, text, mode, images));
    }
}

/// Spawn a turn and arm the wake signal for when it settles.
fn start_turn(
    ctx: &LoopCtx,
    selection: &Selection,
    text: String,
    mode: crate::run::AgentMode,
    images: Vec<crate::history::ImageBlock>,
) -> AbortHandle {
    let handle = tokio::spawn(run_turn(
        TurnCtx {
            config: ctx.config.clone(),
            workspace: ctx.workspace.clone(),
            instructions_text: ctx.instructions_text.clone(),
            selection: selection.clone(),
            state: ctx.state.clone(),
            files: ctx.files.clone(),
            cancel: ctx.cancel_flag.token(),
            subagent_cancels: ctx.subagent_cancels.clone(),
            tx: ctx.evt_tx.clone(),
            permissions: ctx.permissions.clone(),
            mode,
        },
        text,
        images,
    ));
    let abort = handle.abort_handle();
    let wake = Arc::clone(&ctx.wake);
    tokio::spawn(async move {
        let _ = handle.await;
        wake.notify_one();
    });
    abort
}

/// `Command::Shell`: spawn a bang-mode shell run under a per-run child
/// token (a parent interrupt cancels it); its trigger is registered in
/// the session's `ShellState` and visible-run results queue there for
/// the next turn.
fn handle_shell(ctx: &LoopCtx, command: String, visible: bool) {
    let (id, cancel) = {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        let id = shell.reserve_id();
        // Child of the interrupt flag: Esc stops this run without
        // touching anything else, and the registered trigger lets
        // `cancel_all` sweep every in-flight run at teardown.
        let (trigger, cancel) = ctx.cancel_flag.token().child();
        shell.add_trigger(&id, trigger);
        (id, cancel)
    };
    let tx = ctx.evt_tx.clone();
    let shell = Arc::clone(&ctx.shell);
    tokio::spawn(crate::tui::shell::run_shell(
        id, command, visible, tx, cancel, shell,
    ));
}

/// `Command::ArgosyMemory` (Phase 3): search the project's argosy memory
/// concepts off-loop and render the hits as a notice. Errors degrade to a
/// warning notice, never a failed turn.
async fn handle_argosy_memory(ctx: &LoopCtx, query: String) {
    let filtered = !query.trim().is_empty();
    let tx = ctx.evt_tx.clone();
    let cwd = std::path::PathBuf::from(&ctx.cwd);
    let hit = tokio::task::spawn_blocking(move || {
        let params = serde_json::json!({
            "query": if query.trim().is_empty() {
                "project memories and learnings"
            } else {
                query.trim()
            },
            "namespaces": ["memory"],
            "cwd": cwd.display().to_string(),
        });
        crate::knowledge::ArgosyService::global()
            .execute("search", params)
            .map_err(|e| e.to_string())
    })
    .await;
    let report = match hit {
        Ok(Ok(report)) => report,
        Ok(Err(err)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("/memory: {err}"),
            });
            return;
        }
        Err(err) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("/memory: search task failed: {err}"),
            });
            return;
        }
    };
    let text = format_memory_hits(&report, filtered);
    let _ = tx.send(AgentEvent::Notice {
        tone: Tone::Neutral,
        text,
    });
}

/// Render a search report's hits as a compact notice listing.
fn format_memory_hits(report: &serde_json::Value, filtered: bool) -> String {
    let hits = report
        .get("hits")
        .and_then(|h| h.as_array())
        .cloned()
        .unwrap_or_default();
    if hits.is_empty() {
        return "no memory concepts matched".to_string();
    }
    let mut lines = Vec::new();
    for hit in hits.iter().take(12) {
        let uri = hit.get("uri").and_then(|u| u.as_str()).unwrap_or("?");
        let score = hit.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0);
        lines.push(format!("- {uri} ({score:.2})"));
    }
    let header = if filtered {
        format!("memory concepts ({}):", hits.len())
    } else {
        format!("memory concepts ({}):", hits.len())
    };
    format!("{header}\n{}", lines.join("\n"))
}

/// `Command::FetchUsage`: mark loading, then resolve off-loop so the UI
/// keeps ticking. Each fetch carries a generation; answers from an older
/// generation than the latest request are dropped so a slow stale fetch
/// can't overwrite a newer answer.
async fn handle_fetch_usage(
    ctx: &LoopCtx,
    selection: &Selection,
    usage_gen: &Arc<std::sync::atomic::AtomicU64>,
) {
    let config = match ctx.config.providers.get(&selection.provider) {
        Some(config) => Some(config.clone()),
        None => {
            let _ = ctx
                .evt_tx
                .send(AgentEvent::UsageQuota(UsageFetchState::Error(format!(
                    "provider {:?} not found in config",
                    selection.provider
                ))));
            None
        }
    };
    let fetch_gen = usage_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let tx = ctx.evt_tx.clone();
    let latest = Arc::clone(usage_gen);
    let _ = ctx
        .evt_tx
        .send(AgentEvent::UsageQuota(UsageFetchState::Loading));
    let Some(config) = config else {
        // Error already emitted above; no fetch to run.
        return;
    };
    tokio::spawn(async move {
        let state = match crate::providers::usage_fetch::fetch_usage(&config).await {
            Ok(Some(usage)) => UsageFetchState::Ready(usage),
            Ok(None) => UsageFetchState::Unsupported,
            Err(error) => UsageFetchState::Error(report(error)),
        };
        if latest.load(std::sync::atomic::Ordering::SeqCst) != fetch_gen {
            return; // superseded by a newer fetch
        }
        let _ = tx.send(AgentEvent::UsageQuota(state));
    });
}

/// `Command::Compact`: refuse while a turn is running, then compact now.
async fn handle_compact(ctx: &LoopCtx, selection: &Selection, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before compacting.".into(),
        });
        return;
    }
    turn::compact_now(&ctx.config, selection, &ctx.state, &ctx.evt_tx).await;
}

/// Signal cancellation and abort any in-flight turn. Callers then differ
/// only in how much session state they rebuild.
fn interrupt(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>) {
    ctx.cancel_flag.set(true);
    if let Some(h) = current_turn.take() {
        h.abort();
    }
}

/// `Command::Interrupt`: signal cancellation and end the assistant bubble.
/// The turn is left to settle on its own rather than hard-aborted: the run
/// layer already keeps the partial history on cancel (`commit_cancelled`),
/// and aborting the task would drop the user message and streamed reply
/// from the session the next turn reads.
/// Grace period after an Esc before a still-running turn is force-aborted
/// (an await that never checks the cancel flag).
const INTERRUPT_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// `Command::Interrupt`: signal cancellation and end the assistant bubble.
/// The turn is left to settle on its own rather than hard-aborted: the run
/// layer already keeps the partial history on cancel (`commit_cancelled`),
/// and aborting the task would drop the user message and streamed reply
/// from the session the next turn reads.
fn handle_interrupt(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    ctx.cancel_flag.set(true);
    // Abort fallback: if the in-flight turn doesn't settle within the
    // grace window, some await isn't cancellation-aware — force it.
    if let Some(handle) = current_turn.clone() {
        tokio::spawn(async move {
            tokio::time::sleep(INTERRUPT_ABORT_GRACE).await;
            if !handle.is_finished() {
                handle.abort();
            }
        });
    }
    let _ = ctx.evt_tx.send(AgentEvent::AssistantEnd);
    let _ = ctx.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
}

/// `Command::Clear`: interrupt, drop queued shell results, and start a
/// fresh session (keeping the same caches via `linked`).
pub(super) async fn handle_clear(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
) {
    reset_session(ctx, selection, current_turn, false).await;
}

/// `Command::Reset`: like `Clear`, plus dropped files and reset chrome.
pub(super) async fn handle_reset(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
) {
    reset_session(ctx, selection, current_turn, true).await;
}

/// Shared teardown for `Clear` and `Reset`: interrupt any running turn, drop
/// queued shell results, swap in a fresh linked session (clearing the todo
/// plan with it), and report an idle session with its context counter zeroed.
/// `clear_files` additionally drops the tracked file set and clears its
/// chrome (`Reset`).
async fn reset_session(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
    clear_files: bool,
) {
    interrupt(ctx, current_turn);
    {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.clear_results();
        shell.cancel_all();
    }
    // Reset through `linked` so the fresh session's compaction state keeps
    // working dedup/guardrails handles; a bare default would strand the
    // caches the dispatcher still points at.
    let thinking = if clear_files {
        ctx.config
            .always_thinking
            .or(ctx.config.agent.thinking)
            .unwrap_or_default()
    } else {
        ctx.state.lock().await.thinking
    };
    let thinking = ctx
        .config
        .providers
        .get(&selection.provider)
        .map(|p| crate::thinking::reconcile_for(thinking, p, &selection.model))
        .unwrap_or(thinking);
    *ctx.state.lock().await = SessionState::linked().with_thinking(thinking).with_store(
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
    );
    let _ = ctx.evt_tx.send(AgentEvent::ThinkingChanged(thinking));
    if clear_files {
        ctx.files.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    let _ = ctx.evt_tx.send(AgentEvent::AssistantEnd);
    if clear_files {
        let _ = ctx.evt_tx.send(AgentEvent::FilesSet(Vec::new()));
    }
    // The fresh session inherits no todo plan: drop the workspace's todo
    // store and the sidebar checklist the previous session left behind.
    ctx.workspace.clear_todos();
    let _ = ctx.evt_tx.send(AgentEvent::PlanSet(Vec::new()));
    // Zero the composer's context counter: the fresh session holds no
    // context, so a stale label would misreport usage until the next turn.
    let _ = ctx.evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));
    let _ = ctx.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
}

pub(super) async fn handle_set_thinking(
    ctx: &LoopCtx,
    selection: &Selection,
    thinking: crate::thinking::ThinkingConfig,
) {
    let effective = ctx
        .config
        .providers
        .get(&selection.provider)
        .map(|p| crate::thinking::reconcile_for(thinking, p, &selection.model))
        .unwrap_or(thinking);
    if thinking != effective {
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: format!(
                "{} requires thinking setting {effective}; requested {thinking}",
                selection.model
            ),
        });
    }
    let mut guard = ctx.state.lock().await;
    guard.thinking = effective;
    if let Some(store) = &mut guard.store {
        store.set_thinking(effective);
    }
    let _ = ctx.evt_tx.send(AgentEvent::ThinkingChanged(effective));
}

/// `Command::Undo`: refuse while a turn is running, then roll the
/// workspace snapshots back one step.
async fn handle_undo(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        // Restoring mid-run would race the turn's writes and drain its
        // live capture session.
        let _ = ctx.evt_tx.send(AgentEvent::AssistantText(
            "A turn is still running; wait for it to finish before undoing.".into(),
        ));
        return;
    }
    let message = ctx
        .snapshots
        .rollback()
        .await
        .unwrap_or_else(|| "Nothing to undo.".into());
    let _ = ctx.evt_tx.send(AgentEvent::AssistantText(message));
}

/// `Command::SelectModel`: switch the selection when the provider/model
/// pair exists in the catalogs, remember it in the session so the next
/// launch reuses it, then re-announce the menu.
pub(super) async fn handle_select_model(
    ctx: &LoopCtx,
    selection: &mut Selection,
    provider: String,
    model: String,
) {
    let found = ctx
        .catalogs
        .get(&provider)
        .and_then(|models| models.iter().find(|m| m.id == model))
        .map(|m| m.context_length);
    match found {
        Some(context_length) => {
            *selection = Selection {
                provider,
                model,
                context_length,
            };
            let mut guard = ctx.state.lock().await;
            if let Some(store) = &mut guard.store {
                store.set_model(LoopCtx::model_spec(selection));
            }
            drop(guard);
            let (models, current) = ctx.catalog_choices(selection);
            let _ = ctx.evt_tx.send(AgentEvent::CatalogSet { models, current });
            let thinking = ctx.state.lock().await.thinking;
            handle_set_thinking(ctx, selection, thinking).await;
        }
        None => {
            let _ = ctx.evt_tx.send(AgentEvent::AssistantText(format!(
                "Unknown model {model:?} on provider {provider:?}."
            )));
        }
    }
}

/// Move queued bang-mode visible-run results into the session history.
/// Called at the next `SendMessage`: pushing them when they finish would
/// race the running turn's whole-history commit (turn.rs replaces
/// `session.history` only on success). Returns how many landed.
pub(super) async fn drain_shell_results(
    shell: &Arc<std::sync::Mutex<crate::tui::shell::ShellState>>,
    state: &Arc<Mutex<SessionState>>,
) -> usize {
    let queued = shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain_results();
    let drained = queued.len();
    if drained > 0 {
        state.lock().await.history.extend(queued);
    }
    drained
}

impl Provider for CraftProvider {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        // Forward MCP log notices into the same stream the TUI already reads.
        let mut this = self;
        let mut mcp_evt_rx = std::mem::replace(&mut this.mcp_evt_rx, mpsc::unbounded_channel().1);
        let forward_tx = evt_tx.clone();
        tokio::spawn(async move {
            while let Some(event) = mcp_evt_rx.recv().await {
                if forward_tx.send(event).is_err() {
                    break;
                }
            }
        });

        tokio::spawn(this.spawn_command_loop(cmd_rx, evt_tx));

        (cmd_tx, evt_rx)
    }

    fn mcp(&self) -> Option<crate::mcp::McpHandle> {
        self.mcp.clone()
    }
}
