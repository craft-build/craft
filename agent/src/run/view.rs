//! Request-view rewriting: output pre-compression applied to the wire
//! copy only; history and events always keep the raw results.

use crate::compression::{self, CompressionConfig};
use crate::history::{self, Message};

/// Rewrite tool-result texts in the request copy through pre-compression.
/// Only the wire view is affected: `turn` and `history` keep raw results.
/// Request-time compression is unconditional (per the reference);
/// `protect_recent_tool_outputs` is a compaction-stage knob, not ours.
pub(crate) fn compress_request_view(full: &mut [Message], config: &CompressionConfig) {
    for message in full {
        let Message::User { content } = message else {
            continue;
        };
        for block in content {
            if let history::UserContent::ToolResult(result) = block {
                // Verbatim tools (e.g. `read`) return caller-selected content;
                // compressing it would drop lines the model explicitly asked for.
                if !compression::should_compress_tool(&result.name) {
                    continue;
                }
                for item in &mut result.content {
                    if let history::ToolResultContent::Text(text) = item {
                        text.text = compression::compress_for_llm(&text.text, config);
                    }
                }
            }
        }
    }
}
