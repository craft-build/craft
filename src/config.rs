//! Application-wide registry of ACP process configurations.
//!
//! Forge deliberately stores executable commands rather than an enum of known
//! agents. Any number of ACP-compatible executables can therefore be registered
//! once and selected for sessions in any workspace without a Forge release.

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
pub struct AgentConfig {
    /// A shell-like command line parsed locally into executable and arguments.
    pub agent_command: String,
    pub transport: TransportConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub config: AgentConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRegistry {
    #[serde(default)]
    pub agents: Vec<AgentProfile>,
}

impl AgentConfig {
    pub fn workspace_for(&self, local_workspace: &Path) -> PathBuf {
        match &self.transport {
            TransportConfig::Local => local_workspace.to_path_buf(),
            TransportConfig::Ssh {
                remote_workspace, ..
            } => remote_workspace.clone(),
        }
    }
}

impl AgentConfig {
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
            .join("forge");
        Self { root }
    }

    pub fn load(&self) -> io::Result<Option<AgentRegistry>> {
        match self.load_registry(&self.path()) {
            Ok(registry) => return Ok(Some(registry)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.migrate_project_registries()
    }

    pub fn save(&self, registry: &AgentRegistry) -> io::Result<()> {
        let path = self.path();
        fs::create_dir_all(&self.root)?;
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(registry).map_err(io::Error::other)?,
        )?;
        fs::rename(temporary, path)
    }

    fn path(&self) -> PathBuf {
        self.root.join("agents.json")
    }

    fn load_registry(&self, path: &Path) -> io::Result<AgentRegistry> {
        let bytes = fs::read(path)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if value.get("agents").is_some() {
            return serde_json::from_value(value).map_err(io::Error::other);
        }
        let legacy: AgentConfig = serde_json::from_value(value).map_err(io::Error::other)?;
        Ok(AgentRegistry {
            agents: vec![AgentProfile {
                id: uuid::Uuid::new_v4().simple().to_string(),
                name: "Default agent".into(),
                config: legacy,
            }],
        })
    }

    fn migrate_project_registries(&self) -> io::Result<Option<AgentRegistry>> {
        let projects = self.root.join("projects");
        let entries = match fs::read_dir(projects) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        paths.sort();

        let mut registry = AgentRegistry::default();
        for path in paths {
            let project_registry = self.load_registry(&path)?;
            for mut profile in project_registry.agents {
                if registry.agents.iter().any(|existing| {
                    existing.id == profile.id
                        || (existing.name.eq_ignore_ascii_case(&profile.name)
                            && existing.config == profile.config)
                }) {
                    continue;
                }
                let original_name = profile.name.clone();
                let mut suffix = 2;
                while registry
                    .agents
                    .iter()
                    .any(|existing| existing.name.eq_ignore_ascii_case(&profile.name))
                {
                    profile.name = format!("{original_name} ({suffix})");
                    suffix += 1;
                }
                registry.agents.push(profile);
            }
        }
        if registry.agents.is_empty() {
            return Ok(None);
        }
        self.save(&registry)?;
        Ok(Some(registry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_transport_uses_argument_boundaries_not_a_local_shell() {
        let config = AgentConfig {
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
        let config = AgentConfig {
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
        fs::create_dir_all(root.join("projects")).unwrap();
        let store = ConfigStore { root: root.clone() };
        let legacy = AgentConfig {
            agent_command: "agent --acp".into(),
            transport: TransportConfig::Local,
        };
        fs::write(
            root.join("projects").join("project.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();

        let registry = store.load().unwrap().unwrap();

        assert_eq!(registry.agents.len(), 1);
        assert_eq!(registry.agents[0].name, "Default agent");
        assert_eq!(registry.agents[0].config, legacy);
        assert!(root.join("agents.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn registry_preserves_multiple_named_agents() {
        let registry = AgentRegistry {
            agents: vec![
                AgentProfile {
                    id: "claude".into(),
                    name: "Claude Code".into(),
                    config: AgentConfig {
                        agent_command: "claude --acp".into(),
                        transport: TransportConfig::Local,
                    },
                },
                AgentProfile {
                    id: "gemini".into(),
                    name: "Gemini".into(),
                    config: AgentConfig {
                        agent_command: "gemini --experimental-acp".into(),
                        transport: TransportConfig::Local,
                    },
                },
            ],
        };

        let restored: AgentRegistry =
            serde_json::from_slice(&serde_json::to_vec(&registry).unwrap()).unwrap();

        assert_eq!(restored, registry);
    }

    #[test]
    fn application_registry_is_shared_without_a_project_key() {
        let root = std::env::temp_dir().join(format!("forge-config-test-{}", uuid::Uuid::new_v4()));
        let store = ConfigStore { root: root.clone() };
        let registry = AgentRegistry {
            agents: vec![AgentProfile {
                id: "shared-agent".into(),
                name: "Shared agent".into(),
                config: AgentConfig {
                    agent_command: "agent --acp".into(),
                    transport: TransportConfig::Local,
                },
            }],
        };

        store.save(&registry).unwrap();

        assert_eq!(store.load().unwrap(), Some(registry));
        assert!(root.join("agents.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migration_merges_agents_from_all_project_registries() {
        let root = std::env::temp_dir().join(format!("forge-config-test-{}", uuid::Uuid::new_v4()));
        let projects = root.join("projects");
        fs::create_dir_all(&projects).unwrap();
        let shared_config = AgentConfig {
            agent_command: "agent --acp".into(),
            transport: TransportConfig::Local,
        };
        let first = AgentRegistry {
            agents: vec![AgentProfile {
                id: "first-agent".into(),
                name: "Agent".into(),
                config: shared_config.clone(),
            }],
        };
        let second = AgentRegistry {
            agents: vec![
                AgentProfile {
                    id: "duplicate-agent".into(),
                    name: "Agent".into(),
                    config: shared_config,
                },
                AgentProfile {
                    id: "other-agent".into(),
                    name: "Agent".into(),
                    config: AgentConfig {
                        agent_command: "other-agent --acp".into(),
                        transport: TransportConfig::Local,
                    },
                },
            ],
        };
        fs::write(
            projects.join("first.json"),
            serde_json::to_vec(&first).unwrap(),
        )
        .unwrap();
        fs::write(
            projects.join("second.json"),
            serde_json::to_vec(&second).unwrap(),
        )
        .unwrap();

        let migrated = ConfigStore { root: root.clone() }.load().unwrap().unwrap();

        assert_eq!(migrated.agents.len(), 2);
        assert_eq!(migrated.agents[0].name, "Agent");
        assert_eq!(migrated.agents[1].name, "Agent (2)");
        fs::remove_dir_all(root).unwrap();
    }
}
