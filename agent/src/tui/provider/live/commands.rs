//! The session command loop: dispatches [`Command`]s, drives turns and
//! approvals, and streams [`AgentEvent`]s back to the UI.

use std::collections::VecDeque;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::AbortHandle;

use crate::permissions::PermissionAnswer;
use crate::harness;

use super::approval::decide;
use super::bootstrap::{install_session, open_session};
use super::mcp_request;
use super::question::answer_question;
use super::resume::{
    handle_load_session, handle_resume_latest, handle_set_draft, handle_set_mode, persist_on_exit,
};
use super::turn::{TurnCtx, run_turn};
use super::{AgentEvent, Command, Provider, Selection, Status, Tone, UsageFetchState};
use super::{CraftProvider, LoopCtx, ParkSlots, SessionKit, SessionState, report};

impl CraftProvider {
    /// The session's command loop: receives [`Command`]s, drives turns and
    /// approvals, and streams [`AgentEvent`]s back. Spawned by `start`.
    async fn spawn_command_loop(
        self,
        mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
        evt_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    ) {
        let cwd = self.workspace.root().display().to_string();
        let state_dir = crate::storage::StateDir::resolve().ok();
        // Seed the fresh session with the level the user last chose, the way
        // the model selection reuses the newest session's header. An explicit
        // `always_thinking` (config or `--thinking`) still wins, then the
        // agent default.
        let last_thinking = state_dir.as_ref().and_then(|dir| {
            crate::storage::sessions::latest_thinking(&cwd, dir)
                .ok()
                .flatten()
        });
        let seeded_thinking = self
            .config
            .always_thinking
            .or(last_thinking)
            .or(self.config.agent.thinking)
            .unwrap_or_default();
        let kit = SessionKit {
            config: self.config.clone(),
            workspace: self.workspace.clone(),
            instructions_text: self.instructions.text.clone(),
            permissions: self.permissions.clone(),
            cancel_cell: Arc::new(ArcSwap::from_pointee(
                animist::agent::Cancellation::default(),
            )),
            subagent_cancels: Arc::new(harness::cancel::CancelMap::new()),
            park: Arc::new(Mutex::new(ParkSlots::default())),
            evt_tx: evt_tx.clone(),
            files: super::cards::Files::default(),
            selection: Arc::new(ArcSwap::from_pointee(self.selection.clone())),
            cwd: cwd.clone(),
            state_dir: state_dir.clone(),
        };
        // The first session build is the one thing that can make the loop
        // unusable: bootstrap validated the catalogs, so a failure here
        // means the environment broke between the two.
        let state = match open_session(&kit, seeded_thinking, None).await {
            Ok(state) => state,
            Err(error) => {
                let _ = evt_tx.send(AgentEvent::AssistantText(error));
                let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Failed));
                return;
            }
        };
        // Bang-mode bookkeeping lives in `ctx.shell` (ShellState).
        // Quota-fetch generation: shared with the spawned fetches so a slow
        // stale answer never overwrites a newer one (rapid Ctrl+R).
        let usage_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let shell: Arc<std::sync::Mutex<crate::tui::shell::ShellState>> = Arc::default();
        // Signals the loop when a running turn finishes (see LoopCtx).
        let wake = Arc::new(Notify::new());
        let ctx = LoopCtx {
            state,
            kit,
            shell,
            catalogs: self.catalogs,
            catalog: self.catalog.clone(),
            wake: wake.clone(),
        };
        // Messages submitted while a turn is still running queue here and
        // are sent, in order, when that turn settles (never aborting it).
        let mut pending_messages: VecDeque<PendingMessage> = VecDeque::new();
        let mut current_turn: Option<AbortHandle> = None;
        // Server-initiated MCP requests (elicitation) arrive on
        // their own channel; each is answered on its own task so a parked
        // form never blocks the command loop.
        let mut server_requests = ctx
            .kit
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
        let selection = ctx.kit.selection.load_full();
        let (models, current) = ctx.catalog_choices(&selection);
        let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
        let _ = evt_tx.send(AgentEvent::ThinkingChanged(ctx.state.lock().await.thinking));
        let _ = evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));

        if let Some(id) = &self.resume_session {
            handle_load_session(&ctx, &current_turn, id.clone()).await;
        } else if self.resume_latest {
            handle_resume_latest(&ctx, &current_turn).await;
        }
        // The settle signal lands after the resume block (not before it, as
        // the chrome events do): the TUI holds a startup prompt back until
        // the first `Done`, so on a successful resume it must arrive after
        // `SessionLoaded` (and the mode the session restores). Every path —
        // fresh session, resume success, resume failure — ends with it.
        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));

        let thinking = ctx.state.lock().await.thinking;
        handle_set_thinking(&ctx, thinking).await;

        loop {
            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
                _ = wake.notified() => {
                    // A turn settled: drop a finished handle and send the
                    // next queued message, if any.
                    maybe_send_next(&ctx, &mut current_turn, &mut pending_messages).await;
                    continue;
                }
                req = async { server_requests.as_mut().expect("polled only when present").recv().await },
                    if server_requests.is_some() => {
                    match req {
                        Some(request) => {
                            mcp_request::spawn_server_request(
                                mcp_request::ServerRequestCtx {
                                    park: ctx.kit.park.clone(),
                                    evt_tx: ctx.kit.evt_tx.clone(),
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
            maybe_send_next(&ctx, &mut current_turn, &mut pending_messages).await;
            match cmd {
                Command::SetThinking(thinking) => handle_set_thinking(&ctx, thinking).await,
                Command::SendMessage(text, mode, images) => {
                    handle_send_message(
                        &ctx,
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
                    decide(&ctx.kit.park, id, answer).await
                }
                Command::Reject { id, always } => {
                    let answer = if always {
                        PermissionAnswer::DenyAlwaysLocal
                    } else {
                        PermissionAnswer::Deny
                    };
                    decide(&ctx.kit.park, id, answer).await
                }
                Command::AnswerPermission { id, answer } => {
                    decide(&ctx.kit.park, id, answer).await;
                }
                Command::AnswerQuestion { id, answer } => {
                    answer_question(&ctx.kit.park, id, answer).await;
                }
                Command::ToggleAutoReview => {
                    let on = ctx.kit.permissions.toggle_auto_review();
                    let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantText(format!(
                        "auto-review {}.",
                        if on { "on" } else { "off" }
                    )));
                }
                Command::ArgosyMemory { query } => handle_argosy_memory(&ctx, query).await,
                Command::GetUsage => {
                    let rows = ctx.state.lock().await.usage.rows();
                    let _ = ctx.kit.evt_tx.send(AgentEvent::UsageSnapshot(rows));
                }
                Command::FetchUsage => handle_fetch_usage(&ctx, &usage_gen).await,
                Command::Compact => handle_compact(&ctx, &current_turn).await,
                Command::LoadSession { id } => handle_load_session(&ctx, &current_turn, id).await,
                Command::ResumeLatest => handle_resume_latest(&ctx, &current_turn).await,
                Command::SetDraft(draft) => handle_set_draft(&ctx, draft).await,
                Command::SetMode { plan } => handle_set_mode(&ctx, plan).await,
                Command::Interrupt => {
                    handle_interrupt(&ctx, &current_turn);
                    pending_messages.clear();
                }
                Command::CancelSubagent { tool_use_id } => {
                    // Marks the id so children spawned later under it die
                    // too; the parent turn keeps running.
                    ctx.kit.subagent_cancels.cancel_or_precancel(tool_use_id);
                }
                Command::Clear => {
                    handle_clear(&ctx, &mut current_turn).await;
                    pending_messages.clear();
                }
                Command::Reset => {
                    handle_reset(&ctx, &mut current_turn).await;
                    pending_messages.clear();
                }
                Command::Undo => handle_undo(&ctx, &current_turn).await,
                Command::SelectModel { provider, model } => {
                    handle_select_model(&ctx, provider, model).await
                }
            }
        }
        // The UI dropped its command half: the session is over. Persist
        // queued bang-mode results (they never got a next turn to ride)
        // and flush a soft checkpointed draft that never hit its write
        // window, so a keystroke from a second ago still reaches disk.
        persist_on_exit(&ctx).await;
        // Release the conversation's resources before the loop task ends
        // (async cleanup cannot run in `Drop`).
        let _ = ctx.state.lock().await.conversation.shutdown().await;
        // Drain outstanding memory extractions before the loop task ends.
        crate::knowledge_memory::wait_for_pending(std::time::Duration::from_secs(15)).await;
        // The UI dropped its command half, so no turn can follow; tear
        // the MCP servers down before the loop task ends (bounded by the
        // manager's own shutdown timeout).
        if let Some(handle) = ctx.kit.workspace.mcp() {
            handle.shutdown().await;
        }
    }
}

/// A message submitted while a turn is running, waiting for that turn to
/// settle before it is sent.
pub(super) type PendingMessage = (
    String,
    crate::harness::AgentMode,
    Vec<crate::history::ContentBlock>,
);

/// `Command::SendMessage`: send now, or queue behind a still-running turn
/// (submitting never aborts the in-flight turn). Queued messages are sent
/// in order when the turn settles (`maybe_send_next`).
pub(super) async fn handle_send_message(
    ctx: &LoopCtx,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
    text: String,
    mode: crate::harness::AgentMode,
    images: Vec<crate::history::ContentBlock>,
) {
    if text.trim().is_empty() {
        return;
    }
    if current_turn.as_ref().is_some_and(|h| !h.is_finished()) {
        pending.push_back((text, mode, images));
        return;
    }
    *current_turn = None;
    // Bang-mode results queue until the next turn: committing them into
    // the conversation here keeps them clear of the running turn's commits.
    drain_shell_results(&ctx.shell, &ctx.state).await;
    *current_turn = Some(start_turn(ctx, text, mode, images));
}

/// `Command::RunMcpPrompt` (`/server:name`): render the prompt on its
/// server, then send the rendered messages as a normal user turn (queueing
/// behind a running turn like any other submit). Render failures deny with
/// a notice instead of a turn.
async fn handle_mcp_prompt(
    ctx: &LoopCtx,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
    qualified: String,
    arguments: std::collections::HashMap<String, String>,
    mode: crate::harness::AgentMode,
) {
    let Some(handle) = ctx.kit.workspace.mcp() else {
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
            handle_send_message(ctx, current_turn, pending, text, mode, Vec::new()).await;
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
    let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
        tone,
        text: text.into(),
    });
}

/// Drop a settled turn's handle and, if messages queued behind it, send
/// the next one. No-op while a turn is still running or none is.
pub(super) async fn maybe_send_next(
    ctx: &LoopCtx,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
) {
    if !current_turn.as_ref().is_some_and(AbortHandle::is_finished) {
        return;
    }
    *current_turn = None;
    if let Some((text, mode, images)) = pending.pop_front() {
        drain_shell_results(&ctx.shell, &ctx.state).await;
        *current_turn = Some(start_turn(ctx, text, mode, images));
    }
}

/// Spawn a turn and arm the wake signal for when it settles.
fn start_turn(
    ctx: &LoopCtx,
    text: String,
    mode: crate::harness::AgentMode,
    images: Vec<crate::history::ContentBlock>,
) -> AbortHandle {
    let handle = tokio::spawn(run_turn(
        TurnCtx {
            kit: ctx.kit.clone(),
            state: ctx.state.clone(),
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

/// `Command::Shell`: spawn a bang-mode shell run under its own cancel
/// pair; its trigger is registered in the session's `ShellState` (Esc
/// fires it through `cancel_all`) and visible-run results queue there for
/// the next turn.
fn handle_shell(ctx: &LoopCtx, command: String, visible: bool) {
    let (id, cancel) = {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        let id = shell.reserve_id();
        let (trigger, cancel) = harness::cancel::cancel_pair();
        shell.add_trigger(&id, trigger);
        (id, cancel)
    };
    let tx = ctx.kit.evt_tx.clone();
    let shell = Arc::clone(&ctx.shell);
    tokio::spawn(crate::tui::shell::run_shell(
        id, command, visible, tx, cancel, shell,
    ));
}

/// `Command::ArgosyMemory`: search the project's argosy memory
/// concepts off-loop and render the hits as a notice. Errors degrade to a
/// warning notice, never a failed turn.
async fn handle_argosy_memory(ctx: &LoopCtx, query: String) {
    let filtered = !query.trim().is_empty();
    let tx = ctx.kit.evt_tx.clone();
    let cwd = std::path::PathBuf::from(&ctx.kit.cwd);
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
fn format_memory_hits(report: &serde_json::Value, _filtered: bool) -> String {
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
    format!("memory concepts ({}):\n{}", hits.len(), lines.join("\n"))
}

/// `Command::FetchUsage`: mark loading, then resolve off-loop so the UI
/// keeps ticking. Each fetch carries a generation; answers from an older
/// generation than the latest request are dropped so a slow stale fetch
/// can't overwrite a newer answer.
async fn handle_fetch_usage(ctx: &LoopCtx, usage_gen: &Arc<std::sync::atomic::AtomicU64>) {
    let selection = ctx.kit.selection.load_full();
    let config = match ctx.kit.config.providers.get(&selection.provider) {
        Some(config) => Some(config.clone()),
        None => {
            let _ = ctx
                .kit
                .evt_tx
                .send(AgentEvent::UsageQuota(UsageFetchState::Error(format!(
                    "provider {:?} not found in config",
                    selection.provider
                ))));
            None
        }
    };
    let fetch_gen = usage_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let tx = ctx.kit.evt_tx.clone();
    let latest = Arc::clone(usage_gen);
    let _ = ctx
        .kit
        .evt_tx
        .send(AgentEvent::UsageQuota(UsageFetchState::Loading));
    let Some(config) = config else {
        // Error already emitted above; no fetch to run.
        return;
    };
    tokio::spawn(async move {
        let state = match crate::usage_fetch::fetch_usage(&config).await {
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

/// `Command::Compact`: refuse while a turn is running, then force the
/// between-runs history edits (alteration passes, then the forced
/// compaction cascade) through the same driver function the turn loop uses.
async fn handle_compact(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before compacting.".into(),
        });
        return;
    }
    let conversation = ctx.state.lock().await.conversation.clone();
    let ran = crate::harness::compact_between_runs(&conversation, true).await;
    // Land the rewrite on disk now (the driver orders its between-runs
    // edits before its flush; a quit right after /compact must not lose
    // them). Persistence errors surface through the session stream.
    conversation.flush().await;
    // Compaction events (AutoCompacting/CompactionDone) ride the session's
    // own stream; the command answers with a terminal verdict either way.
    let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
        tone: if ran { Tone::Success } else { Tone::Info },
        text: if ran {
            "context compacted.".to_owned()
        } else {
            "nothing to compact: the context is already tight.".to_owned()
        },
    });
}

/// Signal cancellation — the running turn through the per-turn cell, and
/// every in-flight bang run through its registered trigger — and abort any
/// in-flight turn. Callers then differ only in how much session state they
/// rebuild.
fn interrupt(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>) {
    ctx.kit.cancel_cell.load().cancel();
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancel_all();
    if let Some(h) = current_turn.take() {
        h.abort();
    }
}

/// Grace period after an Esc before a still-running turn is force-aborted
/// (an await that never checks cancellation).
const INTERRUPT_ABORT_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// `Command::Interrupt`: signal cancellation and end the assistant bubble.
/// The turn settles on its own: the driver commits the partial history on
/// cancel, so the settle path cannot drop the turn. The abort fallback
/// below fires only on an await that never observes cancellation — and
/// even then [`close_aborted_turn`] marks the interrupted conversation
/// instead of losing it.
pub(super) fn handle_interrupt(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    ctx.kit.cancel_cell.load().cancel();
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancel_all();
    // Abort fallback: if the in-flight turn doesn't settle within the
    // grace window, some await isn't cancellation-aware — force it, then
    // close the conversation with the cancel marker a cooperative cancel
    // would have added.
    if let Some(handle) = current_turn.clone() {
        let state = Arc::clone(&ctx.state);
        tokio::spawn(async move {
            tokio::time::sleep(INTERRUPT_ABORT_GRACE).await;
            if !handle.is_finished() {
                handle.abort();
                close_aborted_turn(&state).await;
            }
        });
    }
    let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantEnd);
    let _ = ctx.kit.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
}

/// Close a turn killed by the interrupt abort fallback. The aborted task
/// never reaches the driver's cancel path, so unless the conversation
/// already ends with one, append the cancel marker the next turn reads.
pub(super) async fn close_aborted_turn(state: &Arc<Mutex<SessionState>>) {
    let conversation = state.lock().await.conversation.clone();
    let history = conversation.history();
    let already_closed = history
        .snapshot()
        .await
        .entries()
        .last()
        .and_then(animist::history::HistoryEntry::current)
        .is_some_and(|message| crate::history::marker::is(message, crate::history::marker::CANCEL));
    if !already_closed {
        history
            .add_stored(crate::history::marker::observation(
                crate::history::marker::CANCEL,
                crate::harness::driver::CANCEL_MARKER,
            ))
            .await;
    }
    // The aborted run never reached `turn_end`: flush the change feed the
    // persist layer would have checkpointed, and close the snapshot
    // capture session onto the `/undo` stack the same way a settled
    // cancelled turn does.
    conversation.flush().await;
    conversation.snapshots().commit();
}

/// `Command::Clear`: interrupt, drop queued shell results, and start a
/// fresh session.
pub(super) async fn handle_clear(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>) {
    reset_session(ctx, current_turn, false).await;
}

/// `Command::Reset`: like `Clear`, plus dropped files and reset chrome.
pub(super) async fn handle_reset(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>) {
    reset_session(ctx, current_turn, true).await;
}

/// Shared teardown for `Clear` and `Reset`: interrupt any running turn, drop
/// queued shell results, swap in a fresh session (clearing the todo plan
/// with it), and report an idle session with its context counter zeroed.
/// `clear_files` additionally drops the tracked file set and clears its
/// chrome (`Reset`). Both carry the user's last thinking level into the
/// fresh session, as `SelectModel` carries the model: a new session must
/// not silently drop the reasoning effort the user chose.
async fn reset_session(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>, clear_files: bool) {
    interrupt(ctx, current_turn);
    {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.clear_results();
    }
    let thinking = ctx.state.lock().await.thinking;
    let thinking = {
        let selection = ctx.kit.selection.load_full();
        let info = ctx
            .catalogs
            .get(&selection.provider)
            .and_then(|models| {
                models
                    .iter()
                    .find(|info| info.ids.first().is_some_and(|id| id == &selection.model))
            })
            .cloned();
        crate::thinking::reconcile_for(thinking, info.as_ref())
    };
    // A failed rebuild keeps the current session (and says so): a fresh
    // unusable one would strand the loop.
    if let Err(error) = install_session(&ctx.kit, &ctx.state, thinking, None).await {
        notice(
            ctx,
            Tone::Danger,
            format!("could not start a fresh session: {error}"),
        )
        .await;
        return;
    }
    let _ = ctx.kit.evt_tx.send(AgentEvent::ThinkingChanged(thinking));
    if clear_files {
        ctx.kit
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
    let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantEnd);
    if clear_files {
        let _ = ctx.kit.evt_tx.send(AgentEvent::FilesSet(Vec::new()));
    }
    // The fresh session inherits no todo plan: drop the workspace's todo
    // store and the sidebar checklist the previous session left behind.
    ctx.kit.workspace.clear_todos();
    let _ = ctx.kit.evt_tx.send(AgentEvent::PlanSet(Vec::new()));
    // Zero the composer's context counter: the fresh session holds no
    // context, so a stale label would misreport usage until the next turn.
    let _ = ctx.kit.evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));
    let _ = ctx.kit.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
}

pub(super) async fn handle_set_thinking(ctx: &LoopCtx, thinking: crate::thinking::ThinkingConfig) {
    let selection = ctx.kit.selection.load_full();
    let info = ctx
        .catalogs
        .get(&selection.provider)
        .and_then(|models| {
            models
                .iter()
                .find(|info| info.ids.first().is_some_and(|id| id == &selection.model))
        })
        .cloned();
    let effective = crate::thinking::reconcile_for(thinking, info.as_ref());
    if thinking != effective {
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: format!(
                "{} requires thinking setting {effective}; requested {thinking}",
                selection.model
            ),
        });
    }
    let conversation = {
        let mut guard = ctx.state.lock().await;
        guard.thinking = effective;
        guard.conversation.clone()
    };
    conversation.update_settings(|settings| settings.thinking = effective);
    if let Some(persister) = conversation.persister() {
        persister.lock().await.store_mut().set_thinking(effective);
    }
    let _ = ctx.kit.evt_tx.send(AgentEvent::ThinkingChanged(effective));
}

/// `Command::Undo`: refuse while a turn is running, then roll the
/// workspace snapshots back one step.
async fn handle_undo(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        // Restoring mid-run would race the turn's writes and drain its
        // live capture session.
        let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantText(
            "A turn is still running; wait for it to finish before undoing.".into(),
        ));
        return;
    }
    let snapshots = ctx.state.lock().await.conversation.snapshots();
    let message = snapshots
        .rollback()
        .await
        .unwrap_or_else(|| "Nothing to undo.".into());
    let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantText(message));
}

/// `Command::SelectModel`: switch the selection when the provider/model
/// pair exists in the catalogs, remember it in the session so the next
/// launch reuses it, then re-announce the menu.
pub(super) async fn handle_select_model(ctx: &LoopCtx, provider: String, model: String) {
    let found = ctx
        .catalogs
        .get(&provider)
        .and_then(|models| {
            models
                .iter()
                .find(|info| info.ids.first().is_some_and(|id| id.as_str() == model))
        })
        .and_then(|info| info.config.context_window);
    match found {
        Some(context_length) => {
            let selection = Selection {
                provider,
                model,
                context_length: Some(context_length),
            };
            ctx.kit.selection.store(Arc::new(selection.clone()));
            let conversation = ctx.state.lock().await.conversation.clone();
            if let Some(persister) = conversation.persister() {
                persister
                    .lock()
                    .await
                    .store_mut()
                    .set_model(LoopCtx::model_spec(&selection));
            }
            let (models, current) = ctx.catalog_choices(&selection);
            let _ = ctx
                .kit
                .evt_tx
                .send(AgentEvent::CatalogSet { models, current });
            let thinking = ctx.state.lock().await.thinking;
            handle_set_thinking(ctx, thinking).await;
        }
        None => {
            let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantText(format!(
                "Unknown model {model:?} on provider {provider:?}."
            )));
        }
    }
}

/// Commit queued bang-mode visible-run results into the conversation's
/// history. Called at the next `SendMessage`: committing them when they
/// finish would race a running turn's commits; the history's atomic
/// appends keep them ordered here.
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
        let history = state.lock().await.conversation.history();
        for message in queued {
            history.add_stored(message).await;
        }
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

    fn custom_commands(&self) -> bool {
        self.custom_commands
    }

    fn starts_in_plan_mode(&self) -> bool {
        self.starts_in_plan_mode
    }

    fn expects_startup_resume(&self) -> bool {
        self.resume_session.is_some() || self.resume_latest
    }
}
