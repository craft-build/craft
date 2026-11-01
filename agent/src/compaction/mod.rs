pub mod alter;
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
pub use estimate::{RequestOverhead, TokenEstimator, estimate_prompt_tokens, estimate_tokens};

/// Constructors shared by compaction tests: wire messages for request-level
/// suites, stored messages for history-level ones.
#[cfg(test)]
pub(crate) mod test_support {
    use animist::history::{ContentBlock, Message, Role};

    use crate::history::{StoredMessage, ext};

    pub fn user(text: &str) -> Message {
        ext::user_text(text)
    }

    pub fn assistant_text(text: &str) -> Message {
        ext::assistant_text(text)
    }

    pub fn assistant_tool(id: &str, name: &str) -> Message {
        assistant_tool_args(id, name, serde_json::json!({}))
    }

    pub fn assistant_tool_args(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: args,
                thought_signature: None,
            }],
            display_text: None,
            kind: animist::history::MessageKind::Turn,
        }
    }

    pub fn tool_result(id: &str) -> Message {
        tool_result_of(id, "output")
    }

    pub fn tool_result_of(id: &str, output: &str) -> Message {
        ext::observation(vec![ext::tool_result(id, Some("bash"), output, false)])
    }

    pub fn stored_user(text: &str) -> StoredMessage {
        ext::stored(user(text))
    }

    pub fn stored_assistant_text(text: &str) -> StoredMessage {
        ext::stored(assistant_text(text))
    }

    pub fn stored_assistant_tool(id: &str, name: &str) -> StoredMessage {
        ext::stored(assistant_tool(id, name))
    }

    pub fn stored_assistant_tool_args(
        id: &str,
        name: &str,
        args: serde_json::Value,
    ) -> StoredMessage {
        ext::stored(assistant_tool_args(id, name, args))
    }

    pub fn stored_tool_result(id: &str) -> StoredMessage {
        ext::stored(tool_result(id))
    }

    pub fn stored_tool_result_of(id: &str, output: &str) -> StoredMessage {
        ext::stored(tool_result_of(id, output))
    }
}
