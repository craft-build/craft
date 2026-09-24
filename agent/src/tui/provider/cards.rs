//! Pure view helpers for the live backend: tool-card construction, usage
//! labels, and display paths. Kept free of session state so they can be
//! unit-tested and reused by other turn drivers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use crate::history;

use super::{LineKind, Tone, ToolCallData, ToolKind, ToolLine, TouchedFile};

/// Mutating workspace tools; their results feed the Files panel.
pub(super) const EDIT_TOOLS: [&str; 6] = [
    "edit",
    "edit_lines",
    "insert_lines",
    "write",
    "delete",
    "move",
];

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

/// Header annotation for a read result: `N of M lines` when the tool's
/// "Truncated lines: a-b" marker reports the file total, `N lines` otherwise.
/// Counts only content rows (`"<nr>: <text>"`), not trailing markers or
/// discovered-instruction blocks.
fn read_annotation(text: &str) -> String {
    let shown = text
        .lines()
        .take_while(|l| !l.is_empty())
        .filter(|l| {
            l.split_once(": ")
                .is_some_and(|(nr, _)| !nr.is_empty() && nr.chars().all(|c| c.is_ascii_digit()))
        })
        .count();
    let total = text
        .lines()
        .find_map(|l| l.strip_prefix("Truncated lines: "))
        .and_then(|l| l.split('-').nth(1))
        .and_then(|m| m.split('.').next())
        .and_then(|m| m.trim().parse::<usize>().ok());
    match total {
        Some(total) if total > shown => format!("{shown} of {total} lines"),
        _ => format!("{shown} lines"),
    }
}

/// Header annotation for a grep result: `N matches in M files`, counting
/// `path:` headers and indented `<line>: <text>` match rows (the tool's
/// bracketed notice rows do not count as matches).
fn grep_annotation(text: &str) -> String {
    let mut matches = 0usize;
    let mut files = 0usize;
    let mut truncated = false;
    for line in text.lines() {
        if line.ends_with(':') && !line.starts_with(' ') {
            files += 1;
        } else if let Some(rest) = line.strip_prefix("  ") {
            let is_match = rest
                .split_once(": ")
                .is_some_and(|(nr, _)| !nr.is_empty() && nr.chars().all(|c| c.is_ascii_digit()));
            if is_match {
                matches += 1;
            }
        } else if line.starts_with("[Search truncated") {
            truncated = true;
        }
    }
    let f = if files == 1 { "file" } else { "files" };
    let m = if matches == 1 { "match" } else { "matches" };
    let suffix = if truncated { " (truncated)" } else { "" };
    format!("{matches} {m} in {files} {f}{suffix}")
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
        name if EDIT_TOOLS.contains(&name) => ToolKind::Edit {
            path: detail,
            summary: String::new(),
        },
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
            (
                ToolKind::Read {
                    path: detail,
                    summary: read_annotation(&text),
                },
                lines,
                None,
            )
        }
        "grep" => {
            let lines = context_lines(&text);
            (
                ToolKind::Grep {
                    pattern: detail,
                    summary: grep_annotation(&text),
                },
                lines,
                None,
            )
        }
        "write" => {
            let (title, body) = edit_title_body(&text, result.is_error);
            (
                ToolKind::Edit {
                    path: detail.clone(),
                    summary: title,
                },
                diff_lines(body),
                if detail.is_empty() || result.is_error {
                    None
                } else {
                    Some((detail, FileStatus::Created))
                },
            )
        }
        tool if EDIT_TOOLS.contains(&tool) => {
            let status = match tool {
                "delete" => FileStatus::Deleted,
                "move" => FileStatus::Created,
                _ => FileStatus::Modified,
            };
            let (title, body) = edit_title_body(&text, result.is_error);
            (
                ToolKind::Edit {
                    path: detail.clone(),
                    summary: title,
                },
                diff_lines(body),
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

/// Model-facing tool output arrives wrapped in `<untrusted-content>` marker
/// lines (`crate::tools::bash::wrap_untrusted`); on screen they are noise,
/// so the card drops them. The filter only engages when both markers are
/// present as full lines, so output that merely quotes a tag keeps it (the
/// wrapper zero-width-breaks its own payload copies, which never match a
/// full-line marker anyway).
fn strip_untrusted_markers(text: &str) -> impl Iterator<Item = &str> {
    let wrapped = text.lines().any(|l| l == "<untrusted-content>")
        && text.lines().any(|l| l == "</untrusted-content>");
    text.lines()
        .filter(move |l| !wrapped || !matches!(*l, "<untrusted-content>" | "</untrusted-content>"))
}

fn context_lines(text: &str) -> Vec<ToolLine> {
    strip_untrusted_markers(text)
        .map(|line| ToolLine::new(LineKind::Context, line))
        .collect()
}

fn edit_title_body(text: &str, is_error: bool) -> (String, &str) {
    if is_error {
        (String::new(), text)
    } else {
        split_title_line(text)
    }
}

/// Split a mutation tool's result into its title line and body. Edit-family
/// results open with a verb summary (`edited x`, `overwrote x (N bytes)`,
/// `deleted: x`, `moved a -> b`) that belongs in the card header, not the
/// body. A line only counts as the title when it can't be mistaken for a
/// diff row; anything else (errors, foreign formats) stays in the body.
fn split_title_line(text: &str) -> (String, &str) {
    let (first, rest) = match text.split_once('\n') {
        Some((first, rest)) => (first, rest),
        None => return (String::new(), text),
    };
    let is_diff_row = ["--- ", "+++ ", "@@ ", "+", "-", " "]
        .iter()
        .any(|p| first.starts_with(p));
    if first.is_empty() || is_diff_row {
        (String::new(), text)
    } else {
        (first.to_string(), rest)
    }
}

/// Parse a unified diff (as produced by [`crate::diff::unified_text`]) into
/// structured card lines: `@@` hunk headers set the before-side line counter
/// and separate hunks with a [`LineKind::Gap`] marker, `- `/`+ `/`  ` prefixes
/// classify lines, and adjacent removed/added line pairs get word-level
/// emphasis ranges. Results without `@@` headers (legacy or foreign formats)
/// fall back to prefix-only classification starting at line 1.
fn diff_lines(text: &str) -> Vec<ToolLine> {
    let mut lines: Vec<ToolLine> = Vec::new();
    let mut before_line = 0usize;
    let mut saw_hunk = false;
    for line in text.lines() {
        if line.starts_with("--- ") || line.starts_with("+++") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            // "@@ -<b> +<a> @@"
            if let Some(b) = rest
                .split_whitespace()
                .next()
                .and_then(|t| t.strip_prefix('-'))
                .and_then(|t| t.parse::<usize>().ok())
            {
                if saw_hunk {
                    lines.push(ToolLine::new(LineKind::Gap, "..."));
                }
                before_line = b;
                saw_hunk = true;
            }
            continue;
        }
        let (kind, body) = match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => (LineKind::Add, &line[1..]),
            Some(b'-') if !line.starts_with("---") => (LineKind::Del, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            _ => {
                // Summary header line before the first hunk.
                if !saw_hunk {
                    lines.push(ToolLine::new(LineKind::Context, line));
                }
                continue;
            }
        };
        let body = body.strip_prefix(' ').unwrap_or(body);
        let nr = if kind == LineKind::Add {
            0
        } else {
            before_line
        };
        if kind != LineKind::Add {
            before_line += 1;
        }
        lines.push(ToolLine {
            kind,
            text: body.to_string(),
            nr,
            emph: Vec::new(),
        });
    }
    emphasize_pairs(&mut lines);
    lines
}

/// Word-level emphasis for adjacent removed/added line pairs, mirroring the
/// reference's inline-change `DiffSpan.emphasized` from the unified text
/// alone: the i-th removed line of a run is paired with the i-th added line
/// of the run that follows it.
fn emphasize_pairs(lines: &mut [ToolLine]) {
    use similar::TextDiff;

    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != LineKind::Del {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Del {
            i += 1;
        }
        let add_start = i;
        while i < lines.len() && lines[i].kind == LineKind::Add {
            i += 1;
        }
        let pairs = (i - add_start).min(add_start - del_start);
        for k in 0..pairs {
            let (del, add) = lines.split_at_mut(add_start);
            let d = &mut del[del_start + k];
            let a = &mut add[k];
            let diff = TextDiff::from_words(&d.text, &a.text);
            let (mut doff, mut aoff) = (0usize, 0usize);
            for change in diff.iter_all_changes() {
                let len = change.value().chars().count();
                match change.tag() {
                    similar::ChangeTag::Equal => {
                        doff += len;
                        aoff += len;
                    }
                    similar::ChangeTag::Delete => {
                        if len > 0 {
                            d.emph.push((doff, doff + len));
                        }
                        doff += len;
                    }
                    similar::ChangeTag::Insert => {
                        if len > 0 {
                            a.emph.push((aoff, aoff + len));
                        }
                        aoff += len;
                    }
                }
            }
        }
    }
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

/// "45.9K/200K (23%)"-style label: tokens used over the context window,
/// with the prompt's share as a percentage. The total and percentage are
/// omitted when the context length is unknown.
pub(super) fn usage_label(tokens: u64, prompt_tokens: u64, context_length: Option<u32>) -> String {
    let used = tokens as f64 / 1000.0;
    match context_length {
        Some(size) if size > 0 => {
            let pct = (prompt_tokens as f64 / f64::from(size) * 100.0).round();
            let total = f64::from(size);
            let total = if total >= 1_000_000.0 {
                format!("{:.1}M", total / 1_000_000.0)
            } else {
                format!("{:.0}K", total / 1000.0)
            };
            format!("{used:.1}K/{total} ({pct:.0}%)")
        }
        _ => format!("{used:.1}K"),
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
    fn read_annotation_counts_content_rows_and_totals() {
        let full = "1: a\n2: b\n3: c";
        assert_eq!(read_annotation(full), "3 lines");
        let truncated = "1: a\n2: b\n\n...\n\nTruncated lines: 3-90. Use offset=3 to read further.";
        assert_eq!(read_annotation(truncated), "2 of 90 lines");
    }

    #[test]
    fn grep_annotation_counts_matches_and_files() {
        let one = "src/a.rs:\n  12: fn a() {}\n  30: fn b() {}";
        assert_eq!(grep_annotation(one), "2 matches in 1 file");
        let two = "src/a.rs:\n  12: x\n\nsrc/b.rs:\n  5: y\n  [line truncated; excerpt starts at byte column 1; match at byte column 1]";
        assert_eq!(grep_annotation(two), "2 matches in 2 files");
        let truncated = "src/a.rs:\n  12: x\n\n[Search truncated: more matches may exist.]";
        assert_eq!(grep_annotation(truncated), "1 match in 1 file (truncated)");
    }

    #[test]
    fn write_done_titles_with_the_verb_header() {
        let result = history::ToolResult::text(
            "t1",
            "write",
            "overwrote a.txt (5 bytes)\n--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n- old\n+ new",
        );
        let done = tool_done(
            "t1".into(),
            "write",
            &serde_json::json!({"content": "hello", "path": "a.txt"}),
            &result,
        );
        assert!(
            matches!(&done.card.kind, ToolKind::Edit { summary, .. } if summary == "overwrote a.txt (5 bytes)")
        );
        // The title line is not duplicated in the body.
        let body: Vec<&str> = done.card.lines.iter().map(|l| l.text.as_str()).collect();
        assert!(!body.contains(&"overwrote a.txt (5 bytes)"), "{body:?}");
    }

    #[test]
    fn delete_done_titles_with_the_verb_header() {
        let result = history::ToolResult::text("t1", "delete", "deleted: a.txt\nskipped: b.txt");
        let done = tool_done(
            "t1".into(),
            "delete",
            &serde_json::json!({"path": "a.txt"}),
            &result,
        );
        assert!(
            matches!(&done.card.kind, ToolKind::Edit { summary, .. } if summary == "deleted: a.txt")
        );
    }

    #[test]
    fn error_results_keep_the_plain_edit_title() {
        let result = history::ToolResult {
            is_error: true,
            ..history::ToolResult::text("t1", "edit", "boom: path missing\n--- a\n+++ a")
        };
        let done = tool_done(
            "t1".into(),
            "edit",
            &serde_json::json!({"path": "a.txt"}),
            &result,
        );
        assert!(matches!(&done.card.kind, ToolKind::Edit { summary, .. } if summary.is_empty()));
    }

    #[test]
    fn grep_done_uses_structured_summary() {
        let text = "src/a.rs:\n  12: fn a() {}";
        let result = history::ToolResult::text("t1", "grep", text);
        let done = tool_done(
            "t1".into(),
            "grep",
            &serde_json::json!({"pattern": "fn"}),
            &result,
        );
        assert!(
            matches!(&done.card.kind, ToolKind::Grep { summary, .. } if summary == "1 match in 1 file")
        );
    }

    #[test]
    fn bash_done_drops_untrusted_markers_but_keeps_the_payload() {
        // Same shape as `tools::bash::wrap_untrusted`, including the exit
        // tail and a zero-width-broken payload copy of the closing tag.
        let text = "<untrusted-content>\nok: done\n</untrusted-content\u{200b}>\n</untrusted-content>\nExit code: 1";
        let result = history::ToolResult {
            is_error: true,
            ..history::ToolResult::text("t1", "bash", text)
        };
        let done = tool_done(
            "t1".into(),
            "bash",
            &serde_json::json!({"command": "false"}),
            &result,
        );
        let body: Vec<&str> = done.card.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            body,
            vec!["ok: done", "</untrusted-content\u{200b}>", "Exit code: 1"]
        );
    }

    #[test]
    fn content_quoting_a_single_marker_keeps_it() {
        // No closing marker: this is payload, not the wrapper, so nothing
        // is filtered.
        let lines = context_lines("<untrusted-content>\nsome text");
        let body: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(body, vec!["<untrusted-content>", "some text"]);
    }

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
        assert!(matches!(done.card.kind, ToolKind::Edit { ref path, .. } if path == "a.txt"));
        assert_eq!(done.touched.map(|(p, _)| p).as_deref(), Some("a.txt"));
    }
    #[test]
    fn diff_lines_parses_hunks_gaps_and_numbers() {
        let text = "edited f\n--- f\n+++ f\n@@ -2 +2 @@\n  ctx\n- old\n+ new\n@@ -9 +9 @@\n  far";
        let lines = diff_lines(text);
        let kinds: Vec<LineKind> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Context, // summary line
                LineKind::Context, // ctx
                LineKind::Del,
                LineKind::Add,
                LineKind::Gap,
                LineKind::Context,
            ]
        );
        // Before-side numbering: ctx=2, old=3, add=0; second hunk restarts at 9.
        assert_eq!(
            lines.iter().map(|l| l.nr).collect::<Vec<_>>(),
            vec![0, 2, 3, 0, 0, 9]
        );
        assert_eq!(lines[2].text, "old");
        assert_eq!(lines[3].text, "new");
    }

    #[test]
    fn diff_lines_pairs_removed_and_added_for_word_emphasis() {
        let text = "@@ -1 +1 @@\n- fn a() { one }\n+ fn a() { two }";
        let lines = diff_lines(text);
        let (del, add) = (&lines[0], &lines[1]);
        assert_eq!(del.text, "fn a() { one }");
        assert_eq!(add.text, "fn a() { two }");
        let slice = |l: &ToolLine| {
            l.emph
                .iter()
                .map(|&(s, e)| l.text.chars().skip(s).take(e - s).collect::<String>())
                .collect::<String>()
        };
        assert_eq!(slice(del), "one");
        assert_eq!(slice(add), "two");
    }

    #[test]
    fn diff_lines_legacy_prefix_only_results_start_unnumbered() {
        let lines = diff_lines("+added\n-removed\nplain");
        assert_eq!(
            lines.iter().map(|l| l.kind).collect::<Vec<_>>(),
            vec![LineKind::Add, LineKind::Del, LineKind::Context]
        );
        assert!(lines.iter().all(|l| l.nr == 0));
    }
}
