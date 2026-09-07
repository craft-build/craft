//! Durable local Forge project and thread state.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::checkpoint::Checkpoint;
use crate::state::{Comment, Project, Session};

#[derive(Default, Serialize, Deserialize)]
pub struct PersistedState {
    pub projects: Vec<Project>,
    pub sessions_by_project: HashMap<String, Vec<Session>>,
    pub comments: HashMap<String, Vec<Comment>>,
    pub checkpoints_by_project: HashMap<String, Vec<Checkpoint>>,
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn for_user() -> Self {
        let root = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("forge");
        Self {
            path: root.join("state.json"),
        }
    }

    pub fn load(&self) -> io::Result<PersistedState> {
        match fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PersistedState::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, state: &PersistedState) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("invalid Forge state path"))?;
        fs::create_dir_all(parent)?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(state).map_err(io::Error::other)?,
        )?;
        fs::rename(temporary, &self.path)
    }
}
