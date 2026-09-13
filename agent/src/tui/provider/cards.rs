//! Pure view helpers for the live backend: tool-card construction, usage
//! labels, and display paths. Kept free of session state so they can be
//! unit-tested and reused by other turn drivers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use crate::history;

use super::{LineKind, Tone, ToolCallData, ToolKind, ToolLine, TouchedFile};

/// Mutating workspace tools; their results feed the Files panel.
pub(super) const EDIT_TOOLS: [&str; 5] = ["edit", "edit_lines", "insert_lines", "write", "delete"];

/// What an edit-family tool did to a file, for the sidebar badge.
#[derive(Clone, Copy)]
pub(super) enum FileStatus {
    Modified,
    Created,
    Deleted,
}

impl FileStatus {
    fn label(self) -> &'static str {
        match self {
            FileStatus::Modified => "modified",
            FileStatus::Created => "created",
            FileStatus::Deleted => "deleted",
        }
    }

    fn tone(self) -> Tone {
        match self {
            FileStatus::Modified => Tone::Warning,
            FileStatus::Created => Tone::Success,
            FileStatus::Deleted => Tone::Danger,
        }
    }
}

/// Files touched by edit-family tools, keyed by path. A plain mutex: it is
/// only locked briefly from the run's synchronous event callback.
pub(super) type Files = Arc<StdMutex<BTreeMap<String, FileStatus>>>;

/// The Files-panel contents for the current session state.
pub(super) fn touched_files(files: &Files) -> Vec<TouchedFile> {
    files
        .lock()
        .expect("files lock")
        .iter()
        .map(|(path, status)| TouchedFile {
            path: path.clone(),
            status: status.label().to_string(),
            tone: status.tone(),
        })
        .collect()
}

/// Card emitted when a tool call starts: kind from the tool name and its
/// most descriptive string argument; the body stays empty until the result
/// arrives.
pub(super) fn tool_head(name: &str, arguments: &serde_json::Value) -> ToolKind {
    let detail = first_string_argument(arguments).unwrap_or_default();
    match name {
        "read" => ToolKind::Read {
            path: detail,
            summary: String::new(),
        },
        "grep" => ToolKind::Grep {
            pattern: detail,
            summary: String::new(),
        },
        name if EDIT_TOOLS.contains(&name) => ToolKind::Edit { path: detail },
        other => ToolKind::Bash {
            cmd: format!("{other} {detail}").trim().to_string(),
        },
    }
}

/// The state produced by a completed tool call.
pub(super) struct ToolDone {
    pub card: ToolCallData,
    /// (path, status) when an edit-family tool succeeded.
    pub touched: Option<(String, FileStatus)>,
}

/// Card emitted when a tool result arrives: same id as the start card, with
/// the body and summary filled from the result text.
pub(super) fn tool_done(
    id: String,
    name: &str,
    arguments: &serde_json::Value,
    result: &history::ToolResult,
) -> ToolDone {
    let text = result
        .content
        .iter()
        .map(history::ToolResultContent::to_text)
        .collect::<Vec<_>>()
        .join("\n");
    let detail = first_string_argument(arguments).unwrap_or_default();

    let (kind, lines, touched) = match name {
        "read" => {
            let lines = context_lines(&text);
            let summary = format!("{} lines", lines.len());
            (
                ToolKind::Read {
                    path: detail,
                    summary,
                },
                lines,
                None,
            )
        }
        "grep" => {
            let lines = context_lines(&text);
            let summary = format!("{} lines of output", lines.len());
            (
                ToolKind::Grep {
                    pattern: detail,
                    summary,
                },
                lines,
                None,
            )
        }
        tool if EDIT_TOOLS.contains(&tool) => {
            let status = match tool {
                "delete" => FileStatus::Deleted,
                "write" => FileStatus::Created,
                _ => FileStatus::Modified,
            };
            (
                ToolKind::Edit {
                    path: detail.clone(),
                },
                diff_lines(&text),
                if detail.is_empty() || result.is_error {
                    None
                } else {
                    Some((detail, status))
                },
            )
        }
        other => (
            ToolKind::Bash {
                cmd: format!("{other} {detail}").trim().to_string(),
            },
            context_lines(&text),
            None,
        ),
    };
    ToolDone {
        card: ToolCallData {
            id,
            kind,
            lines,
            awaiting_approval: false,
        },
        touched,
    }
}

fn context_lines(text: &str) -> Vec<ToolLine> {
    text.lines()
        .map(|line| ToolLine {
            kind: LineKind::Context,
            text: line.to_string(),
        })
        .collect()
}

/// Split a diff-formatted tool result into Add/Del lines; anything else is
/// context.
fn diff_lines(text: &str) -> Vec<ToolLine> {
    text.lines()
        .map(|line| ToolLine {
            kind: match line.as_bytes().first() {
                Some(b'+') => LineKind::Add,
                Some(b'-') => LineKind::Del,
                _ => LineKind::Context,
            },
            text: line.to_string(),
        })
        .collect()
}

/// The most descriptive string argument of a tool call, for card labels.
/// Known keys win over map order (which is alphabetical, not semantic —
/// `write`'s `content` would otherwise outrank its `path`).
pub(crate) fn first_string_argument(arguments: &serde_json::Value) -> Option<String> {
    let object = arguments.as_object()?;
    for key in ["path", "command", "pattern"] {
        if let Some(value) = object.get(key).and_then(|value| value.as_str()) {
            return Some(value.to_owned());
        }
    }
    let (_, value) = object.iter().find(|(_, value)| value.is_string())?;
    value.as_str().map(str::to_owned)
}

/// "45.9K (4%)"-style label; the percentage is the prompt's share of the
/// context window, omitted when the context length is unknown.
pub(super) fn usage_label(tokens: u64, prompt_tokens: u64, context_length: Option<u32>) -> String {
    let k = tokens as f64 / 1000.0;
    match context_length {
        Some(size) if size > 0 => {
            let pct = (prompt_tokens as f64 / f64::from(size) * 100.0).round();
            format!("{k:.1}K ({pct:.0}%)")
        }
        _ => format!("{k:.1}K"),
    }
}

/// "~"-shortened display path for the sidebar.
pub(super) fn display_path(path: &Path) -> String {
    let display = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        if let Some(rest) = display.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    display
}

/// Current git branch for the sidebar, if the workspace is a repository.
/// Runs on the blocking pool so a slow git invocation (e.g. an NFS-mounted
/// repository) cannot stall the async runtime thread.
pub(super) async fn git_branch(cwd: &Path) -> String {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| "no branch".into())
    })
    .await
    .unwrap_or_else(|_| "no branch".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_string_argument_prefers_path_over_content() {
        let args = serde_json::json!({
            "content": "fn main() {}",
            "path": "src/main.rs",
        });
        assert_eq!(first_string_argument(&args).as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn first_string_argument_falls_back_to_first_string() {
        let args = serde_json::json!({"pattern": "needle"});
        assert_eq!(first_string_argument(&args).as_deref(), Some("needle"));
    }

    #[test]
    fn edit_card_path_is_the_path_not_the_contents() {
        let result = history::ToolResult::text("t1", "write", "+new");
        let done = tool_done(
            "t1".into(),
            "write",
            &serde_json::json!({"content": "x".repeat(100), "path": "a.txt"}),
            &result,
        );
        assert!(matches!(done.card.kind, ToolKind::Edit { ref path } if path == "a.txt"));
        assert_eq!(done.touched.map(|(p, _)| p).as_deref(), Some("a.txt"));
    }
}
