mod brief;
mod cut;
mod extract;
mod filter;
mod format;
mod merge;
mod normalize;
pub(crate) mod recall;
mod sections;
mod util;

use craft_providers::Message;

use filter::filter_noise;
use format::{format_summary, wrap_long_lines};
use merge::{HANDOFF_PREAMBLE, merge_previous, strip_recall_note};
use normalize::normalize;
use sections::build_sections;

/// A VCC compaction summary and the index at which the unsummarized tail begins.
#[derive(Debug, Clone)]
pub struct VccSummary {
    pub summary: String,
    pub tail_start: usize,
}

/// Deterministically build a structured, no-LLM summary of `messages`.
///
/// `prev_summary` (a prior VCC summary, with its preamble) is merged so that
/// sticky sections accumulate across compactions while volatile sections are
/// replaced. The returned `tail_start` is 0 for compact-all.
pub fn compact(messages: &[Message], prev_summary: Option<&str>) -> VccSummary {
    let tail_start = match cut::find_cut(messages) {
        Some(c) => c.tail_start,
        None => 0,
    };
    let head = &messages[..tail_start];
    let blocks = filter_noise(normalize(head));
    let data = build_sections(&blocks);
    let fresh = format_summary(&data);

    let body = match prev_summary {
        Some(prev) => {
            let stripped = strip_recall_note(prev);
            if stripped.is_empty() {
                fresh
            } else {
                merge_previous(&stripped, &fresh)
            }
        }
        None => fresh,
    };

    let summary = if body.is_empty() {
        String::new()
    } else {
        wrap_long_lines(&format!("{HANDOFF_PREAMBLE}\n\n{body}"))
    };

    VccSummary {
        summary,
        tail_start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_providers::{ContentBlock, Role};

    fn user(text: &str) -> Message {
        Message::user(text.into())
    }
    fn assistant_tool(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id,
                name,
                serde_json::json!({"command": "ls"}),
            )],
            ..Default::default()
        }
    }
    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "done".into(),
                images: vec![],
                is_error: false,
            }],
            ..Default::default()
        }
    }

    fn sample_history() -> Vec<Message> {
        vec![
            user("Implement the login feature"),
            assistant_tool("t1", "bash"),
            tool_result("t1"),
            user("Now add tests for it"),
            assistant_tool("t2", "bash"),
            tool_result("t2"),
            user("Run the test suite"),
            assistant_tool("t3", "bash"),
            tool_result("t3"),
        ]
    }

    #[test]
    fn compact_produces_nonempty_summary_with_tail() {
        let history = sample_history();
        let result = compact(&history, None);
        assert!(!result.summary.is_empty());
        assert!(result.summary.contains("[Session Goal]"));
        assert!(result.summary.starts_with("This summary captures"));
        assert!(result.tail_start > 0);
    }

    #[test]
    fn compact_is_deterministic() {
        let history = sample_history();
        let a = compact(&history, None);
        let b = compact(&history, None);
        assert_eq!(a.summary, b.summary);
        assert_eq!(a.tail_start, b.tail_start);
    }

    #[test]
    fn compact_merges_previous_summary() {
        let history = sample_history();
        let first = compact(&history, None);
        let second = compact(&history, Some(&first.summary));
        assert!(second.summary.contains("[Session Goal]"));
    }

    #[test]
    fn compact_handles_empty_history() {
        let result = compact(&[], None);
        assert!(result.summary.is_empty());
        assert_eq!(result.tail_start, 0);
    }
}
