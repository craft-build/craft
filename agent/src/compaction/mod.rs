pub mod engine;
pub mod estimate;
pub mod llm;
mod strip;
pub mod vcc;

/// Collapse marker substituted for oversized tool results in compaction
/// requests (ported from Craft).
pub(crate) const TOOL_RESULT_PLACEHOLDER: &str = "[tool result]";

/// Char budget the most recent tool results keep their full text for when a
/// summary request is first built (ported from Craft).
pub(crate) const RECENT_TOOL_RESULT_BUDGET: usize = 64 * 1024;

/// Progressive shares of tool results collapsed per step once the overflow
/// retry ladder runs out of collapse/truncate attempts.
pub(crate) const PROGRESSIVE_TOOL_REMOVAL_RATIOS: &[f32] = &[0.10, 0.20, 0.50, 1.00];

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
