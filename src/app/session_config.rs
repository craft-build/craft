use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigOptionValue,
    SessionConfigSelectOptions,
};
use gpui::Context;

use crate::acp::AcpClient;
use crate::async_runtime;
use crate::config::{AgentConfig, AgentProfile, AgentRegistry, ConfigStore, TransportConfig};

use super::App;

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

pub(crate) fn session_config_controls(
    options: Vec<SessionConfigOption>,
) -> Vec<SessionConfigControl> {
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

impl App {
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

    pub(crate) fn set_session_config_options(&mut self, options: Vec<SessionConfigOption>) {
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
            let _ = sender.send(crate::acp::validate_agent(config).await);
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
                self.config_cache = Some(registry.clone());
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
                self.config_cache = Some(registry.clone());
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

    pub(crate) fn connect_project(&mut self, cx: &mut Context<Self>) {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        let connection_generation = self.connection_generation;
        self.acp_client = None;
        self.agent_config = None;
        self.agent_menu_open = false;
        self.pending_permission = None;
        self.pending_elicitation = None;
        self.clear_elicitation();
        self.context_usage = None;
        self.session_config_controls.clear();
        self.open_config_menu = None;
        let Some(project) = self.active_project.clone() else {
            return;
        };
        let registry =
            match self
                .config_cache
                .clone()
                .or_else(|| match ConfigStore::for_user().load() {
                    Ok(registry) => {
                        let registry = registry.unwrap_or_default();
                        self.config_cache = Some(registry.clone());
                        Some(registry)
                    }
                    Err(error) => {
                        self.connection_status = format!("Config error: {error}");
                        None
                    }
                }) {
                Some(registry) => registry,
                None => return,
            };
        if registry.agents.is_empty() {
            self.agent_profiles.clear();
            self.connection_status = "No agents registered".into();
            self.refresh_workspace(cx);
            return;
        }
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
                    .update(cx, |app, cx| {
                        if app.connection_generation == connection_generation {
                            app.apply_acp_event(event, cx);
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}
