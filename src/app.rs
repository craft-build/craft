use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, ToolCallContent};
use gpui::prelude::*;
use gpui::{Context, Entity, ScrollHandle, Window, div, px, rgb};

use crate::acp::{
    AcpClient, AcpEvent, AnchoredComment, ElicitationDecision, PendingElicitation,
    PendingPermission, PermissionDecision, TurnInput,
};
use crate::async_runtime;
use crate::checkpoint::{Checkpoint, CheckpointManager};
use crate::config::{ConfigStore, ProjectAgentConfig, TransportConfig};
use crate::screens;
use crate::state::*;
use crate::text_input::TextInput;
use crate::theme;

pub struct PendingComment {
    pub key: String,
    pub idx: usize,
    pub text: String,
    pub label: String,
}

pub struct App {
    pub screen: Screen,
    pub projects: Vec<Project>,
    pub sessions_by_project: HashMap<String, Vec<Session>>,
    pub active_project: Option<Project>,
    pub active_session_id: Option<String>,

    pub selected_model: String,
    pub available_models: Vec<String>,
    pub model_options: HashMap<String, (String, String)>,
    pub model_menu_open: bool,

    pub sidebar_visible: bool,
    pub file_tree_visible: bool,
    pub collapsed_projects: HashSet<String>,

    pub active_diff_file: Option<String>,
    pub file_diffs: HashMap<String, Diff>,

    pub composer: Entity<TextInput>,
    pub context_chips: Vec<String>,
    pub thinking: bool,
    pub connection_status: String,
    pub context_usage: Option<u8>,
    pub acp_client: Option<AcpClient>,
    pub agent_config: Option<ProjectAgentConfig>,
    pub pending_permission: Option<PendingPermission>,
    pub pending_elicitation: Option<PendingElicitation>,
    pub sent_count: usize,

    pub show_checkpoints: bool,
    pub toast: Option<String>,
    pub toast_generation: u64,

    pub expanded_steps: HashSet<String>,
    pub open_comment_boxes: HashSet<String>,
    pub comment_inputs: HashMap<String, Entity<TextInput>>,
    pub comments: HashMap<String, Vec<Comment>>,

    pub thread_scroll: ScrollHandle,
    pub checkpoints: Vec<Checkpoint>,

    pub config_remote: bool,
    pub agent_command_input: Entity<TextInput>,
    pub workspace_input: Entity<TextInput>,
    pub ssh_host_input: Entity<TextInput>,
    pub ssh_user_input: Entity<TextInput>,
    pub ssh_key_input: Entity<TextInput>,
    pub elicitation_input: Entity<TextInput>,
}

impl App {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let weak = cx.weak_entity();
        let composer = cx.new(|cx| {
            TextInput::new(cx, "Message the agent... (Cmd+Enter to send)")
                .multiline()
                .on_submit(move |text, _window, cx| {
                    let text = text.to_string();
                    weak.update(cx, |app, cx| app.send_message(text, cx)).ok();
                })
        });
        let agent_command_input = cx.new(|cx| TextInput::new(cx, "e.g. gemini --experimental-acp"));
        let workspace_input = cx.new(|cx| TextInput::new(cx, "/path/to/project"));
        let ssh_host_input = cx.new(|cx| TextInput::new(cx, "host from ~/.ssh/config"));
        let ssh_user_input = cx.new(|cx| TextInput::new(cx, "optional SSH user"));
        let ssh_key_input = cx.new(|cx| TextInput::new(cx, "optional identity file"));
        let elicitation_input =
            cx.new(|cx| TextInput::new(cx, r#"JSON object, e.g. {"answer":"yes"}"#));

        App {
            screen: Screen::Onboarding,
            projects: seed_projects(),
            sessions_by_project: seed_sessions(),
            active_project: None,
            active_session_id: None,
            selected_model: String::new(),
            available_models: vec![],
            model_options: HashMap::new(),
            model_menu_open: false,
            sidebar_visible: true,
            file_tree_visible: true,
            collapsed_projects: HashSet::new(),
            active_diff_file: None,
            file_diffs: seed_file_diffs(),
            composer,
            context_chips: vec![],
            thinking: false,
            connection_status: "Not configured".into(),
            context_usage: None,
            acp_client: None,
            agent_config: None,
            pending_permission: None,
            pending_elicitation: None,
            sent_count: 0,
            show_checkpoints: false,
            toast: None,
            toast_generation: 0,
            expanded_steps: HashSet::new(),
            open_comment_boxes: HashSet::new(),
            comment_inputs: HashMap::new(),
            comments: seed_comments(),
            thread_scroll: ScrollHandle::new(),
            checkpoints: vec![],
            config_remote: false,
            agent_command_input,
            workspace_input,
            ssh_host_input,
            ssh_user_input,
            ssh_key_input,
            elicitation_input,
        }
    }

    // ---- navigation ----

    pub fn go_projects(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Projects;
        cx.notify();
    }

    pub fn go_settings(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Settings;
        cx.notify();
    }

    pub fn go_back_from_settings(&mut self, cx: &mut Context<Self>) {
        self.screen = if self.active_project.is_some() {
            Screen::Workspace
        } else {
            Screen::Projects
        };
        cx.notify();
    }

    pub fn open_project(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let session_id = self
            .sessions_by_project
            .get(id)
            .and_then(|s| s.first())
            .map(|s| s.id.clone());
        self.active_project = Some(project);
        self.active_session_id = session_id;
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        self.connect_project(cx);
        cx.notify();
    }

    pub fn open_session(&mut self, project_id: &str, session_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id).cloned() else {
            return;
        };
        self.active_project = Some(project);
        self.active_session_id = Some(session_id.to_string());
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        self.connect_project(cx);
        cx.notify();
    }

    pub fn add_session_to_project(&mut self, project_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.projects.iter().find(|p| p.id == project_id).cloned() else {
            return;
        };
        let new_id = format!("s{}", now_ms());
        let sessions = self
            .sessions_by_project
            .entry(project_id.to_string())
            .or_default();
        sessions.push(Session {
            id: new_id.clone(),
            name: "New session".into(),
            messages: vec![],
        });
        self.active_project = Some(project);
        self.active_session_id = Some(new_id);
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        self.connect_project(cx);
        cx.notify();
    }

    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        let project = Project {
            id: "untitled".into(),
            name: "untitled-session".into(),
            path: "~/dev/untitled-session".into(),
            desc: String::new(),
            updated: String::new(),
            checkpoint_label: String::new(),
            model: self.selected_model.clone(),
        };
        self.sessions_by_project.insert(
            "untitled".to_string(),
            vec![Session {
                id: "s1".into(),
                name: "New session".into(),
                messages: vec![],
            }],
        );
        self.active_project = Some(project);
        self.active_session_id = Some("s1".into());
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        cx.notify();
    }

    // ---- workspace chrome ----

    pub fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    pub fn toggle_file_tree(&mut self, cx: &mut Context<Self>) {
        self.file_tree_visible = !self.file_tree_visible;
        cx.notify();
    }

    pub fn toggle_project_collapse(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.collapsed_projects.insert(id.to_string()) {
            self.collapsed_projects.remove(id);
        }
        cx.notify();
    }

    pub fn toggle_model_menu(&mut self, cx: &mut Context<Self>) {
        self.model_menu_open = !self.model_menu_open;
        cx.notify();
    }

    pub fn select_model(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some((id, value)) = self.model_options.get(name)
            && let Some(client) = &self.acp_client
            && let Err(error) = client.set_config(id.clone(), value.clone())
        {
            self.connection_status = error;
            cx.notify();
            return;
        }
        self.selected_model = name.to_string();
        self.model_menu_open = false;
        cx.notify();
    }

    pub fn toggle_config_remote(&mut self, cx: &mut Context<Self>) {
        self.config_remote = !self.config_remote;
        cx.notify();
    }

    pub fn save_agent_config(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.active_project.as_ref() else {
            self.toast = Some("Open a project before configuring an agent".into());
            cx.notify();
            return;
        };
        let project_id = project.id.clone();
        let command = self.agent_command_input.read(cx).content.trim().to_string();
        let workspace = self.workspace_input.read(cx).content.trim().to_string();
        let host = self.ssh_host_input.read(cx).content.trim().to_string();
        let user = self.ssh_user_input.read(cx).content.trim().to_string();
        let key = self.ssh_key_input.read(cx).content.trim().to_string();
        let transport = if self.config_remote {
            TransportConfig::Ssh {
                host,
                user: (!user.is_empty()).then_some(user),
                identity_file: (!key.is_empty()).then(|| PathBuf::from(key)),
            }
        } else {
            TransportConfig::Local
        };
        let config = ProjectAgentConfig {
            agent_command: command,
            transport,
            workspace: PathBuf::from(workspace),
        };
        match ConfigStore::for_user().save(&project_id, &config) {
            Ok(()) => {
                self.agent_config = Some(config);
                self.toast = Some("Agent configuration saved".into());
                self.connect_project(cx);
            }
            Err(error) => self.toast = Some(format!("Could not save agent configuration: {error}")),
        }
        cx.notify();
    }

    fn connect_project(&mut self, cx: &mut Context<Self>) {
        self.acp_client = None;
        self.pending_permission = None;
        self.pending_elicitation = None;
        self.context_usage = None;
        self.available_models.clear();
        self.model_options.clear();
        let Some(project) = self.active_project.as_ref() else {
            return;
        };
        let config = match ConfigStore::for_user().load(&project.id) {
            Ok(Some(config)) => config,
            Ok(None) => {
                self.connection_status = "Not configured".into();
                return;
            }
            Err(error) => {
                self.connection_status = format!("Config error: {error}");
                return;
            }
        };
        self.config_remote = matches!(config.transport, TransportConfig::Ssh { .. });
        self.agent_command_input.update(cx, |input, _| {
            input.set_content(config.agent_command.clone())
        });
        self.workspace_input.update(cx, |input, _| {
            input.set_content(config.workspace.to_string_lossy())
        });
        if let TransportConfig::Ssh {
            host,
            user,
            identity_file,
        } = &config.transport
        {
            self.ssh_host_input
                .update(cx, |input, _| input.set_content(host));
            self.ssh_user_input.update(cx, |input, _| {
                input.set_content(user.clone().unwrap_or_default())
            });
            self.ssh_key_input.update(cx, |input, _| {
                input.set_content(
                    identity_file
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
            });
        }
        self.agent_config = Some(config.clone());
        let (client, mut events) = AcpClient::connect(config);
        self.acp_client = Some(client);
        self.connection_status = "Connecting".into();
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |app, cx| app.apply_acp_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn toggle_checkpoints(&mut self, cx: &mut Context<Self>) {
        self.show_checkpoints = !self.show_checkpoints;
        cx.notify();
    }

    pub fn toggle_diff_file(&mut self, path: &str, cx: &mut Context<Self>) {
        self.active_diff_file = if self.active_diff_file.as_deref() == Some(path) {
            None
        } else {
            Some(path.to_string())
        };
        cx.notify();
    }

    pub fn toggle_steps(&mut self, msg_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_steps.insert(msg_id.to_string()) {
            self.expanded_steps.remove(msg_id);
        }
        cx.notify();
    }

    // ---- comments ----

    pub fn open_comment_box(&mut self, key: String, label: String, cx: &mut Context<Self>) {
        if !self.open_comment_boxes.insert(key.clone()) {
            self.open_comment_boxes.remove(&key);
            self.comment_inputs.remove(&key);
            cx.notify();
            return;
        }
        let weak = cx.weak_entity();
        let submit_key = key.clone();
        let input = cx.new(|cx| {
            TextInput::new(cx, "Comment...").on_submit(move |text, _window, cx| {
                let text = text.to_string();
                let key = submit_key.clone();
                let label = label.clone();
                weak.update(cx, |app, cx| app.submit_comment(key, text, label, cx))
                    .ok();
            })
        });
        self.comment_inputs.insert(key, input);
        cx.notify();
    }

    pub fn submit_comment(
        &mut self,
        key: String,
        text: String,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.comments.entry(key.clone()).or_default().push(Comment {
            author: "you".into(),
            text,
            pending: true,
            label,
        });
        self.open_comment_boxes.remove(&key);
        self.comment_inputs.remove(&key);
        cx.notify();
    }

    pub fn pending_comments(&self) -> Vec<PendingComment> {
        let mut out = vec![];
        for (key, list) in &self.comments {
            for (idx, c) in list.iter().enumerate() {
                if c.pending {
                    out.push(PendingComment {
                        key: key.clone(),
                        idx,
                        text: c.text.clone(),
                        label: c.label.clone(),
                    });
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key).then(a.idx.cmp(&b.idx)));
        out
    }

    pub fn remove_pending_comment(&mut self, key: &str, idx: usize, cx: &mut Context<Self>) {
        if let Some(list) = self.comments.get_mut(key) {
            if idx < list.len() {
                list.remove(idx);
            }
        }
        cx.notify();
    }

    // ---- thread / messages ----

    fn current_thread_key(&self) -> String {
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

    fn update_active_messages(&mut self, new_messages: Vec<Message>) {
        let key = self.current_thread_key();
        if let Some(sessions) = self.sessions_by_project.get_mut(&key) {
            if let Some(sess) = sessions
                .iter_mut()
                .find(|s| Some(&s.id) == self.active_session_id.as_ref())
            {
                sess.messages = new_messages;
            }
        }
    }

    pub fn send_message(&mut self, text: String, cx: &mut Context<Self>) {
        if self.thinking {
            self.toast = Some("Stop the running turn before sending another message".into());
            cx.notify();
            return;
        }
        let pending = self.pending_comments();
        let text = text.trim().to_string();
        if text.is_empty() && pending.is_empty() {
            return;
        }
        let mut thread = self.active_messages();
        let chips = self.context_chips.clone();
        let attached_comments = pending
            .iter()
            .map(|p| (p.label.clone(), p.text.clone()))
            .collect();
        let id = format!("u{}", now_ms());
        thread.push(Message {
            id,
            role: Role::User,
            text,
            time: None,
            context: chips,
            attached_comments,
            checkpoint_label: None,
            steps: None,
            diff: None,
            terminal: None,
        });
        self.update_active_messages(thread);

        for p in &pending {
            if let Some(list) = self.comments.get_mut(&p.key) {
                if let Some(c) = list.get_mut(p.idx) {
                    c.pending = false;
                }
            }
        }

        self.context_chips.clear();
        self.sent_count += 1;
        let turn = TurnInput {
            text: self
                .active_messages()
                .last()
                .map(|message| message.text.clone())
                .unwrap_or_default(),
            context_files: self
                .active_messages()
                .last()
                .map(|message| message.context.clone())
                .unwrap_or_default(),
            comments: pending
                .iter()
                .map(|comment| AnchoredComment {
                    target: comment.label.clone(),
                    body: comment.text.clone(),
                })
                .collect(),
        };
        match self.acp_client.as_ref() {
            Some(client) => {
                if let Err(error) = client.prompt(turn) {
                    self.connection_status = error;
                }
            }
            None => {
                self.connection_status =
                    "No ACP agent configured; open Settings to connect one".into();
            }
        }

        cx.notify();
    }

    fn apply_acp_event(&mut self, event: AcpEvent, cx: &mut Context<Self>) {
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
            AcpEvent::SessionReady => self.connection_status = "Idle".into(),
            AcpEvent::TurnStarted => {
                self.thinking = true;
                self.connection_status = "Running".into();
                let mut thread = self.active_messages();
                thread.push(Message {
                    id: format!("a{}", now_ms()),
                    role: Role::Assistant,
                    text: String::new(),
                    time: Some("Just now".into()),
                    context: vec![],
                    attached_comments: vec![],
                    checkpoint_label: None,
                    steps: None,
                    diff: None,
                    terminal: None,
                });
                self.update_active_messages(thread);
            }
            AcpEvent::Update(update) => self.apply_session_update(update),
            AcpEvent::Permission(permission) => self.pending_permission = Some(permission),
            AcpEvent::Elicitation(elicitation) => self.pending_elicitation = Some(elicitation),
            AcpEvent::TurnFinished => {
                self.thinking = false;
                self.connection_status = "Idle".into();
                self.create_turn_checkpoint(cx);
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
            }
            AcpEvent::Error(error) | AcpEvent::Disconnected(error) => {
                self.thinking = false;
                self.connection_status = error.clone();
                self.toast = Some(error);
            }
        }
        cx.notify();
    }

    fn apply_session_update(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = chunk.content {
                    let mut thread = self.active_messages();
                    if let Some(message) = thread
                        .iter_mut()
                        .rev()
                        .find(|message| matches!(message.role, Role::Assistant))
                    {
                        message.text.push_str(&text.text);
                    }
                    self.update_active_messages(thread);
                }
            }
            SessionUpdate::ToolCall(call) => {
                self.apply_tool_content(call.title, call.content);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.apply_tool_content(
                    update
                        .fields
                        .title
                        .unwrap_or_else(|| "Agent operation".into()),
                    update.fields.content.unwrap_or_default(),
                );
            }
            SessionUpdate::UsageUpdate(usage) => {
                self.context_usage = (usage.size > 0)
                    .then_some(((usage.used.saturating_mul(100) / usage.size).min(100)) as u8);
            }
            SessionUpdate::ConfigOptionUpdate(options) => {
                let value = serde_json::to_value(options).unwrap_or_default();
                let options = extract_model_options(&value);
                self.available_models = options.iter().map(|(name, _, _)| name.clone()).collect();
                self.model_options = options
                    .into_iter()
                    .map(|(name, id, value)| (name, (id, value)))
                    .collect();
                if self.selected_model.is_empty() {
                    self.selected_model =
                        self.available_models.first().cloned().unwrap_or_default();
                }
            }
            _ => {}
        }
    }

    fn apply_tool_content(&mut self, title: String, content: Vec<ToolCallContent>) {
        let mut thread = self.active_messages();
        let Some(message) = thread
            .iter_mut()
            .rev()
            .find(|message| matches!(message.role, Role::Assistant))
        else {
            return;
        };
        for item in content {
            match item {
                ToolCallContent::Diff(diff) => {
                    let rendered = render_acp_diff(diff);
                    self.file_diffs
                        .insert(rendered.file.clone(), rendered.clone());
                    message.diff = Some(rendered);
                }
                ToolCallContent::Content(content) => {
                    if let ContentBlock::Text(text) = content.content {
                        message.terminal = Some(Terminal {
                            cmd: title.clone(),
                            output: text.text,
                        });
                    }
                }
                ToolCallContent::Terminal(_) => {
                    message.terminal.get_or_insert(Terminal {
                        cmd: title.clone(),
                        output: "Interactive terminal is managed by the connected agent".into(),
                    });
                }
                _ => {}
            }
        }
        self.update_active_messages(thread);
    }

    pub fn decide_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(permission) = self.pending_permission.take() else {
            return;
        };
        let option = permission.options.iter().find(|option| {
            let kind = format!("{:?}", option.kind);
            kind.starts_with(if allow { "Allow" } else { "Reject" })
        });
        let decision = option
            .map(|option| PermissionDecision::Select(option.option_id.to_string()))
            .unwrap_or(PermissionDecision::Cancel);
        permission.respond(decision);
        cx.notify();
    }

    pub fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = &self.acp_client {
            if let Err(error) = client.cancel() {
                self.connection_status = error;
            }
        }
        cx.notify();
    }

    pub fn decline_elicitation(&mut self, cx: &mut Context<Self>) {
        if let Some(elicitation) = self.pending_elicitation.take() {
            elicitation.respond(ElicitationDecision::Decline);
        }
        cx.notify();
    }

    pub fn accept_elicitation(&mut self, cx: &mut Context<Self>) {
        let raw = self.elicitation_input.read(cx).content.clone();
        let parsed = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw);
        let content = match parsed {
            Ok(object) => {
                let mut content = std::collections::BTreeMap::new();
                for (key, value) in object {
                    let Some(value) = json_elicitation_value(value) else {
                        self.toast = Some(format!(
                            "Unsupported elicitation value for {key}; use strings, numbers, booleans, or string arrays"
                        ));
                        cx.notify();
                        return;
                    };
                    content.insert(key, value);
                }
                content
            }
            Err(error) => {
                self.toast = Some(format!("Invalid elicitation response: {error}"));
                cx.notify();
                return;
            }
        };
        if let Some(elicitation) = self.pending_elicitation.take() {
            elicitation.respond(ElicitationDecision::Accept(content));
            self.elicitation_input.update(cx, |input, _| input.clear());
        }
        cx.notify();
    }

    fn create_turn_checkpoint(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.agent_config.clone() else {
            return;
        };
        let label = format!("Checkpoint {}", self.checkpoints.len() + 1);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || CheckpointManager::new(config).create(&label))
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
                            app.checkpoints.push(checkpoint);
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
        let Some(checkpoint) = self
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.label == label)
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
        self.toast = Some(format!("Restoring {label}…"));
        let restored_label = label.to_string();
        self.show_checkpoints = false;
        self.toast_generation += 1;
        let generation = self.toast_generation;
        let (sender, rx) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CheckpointManager::new(config).restore(&checkpoint)
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

    // ---- diff / files ----

    pub fn changed_files(&self) -> &'static [FileEntry] {
        CHANGED_FILES
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .font_family(theme::FONT_FAMILY)
            .text_size(px(13.))
            .overflow_hidden()
            .child(match self.screen {
                Screen::Onboarding => {
                    screens::onboarding::render(self, window, cx).into_any_element()
                }
                Screen::Projects => screens::projects::render(self, window, cx).into_any_element(),
                Screen::Workspace => {
                    screens::workspace::render(self, window, cx).into_any_element()
                }
                Screen::Settings => screens::settings::render(self, window, cx).into_any_element(),
            })
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn render_acp_diff(diff: agent_client_protocol::schema::v1::Diff) -> Diff {
    use similar::{ChangeTag, TextDiff};

    let old = diff.old_text.unwrap_or_default();
    let patch = TextDiff::from_lines(&old, &diff.new_text);
    let mut added = 0;
    let mut removed = 0;
    let lines = patch
        .iter_all_changes()
        .map(|change| {
            let (kind, text) = match change.tag() {
                ChangeTag::Equal => (DiffLineKind::Ctx, change.value()),
                ChangeTag::Delete => {
                    removed += 1;
                    (DiffLineKind::Del, change.value())
                }
                ChangeTag::Insert => {
                    added += 1;
                    (DiffLineKind::Add, change.value())
                }
            };
            DiffLine {
                kind,
                text: text.trim_end_matches('\n').to_string(),
            }
        })
        .collect();
    Diff {
        file: diff.path.to_string_lossy().into_owned(),
        stat: format!("+{added} -{removed}"),
        hunk_header: "@@ ACP structured edit @@".into(),
        lines,
    }
}

fn extract_model_options(value: &serde_json::Value) -> Vec<(String, String, String)> {
    fn visit(value: &serde_json::Value, output: &mut Vec<(String, String, String)>) {
        match value {
            serde_json::Value::Object(object) => {
                let is_model = object
                    .get("category")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|category| category.eq_ignore_ascii_case("model"));
                if is_model {
                    let config_id = object
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("model");
                    if let Some(options) =
                        object.get("options").and_then(serde_json::Value::as_array)
                    {
                        for option in options {
                            if let (Some(name), Some(value)) = (
                                option
                                    .get("name")
                                    .or_else(|| option.get("label"))
                                    .and_then(serde_json::Value::as_str),
                                option.get("value").and_then(serde_json::Value::as_str),
                            ) && !output.iter().any(|(existing, _, _)| existing == name)
                            {
                                output.push((
                                    name.to_string(),
                                    config_id.to_string(),
                                    value.to_string(),
                                ));
                            } else if let Some(value) = option.as_str()
                                && !output.iter().any(|(existing, _, _)| existing == value)
                            {
                                output.push((
                                    value.to_string(),
                                    config_id.to_string(),
                                    value.to_string(),
                                ));
                            }
                        }
                    }
                }
                object.values().for_each(|child| visit(child, output));
            }
            serde_json::Value::Array(array) => {
                array.iter().for_each(|child| visit(child, output));
            }
            _ => {}
        }
    }

    let mut output = vec![];
    visit(value, &mut output);
    output
}

fn json_elicitation_value(
    value: serde_json::Value,
) -> Option<agent_client_protocol::schema::v1::ElicitationContentValue> {
    use agent_client_protocol::schema::v1::ElicitationContentValue;
    match value {
        serde_json::Value::String(value) => Some(ElicitationContentValue::String(value)),
        serde_json::Value::Bool(value) => Some(ElicitationContentValue::Boolean(value)),
        serde_json::Value::Number(value) if value.is_i64() => {
            Some(ElicitationContentValue::Integer(value.as_i64()?))
        }
        serde_json::Value::Number(value) => Some(ElicitationContentValue::Number(value.as_f64()?)),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(ElicitationContentValue::StringArray),
        _ => None,
    }
}
