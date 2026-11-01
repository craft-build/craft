//! Session load/resume and exit-time persistence: swap a persisted session
//! in (`/sessions`, F.3 resume-latest-by-cwd), rebuild the transcript, and
//! flush queued results and drafts when the command loop ends.

use std::sync::Arc;

use tokio::task::AbortHandle;

use super::bootstrap::{RestoredSession, install_session};
use super::commands::drain_shell_results;
use super::{AgentEvent, LoopCtx, Status, Tone};

/// `/sessions` load: replace the session with a persisted one and push the
/// rebuilt transcript plus fresh chrome back to the UI. Load failures only
/// warn; the current session stays put.
pub(super) async fn load_session(ctx: &LoopCtx, id: &str) {
    let Some(dir) = ctx.kit.state_dir.clone() else {
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "session storage is unavailable".into(),
        });
        return;
    };
    let craft_id = match id.parse::<crate::id::CraftId>() {
        Ok(id) => id,
        Err(_) => {
            let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("unknown session id {id:?}"),
            });
            return;
        }
    };
    // The store binds the loaded id so future turns resume the same session
    // file and its cost records join its ledger id; the model stays on the
    // current selection, as the load always did.
    let selection = Arc::unwrap_or_clone(ctx.kit.selection.load_full());
    let persister = match crate::harness::Persister::open_in(
        dir,
        crate::id::SessionRef::from(craft_id),
        &ctx.kit.cwd,
        &LoopCtx::model_spec(&selection),
    )
    .await
    {
        Ok(persister) => Arc::new(tokio::sync::Mutex::new(persister)),
        Err(e) => {
            let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not load session: {e}"),
            });
            return;
        }
    };
    let (title, draft, mode, stored_thinking, messages) = {
        let guard = persister.lock().await;
        let session = guard.store().session();
        (
            session.title.clone(),
            session.meta.input_draft.clone().unwrap_or_default(),
            session.meta.mode.clone(),
            session.meta.thinking,
            session.messages().to_vec(),
        )
    };
    // The shared replay list (the same one ACP `session/load` and the
    // headless resume build): current view, inactive entries hidden,
    // compaction cards, markers, altered results with their originals.
    let rendered = loaded_messages(&crate::harness::replay::transcript(&messages));
    // Stored thinking wins on resume; anything the file does not spell out
    // (legacy sessions) keeps the session's current level.
    let thinking = match stored_thinking {
        Some(thinking) => thinking,
        None => ctx.state.lock().await.thinking,
    };
    if let Err(error) = install_session(
        &ctx.kit,
        &ctx.state,
        thinking,
        Some(RestoredSession { persister }),
    )
    .await
    {
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: format!("could not load session: {error}"),
        });
        return;
    }
    // Stored mode wins on resume; anything the file does not spell out
    // (legacy sessions) keeps the CLI-seeded mode.
    let mode = match mode.as_deref() {
        Some("plan") => Some(crate::tui::provider::SessionMode::Plan),
        Some("build") => Some(crate::tui::provider::SessionMode::Build),
        _ => None,
    };
    ctx.kit
        .files
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let _ = ctx.kit.evt_tx.send(AgentEvent::FilesSet(Vec::new()));
    let _ = ctx.kit.evt_tx.send(AgentEvent::AssistantEnd);
    let _ = ctx.kit.evt_tx.send(AgentEvent::SessionLoaded {
        messages: rendered,
        draft,
        mode,
    });
    let _ = ctx.kit.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
    let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
        tone: Tone::Success,
        text: format!("resumed session \"{title}\""),
    });
}

/// F.3 resume-latest-by-cwd: load the newest session recorded for this
/// directory (cwd index first, disk scan fallback), reusing the `/sessions`
/// load path. Nothing found keeps the fresh session.
pub(super) async fn resume_latest(ctx: &LoopCtx) {
    let Some(dir) = ctx.kit.state_dir.clone() else {
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "session storage is unavailable".into(),
        });
        return;
    };
    let read_dir = dir;
    let read_cwd = ctx.kit.cwd.clone();
    let found = tokio::task::spawn_blocking(move || {
        crate::storage::sessions::StoredSession::latest(&read_cwd, &read_dir)
    })
    .await;
    match found {
        Ok(Ok(Some(session))) => {
            let id = session.id.id().to_string();
            load_session(ctx, &id).await;
        }
        Ok(Ok(None)) => {
            let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
                tone: Tone::Neutral,
                text: "no previous session in this directory".into(),
            });
        }
        Ok(Err(e)) => {
            let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not find the latest session: {e}"),
            });
        }
        Err(e) => {
            let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not find the latest session: {e}"),
            });
        }
    }
}

/// The shared replay list as the TUI's loaded-message payload: the same
/// items the app's `SessionLoaded` handler maps onto transcript messages.
pub(super) fn loaded_messages(items: &[crate::harness::ReplayItem]) -> Vec<super::LoadedMessage> {
    items
        .iter()
        .map(|item| match item {
            crate::harness::ReplayItem::User { text } => super::LoadedMessage::User(text.clone()),
            crate::harness::ReplayItem::Assistant { text } => {
                super::LoadedMessage::Assistant(text.clone())
            }
            crate::harness::ReplayItem::ToolResult {
                id,
                name,
                text,
                full,
                ..
            } => {
                super::LoadedMessage::Tool {
                    id: id.clone(),
                    // The same header derivation live cards get, minus the
                    // argument detail the replay does not hold.
                    kind: super::cards::tool_head(name, &serde_json::Value::Null),
                    text: text.clone(),
                    full: full.clone(),
                }
            }
            crate::harness::ReplayItem::Compaction { kind, text } => {
                super::LoadedMessage::Compaction {
                    kind: kind.clone(),
                    text: text.clone(),
                }
            }
            crate::harness::ReplayItem::Marker { kind, text } => super::LoadedMessage::Marker {
                kind: kind.clone(),
                text: text.clone(),
            },
        })
        .collect()
}

/// `Command::LoadSession`: refuse while a turn is running, then swap in the
/// persisted session.
pub(super) async fn handle_load_session(
    ctx: &LoopCtx,
    current_turn: &Option<AbortHandle>,
    id: String,
) {
    if current_turn.is_some() {
        // Loading mid-run would race the running turn's conversation.
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before loading a session.".into(),
        });
        return;
    }
    load_session(ctx, &id).await;
}

/// `Command::ResumeLatest`: refuse while a turn is running, then load this
/// directory's newest session.
pub(super) async fn handle_resume_latest(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        let _ = ctx.kit.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before resuming.".into(),
        });
        return;
    }
    resume_latest(ctx).await;
}

/// `Command::SetDraft`: soft-checkpoint the input draft, with a delayed
/// write if the store wants one.
pub(super) async fn handle_set_draft(ctx: &LoopCtx, draft: String) {
    let persister = ctx.state.lock().await.conversation.persister();
    let Some(persister) = persister else {
        return;
    };
    let wait = {
        let mut guard = persister.lock().await;
        guard.store_mut().checkpoint_draft(&draft);
        guard.store().soft_save_wait()
    };
    if let Some(wait) = wait {
        let state = Arc::clone(&ctx.state);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(persister) = state.lock().await.conversation.persister() {
                persister.lock().await.store_mut().checkpoint_now();
            }
        });
    }
}

/// `Command::SetMode`: persist the session's mode (Tab toggle, plan
/// implementation) so a resume restores it.
pub(super) async fn handle_set_mode(ctx: &LoopCtx, plan: bool) {
    let persister = ctx.state.lock().await.conversation.persister();
    if let Some(persister) = persister {
        persister.lock().await.store_mut().set_mode(plan);
    }
}

/// Command-loop exit: fold queued bang-mode results into the conversation
/// and persist them (no next turn exists to carry them), then flush the
/// history change feed and any soft checkpointed draft.
pub(super) async fn persist_on_exit(ctx: &LoopCtx) {
    // No next turn exists: stop any in-flight bang runs, fold their queued
    // results into the conversation, and persist them.
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancel_all();
    drain_shell_results(&ctx.shell, &ctx.state).await;
    let selection = Arc::unwrap_or_clone(ctx.kit.selection.load_full());
    let conversation = ctx.state.lock().await.conversation.clone();
    // Always checkpoint (not only when shell results drained): a turn
    // killed by the interrupt abort fallback leaves its committed messages
    // only in memory, and no later turn exists to carry them out.
    conversation.flush().await;
    if let Some(persister) = conversation.persister() {
        let mut guard = persister.lock().await;
        guard.store_mut().set_model(LoopCtx::model_spec(&selection));
        guard.store_mut().checkpoint_now();
    }
}
