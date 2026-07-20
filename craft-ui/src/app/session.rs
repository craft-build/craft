use std::sync::Arc;

use crate::chat::{Chat, DONE_TEXT, history_to_display};
use crate::components::DisplayRole;
use crate::components::permission_prompt::PermissionPrompt;
use crate::components::rewind_picker::RewindEntry;
use crate::components::{Action, LoadedSession};
use craft_providers::{Model, TokenUsage};
use craft_storage::id::CraftId;
use craft_storage::sessions::StoredSubagent;

use crate::AppSession;

use super::session_state::{SessionState, stored_to_rules};
use super::{App, Mode, PendingInput, PlanState};
use crate::agent::QueuedMessage;

/// The single content predicate: `App::save_session` persists a session
/// iff this holds, and the shutdown path reuses it to tell which tabs were
/// saved, so the report and the disk can never disagree. Sync the session
/// first (`save_session` does).
pub(crate) fn session_has_content(session: &AppSession) -> bool {
    !session.messages.is_empty()
        || session.meta.input_draft.is_some()
        || !session.meta.queued_messages.is_empty()
        || session.meta.mode != Some(craft_storage::sessions::StoredMode::Build)
}

impl App {
    pub(crate) fn has_content(&self) -> bool {
        session_has_content(&self.state.session)
    }

    /// True when the session is blocked on the user (permission prompt,
    /// pending auth retry, or any other `PendingInput`). Used by the
    /// multi-session supervisor to report `needs_input` status.
    pub(crate) fn awaiting_input(&self) -> bool {
        matches!(self.permission_prompt, PermissionPrompt::Open { .. })
            || self.pending_input != PendingInput::None
    }

    pub(crate) fn save_session(&mut self) {
        self.state.sync_session(
            &self.shared_history,
            &self.shared_tool_outputs,
            &self.permissions,
        );
        self.sync_ephemeral_state();
        if !self.has_content() {
            return;
        }
        self.enqueue_save();
    }

    pub(crate) fn record_cost(&self, chat_idx: usize, usage: TokenUsage) {
        let chat = &self.chats[chat_idx];
        let resolved = chat
            .model_id
            .as_deref()
            .and_then(|spec| Model::from_spec(spec).ok());
        let model = resolved.as_ref().unwrap_or(&self.state.model);
        let cost_usd = usage.cost(&model.pricing, self.state.fast);
        let provider = model.provider_display_name().to_string();
        let record = craft_storage::stats::CostRecord {
            session_id: self.state.session.id.as_str().to_string(),
            turn_id: chat.model_id.clone(),
            ts: craft_storage::now_epoch(),
            model: model.spec(),
            provider,
            usage: craft_storage::stats::CostUsage {
                input: usage.input as u64,
                output: usage.output as u64,
                cache_creation: usage.cache_creation as u64,
                cache_read: usage.cache_read as u64,
            },
            cost_usd,
            fast: self.state.fast,
        };
        self.storage_writer.record_cost(record);
    }

    fn sync_ephemeral_state(&mut self) {
        let draft = self.input_box.buffer.value();
        self.state.session.meta.input_draft = if draft.is_empty() { None } else { Some(draft) };

        self.state.session.meta.queued_messages = self.queue.text_messages();

        self.state.session.meta.subagents = self
            .chats
            .iter()
            .skip(1)
            .zip(self.chat_index.iter())
            .map(|(chat, (tool_id, _))| StoredSubagent {
                tool_use_id: tool_id.clone(),
                name: chat.name.clone(),
                prompt: None,
                model: chat.model_id.clone(),
                lifecycle: None,
                context_mode: None,
            })
            .collect();
    }

    pub(super) fn save_input_history(&self) {
        if let Err(e) = self.input_box.history().save(&self.storage) {
            tracing::warn!(error = %e, "input history save failed");
        }
    }

    pub(super) fn enqueue_save(&self) {
        self.storage_writer
            .send(Box::new(self.state.session.clone()));
    }

    pub(super) fn reset_ui_chrome(&mut self) {
        self.chats.clear();
        self.chats
            .push(Chat::new("Main".into(), self.ui_config.clone()));
        self.active_chat = 0;
        self.chat_index.clear();
        self.status = super::Status::Idle;
        self.queue.clear();
        self.close_all_overlays();
        if self.flow_awaiting_approval {
            self.flow_awaiting_approval = false;
            self.send_answer(craft_flow::FLOW_CANCEL_ANSWER.to_owned());
        }
        self.pending_input = PendingInput::None;
        self.status_bar.clear_flash();
        self.task_picker_original = None;
        self.last_esc = None;
        self.plan_form.reset();
        self.flow_panel.reset();
        self.restoring = Arc::new(std::sync::atomic::AtomicBool::new(false));
    }

    pub(crate) fn restore_display(&mut self) {
        let restoring = Arc::new(std::sync::atomic::AtomicBool::new(true));
        self.restoring = restoring.clone();

        let (display_msgs, restore_items) = history_to_display(
            &self.state.session.messages,
            &self.state.session.tool_outputs,
            &self.ui_config.tool_output_lines,
        );
        self.main_chat().load_messages(display_msgs);
        self.main_chat().token_usage = self.state.token_usage;
        self.main_chat().context_size = self.state.context_size;

        if let Some(draft) = self.state.session.meta.input_draft.take() {
            self.input_box.set_input(draft);
            self.input_box.buffer.move_to_end();
        }

        for text in std::mem::take(&mut self.state.session.meta.queued_messages) {
            let msg = QueuedMessage {
                text,
                images: Vec::new(),
            };
            self.queue_and_notify(msg);
        }

        self.fire_restore_items(restore_items);

        for sa in std::mem::take(&mut self.state.session.meta.subagents) {
            let idx = self.chats.len();
            self.chat_index.insert(sa.tool_use_id.clone(), idx);
            let mut chat = Chat::new(sa.name, self.ui_config.clone());
            chat.model_id = sa.model;
            chat.set_lua_event_handle(self.lua_event_handle.clone());
            chat.set_restore_event_tx(self.restore_event_tx.clone());
            if let Some(messages) = self.state.session.subagent_messages.get(&sa.tool_use_id) {
                let (display, items) = history_to_display(
                    messages,
                    &self.state.session.tool_outputs,
                    &self.ui_config.tool_output_lines,
                );
                chat.load_messages(display);
                chat.mark_finished(DisplayRole::Done, DONE_TEXT);
                self.fire_restore_items(items);
            }
            self.chats.push(chat);
        }

        if let Some(eh) = &self.lua_event_handle {
            eh.send_restore_complete(restoring);
        } else {
            self.restoring
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn fire_restore_items(&self, items: Vec<craft_lua::RestoreItem>) {
        let (Some(eh), Some(tx)) = (&self.lua_event_handle, &self.restore_event_tx) else {
            return;
        };
        let theme_gen = crate::theme::generation();
        for mut item in items {
            item.theme_gen = Some(theme_gen);
            eh.request_restore(item, tx.clone());
        }
    }

    fn loaded_session_snapshot(&self) -> LoadedSession {
        LoadedSession {
            messages: self.state.session.messages.clone(),
            tool_outputs: self.state.session.tool_outputs.clone(),
            model_spec: self.state.session.model.clone(),
        }
    }

    pub(super) fn reset_session(&mut self) -> Vec<Action> {
        self.reset_ui_chrome();
        if let Some(ref handle) = self.lua_event_handle {
            handle.fire_autocmd("SessionReset", serde_json::json!({}));
        }
        self.state.token_usage = TokenUsage::default();
        self.state.context_size = 0;
        self.state.plan = PlanState::None;
        if self.state.mode == Mode::Plan {
            self.enter_plan();
        }
        self.state.session = AppSession::new(&self.state.session.model, &self.state.session.cwd);
        if let Some(ref handle) = self.lua_event_handle {
            handle.fire_autocmd("SessionStart", serde_json::json!({}));
        }
        vec![Action::NewSession]
    }

    pub(super) fn open_rewind_picker(&mut self) -> Vec<Action> {
        self.save_session();
        match self.rewind_picker.open(&self.state.session.messages) {
            Ok(()) => vec![],
            Err(msg) => {
                self.status_bar.flash(msg);
                vec![]
            }
        }
    }

    pub(super) fn rewind_to(&mut self, entry: RewindEntry) -> Vec<Action> {
        self.run_id += 1;

        self.state.session.messages.truncate(entry.turn_index);
        self.state
            .session
            .prune_orphans(|m| m.tool_uses().map(|(id, _, _)| id.to_owned()).collect());
        self.state.context_size =
            craft_agent::agent::estimate_message_tokens(&self.state.session.messages);

        self.reset_ui_chrome();
        self.restore_display();

        self.input_box.set_input(entry.prompt_text);
        self.input_box.buffer.move_to_end();

        self.state.session.update_title_if_default();
        self.enqueue_save();

        vec![Action::LoadSession(Box::new(
            self.loaded_session_snapshot(),
        ))]
    }

    pub(crate) fn apply_loaded_session(
        &mut self,
        session: AppSession,
        fallback_model: &Model,
    ) -> LoadedSession {
        self.permissions
            .load_session_rules(stored_to_rules(&session.meta.session_rules));
        self.state = SessionState::from_session(session, fallback_model, &self.storage);
        for w in self.state.warnings.drain(..) {
            self.status_bar.flash(w);
        }
        self.reset_ui_chrome();
        self.restore_display();

        self.enqueue_save();
        self.loaded_session_snapshot()
    }

    pub(crate) fn load_session(&mut self, session_id: CraftId) -> Vec<Action> {
        let session = match AppSession::load(session_id, &self.storage) {
            Ok(s) => s,
            Err(e) => {
                self.status_bar
                    .flash(format!("Failed to load session: {e}"));
                return vec![];
            }
        };
        self.save_session();
        let loaded = self.apply_loaded_session(session, &self.state.model.clone());
        vec![Action::LoadSession(Box::new(loaded))]
    }
}
