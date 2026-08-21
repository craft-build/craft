//! Unified diff renderer: old/new text → colored add/del/context lines.
//! Replaces the old React `DiffView` (same LCS approach, via `similar`).

use dioxus::prelude::*;
use similar::{ChangeTag, TextDiff};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    Add,
    Del,
    Ctx,
}

#[derive(Clone, PartialEq, Debug)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Diff two texts line-wise. A missing/empty old text is a pure addition.
pub fn diff_lines(old_text: &str, new_text: &str) -> Vec<DiffLine> {
    if old_text.is_empty() {
        return new_text
            .lines()
            .map(|text| DiffLine {
                kind: DiffKind::Add,
                text: text.to_string(),
            })
            .collect();
    }
    let diff = TextDiff::from_lines(old_text, new_text);
    diff.iter_all_changes()
        .map(|change| DiffLine {
            kind: match change.tag() {
                ChangeTag::Insert => DiffKind::Add,
                ChangeTag::Delete => DiffKind::Del,
                ChangeTag::Equal => DiffKind::Ctx,
            },
            text: change.value().trim_end_matches('\n').to_string(),
        })
        .collect()
}

/// Line-count delta for the +N/-N stat badge.
pub fn diff_stat(old_text: &str, new_text: &str) -> (usize, usize) {
    let lines = diff_lines(old_text, new_text);
    (
        lines.iter().filter(|l| l.kind == DiffKind::Add).count(),
        lines.iter().filter(|l| l.kind == DiffKind::Del).count(),
    )
}

#[component]
pub fn DiffLines(lines: Vec<DiffLine>) -> Element {
    let rows: Vec<(&'static str, &'static str, &str)> = lines
        .iter()
        .map(|l| match l.kind {
            DiffKind::Add => ("code-line add", "+", l.text.as_str()),
            DiffKind::Del => ("code-line del", "-", l.text.as_str()),
            DiffKind::Ctx => ("code-line dctx", " ", l.text.as_str()),
        })
        .collect();
    rsx! {
        div { class: "codeblock",
            for (i, (class, gutter, text)) in rows.iter().enumerate() {
                div { key: "{i}", class: "{class}",
                    span { class: "code-gutter", "{gutter}" }
                    span { class: "code-text", "{text}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_add_when_no_old_text() {
        let lines = diff_lines("", "a\nb");
        assert!(lines.iter().all(|l| l.kind == DiffKind::Add));
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn mixed_changes() {
        let lines = diff_lines("a\nb\nc", "a\nx\nc");
        let adds = lines.iter().filter(|l| l.kind == DiffKind::Add).count();
        let dels = lines.iter().filter(|l| l.kind == DiffKind::Del).count();
        assert_eq!((adds, dels), (1, 1));
        assert_eq!(diff_stat("a\nb\nc", "a\nx\nc"), (1, 1));
    }
}
