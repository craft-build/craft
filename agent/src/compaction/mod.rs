pub mod engine;
pub mod estimate;
pub mod llm;
pub mod vcc;

pub use engine::{CompactionEngine, CompactionState};
pub use estimate::{TokenEstimator, estimate_prompt_tokens, estimate_tokens};

/// Constructors shared by compaction tests (history has no struct-literal
/// sugar for the rich shapes these tests need).
#[cfg(test)]
pub(crate) mod test_support {
    use crate::history::{AssistantContent, Message, ToolCall, ToolResult, UserContent};

    pub fn user(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::text(text)],
        }
    }

    pub fn assistant_text(text: &str) -> Message {
        Message::Assistant {
            content: vec![AssistantContent::text(text)],
        }
    }

    pub fn assistant_tool(id: &str, name: &str) -> Message {
        assistant_tool_args(id, name, serde_json::json!({}))
    }

    pub fn assistant_tool_args(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message::Assistant {
            content: vec![AssistantContent::ToolCall(ToolCall::new(id, name, args))],
        }
    }

    pub fn tool_result(id: &str) -> Message {
        tool_result_of(id, "output")
    }

    pub fn tool_result_of(id: &str, output: &str) -> Message {
        Message::User {
            content: vec![UserContent::ToolResult(ToolResult::text(
                id, "bash", output,
            ))],
        }
    }
}
