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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Comment, Project, Session};

    fn temporary_store() -> (PathBuf, StateStore) {
        let root = std::env::temp_dir().join(format!("forge-state-test-{}", uuid::Uuid::new_v4()));
        let path = root.join("nested").join("state.json");
        (root, StateStore { path })
    }

    #[test]
    fn missing_state_file_loads_an_empty_state() {
        let (root, store) = temporary_store();

        let state = store.load().unwrap();

        assert!(state.projects.is_empty());
        assert!(state.sessions_by_project.is_empty());
        assert!(state.comments.is_empty());
        assert!(state.checkpoints_by_project.is_empty());
        assert!(!root.exists(), "loading must not create state directories");
    }

    #[test]
    fn state_round_trips_all_top_level_collections() {
        let (root, store) = temporary_store();
        let project = Project {
            id: "project-1".into(),
            name: "Forge".into(),
            path: "/tmp/forge".into(),
            desc: "Desktop client".into(),
            updated: "now".into(),
            checkpoint_label: "Initial".into(),
            model: "agent".into(),
        };
        let session = Session {
            id: "session-1".into(),
            name: "Testing".into(),
            messages: vec![],
            acp_session_id: Some("acp-1".into()),
            archived: true,
            agent_profile_id: Some("agent-1".into()),
        };
        let mut state = PersistedState {
            projects: vec![project],
            ..PersistedState::default()
        };
        state
            .sessions_by_project
            .insert("project-1".into(), vec![session]);
        state.comments.insert(
            "project-1:session-1".into(),
            vec![Comment {
                author: "Reviewer".into(),
                text: "Keep this".into(),
                pending: true,
                label: "src/main.rs:1".into(),
            }],
        );
        state.checkpoints_by_project.insert(
            "project-1".into(),
            vec![Checkpoint {
                label: "Before tests".into(),
                commit: "abc123".into(),
            }],
        );

        store.save(&state).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(loaded.projects[0].id, "project-1");
        assert_eq!(
            loaded.sessions_by_project["project-1"][0]
                .agent_profile_id
                .as_deref(),
            Some("agent-1")
        );
        assert!(loaded.sessions_by_project["project-1"][0].archived);
        assert_eq!(loaded.comments["project-1:session-1"][0].text, "Keep this");
        assert_eq!(
            loaded.checkpoints_by_project["project-1"][0].commit,
            "abc123"
        );
        assert!(
            !store.path.with_extension("json.tmp").exists(),
            "the atomic-save temporary file should be renamed away"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_state_is_reported_instead_of_resetting_user_data() {
        let (root, store) = temporary_store();
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, b"{not valid json").unwrap();

        let error = match store.load() {
            Ok(_) => panic!("malformed state should not load"),
            Err(error) => error,
        };

        assert!(!error.to_string().is_empty());
        assert_eq!(fs::read(&store.path).unwrap(), b"{not valid json");
        fs::remove_dir_all(root).unwrap();
    }
}
