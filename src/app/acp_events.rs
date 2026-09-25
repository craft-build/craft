use agent_client_protocol::schema::v1::{
    ContentBlock, SessionUpdate, ToolCallContent, ToolCallUpdate,
};
use gpui::Context;

use crate::acp::{AcpEvent, ElicitationDecision, PermissionDecision};
use crate::state::*;

use super::App;

impl App {
    pub(crate) fn current_thread_key(&self) -> String {
        self.active_project
            .as_ref()
            .map(|p| p.id.clone())
            .unwrap_or_else(|| "untitled".to_string())
    }

    pub fn active_messages(&self) -> Vec<Message> {
        let key = self.current_thread_key();
        let sessions = self.sessions_by_project.get(&key);
        let sess = sessions.and_then(|list| {
            list.iter()
                .find(|s| Some(&s.id) == self.active_session_id.as_ref())
                .or_else(|| list.first())
        });
        sess.map(|s| s.messages.clone()).unwrap_or_default()
    }

    pub(crate) fn active_messages_mut(&mut self) -> Option<&mut Vec<Message>> {
        let key = self.current_thread_key();
        self.sessions_by_project
            .get_mut(&key)?
            .iter_mut()
            .find(|s| Some(&s.id) == self.active_session_id.as_ref())
            .map(|s| &mut s.messages)
    }

    pub(crate) fn update_active_messages(&mut self, new_messages: Vec<Message>) {
        let key = self.current_thread_key();
        if let Some(sessions) = self.sessions_by_project.get_mut(&key)
            && let Some(sess) = sessions
                .iter_mut()
                .find(|s| Some(&s.id) == self.active_session_id.as_ref())
        {
            sess.messages = new_messages;
        }
    }

    pub(crate) fn apply_acp_event(&mut self, event: AcpEvent, cx: &mut Context<Self>) {
        match event {
            AcpEvent::Connecting => self.connection_status = "Connecting".into(),
            AcpEvent::Connected { agent_name } => {
                self.connection_status = agent_name
                    .map(|name| format!("Connected · {name}"))
                    .unwrap_or_else(|| "Connected".into());
            }
            AcpEvent::Reconnecting { attempt, reason } => {
                self.thinking = false;
                self.connection_status = format!("Reconnecting ({attempt}) · {reason}");
            }
            AcpEvent::SessionReady { session_id } => {
                self.connection_status = "Idle".into();
                let project_id = self.current_thread_key();
                if let Some(session) =
                    self.sessions_by_project
                        .get_mut(&project_id)
                        .and_then(|sessions| {
                            sessions.iter_mut().find(|session| {
                                Some(&session.id) == self.active_session_id.as_ref()
                            })
                        })
                {
                    session.acp_session_id = Some(session_id);
                }
                self.persist_state();
            }
            AcpEvent::ConfigOptions(options) => {
                self.set_session_config_options(options);
            }
            AcpEvent::TurnStarted => {
                self.thinking = true;
                self.connection_status = "Running".into();
                if let Some(thread) = self.active_messages_mut() {
                    thread.push(Message {
                        id: super::new_id("a"),
                        role: Role::Assistant,
                        body: MessageBody::default(),
                        time: Some("Just now".into()),
                        context: vec![],
                        attached_comments: vec![],
                        checkpoint_label: None,
                        steps: None,
                        diff: None,
                        terminal: None,
                    });
                }
                self.thread_scroll.scroll_to_bottom();
            }
            AcpEvent::Update(update) => {
                self.apply_session_update(update);
            }
            AcpEvent::Permission(permission) => self.pending_permission = Some(permission),
            AcpEvent::Elicitation(elicitation) => {
                self.prepare_elicitation_form(&elicitation, cx);
                self.pending_elicitation = Some(elicitation);
            }
            AcpEvent::TurnFinished => {
                self.thinking = false;
                self.connection_status = "Idle".into();
                self.persist_state();
                self.create_turn_checkpoint(cx);
                self.refresh_workspace(cx);
            }
            AcpEvent::TurnCancelled => {
                self.thinking = false;
                self.connection_status = "Cancelled".into();
                if let Some(permission) = self.pending_permission.take() {
                    permission.respond(PermissionDecision::Cancel);
                }
                if let Some(elicitation) = self.pending_elicitation.take() {
                    elicitation.respond(ElicitationDecision::Cancel);
                }
                self.clear_elicitation();
                self.persist_state();
            }
            AcpEvent::Error(error) | AcpEvent::Disconnected(error) => {
                self.thinking = false;
                self.connection_status = error.clone();
                self.toast = Some(error);
            }
        }
        cx.notify();
    }

    pub(crate) fn apply_session_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content
                    && let Some(thread) = self.active_messages_mut()
                    && let Some(message) = thread
                        .iter_mut()
                        .rev()
                        .find(|message| matches!(message.role, Role::Assistant))
                {
                    message.body.push_text(&text.text);
                }
            }
            SessionUpdate::ToolCall(call) => self.apply_tool_update(call.into()),
            SessionUpdate::ToolCallUpdate(update) => self.apply_tool_update(update),
            SessionUpdate::UsageUpdate(usage) => {
                self.context_usage = (usage.size > 0)
                    .then_some(((usage.used.saturating_mul(100) / usage.size).min(100)) as u8);
            }
            SessionUpdate::ConfigOptionUpdate(options) => {
                self.set_session_config_options(options.config_options);
            }
            _ => {}
        }
    }

    pub(crate) fn apply_tool_update(&mut self, update: ToolCallUpdate) {
        for item in update.fields.content.iter().flatten() {
            if let ToolCallContent::Diff(diff) = item {
                let rendered = super::render_acp_diff(diff.clone());
                self.file_diffs.insert(rendered.file.clone(), rendered);
            }
        }
        let Some(thread) = self.active_messages_mut() else {
            return;
        };
        // IDs are session-scoped. A late update must still reach its original
        // assistant message rather than being attached to the newest reply.
        let owner = thread
            .iter()
            .rposition(|message| {
                message
                    .body
                    .tool_calls()
                    .any(|call| call.tool_call_id == update.tool_call_id)
            })
            .or_else(|| {
                thread
                    .iter()
                    .rposition(|message| matches!(message.role, Role::Assistant))
            });
        let Some(owner) = owner else { return };
        // ACP collections replace previous contents; omitted fields stay intact.
        thread[owner].body.update_tool_call(update);
    }
}
