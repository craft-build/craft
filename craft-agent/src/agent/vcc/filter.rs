use std::collections::HashSet;
use std::sync::LazyLock;

use super::normalize::NormalizedBlock;

static NOISE_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "TodoWrite",
        "TodoRead",
        "todo_write",
        "ToolSearch",
        "WebSearch",
        "websearch",
        "AskUser",
        "ask",
        "ExitSpecMode",
        "GenerateDroid",
    ]
    .into_iter()
    .collect()
});

const NOISE_STRINGS: &[&str] = &[
    "Continue from where you left off.",
    "No response requested.",
    "IMPORTANT: TodoWrite was not called yet.",
];

const XML_WRAPPERS: &[&str] = &[
    "system-reminder",
    "ide_opened_file",
    "command-message",
    "context-window-usage",
];

fn strip_xml_wrappers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut remaining = text;
    while let Some(start) = remaining.find("<") {
        let after = &remaining[start..];
        let mut matched = false;
        for tag in XML_WRAPPERS {
            let open = format!("<{tag}");
            if after.starts_with(&open) {
                let close = format!("</{tag}>");
                if let Some(end) = after.find(&close) {
                    out.push_str(&remaining[..start]);
                    remaining = &after[end + close.len()..];
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            out.push_str(&remaining[..start + 1]);
            remaining = &remaining[start + 1..];
        }
    }
    out.push_str(remaining);
    out
}

fn is_noise_user_block(text: &str) -> bool {
    let trimmed = text.trim();
    if NOISE_STRINGS.iter().any(|s| trimmed.contains(s)) {
        return true;
    }
    let stripped = strip_xml_wrappers(trimmed);
    stripped.trim().is_empty()
}

/// Drop thinking blocks, noise tools, and empty user text.
pub(crate) fn filter_noise(blocks: Vec<NormalizedBlock>) -> Vec<NormalizedBlock> {
    blocks
        .into_iter()
        .filter(|b| match b {
            NormalizedBlock::Thinking { .. } => false,
            NormalizedBlock::ToolCall { name, .. } | NormalizedBlock::ToolResult { name, .. } => {
                !NOISE_TOOLS.contains(name.as_str())
            }
            NormalizedBlock::User { text, .. } => {
                if is_noise_user_block(text) {
                    return false;
                }
                let cleaned = strip_xml_wrappers(text);
                !cleaned.trim().is_empty()
            }
            _ => true,
        })
        .map(|b| match b {
            NormalizedBlock::User { text, source_index } => {
                let cleaned = strip_xml_wrappers(&text).trim().to_string();
                NormalizedBlock::User {
                    text: cleaned,
                    source_index,
                }
            }
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_drops_thinking() {
        let blocks = vec![
            NormalizedBlock::Thinking {
                text: "hmm".into(),
                redacted: false,
                source_index: 0,
            },
            NormalizedBlock::User {
                text: "keep".into(),
                source_index: 1,
            },
        ];
        let out = filter_noise(blocks);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], NormalizedBlock::User { .. }));
    }

    #[test]
    fn filter_drops_noise_tools() {
        let blocks = vec![
            NormalizedBlock::ToolCall {
                name: "TodoWrite".into(),
                args: serde_json::json!({}),
                source_index: 0,
            },
            NormalizedBlock::ToolCall {
                name: "edit".into(),
                args: serde_json::json!({}),
                source_index: 1,
            },
        ];
        let out = filter_noise(blocks);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn filter_strips_xml_wrappers() {
        let blocks = vec![NormalizedBlock::User {
            text: "<system-reminder>noise</system-reminder>real task".into(),
            source_index: 0,
        }];
        let out = filter_noise(blocks);
        assert!(matches!(&out[0], NormalizedBlock::User { text, .. } if text == "real task"));
    }
}
