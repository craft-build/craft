//! Persisted Forge project, session, and conversation data.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    Onboarding,
    Projects,
    Workspace,
    Settings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub desc: String,
    pub updated: String,
    pub checkpoint_label: String,
    pub model: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub acp_session_id: Option<String>,
    #[serde(default)]
    pub archived: bool,
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[test]
    fn sessions_saved_before_archiving_default_to_active() {
        let session: Session = serde_json::from_str(
            r#"{"id":"session-1","name":"Old session","messages":[],"acp_session_id":null}"#,
        )
        .expect("older persisted sessions should still load");

        assert!(!session.archived);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DiffLineKind {
    Ctx,
    Add,
    Del,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diff {
    pub file: String,
    pub stat: String,
    pub hunk_header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Terminal {
    pub cmd: String,
    pub output: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Steps {
    pub summary: String,
    pub items: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub text: String,
    pub pending: bool,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub text: String,
    pub time: Option<String>,
    pub context: Vec<String>,
    pub attached_comments: Vec<(String, String)>,
    pub checkpoint_label: Option<String>,
    pub steps: Option<Steps>,
    pub diff: Option<Diff>,
    pub terminal: Option<Terminal>,
}
