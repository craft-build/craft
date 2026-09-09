//! Persisted Craft project, session, and conversation data.

use agent_client_protocol::schema::v1::ToolCall;
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
    #[serde(default)]
    pub agent_profile_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{Message, Session};

    #[test]
    fn sessions_saved_before_archiving_default_to_active() {
        let session: Session = serde_json::from_str(
            r#"{"id":"session-1","name":"Old session","messages":[],"acp_session_id":null}"#,
        )
        .expect("older persisted sessions should still load");

        assert!(!session.archived);
        assert!(session.agent_profile_id.is_none());
    }

    #[test]
    fn messages_saved_before_tool_call_history_keep_their_legacy_output() {
        let message: Message = serde_json::from_str(
            r#"{
            "id":"old-reply", "role":"Assistant", "text":"Done", "time":null,
            "context":[], "attached_comments":[], "checkpoint_label":null,
            "steps":null,
            "diff":{"file":"a.rs", "stat":"+1", "hunk_header":"@@", "lines":[]},
            "terminal":{"cmd":"read a.rs", "output":"1: old file"}
        }"#,
        )
        .unwrap();
        assert!(message.tool_calls.is_empty());
        assert_eq!(message.terminal.as_ref().unwrap().output, "1: old file");
        assert_eq!(message.diff.as_ref().unwrap().file, "a.rs");
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(restored.terminal.unwrap().cmd, "read a.rs");
        assert!(restored.tool_calls.is_empty());
    }

    #[test]
    fn session_remembers_its_agent_profile() {
        let session: Session = serde_json::from_str(
            r#"{"id":"session-1","name":"Thread","messages":[],"agent_profile_id":"claude"}"#,
        )
        .unwrap();

        assert_eq!(session.agent_profile_id.as_deref(), Some("claude"));
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
    // Legacy single-result fields remain readable in saved conversations.
    pub diff: Option<Diff>,
    pub terminal: Option<Terminal>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}
