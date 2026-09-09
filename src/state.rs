//! Persisted Craft project, session, and conversation data.

use agent_client_protocol::schema::v1::{ToolCall, ToolCallUpdate};
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
    use super::{Message, MessageBody, MessagePart, Session};

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
        assert_eq!(message.body.tool_calls().count(), 0);
        assert_eq!(message.body.text(), "Done");
        assert_eq!(message.terminal.as_ref().unwrap().output, "1: old file");
        assert_eq!(message.diff.as_ref().unwrap().file, "a.rs");
        let restored: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(restored.terminal.unwrap().cmd, "read a.rs");
        assert_eq!(restored.body.tool_calls().count(), 0);
        assert_eq!(restored.body.text(), "Done");
    }

    #[test]
    fn separate_tool_history_and_text_migrate_without_duplicating_content() {
        let body: MessageBody = serde_json::from_value(serde_json::json!({
            "text": "Finished.",
            "tool_calls": [
                {"toolCallId":"first", "title":"read a.rs", "status":"completed"},
                {"toolCallId":"second", "title":"read b.rs", "status":"completed"}
            ]
        }))
        .unwrap();
        assert_eq!(body.parts.len(), 3);
        let ids = body
            .tool_calls()
            .map(|call| call.tool_call_id.to_string())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["first", "second"]);
        assert_eq!(body.parts[2], MessagePart::Text("Finished.".into()));
        let saved = serde_json::to_value(&body).unwrap();
        assert!(saved.get("tool_calls").is_none());
        assert!(saved.get("text").is_none());
        let restored: MessageBody = serde_json::from_value(saved).unwrap();
        assert_eq!(restored, body);
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MessagePart {
    Text(String),
    ToolCall(Box<ToolCall>),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredMessageBody")]
pub struct MessageBody {
    pub parts: Vec<MessagePart>,
}

// Older messages stored all tools before a single text field. Preserve that
// display order on load; those records contain no original interleaving data.
#[derive(Deserialize)]
struct StoredMessageBody {
    parts: Option<Vec<MessagePart>>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
}

impl From<StoredMessageBody> for MessageBody {
    fn from(stored: StoredMessageBody) -> Self {
        if let Some(parts) = stored.parts {
            return Self { parts };
        }
        let mut body = Self {
            parts: stored
                .tool_calls
                .into_iter()
                .map(|call| MessagePart::ToolCall(Box::new(call)))
                .collect(),
        };
        body.push_text(&stored.text);
        body
    }
}

impl From<String> for MessageBody {
    fn from(text: String) -> Self {
        Self {
            parts: if text.is_empty() {
                vec![]
            } else {
                vec![MessagePart::Text(text)]
            },
        }
    }
}

impl MessageBody {
    pub fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(MessagePart::Text(previous)) = self.parts.last_mut() {
            previous.push_str(text);
        } else {
            self.parts.push(MessagePart::Text(text.to_owned()));
        }
    }

    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::Text(text) => Some(text.as_str()),
                MessagePart::ToolCall(_) => None,
            })
            .collect()
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|part| match part {
            MessagePart::ToolCall(call) => Some(call.as_ref()),
            MessagePart::Text(_) => None,
        })
    }

    pub fn update_tool_call(&mut self, update: ToolCallUpdate) {
        let call = self.parts.iter_mut().find_map(|part| match part {
            MessagePart::ToolCall(call) if call.tool_call_id == update.tool_call_id => Some(call),
            _ => None,
        });
        if let Some(call) = call {
            call.update(update.fields);
        } else {
            let mut call = ToolCall::new(update.tool_call_id, "Agent operation");
            call.update(update.fields);
            self.parts.push(MessagePart::ToolCall(Box::new(call)));
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    #[serde(flatten)]
    pub body: MessageBody,
    pub time: Option<String>,
    pub context: Vec<String>,
    pub attached_comments: Vec<(String, String)>,
    pub checkpoint_label: Option<String>,
    pub steps: Option<Steps>,
    // Legacy single-result fields remain readable in saved conversations.
    pub diff: Option<Diff>,
    pub terminal: Option<Terminal>,
}
