use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ContentBlock, ElicitationMode, ElicitationPropertySchema, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions, SessionUpdate, ToolCallContent,
};
use gpui::prelude::*;
use gpui::{Context, Entity, PathPromptOptions, ScrollHandle, Window, div, px, rgb};

use crate::acp::{
    AcpClient, AcpEvent, AnchoredComment, ElicitationDecision, PendingElicitation,
    PendingPermission, PermissionDecision, TurnInput, validate_agent,
};
use crate::async_runtime;
use crate::checkpoint::{Checkpoint, CheckpointManager};
use crate::config::{AgentConfig, AgentProfile, AgentRegistry, ConfigStore, TransportConfig};
use crate::persistence::{PersistedState, StateStore};
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

#[derive(Clone, Debug)]
pub struct WorkspaceFile {
    pub path: String,
    pub status: Option<String>,
    pub supports_text_diff: bool,
}

#[derive(Clone, Debug)]
pub struct SessionConfigChoice {
    pub name: String,
    pub value: SessionConfigOptionValue,
}

#[derive(Clone, Debug)]
pub struct SessionConfigControl {
    pub id: String,
    pub name: String,
    pub selected_name: String,
    pub choices: Vec<SessionConfigChoice>,
    pub searchable: bool,
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
    pub open_comment_boxes: HashSet<String>,
    pub comment_inputs: HashMap<String, Entity<TextInput>>,
    pub comments: HashMap<String, Vec<Comment>>,

    pub thread_scroll: ScrollHandle,
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
        let persisted = StateStore::for_user().load().unwrap_or_default();
        let (agent_profiles, config_error) = match ConfigStore::for_user().load() {
            Ok(Some(registry)) => (registry.agents, None),
            Ok(None) => (vec![], None),
            Err(error) => (vec![], Some(format!("Could not load agents: {error}"))),
        };
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
            open_comment_boxes: HashSet::new(),
            comment_inputs: HashMap::new(),
            comments: persisted.comments,
            thread_scroll: ScrollHandle::new(),
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
        let new_id = format!("s{}", now_ms());
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
                    let id = format!("s{}", now_ms());
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

    pub fn select_session_config(
        &mut self,
        id: String,
        value: SessionConfigOptionValue,
        cx: &mut Context<Self>,
    ) {
        if let Some(client) = &self.acp_client
            && let Err(error) = client.set_config(id, value)
        {
            self.connection_status = error;
            cx.notify();
            return;
        }
        self.open_config_menu = None;
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

    pub fn save_agent_config(&mut self, cx: &mut Context<Self>) {
        if self.validating_agent_config {
            return;
        }
        let name = self
            .agent_profile_name_input
            .read(cx)
            .content
            .trim()
            .to_string();
        let command = self.agent_command_input.read(cx).content.trim().to_string();
        let remote_workspace = self
            .remote_workspace_input
            .read(cx)
            .content
            .trim()
            .to_string();
        let host = self.ssh_host_input.read(cx).content.trim().to_string();
        let user = self.ssh_user_input.read(cx).content.trim().to_string();
        let key = self.ssh_key_input.read(cx).content.trim().to_string();
        if name.is_empty() || command.is_empty() {
            self.toast = Some("Agent name and command are required".into());
            cx.notify();
            return;
        }
        if self.config_remote && (host.is_empty() || remote_workspace.is_empty()) {
            self.toast = Some("SSH host and remote workspace are required".into());
            cx.notify();
            return;
        }
        let transport = if self.config_remote {
            TransportConfig::Ssh {
                host,
                user: (!user.is_empty()).then_some(user),
                identity_file: (!key.is_empty()).then(|| PathBuf::from(key)),
                remote_workspace: PathBuf::from(remote_workspace),
            }
        } else {
            TransportConfig::Local
        };
        let profile = AgentProfile {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name,
            config: AgentConfig {
                agent_command: command,
                transport,
            },
        };
        if self
            .agent_profiles
            .iter()
            .any(|registered| registered.name.eq_ignore_ascii_case(&profile.name))
        {
            self.toast = Some("An agent with this name is already registered".into());
            cx.notify();
            return;
        }
        self.validating_agent_config = true;
        self.toast = Some("Checking agent command…".into());
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let config = profile.config.clone();
        async_runtime::spawn(async move {
            let _ = sender.send(validate_agent(config).await);
        });
        cx.spawn(async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err("agent validation task stopped unexpectedly".into()));
            this.update(cx, |app, cx| {
                app.finish_agent_registration(profile, result, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_agent_registration(
        &mut self,
        profile: AgentProfile,
        validation: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.validating_agent_config = false;
        if let Err(error) = validation {
            self.toast = Some(format!("Could not register agent: {error}"));
            cx.notify();
            return;
        }
        let mut registry = AgentRegistry {
            agents: self.agent_profiles.clone(),
        };
        if registry
            .agents
            .iter()
            .any(|registered| registered.name.eq_ignore_ascii_case(&profile.name))
        {
            self.toast = Some("An agent with this name is already registered".into());
            cx.notify();
            return;
        }
        registry.agents.push(profile);
        match ConfigStore::for_user().save(&registry) {
            Ok(()) => {
                self.agent_profiles = registry.agents;
                self.agent_profile_name_input
                    .update(cx, |input, _| input.clear());
                self.agent_command_input
                    .update(cx, |input, _| input.clear());
                self.remote_workspace_input
                    .update(cx, |input, _| input.clear());
                self.ssh_host_input.update(cx, |input, _| input.clear());
                self.ssh_user_input.update(cx, |input, _| input.clear());
                self.ssh_key_input.update(cx, |input, _| input.clear());
                self.config_remote = false;
                if self.can_select_agent_for_session() {
                    self.connection_status = "Select an agent".into();
                }
                self.toast = Some("Agent registered".into());
            }
            Err(error) => self.toast = Some(format!("Could not register agent: {error}")),
        }
        cx.notify();
    }

    pub fn delete_agent_profile(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        let in_use = self
            .sessions_by_project
            .values()
            .flatten()
            .any(|session| session.agent_profile_id.as_deref() == Some(profile_id));
        if in_use {
            self.toast = Some("This agent is used by a saved session and cannot be removed".into());
            cx.notify();
            return;
        }
        let remaining_profiles = self
            .agent_profiles
            .iter()
            .filter(|profile| profile.id != profile_id)
            .cloned()
            .collect::<Vec<_>>();
        let registry = AgentRegistry {
            agents: remaining_profiles,
        };
        match ConfigStore::for_user().save(&registry) {
            Ok(()) => {
                self.agent_profiles = registry.agents;
                if self.agent_profiles.is_empty() {
                    self.connection_status = "No agents registered".into();
                }
                self.toast = Some("Agent removed".into());
            }
            Err(error) => self.toast = Some(format!("Could not remove agent: {error}")),
        }
        cx.notify();
    }

    fn connect_project(&mut self, cx: &mut Context<Self>) {
        self.acp_client = None;
        self.agent_config = None;
        self.agent_menu_open = false;
        self.pending_permission = None;
        self.pending_elicitation = None;
        self.elicitation_inputs.clear();
        self.elicitation_values.clear();
        self.context_usage = None;
        self.session_config_controls.clear();
        self.open_config_menu = None;
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let registry = match ConfigStore::for_user().load() {
            Ok(Some(registry)) => registry,
            Ok(None) => {
                self.agent_profiles.clear();
                self.connection_status = "No agents registered".into();
                self.refresh_workspace(cx);
                return;
            }
            Err(error) => {
                self.connection_status = format!("Config error: {error}");
                return;
            }
        };
        self.agent_profiles = registry.agents;
        let selected_profile_id = self
            .sessions_by_project
            .get_mut(&project.id)
            .and_then(|sessions| {
                sessions
                    .iter_mut()
                    .find(|session| Some(&session.id) == self.active_session_id.as_ref())
            })
            .and_then(|session| {
                if session.agent_profile_id.is_none()
                    && (session.acp_session_id.is_some() || !session.messages.is_empty())
                    && self.agent_profiles.len() == 1
                {
                    session.agent_profile_id = Some(self.agent_profiles[0].id.clone());
                }
                session.agent_profile_id.clone()
            });
        let Some(profile_id) = selected_profile_id else {
            self.connection_status = if self.agent_profiles.is_empty() {
                "No agents registered".into()
            } else {
                "Select an agent".into()
            };
            self.persist_state();
            self.refresh_workspace(cx);
            return;
        };
        let Some(profile) = self
            .agent_profiles
            .iter()
            .find(|profile| profile.id == profile_id)
            .cloned()
        else {
            self.connection_status = "Session agent is no longer registered".into();
            self.refresh_workspace(cx);
            return;
        };
        let config = profile.config;
        if matches!(
            &config.transport,
            TransportConfig::Ssh {
                remote_workspace,
                ..
            } if remote_workspace.as_os_str().is_empty()
        ) {
            self.connection_status =
                "SSH agent needs a remote workspace mapping in Settings".into();
            self.agent_config = Some(config);
            return;
        }
        self.agent_config = Some(config.clone());
        self.refresh_workspace(cx);
        let local_workspace = PathBuf::from(&project.path);
        let resume_session = self
            .sessions_by_project
            .get(&project.id)
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| Some(&session.id) == self.active_session_id.as_ref())
            })
            .and_then(|session| session.acp_session_id.clone());
        let (client, mut events) = AcpClient::connect(config, local_workspace, resume_session);
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

    // ---- comments ----

    pub fn comment_key(&self, anchor: &str) -> String {
        scoped_comment_key(self.active_session_id.as_deref(), anchor)
    }

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
            TextInput::new(cx, "Comment...")
                .soft_wrap()
                .on_submit(move |text, _window, cx| {
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
        self.persist_state();
        cx.notify();
    }

    pub fn pending_comments(&self) -> Vec<PendingComment> {
        let mut out = vec![];
        let scope = comment_scope_prefix(self.active_session_id.as_deref());
        for (key, list) in self
            .comments
            .iter()
            .filter(|(key, _)| key.starts_with(&scope))
        {
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
        if let Some(list) = self.comments.get_mut(key)
            && idx < list.len()
        {
            list.remove(idx);
        }
        self.persist_state();
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
        if let Some(sessions) = self.sessions_by_project.get_mut(&key)
            && let Some(sess) = sessions
                .iter_mut()
                .find(|s| Some(&s.id) == self.active_session_id.as_ref())
        {
            sess.messages = new_messages;
        }
    }

    fn persist_state(&mut self) {
        let state = PersistedState {
            projects: self.projects.clone(),
            sessions_by_project: self.sessions_by_project.clone(),
            comments: self.comments.clone(),
            checkpoints_by_project: self.checkpoints_by_project.clone(),
        };
        if let Err(error) = StateStore::for_user().save(&state) {
            self.toast = Some(format!("Could not save Forge state: {error}"));
        }
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
        self.thread_scroll.scroll_to_bottom();
        let session_title: String = self
            .active_messages()
            .last()
            .map(|message| message.text.trim())
            .unwrap_or("Session")
            .chars()
            .take(48)
            .collect();
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
        if let Err(error) = client.prompt(turn) {
            self.connection_status = error;
        }
        self.persist_state();

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
                self.elicitation_inputs.clear();
                self.elicitation_values.clear();
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
                self.set_session_config_options(options.config_options);
            }
            _ => {}
        }
    }

    fn set_session_config_options(&mut self, options: Vec<SessionConfigOption>) {
        self.session_config_controls = session_config_controls(options);
        if self.open_config_menu.as_ref().is_some_and(|open| {
            !self
                .session_config_controls
                .iter()
                .any(|item| &item.id == open)
        }) {
            self.open_config_menu = None;
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
        if let Some(client) = &self.acp_client
            && let Err(error) = client.cancel()
        {
            self.connection_status = error;
        }
        cx.notify();
    }

    pub fn decline_elicitation(&mut self, cx: &mut Context<Self>) {
        if let Some(elicitation) = self.pending_elicitation.take() {
            elicitation.respond(ElicitationDecision::Decline);
        }
        self.elicitation_inputs.clear();
        self.elicitation_values.clear();
        cx.notify();
    }

    fn prepare_elicitation_form(
        &mut self,
        elicitation: &PendingElicitation,
        cx: &mut Context<Self>,
    ) {
        self.elicitation_inputs.clear();
        self.elicitation_values.clear();
        let ElicitationMode::Form(form) = &elicitation.request.mode else {
            return;
        };

        for (key, property) in &form.requested_schema.properties {
            match property {
                ElicitationPropertySchema::String(schema)
                    if schema.enum_values.is_none() && schema.one_of.is_none() =>
                {
                    let default = schema.default.clone().unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "Enter a response");
                        input.set_content(default);
                        input
                    });
                    self.elicitation_inputs.insert(key.clone(), input);
                }
                ElicitationPropertySchema::String(schema) => {
                    if let Some(default) = &schema.default {
                        self.elicitation_values
                            .insert(key.clone(), serde_json::Value::String(default.clone()));
                    }
                }
                ElicitationPropertySchema::Number(schema) => {
                    let default = schema
                        .default
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "Enter a number");
                        input.set_content(default);
                        input
                    });
                    self.elicitation_inputs.insert(key.clone(), input);
                }
                ElicitationPropertySchema::Integer(schema) => {
                    let default = schema
                        .default
                        .map(|value| value.to_string())
                        .unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "Enter a whole number");
                        input.set_content(default);
                        input
                    });
                    self.elicitation_inputs.insert(key.clone(), input);
                }
                ElicitationPropertySchema::Boolean(schema) => {
                    if let Some(default) = schema.default {
                        self.elicitation_values
                            .insert(key.clone(), serde_json::Value::Bool(default));
                    }
                }
                ElicitationPropertySchema::Array(schema) => {
                    if let Some(default) = &schema.default {
                        self.elicitation_values.insert(
                            key.clone(),
                            serde_json::Value::Array(
                                default
                                    .iter()
                                    .cloned()
                                    .map(serde_json::Value::String)
                                    .collect(),
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    pub fn set_elicitation_value(
        &mut self,
        key: String,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.elicitation_values.insert(key, value);
        cx.notify();
    }

    pub fn toggle_elicitation_value(&mut self, key: String, value: String, cx: &mut Context<Self>) {
        let selected = self
            .elicitation_values
            .entry(key)
            .or_insert_with(|| serde_json::Value::Array(vec![]));
        let serde_json::Value::Array(values) = selected else {
            return;
        };
        if let Some(index) = values
            .iter()
            .position(|selected| selected.as_str() == Some(value.as_str()))
        {
            values.remove(index);
        } else {
            values.push(serde_json::Value::String(value));
        }
        cx.notify();
    }

    fn refresh_workspace(&mut self, cx: &mut Context<Self>) {
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

    pub fn accept_elicitation(&mut self, cx: &mut Context<Self>) {
        let Some(elicitation) = self.pending_elicitation.as_ref() else {
            return;
        };
        let ElicitationMode::Form(form) = &elicitation.request.mode else {
            self.toast = Some("This elicitation mode is not supported".into());
            cx.notify();
            return;
        };
        let schema = form.requested_schema.clone();
        let required = schema.required.unwrap_or_default();
        let mut content = std::collections::BTreeMap::new();

        for (key, property) in schema.properties {
            let value = match property {
                ElicitationPropertySchema::String(schema)
                    if schema.enum_values.is_none() && schema.one_of.is_none() =>
                {
                    let value = self
                        .elicitation_inputs
                        .get(&key)
                        .map(|input| input.read(cx).content.trim().to_string())
                        .unwrap_or_default();
                    if value.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::String(value))
                    }
                }
                ElicitationPropertySchema::Number(_) => {
                    let raw = self
                        .elicitation_inputs
                        .get(&key)
                        .map(|input| input.read(cx).content.trim().to_string())
                        .unwrap_or_default();
                    if raw.is_empty() {
                        None
                    } else {
                        match raw.parse::<f64>() {
                            Ok(value) => {
                                serde_json::Number::from_f64(value).map(serde_json::Value::Number)
                            }
                            Err(_) => {
                                self.toast = Some(format!("{key} must be a number"));
                                cx.notify();
                                return;
                            }
                        }
                    }
                }
                ElicitationPropertySchema::Integer(_) => {
                    let raw = self
                        .elicitation_inputs
                        .get(&key)
                        .map(|input| input.read(cx).content.trim().to_string())
                        .unwrap_or_default();
                    if raw.is_empty() {
                        None
                    } else {
                        match raw.parse::<i64>() {
                            Ok(value) => Some(serde_json::Value::Number(value.into())),
                            Err(_) => {
                                self.toast = Some(format!("{key} must be a whole number"));
                                cx.notify();
                                return;
                            }
                        }
                    }
                }
                ElicitationPropertySchema::Other(_) => {
                    self.toast = Some(format!("{key} uses an unsupported field type"));
                    cx.notify();
                    return;
                }
                _ => self.elicitation_values.get(&key).cloned(),
            };

            let missing = value.as_ref().is_none_or(|value| {
                value.as_str().is_some_and(str::is_empty)
                    || value.as_array().is_some_and(Vec::is_empty)
            });
            if required.contains(&key) && missing {
                self.toast = Some(format!("{key} is required"));
                cx.notify();
                return;
            }
            if let Some(value) = value {
                let Some(value) = json_elicitation_value(value) else {
                    self.toast = Some(format!("{key} has an unsupported value"));
                    cx.notify();
                    return;
                };
                content.insert(key, value);
            }
        }

        if let Some(elicitation) = self.pending_elicitation.take() {
            elicitation.respond(ElicitationDecision::Accept(content));
        }
        self.elicitation_inputs.clear();
        self.elicitation_values.clear();
        cx.notify();
    }

    fn create_turn_checkpoint(&mut self, cx: &mut Context<Self>) {
        let Some(config) = self.agent_config.clone() else {
            return;
        };
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let project_id = project.id.clone();
        let local_workspace = PathBuf::from(&project.path);
        let label = format!(
            "Checkpoint {}",
            self.checkpoints_by_project
                .get(&project_id)
                .map_or(1, |checkpoints| checkpoints.len() + 1)
        );
        let (sender, receiver) = tokio::sync::oneshot::channel();
        async_runtime::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                CheckpointManager::new(config, local_workspace).create(&label)
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
        let Some(checkpoint) = self
            .checkpoints_by_project
            .get(&project_id)
            .and_then(|checkpoints| {
                checkpoints
                    .iter()
                    .rev()
                    .find(|checkpoint| checkpoint.label == label)
            })
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

    // ---- diff / files ----

    pub fn changed_files(&self) -> &[WorkspaceFile] {
        &self.changed_files
    }
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .bg(rgb(theme::BG))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .font_family(theme::FONT_FAMILY)
            .text_size(px(12.))
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

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn comment_scope_prefix(session_id: Option<&str>) -> String {
    format!("session:{}:", session_id.unwrap_or("<none>"))
}

fn scoped_comment_key(session_id: Option<&str>, anchor: &str) -> String {
    format!("{}{anchor}", comment_scope_prefix(session_id))
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

fn session_config_controls(options: Vec<SessionConfigOption>) -> Vec<SessionConfigControl> {
    options
        .into_iter()
        .map(|option| {
            let id = option.id.to_string();
            let name = option.name;
            let searchable = matches!(option.category, Some(SessionConfigOptionCategory::Model));
            match option.kind {
                SessionConfigKind::Select(select) => {
                    let current_value = select.current_value.to_string();
                    let options = match select.options {
                        SessionConfigSelectOptions::Ungrouped(options) => options,
                        SessionConfigSelectOptions::Grouped(groups) => {
                            groups.into_iter().flat_map(|group| group.options).collect()
                        }
                        _ => vec![],
                    };
                    let choices = options
                        .into_iter()
                        .map(|choice| SessionConfigChoice {
                            name: choice.name,
                            value: SessionConfigOptionValue::value_id(choice.value),
                        })
                        .collect::<Vec<_>>();
                    let selected_name = choices
                        .iter()
                        .find(|choice| {
                            choice
                                .value
                                .as_value_id()
                                .map(ToString::to_string)
                                .as_deref()
                                == Some(current_value.as_str())
                        })
                        .map(|choice| choice.name.clone())
                        .unwrap_or(current_value);
                    SessionConfigControl {
                        id,
                        name,
                        selected_name,
                        choices,
                        searchable,
                    }
                }
                SessionConfigKind::Boolean(boolean) => SessionConfigControl {
                    id,
                    name,
                    selected_name: if boolean.current_value {
                        "On".into()
                    } else {
                        "Off".into()
                    },
                    choices: vec![
                        SessionConfigChoice {
                            name: "On".into(),
                            value: SessionConfigOptionValue::boolean(true),
                        },
                        SessionConfigChoice {
                            name: "Off".into(),
                            value: SessionConfigOptionValue::boolean(false),
                        },
                    ],
                    searchable,
                },
                _ => SessionConfigControl {
                    id,
                    name,
                    selected_name: "Unsupported".into(),
                    choices: vec![],
                    searchable,
                },
            }
        })
        .collect()
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

#[cfg(test)]
mod session_config_tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        Diff as AcpDiff, ElicitationContentValue, SessionConfigSelectOption,
    };

    #[test]
    fn preserves_each_acp_session_option_as_a_distinct_control() {
        let controls = session_config_controls(vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "large",
                vec![
                    SessionConfigSelectOption::new("small", "Small"),
                    SessionConfigSelectOption::new("large", "Large"),
                ],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "thought-level",
                "Thought level",
                "high",
                vec![
                    SessionConfigSelectOption::new("low", "Low"),
                    SessionConfigSelectOption::new("high", "High"),
                ],
            ),
            SessionConfigOption::boolean("auto-format", "Auto format", true),
        ]);

        assert_eq!(controls.len(), 3);
        assert_eq!(controls[0].id, "model");
        assert_eq!(controls[0].selected_name, "Large");
        assert!(controls[0].searchable);
        assert_eq!(controls[1].id, "thought-level");
        assert_eq!(controls[1].selected_name, "High");
        assert!(!controls[1].searchable);
        assert_eq!(controls[2].id, "auto-format");
        assert_eq!(controls[2].selected_name, "On");
    }

    #[test]
    fn renders_structured_acp_diff_with_line_kinds_and_stats() {
        let rendered =
            render_acp_diff(AcpDiff::new("/tmp/main.rs", "same\nnew\n").old_text("same\nold\n"));

        assert_eq!(rendered.file, "/tmp/main.rs");
        assert_eq!(rendered.stat, "+1 -1");
        assert_eq!(rendered.lines.len(), 3);
        assert!(matches!(rendered.lines[0].kind, DiffLineKind::Ctx));
        assert_eq!(rendered.lines[0].text, "same");
        assert!(matches!(rendered.lines[1].kind, DiffLineKind::Del));
        assert_eq!(rendered.lines[1].text, "old");
        assert!(matches!(rendered.lines[2].kind, DiffLineKind::Add));
        assert_eq!(rendered.lines[2].text, "new");
    }

    #[test]
    fn converts_supported_json_elicitation_values() {
        assert_eq!(
            json_elicitation_value(serde_json::json!("answer")),
            Some(ElicitationContentValue::String("answer".into()))
        );
        assert_eq!(
            json_elicitation_value(serde_json::json!(42)),
            Some(ElicitationContentValue::Integer(42))
        );
        assert_eq!(
            json_elicitation_value(serde_json::json!(3.5)),
            Some(ElicitationContentValue::Number(3.5))
        );
        assert_eq!(
            json_elicitation_value(serde_json::json!(true)),
            Some(ElicitationContentValue::Boolean(true))
        );
        assert_eq!(
            json_elicitation_value(serde_json::json!(["one", "two"])),
            Some(ElicitationContentValue::StringArray(vec![
                "one".into(),
                "two".into()
            ]))
        );
    }

    #[test]
    fn rejects_json_values_not_supported_by_acp_elicitation() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({"nested": "object"}),
            serde_json::json!(["text", 2]),
        ] {
            assert_eq!(json_elicitation_value(value), None);
        }
    }

    #[test]
    fn comment_keys_are_isolated_by_session() {
        let first = scoped_comment_key(Some("session-1"), "f_src/main.rs_4");
        let second = scoped_comment_key(Some("session-2"), "f_src/main.rs_4");

        assert_ne!(first, second);
        assert!(first.starts_with(&comment_scope_prefix(Some("session-1"))));
        assert!(!first.starts_with(&comment_scope_prefix(Some("session-2"))));
    }

    #[test]
    fn unscoped_legacy_comments_do_not_match_an_active_session() {
        let legacy_key = "f_src/main.rs_4";

        assert!(!legacy_key.starts_with(&comment_scope_prefix(Some("session-1"))));
    }
}
