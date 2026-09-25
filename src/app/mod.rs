mod acp_events;
mod checkpoints;
mod comments;
mod elicitation;
mod permissions;
mod session_config;

#[cfg(test)]
pub(crate) use checkpoints::{find_checkpoint, next_checkpoint_label};
pub use comments::CommentDraft;
#[cfg(test)]
pub(crate) use comments::{comment_reference_label, comment_scope_prefix, scoped_comment_key};
#[cfg(test)]
pub(crate) use elicitation::json_elicitation_value;
#[allow(unused_imports)] // exported for the workspace elicitation renderer (finding B8)
pub(crate) use elicitation::{FieldShape, field_shape};
pub use session_config::SessionConfigControl;
#[cfg(test)]
pub(crate) use session_config::session_config_controls;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{Context, Entity, PathPromptOptions, ScrollHandle, Window, div, px, rgb};

use crate::acp::{AcpClient, AnchoredComment, PendingElicitation, PendingPermission, TurnInput};
use crate::async_runtime;
use crate::checkpoint::{Checkpoint, CheckpointManager};
use crate::config::{AgentConfig, AgentProfile, AgentRegistry, ConfigStore, TransportConfig};
use crate::persistence::{PersistedState, StateStore};
use crate::screens;
use crate::state::*;
use crate::text_input::TextInput;
use crate::theme;

#[derive(Clone)]
pub struct WorkspaceFile {
    pub path: String,
    pub status: Option<String>,
    pub supports_text_diff: bool,
}

pub struct App {
    pub screen: Screen,
    pub projects: Vec<Project>,
    pub sessions_by_project: HashMap<String, Vec<Session>>,
    pub active_project: Option<Project>,
    pub active_session_id: Option<String>,

    pub session_config_controls: Vec<SessionConfigControl>,
    pub open_config_menu: Option<String>,
    pub config_search_input: Entity<TextInput>,
    pub agent_profiles: Vec<AgentProfile>,
    pub agent_menu_open: bool,

    pub sidebar_visible: bool,
    pub file_tree_visible: bool,
    pub collapsed_projects: HashSet<String>,
    pub expanded_archives: HashSet<String>,
    pub pending_session_delete: Option<String>,

    pub active_diff_file: Option<String>,
    pub file_diffs: HashMap<String, Diff>,
    pub changed_files: Vec<WorkspaceFile>,

    pub composer: Entity<TextInput>,
    pub context_chips: Vec<String>,
    pub thinking: bool,
    pub connection_status: String,
    pub context_usage: Option<u8>,
    pub acp_client: Option<AcpClient>,
    /// Identifies the ACP event stream that belongs to the current session.
    /// Dropping a client closes it asynchronously, so older streams can still
    /// have queued events after the user switches sessions.
    connection_generation: u64,
    /// Agent registry loaded once at startup and kept in sync when agents are
    /// registered or removed, so reconnecting does not hit the disk.
    config_cache: Option<AgentRegistry>,
    pub agent_config: Option<AgentConfig>,
    pub pending_permission: Option<PendingPermission>,
    pub pending_elicitation: Option<PendingElicitation>,
    pub elicitation_inputs: HashMap<String, Entity<TextInput>>,
    pub elicitation_values: HashMap<String, serde_json::Value>,
    pub sent_count: usize,

    pub show_checkpoints: bool,
    pub toast: Option<String>,
    pub toast_generation: u64,

    pub expanded_steps: HashSet<String>,
    pub expanded_tool_calls: HashSet<String>,
    pub comment_drafts: HashMap<String, CommentDraft>,
    pub comments: HashMap<String, Vec<Comment>>,
    pub selection_focus: gpui::FocusHandle,

    pub thread_scroll: ScrollHandle,
    pub diff_scroll: ScrollHandle,
    pub checkpoints_by_project: HashMap<String, Vec<Checkpoint>>,

    pub config_remote: bool,
    pub validating_agent_config: bool,
    pub agent_profile_name_input: Entity<TextInput>,
    pub agent_command_input: Entity<TextInput>,
    pub remote_workspace_input: Entity<TextInput>,
    pub ssh_host_input: Entity<TextInput>,
    pub ssh_user_input: Entity<TextInput>,
    pub ssh_key_input: Entity<TextInput>,
}

impl App {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let persisted = StateStore::for_user().load().unwrap_or_default();
        let (agent_profiles, config_error, registry) = match ConfigStore::for_user().load() {
            Ok(Some(registry)) => {
                let agents = registry.agents.clone();
                (agents, None, Some(registry))
            }
            Ok(None) => (vec![], None, Some(AgentRegistry::default())),
            Err(error) => (
                vec![],
                Some(format!("Could not load agents: {error}")),
                None,
            ),
        };
        let mut app = Self::from_state(persisted, agent_profiles, config_error, cx);
        app.config_cache = registry;
        app
    }

    pub(crate) fn from_state(
        persisted: PersistedState,
        agent_profiles: Vec<AgentProfile>,
        config_error: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let weak = cx.weak_entity();
        let composer = cx.new(|cx| {
            TextInput::new(cx, "Message the agent... (Cmd+Enter to send)")
                .multiline()
                .on_submit(move |text, _window, cx| {
                    let text = text.to_string();
                    weak.update(cx, |app, cx| app.send_message(text, cx)).ok();
                })
        });
        let agent_profile_name_input = cx.new(|cx| TextInput::new(cx, "e.g. Claude Code"));
        let search_owner = cx.weak_entity();
        let config_search_input = cx.new(|cx| {
            TextInput::new(cx, "Search models").on_change(move |_, cx| {
                search_owner.update(cx, |_, cx| cx.notify()).ok();
            })
        });
        let agent_command_input = cx.new(|cx| TextInput::new(cx, "e.g. gemini --experimental-acp"));
        let remote_workspace_input = cx.new(|cx| TextInput::new(cx, "/path/on/remote/machine"));
        let ssh_host_input = cx.new(|cx| TextInput::new(cx, "host from ~/.ssh/config"));
        let ssh_user_input = cx.new(|cx| TextInput::new(cx, "optional SSH user"));
        let ssh_key_input = cx.new(|cx| TextInput::new(cx, "optional identity file"));
        let screen = if persisted.projects.is_empty() {
            Screen::Onboarding
        } else {
            Screen::Projects
        };

        App {
            screen,
            projects: persisted.projects,
            sessions_by_project: persisted.sessions_by_project,
            active_project: None,
            active_session_id: None,
            session_config_controls: vec![],
            open_config_menu: None,
            config_search_input,
            agent_profiles,
            agent_menu_open: false,
            sidebar_visible: true,
            file_tree_visible: true,
            collapsed_projects: HashSet::new(),
            expanded_archives: HashSet::new(),
            pending_session_delete: None,
            active_diff_file: None,
            file_diffs: HashMap::new(),
            changed_files: vec![],
            composer,
            context_chips: vec![],
            thinking: false,
            connection_status: "Not configured".into(),
            context_usage: None,
            acp_client: None,
            connection_generation: 0,
            config_cache: None,
            agent_config: None,
            pending_permission: None,
            pending_elicitation: None,
            elicitation_inputs: HashMap::new(),
            elicitation_values: HashMap::new(),
            sent_count: 0,
            show_checkpoints: false,
            toast: config_error,
            toast_generation: 0,
            expanded_steps: HashSet::new(),
            expanded_tool_calls: HashSet::new(),
            comment_drafts: HashMap::new(),
            comments: persisted.comments,
            selection_focus: cx.focus_handle(),
            thread_scroll: ScrollHandle::new(),
            diff_scroll: ScrollHandle::new(),
            checkpoints_by_project: persisted.checkpoints_by_project,
            config_remote: false,
            validating_agent_config: false,
            agent_profile_name_input,
            agent_command_input,
            remote_workspace_input,
            ssh_host_input,
            ssh_user_input,
            ssh_key_input,
        }
    }

    // ---- navigation ----

    pub fn go_projects(&mut self, cx: &mut Context<Self>) {
        self.screen = Screen::Projects;
        cx.notify();
    }

    pub fn select_workspace(&mut self, cx: &mut Context<Self>) {
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open workspace".into()),
        });
        cx.spawn(async move |this, cx| match selection.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |app, cx| app.add_workspace(path, cx)).ok();
                }
            }
            Ok(Err(error)) => {
                this.update(cx, |app, cx| {
                    app.toast = Some(format!("Could not open workspace picker: {error}"));
                    cx.notify();
                })
                .ok();
            }
            _ => {}
        })
        .detach();
    }

    fn add_workspace(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Some(project_id) = self
            .projects
            .iter()
            .find(|project| std::path::Path::new(&project.path) == path)
            .map(|project| project.id.clone())
        {
            self.open_project(&project_id, cx);
            return;
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace")
            .to_string();
        self.projects.push(Project {
            id: id.clone(),
            name,
            path: path.to_string_lossy().into_owned(),
            desc: "Local workspace".into(),
            updated: "Just added".into(),
            checkpoint_label: "No checkpoints".into(),
            model: "Agent not configured".into(),
        });
        self.sessions_by_project.insert(
            id.clone(),
            vec![Session {
                id: uuid::Uuid::new_v4().simple().to_string(),
                name: "New session".into(),
                messages: vec![],
                acp_session_id: None,
                archived: false,
                agent_profile_id: None,
            }],
        );
        self.persist_state();
        self.open_project(&id, cx);
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
            .and_then(|sessions| sessions.iter().find(|session| !session.archived))
            .map(|s| s.id.clone());
        self.active_project = Some(project);
        self.active_session_id = session_id;
        self.thread_scroll.scroll_to_bottom();
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        self.file_diffs.clear();
        self.changed_files.clear();
        self.agent_config = None;
        self.refresh_workspace(cx);
        self.connect_project(cx);
        cx.notify();
    }

    pub fn open_session(&mut self, project_id: &str, session_id: &str, cx: &mut Context<Self>) {
        let already_active = self
            .active_project
            .as_ref()
            .is_some_and(|project| project.id == project_id)
            && self.active_session_id.as_deref() == Some(session_id);
        if already_active {
            return;
        }

        let Some(project) = self.projects.iter().find(|p| p.id == project_id).cloned() else {
            return;
        };
        let is_available = self
            .sessions_by_project
            .get(project_id)
            .is_some_and(|sessions| {
                sessions
                    .iter()
                    .any(|session| session.id == session_id && !session.archived)
            });
        if !is_available {
            return;
        }
        self.active_project = Some(project);
        self.active_session_id = Some(session_id.to_string());
        self.thread_scroll.scroll_to_bottom();
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
        let new_id = new_id("s");
        let sessions = self
            .sessions_by_project
            .entry(project_id.to_string())
            .or_default();
        sessions.push(Session {
            id: new_id.clone(),
            name: "New session".into(),
            messages: vec![],
            acp_session_id: None,
            archived: false,
            agent_profile_id: None,
        });
        self.active_project = Some(project);
        self.active_session_id = Some(new_id);
        self.thread_scroll.scroll_to_bottom();
        self.show_checkpoints = false;
        self.context_chips.clear();
        self.screen = Screen::Workspace;
        self.persist_state();
        self.connect_project(cx);
        cx.notify();
    }

    pub fn toggle_archived_sessions(&mut self, project_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_archives.insert(project_id.to_string()) {
            self.expanded_archives.remove(project_id);
        }
        self.pending_session_delete = None;
        cx.notify();
    }

    pub fn archive_session(&mut self, project_id: &str, session_id: &str, cx: &mut Context<Self>) {
        let was_active = self
            .active_project
            .as_ref()
            .map(|project| project.id.as_str())
            == Some(project_id)
            && self.active_session_id.as_deref() == Some(session_id);
        if was_active && self.thinking {
            self.toast = Some("Stop the running turn before archiving this session".into());
            cx.notify();
            return;
        }
        let Some(sessions) = self.sessions_by_project.get_mut(project_id) else {
            return;
        };
        let Some(session) = sessions.iter_mut().find(|session| session.id == session_id) else {
            return;
        };
        session.archived = true;

        if was_active {
            let next_id = sessions
                .iter()
                .find(|session| !session.archived)
                .map(|session| session.id.clone())
                .unwrap_or_else(|| {
                    let id = new_id("s");
                    sessions.push(Session {
                        id: id.clone(),
                        name: "New session".into(),
                        messages: vec![],
                        acp_session_id: None,
                        archived: false,
                        agent_profile_id: None,
                    });
                    id
                });
            self.active_session_id = Some(next_id);
            self.context_chips.clear();
            self.connect_project(cx);
        }
        self.pending_session_delete = None;
        self.persist_state();
        cx.notify();
    }

    pub fn restore_session(&mut self, project_id: &str, session_id: &str, cx: &mut Context<Self>) {
        if let Some(session) = self
            .sessions_by_project
            .get_mut(project_id)
            .and_then(|sessions| sessions.iter_mut().find(|session| session.id == session_id))
        {
            session.archived = false;
            self.pending_session_delete = None;
            self.persist_state();
            cx.notify();
        }
    }

    pub fn request_delete_session(
        &mut self,
        project_id: &str,
        session_id: &str,
        cx: &mut Context<Self>,
    ) {
        if self.pending_session_delete.as_deref() != Some(session_id) {
            self.pending_session_delete = Some(session_id.to_string());
            cx.notify();
            return;
        }

        if let Some(sessions) = self.sessions_by_project.get_mut(project_id) {
            sessions.retain(|session| session.id != session_id || !session.archived);
        }
        self.pending_session_delete = None;
        self.persist_state();
        cx.notify();
    }

    pub fn new_session(&mut self, cx: &mut Context<Self>) {
        self.select_workspace(cx);
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

    pub fn toggle_config_menu(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.open_config_menu.as_deref() == Some(id) {
            self.open_config_menu = None;
        } else {
            self.open_config_menu = Some(id.to_string());
        }
        self.config_search_input
            .update(cx, |input, _| input.clear());
        cx.notify();
    }

    pub fn toggle_config_remote(&mut self, cx: &mut Context<Self>) {
        self.config_remote = !self.config_remote;
        cx.notify();
    }

    pub fn toggle_agent_menu(&mut self, cx: &mut Context<Self>) {
        if self.can_select_agent_for_session() {
            self.agent_menu_open = !self.agent_menu_open;
            cx.notify();
        }
    }

    pub fn can_select_agent_for_session(&self) -> bool {
        self.active_session().is_some_and(|session| {
            session.agent_profile_id.is_none()
                || (session.messages.is_empty() && session.acp_session_id.is_none())
        })
    }

    pub fn active_agent_profile_id(&self) -> Option<&str> {
        self.active_session()
            .and_then(|session| session.agent_profile_id.as_deref())
    }

    fn active_session(&self) -> Option<&Session> {
        let project_id = self.active_project.as_ref().map(|project| &project.id)?;
        self.sessions_by_project
            .get(project_id)?
            .iter()
            .find(|session| Some(&session.id) == self.active_session_id.as_ref())
    }

    pub fn select_agent_profile(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        if !self.can_select_agent_for_session()
            || !self
                .agent_profiles
                .iter()
                .any(|profile| profile.id == profile_id)
        {
            return;
        }
        let project_id = self.current_thread_key();
        if let Some(session) = self
            .sessions_by_project
            .get_mut(&project_id)
            .and_then(|sessions| {
                sessions
                    .iter_mut()
                    .find(|session| Some(&session.id) == self.active_session_id.as_ref())
            })
        {
            session.agent_profile_id = Some(profile_id.to_string());
        }
        self.agent_menu_open = false;
        self.persist_state();
        self.connect_project(cx);
    }

    pub fn toggle_checkpoints(&mut self, cx: &mut Context<Self>) {
        self.show_checkpoints = !self.show_checkpoints;
        cx.notify();
    }

    pub fn toggle_diff_file(&mut self, path: &str, cx: &mut Context<Self>) {
        if self
            .changed_files
            .iter()
            .any(|file| file.path == path && !file.supports_text_diff)
        {
            return;
        }
        if self.active_diff_file.as_deref() == Some(path) {
            self.active_diff_file = None;
            cx.notify();
            return;
        }
        self.active_diff_file = Some(path.to_string());
        if self.file_diffs.contains_key(path) {
            cx.notify();
            return;
        }
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let config = self.agent_config.clone().unwrap_or(AgentConfig {
            agent_command: String::new(),
            transport: TransportConfig::Local,
        });
        let local_workspace = PathBuf::from(project.path);
        let diff_path = path.to_string();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CheckpointManager::new(config, local_workspace)
                    .file_snapshot(&diff_path)
                    .map(|(old_text, new_text)| {
                        render_acp_diff(
                            agent_client_protocol::schema::v1::Diff::new(&diff_path, new_text)
                                .old_text(old_text),
                        )
                    })
            })
            .await
            .unwrap_or_else(|error| Err(format!("diff task failed: {error}")));
            let _ = sender.send(result);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = receiver.await {
                this.update(cx, |app, cx| {
                    match result {
                        Ok(diff) => {
                            app.file_diffs.insert(diff.file.clone(), diff);
                        }
                        Err(error) => app.toast = Some(error),
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    pub fn add_context_file(&mut self, path: &str, cx: &mut Context<Self>) {
        let workspace_path = self
            .active_project
            .as_ref()
            .map(|project| {
                let local = PathBuf::from(&project.path);
                self.agent_config
                    .as_ref()
                    .map(|config| config.workspace_for(&local))
                    .unwrap_or(local)
                    .join(path)
            })
            .unwrap_or_else(|| PathBuf::from(path));
        let path = workspace_path.to_string_lossy().into_owned();
        if !self.context_chips.contains(&path) {
            self.context_chips.push(path);
        }
        cx.notify();
    }

    pub fn toggle_steps(&mut self, msg_id: &str, cx: &mut Context<Self>) {
        if !self.expanded_steps.insert(msg_id.to_string()) {
            self.expanded_steps.remove(msg_id);
        }
        cx.notify();
    }

    pub fn toggle_tool_call(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded_tool_calls.insert(id.to_string()) {
            self.expanded_tool_calls.remove(id);
        }
        cx.notify();
    }

    // ---- state / persistence ----

    pub(crate) fn persist_state(&mut self) {
        let state = PersistedState {
            projects: self.projects.clone(),
            sessions_by_project: self.sessions_by_project.clone(),
            comments: self.comments.clone(),
            checkpoints_by_project: self.checkpoints_by_project.clone(),
        };
        let _ = state_saver().send(StateSaveRequest::Snapshot(state));
    }

    pub fn send_message(&mut self, text: String, cx: &mut Context<Self>) {
        if self.thinking {
            self.toast = Some("Stop the running turn before sending another message".into());
            cx.notify();
            return;
        }
        let Some(client) = self.acp_client.clone() else {
            self.toast = Some("Select an agent for this session before sending a message".into());
            cx.notify();
            return;
        };
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
        let id = new_id("u");
        thread.push(Message {
            id,
            role: Role::User,
            body: text.clone().into(),
            time: None,
            context: chips.clone(),
            attached_comments,
            checkpoint_label: None,
            steps: None,
            diff: None,
            terminal: None,
        });
        self.update_active_messages(thread);
        self.thread_scroll.scroll_to_bottom();
        let session_title: String = text.trim().chars().take(48).collect();
        let project_id = self.current_thread_key();
        if let Some(session) = self
            .sessions_by_project
            .get_mut(&project_id)
            .and_then(|sessions| {
                sessions
                    .iter_mut()
                    .find(|session| Some(&session.id) == self.active_session_id.as_ref())
            })
            && session.name == "New session"
        {
            session.name = session_title;
        }

        for p in &pending {
            if let Some(list) = self.comments.get_mut(&p.key)
                && let Some(c) = list.get_mut(p.idx)
            {
                c.pending = false;
            }
        }

        self.context_chips.clear();
        self.sent_count += 1;
        let turn = TurnInput {
            text,
            context_files: chips,
            comments: pending
                .iter()
                .map(|comment| AnchoredComment {
                    target: comment.label.clone(),
                    body: comment.text.clone(),
                })
                .collect(),
        };
        if let Err(error) = client.prompt(turn) {
            self.connection_status = error;
        }
        self.persist_state();

        cx.notify();
    }

    pub fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        if let Some(client) = &self.acp_client
            && let Err(error) = client.cancel()
        {
            self.connection_status = error;
        }
        cx.notify();
    }

    pub(crate) fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
        let Some(project) = self.active_project.clone() else {
            self.changed_files.clear();
            return;
        };
        let config = self.agent_config.clone().unwrap_or(AgentConfig {
            agent_command: String::new(),
            transport: TransportConfig::Local,
        });
        let local_workspace = PathBuf::from(project.path);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CheckpointManager::new(config, local_workspace).changed_files()
            })
            .await
            .unwrap_or_else(|error| Err(format!("workspace refresh failed: {error}")));
            let _ = sender.send(result);
        });
        cx.spawn(async move |this, cx| {
            if let Ok(result) = receiver.await {
                this.update(cx, |app, cx| {
                    match result {
                        Ok(files) => {
                            app.changed_files = files
                                .into_iter()
                                .map(|(path, status, supports_text_diff)| WorkspaceFile {
                                    path,
                                    status: Some(status),
                                    supports_text_diff,
                                })
                                .collect();
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

    // ---- diff / files ----

    pub fn changed_files(&self) -> &[WorkspaceFile] {
        &self.changed_files
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("app")
            .track_focus(&self.selection_focus)
            .size_full()
            .relative()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .font_family(theme::FONT_FAMILY)
            .text_size(px(12.))
            .overflow_hidden()
            .on_key_down(cx.listener(|app, event: &gpui::KeyDownEvent, window, cx| {
                // Selection owns keyboard focus until it hands off to a draft.
                // Focused inputs still own their own typing and copy shortcuts.
                if !app.selection_focus.is_focused(window) {
                    return;
                }
                let shortcut =
                    event.keystroke.modifiers.platform || event.keystroke.modifiers.control;
                if shortcut
                    && event.keystroke.key == "c"
                    && crate::selectable_text::copy_active_selection(cx)
                {
                    cx.stop_propagation();
                    return;
                }
                if let Some(typed) =
                    crate::selectable_text::comment_text_for_keystroke(&event.keystroke)
                    && let Some((target, selection)) =
                        crate::selectable_text::take_comment_selection()
                {
                    app.start_selection_comment(target, selection, typed, window, cx);
                    cx.stop_propagation();
                }
            }))
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
            .when_some(self.toast.clone(), |root, message| {
                root.child(
                    div()
                        .id("global-toast")
                        .absolute()
                        .bottom(px(16.))
                        .right(px(16.))
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::SELECTION))
                        .rounded(px(7.))
                        .text_size(px(12.))
                        .px(px(12.))
                        .py(px(8.))
                        .cursor_pointer()
                        .child(message)
                        .on_click(cx.listener(|app, _, _, cx| {
                            app.toast = None;
                            cx.notify();
                        })),
                )
            })
    }
}

pub(crate) fn new_id(prefix: &str) -> String {
    format!("{prefix}{}", uuid::Uuid::new_v4().simple())
}

fn state_saver() -> std::sync::mpsc::Sender<StateSaveRequest> {
    static STATE_SAVER: once_cell::sync::Lazy<std::sync::mpsc::Sender<StateSaveRequest>> =
        once_cell::sync::Lazy::new(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<StateSaveRequest>();
            std::thread::spawn(move || {
                while let Ok(request) = receiver.recv() {
                    match request {
                        StateSaveRequest::Snapshot(state) => {
                            if let Err(error) = StateStore::for_user().save(&state) {
                                eprintln!("Could not save Craft state: {error}");
                            }
                        }
                        StateSaveRequest::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            });
            sender
        });
    STATE_SAVER.clone()
}

enum StateSaveRequest {
    Snapshot(PersistedState),
    Flush(std::sync::mpsc::Sender<()>),
}

/// Block until every snapshot queued so far has been written to disk.
pub fn flush_state_saves() {
    let (ack_sender, ack_receiver) = std::sync::mpsc::channel();
    if state_saver()
        .send(StateSaveRequest::Flush(ack_sender))
        .is_ok()
    {
        let _ = ack_receiver.recv();
    }
}

pub(crate) fn render_acp_diff(diff: agent_client_protocol::schema::v1::Diff) -> Diff {
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

#[cfg(test)]
mod session_config_tests;
