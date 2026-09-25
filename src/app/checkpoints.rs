use std::path::PathBuf;

use gpui::Context;

use crate::async_runtime;
use crate::checkpoint::{Checkpoint, CheckpointManager};
use crate::state::{Message, Role};

use super::App;

impl App {
    pub(crate) fn create_turn_checkpoint(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.agent_config.clone() else {
            return;
        };
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let project_id = project.id.clone();
        let local_workspace = PathBuf::from(&project.path);
        let session_id = self.active_session_id.clone();
        // Numbering belongs to the session: its first checkpoint is
        // "Checkpoint 0" even when earlier sessions already made checkpoints.
        let label = next_checkpoint_label(&self.active_messages());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CheckpointManager::new(config, local_workspace)
                    .create(&label, session_id.as_deref())
            })
            .await
            .unwrap_or_else(|error| Err(format!("checkpoint task failed: {error}")));
            let _ = sender.send(result);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = receiver.await {
                this.update(cx, |app, cx| {
                    match result {
                        Ok(checkpoint) => {
                            let mut thread = app.active_messages();
                            if let Some(message) = thread
                                .iter_mut()
                                .rev()
                                .find(|message| matches!(message.role, Role::Assistant))
                            {
                                message.checkpoint_label = Some(checkpoint.label.clone());
                            }
                            app.update_active_messages(thread);
                            app.checkpoints_by_project
                                .entry(project_id.clone())
                                .or_default()
                                .push(checkpoint);
                            let count = app
                                .checkpoints_by_project
                                .get(&project_id)
                                .map_or(0, Vec::len);
                            if let Some(project) = app
                                .projects
                                .iter_mut()
                                .find(|project| project.id == project_id)
                            {
                                project.checkpoint_label = format!(
                                    "{count} checkpoint{}",
                                    if count == 1 { "" } else { "s" }
                                );
                            }
                            app.persist_state();
                        }
                        Err(error) => app.toast = Some(error),
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Checkpoints in chronological order (oldest first) — matches the JS
    /// `checkpointsFor()`. Callers that want most-recent-first (the
    /// checkpoint history panel) reverse this themselves.
    pub fn checkpoints_for(&self) -> Vec<(String, String)> {
        self.active_messages()
            .into_iter()
            .filter(|m| matches!(m.role, Role::Assistant) && m.checkpoint_label.is_some())
            .map(|m| (m.checkpoint_label.unwrap(), m.time.unwrap_or_default()))
            .collect()
    }

    pub fn restore_checkpoint(&mut self, label: &str, cx: &mut Context<Self>) {
        let project_id = self.current_thread_key();
        let session_id = self.active_session_id.clone();
        let Some(checkpoint) = self
            .checkpoints_by_project
            .get(&project_id)
            .and_then(|checkpoints| find_checkpoint(checkpoints, label, session_id.as_deref()))
            .cloned()
        else {
            self.toast = Some(format!("Checkpoint data is unavailable for {label}"));
            cx.notify();
            return;
        };
        let Some(config) = self.agent_config.clone() else {
            self.toast = Some("Agent workspace is not configured".into());
            cx.notify();
            return;
        };
        let Some(project) = self.active_project.as_ref() else {
            return;
        };
        let local_workspace = PathBuf::from(&project.path);
        self.toast = Some(format!("Restoring {label}…"));
        let restored_label = label.to_string();
        self.show_checkpoints = false;
        self.toast_generation += 1;
        let generation = self.toast_generation;
        let (sender, rx) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CheckpointManager::new(config, local_workspace).restore(&checkpoint)
            })
            .await
            .unwrap_or_else(|error| Err(format!("restore task failed: {error}")));
            let _ = sender.send(result);
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |app, cx| {
                if generation == app.toast_generation {
                    app.toast = Some(match result {
                        Ok(Ok(())) => format!("Restored to {restored_label}"),
                        Ok(Err(error)) => format!("Restore failed: {error}"),
                        Err(_) => "Restore task stopped unexpectedly".into(),
                    });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();

        cx.notify();
    }
}

/// Checkpoint labels are numbered within the session that made them, so a
/// session's first checkpoint is "Checkpoint 0" no matter how many
/// checkpoints other sessions created. Counting the labeled messages of the
/// thread gives the index of the next checkpoint.
pub(crate) fn next_checkpoint_label(messages: &[Message]) -> String {
    let count = messages
        .iter()
        .filter(|message| {
            matches!(message.role, Role::Assistant) && message.checkpoint_label.is_some()
        })
        .count();
    format!("Checkpoint {count}")
}

/// Find a checkpoint by label within the session that created it. Labels
/// repeat across sessions (each session counts from 0), so the session id
/// picks the right one. Checkpoints saved before per-session numbering carry
/// no session and remain restorable by label alone.
pub(crate) fn find_checkpoint<'a>(
    checkpoints: &'a [Checkpoint],
    label: &str,
    session_id: Option<&str>,
) -> Option<&'a Checkpoint> {
    checkpoints
        .iter()
        .rev()
        .find(|checkpoint| {
            checkpoint.label == label && checkpoint.session_id.as_deref() == session_id
        })
        .or_else(|| {
            checkpoints
                .iter()
                .rev()
                .find(|checkpoint| checkpoint.label == label && checkpoint.session_id.is_none())
        })
}
