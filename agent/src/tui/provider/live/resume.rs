//! Session load/resume and exit-time persistence: swap a persisted session
//! in (`/sessions`, F.3 resume-latest-by-cwd), rebuild the transcript, and
//! flush queued results and drafts when the command loop ends.

use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio::task::AbortHandle;

use super::cards::Files;
use super::commands::drain_shell_results;
use super::usage_recorder::UsageLedger;
use super::{AgentEvent, LoadedMessage, Status, Tone};
use super::{LoopCtx, Selection, SessionState};
use crate::tui::provider::SessionMode;

/// `/sessions` load: replace the session with a persisted one and push the
/// rebuilt transcript plus fresh chrome back to the UI. Load failures only
/// warn; the current session stays put.
pub(super) async fn load_session(
    state: &Arc<Mutex<SessionState>>,
    files: &Files,
    id: &str,
    dir: Option<&crate::storage::StateDir>,
    cwd: &str,
    model_spec: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let Some(dir) = dir else {
        let _ = tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "session storage is unavailable".into(),
        });
        return;
    };
    let craft_id = match id.parse::<crate::id::CraftId>() {
        Ok(id) => id,
        Err(_) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("unknown session id {id:?}"),
            });
            return;
        }
    };
    // Session logs can be large; read off the async worker.
    let read_dir = dir.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        crate::headless::StoredSession::load(craft_id, &read_dir)
    })
    .await;
    let loaded = match loaded {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not load session: {e}"),
            });
            return;
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not load session: {e}"),
            });
            return;
        }
    };
    let title = loaded.title.clone();
    let draft = loaded.meta.input_draft.clone().unwrap_or_default();
    // Stored mode wins on resume; anything the file does not spell out
    // (legacy sessions) keeps the CLI-seeded mode.
    let mode = match loaded.meta.mode.as_deref() {
        Some("plan") => Some(SessionMode::Plan),
        Some("build") => Some(SessionMode::Build),
        _ => None,
    };
    let messages = loaded.messages().to_vec();
    let rendered = transcript(&messages);
    {
        // Swap history + persistence in one critical section. Compaction/
        // dedup/guardrail handles are session-lifetime and stay valid: the
        // loaded history is what the next turn's engine sees. The store
        // binds the loaded id so future turns resume the same session file
        // and its cost records join its ledger id.
        let mut guard = state.lock().await;
        guard.history = messages;
        guard.usage = UsageLedger::open().with_session_id(loaded.id.id().to_string());
        guard.store =
            crate::headless::SessionStore::open_in(dir.clone(), loaded.id.clone(), cwd, model_spec)
                .ok();
        guard.thinking = guard
            .store
            .as_ref()
            .and_then(|store| store.thinking())
            .unwrap_or(guard.thinking);
        let _ = tx.send(AgentEvent::ThinkingChanged(guard.thinking));
    }
    files.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let _ = tx.send(AgentEvent::FilesSet(Vec::new()));
    let _ = tx.send(AgentEvent::AssistantEnd);
    let _ = tx.send(AgentEvent::SessionLoaded {
        messages: rendered,
        draft,
        mode,
    });
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
    let _ = tx.send(AgentEvent::Notice {
        tone: Tone::Success,
        text: format!("resumed session \"{title}\""),
    });
}

/// F.3 resume-latest-by-cwd: load the newest session recorded for this
/// directory (cwd index first, disk scan fallback), reusing the `/sessions`
/// load path. Nothing found keeps the fresh session.
pub(super) async fn resume_latest(
    state: &Arc<Mutex<SessionState>>,
    files: &Files,
    dir: Option<&crate::storage::StateDir>,
    cwd: &str,
    model_spec: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let Some(dir) = dir else {
        let _ = tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "session storage is unavailable".into(),
        });
        return;
    };
    let read_dir = dir.clone();
    let read_cwd = cwd.to_owned();
    let found = tokio::task::spawn_blocking(move || {
        crate::headless::StoredSession::latest(&read_cwd, &read_dir)
    })
    .await;
    match found {
        Ok(Ok(Some(session))) => {
            let id = session.id.id().to_string();
            load_session(state, files, &id, Some(dir), cwd, model_spec, tx).await;
        }
        Ok(Ok(None)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Neutral,
                text: "no previous session in this directory".into(),
            });
        }
        Ok(Err(e)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not find the latest session: {e}"),
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not find the latest session: {e}"),
            });
        }
    }
}

/// User/assistant text of a persisted session, for the conversation view's
/// rebuild: tool calls and results carry no displayable text, so messages
/// containing only those are skipped (they leave an empty `text()`).
pub(super) fn transcript(messages: &[crate::history::Message]) -> Vec<LoadedMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let text = message.text();
            if text.trim().is_empty() {
                return None;
            }
            Some(match message {
                crate::history::Message::User { .. } => LoadedMessage::User(text),
                _ => LoadedMessage::Assistant(text),
            })
        })
        .collect()
}

/// `Command::LoadSession`: refuse while a turn is running, then swap in the
/// persisted session.
pub(super) async fn handle_load_session(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &Option<AbortHandle>,
    id: String,
) {
    if current_turn.is_some() {
        // Loading mid-run would race the running turn's history copy and
        // its compaction/dedup/guardrails handles, desyncing the provider
        // from the session.
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before loading a session.".into(),
        });
        return;
    }
    load_session(
        &ctx.state,
        &ctx.files,
        &id,
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
        &ctx.evt_tx,
    )
    .await;
}

/// `Command::ResumeLatest`: refuse while a turn is running, then load this
/// directory's newest session.
pub(super) async fn handle_resume_latest(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &Option<AbortHandle>,
) {
    if current_turn.is_some() {
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before resuming.".into(),
        });
        return;
    }
    resume_latest(
        &ctx.state,
        &ctx.files,
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
        &ctx.evt_tx,
    )
    .await;
}

/// `Command::SetDraft`: soft-checkpoint the input draft, with a delayed
/// write if the store wants one.
pub(super) async fn handle_set_draft(ctx: &LoopCtx, draft: String) {
    let mut guard = ctx.state.lock().await;
    let Some(store) = &mut guard.store else {
        return;
    };
    store.checkpoint_draft(&draft);
    if let Some(wait) = store.soft_save_wait() {
        let state = Arc::clone(&ctx.state);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(store) = &mut state.lock().await.store {
                store.checkpoint_now();
            }
        });
    }
}

/// `Command::SetMode`: persist the session's mode (Tab toggle, plan
/// implementation) so a resume restores it.
pub(super) async fn handle_set_mode(ctx: &LoopCtx, plan: bool) {
    let mut guard = ctx.state.lock().await;
    if let Some(store) = &mut guard.store {
        store.set_mode(plan);
    }
}

/// Command-loop exit: fold queued bang-mode results into the history and
/// persist them (no next turn exists to carry them), then flush any soft
/// checkpointed draft.
pub(super) async fn persist_on_exit(ctx: &LoopCtx, selection: &Selection) {
    // No next turn exists: stop any in-flight bang runs, fold their
    // queued results into the history, and persist them.
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancel_all();
    drain_shell_results(&ctx.shell, &ctx.state).await;
    let mut guard = ctx.state.lock().await;
    let history = guard.history.clone();
    // Always record (not only when shell results drained): a turn killed by
    // the interrupt abort fallback leaves its staged prompt and cancel
    // marker only in memory, and no later turn exists to carry them out.
    if !history.is_empty()
        && let Some(store) = &mut guard.store
    {
        store.record_turn(&history, LoopCtx::model_spec(selection));
    }
    if let Some(store) = &mut guard.store {
        store.checkpoint_now();
    }
}
