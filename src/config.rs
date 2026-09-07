//! Per-project registry of ACP process configurations.
//!
//! Forge deliberately stores executable commands rather than an enum of known
//! agents. Any number of ACP-compatible executables can therefore be registered
//! and selected per session without a Forge release.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use agent_client_protocol::{AcpAgent, AcpAgentConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportConfig {
    #[default]
    Local,
    Ssh {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,
        /// Workspace path as seen by the remote agent. Local projects always
        /// use the path selected by the native workspace picker.
        #[serde(default)]
        remote_workspace: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAgentConfig {
    /// A shell-like command line parsed locally into executable and arguments.
    pub agent_command: String,
    pub transport: TransportConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub config: ProjectAgentConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectAgentRegistry {
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
}

impl ProjectAgentConfig {
    pub fn workspace_for(&self, local_workspace: &Path) -> PathBuf {
        match &self.transport {
            TransportConfig::Local => local_workspace.to_path_buf(),
            TransportConfig::Ssh {
                remote_workspace, ..
            } => remote_workspace.clone(),
        }
    }
}

impl ProjectAgentConfig {
    pub fn agent(&self) -> Result<AcpAgent, String> {
        let words = shell_words::split(&self.agent_command)
            .map_err(|error| format!("invalid agent command: {error}"))?;
        let (program, args) = words
            .split_first()
            .ok_or_else(|| "agent command is empty".to_string())?;

        let process = match &self.transport {
            TransportConfig::Local => AcpAgentConfig::new(program).args(args.iter().cloned()),
            TransportConfig::Ssh {
                host,
                user,
                identity_file,
                ..
            } => {
                if host.trim().is_empty() {
                    return Err("SSH host is empty".into());
                }
                let destination = user
                    .as_deref()
                    .filter(|user| !user.is_empty())
                    .map(|user| format!("{user}@{host}"))
                    .unwrap_or_else(|| host.clone());
                let mut ssh_args = vec![
                    "-o".into(),
                    "BatchMode=yes".into(),
                    "-o".into(),
                    "ServerAliveInterval=15".into(),
                    "-o".into(),
                    "ServerAliveCountMax=3".into(),
                ];
                if let Some(key) = identity_file {
                    ssh_args.push("-i".into());
                    ssh_args.push(key.to_string_lossy().into_owned());
                }
                ssh_args.push(destination);
                ssh_args.push("--".into());
                ssh_args.push(program.clone());
                ssh_args.extend(args.iter().cloned());
                AcpAgentConfig::new("ssh").args(ssh_args)
            }
        };
        Ok(AcpAgent::new(process))
    }
}

pub struct ConfigStore {
    root: PathBuf,
}

impl ConfigStore {
    pub fn for_user() -> Self {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("forge")
            .join("projects");
        Self { root }
    }

    pub fn load(&self, project_id: &str) -> io::Result<Option<ProjectAgentRegistry>> {
        let path = self.path(project_id)?;
        match fs::read(path) {
            Ok(bytes) => {
                let value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if value.get("agents").is_some() {
                    return serde_json::from_value(value)
                        .map(Some)
                        .map_err(io::Error::other);
                }
                let legacy: ProjectAgentConfig =
                    serde_json::from_value(value).map_err(io::Error::other)?;
                Ok(Some(ProjectAgentRegistry {
                    agents: vec![AgentProfile {
                        id: "migrated-default".into(),
                        name: "Default agent".into(),
                        config: legacy,
                    }],
                }))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, project_id: &str, registry: &ProjectAgentRegistry) -> io::Result<()> {
        let path = self.path(project_id)?;
        fs::create_dir_all(&self.root)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(registry).map_err(io::Error::other)?,
        )?;
        fs::rename(temporary, path)
    }

    fn path(&self, project_id: &str) -> io::Result<PathBuf> {
        let safe = project_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character));
        if !safe || project_id == "." || project_id == ".." {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid project id",
            ));
        }
        Ok(self.root.join(Path::new(project_id)).with_extension("json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_transport_uses_argument_boundaries_not_a_local_shell() {
        let config = ProjectAgentConfig {
            agent_command: "gemini --experimental-acp".into(),
            transport: TransportConfig::Ssh {
                host: "build.example".into(),
                user: Some("dev".into()),
                identity_file: None,
                remote_workspace: "/srv/project".into(),
            },
        };
        let agent = config.agent().unwrap();
        assert_eq!(agent.config().command(), Path::new("ssh"));
        assert!(
            agent
                .config()
                .arguments()
                .iter()
                .any(|arg| arg == "dev@build.example")
        );
        assert!(agent.config().arguments().iter().any(|arg| arg == "gemini"));
    }

    #[test]
    fn local_agent_uses_the_selected_project_workspace() {
        let config = ProjectAgentConfig {
            agent_command: "agent --acp".into(),
            transport: TransportConfig::Local,
        };
        assert_eq!(
            config.workspace_for(Path::new("/selected/project")),
            PathBuf::from("/selected/project")
        );
    }

    #[test]
    fn legacy_single_agent_config_migrates_to_a_registry() {
        let root = std::env::temp_dir().join(format!("forge-config-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = ConfigStore { root: root.clone() };
        let legacy = ProjectAgentConfig {
            agent_command: "agent --acp".into(),
            transport: TransportConfig::Local,
        };
        fs::write(
            root.join("project.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let registry = store.load("project").unwrap().unwrap();

        assert_eq!(registry.agents.len(), 1);
        assert_eq!(registry.agents[0].name, "Default agent");
        assert_eq!(registry.agents[0].config, legacy);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_preserves_multiple_named_agents() {
        let registry = ProjectAgentRegistry {
            agents: vec![
                AgentProfile {
                    id: "claude".into(),
                    name: "Claude Code".into(),
                    config: ProjectAgentConfig {
                        agent_command: "claude --acp".into(),
                        transport: TransportConfig::Local,
                    },
                },
                AgentProfile {
                    id: "gemini".into(),
                    name: "Gemini".into(),
                    config: ProjectAgentConfig {
                        agent_command: "gemini --experimental-acp".into(),
                        transport: TransportConfig::Local,
                    },
                },
            ],
        };

        let restored: ProjectAgentRegistry =
            serde_json::from_slice(&serde_json::to_vec(&registry).unwrap()).unwrap();

        assert_eq!(restored, registry);
    }
}
