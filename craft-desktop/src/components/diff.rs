//! Unified diff renderer: old/new text → colored add/del/context lines.
//! Replaces the old React `DiffView` (same LCS approach, via `similar`).

use dioxus::prelude::*;
use similar::{ChangeTag, TextDiff};

/// Unchanged lines kept on each side of a change hunk in a folded diff.
const CONTEXT_LINES: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    Add,
    Del,
    Ctx,
    /// Collapsed run of unchanged lines; text reads "N unchanged lines".
    Fold,
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

/// Diff with long unchanged runs collapsed to fold markers, keeping
/// `CONTEXT_LINES` of context around each change (git-style).
pub fn diff_lines_folded(old_text: &str, new_text: &str) -> Vec<DiffLine> {
    let lines = diff_lines(old_text, new_text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].kind != DiffKind::Ctx {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && lines[i].kind == DiffKind::Ctx {
            i += 1;
        }
        let run = &lines[start..i];
        let head = if start == 0 { 0 } else { CONTEXT_LINES };
        let tail = if i == lines.len() { 0 } else { CONTEXT_LINES };
        if run.len() <= head + tail {
            out.extend(run.iter().cloned());
        } else {
            out.extend(run[..head].iter().cloned());
            out.push(DiffLine {
                kind: DiffKind::Fold,
                text: format!("{} unchanged lines", run.len() - head - tail),
            });
            out.extend(run[run.len() - tail..].iter().cloned());
        }
    }
    out
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
            DiffKind::Fold => ("code-line fold", "⋯", l.text.as_str()),
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
    use test_case::test_case;

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

    fn kinds(lines: &[DiffLine]) -> Vec<DiffKind> {
        lines.iter().map(|l| l.kind).collect()
    }

    #[test_case(20, 10; "interior_change")]
    #[test_case(12, 0; "change_at_file_start")]
    #[test_case(12, 11; "change_at_file_end")]
    #[test_case(7, 3; "short_file_stays_unfolded")]
    fn folded_keeps_context_around_change(len: usize, change_at: usize) {
        let old: Vec<String> = (0..len).map(|i| format!("line{i}")).collect();
        let mut new = old.clone();
        new[change_at] = "changed".to_string();
        let lines = diff_lines_folded(&old.join("\n"), &new.join("\n"));

        let mut expected: Vec<(DiffKind, Option<usize>)> = Vec::new();
        let leading = change_at;
        if leading > CONTEXT_LINES {
            expected.push((DiffKind::Fold, Some(leading - CONTEXT_LINES)));
            expected.extend(std::iter::repeat_n((DiffKind::Ctx, None), CONTEXT_LINES));
        } else {
            expected.extend(std::iter::repeat_n((DiffKind::Ctx, None), leading));
        }
        expected.push((DiffKind::Del, None));
        expected.push((DiffKind::Add, None));
        let trailing = len - change_at - 1;
        if trailing > CONTEXT_LINES {
            expected.extend(std::iter::repeat_n((DiffKind::Ctx, None), CONTEXT_LINES));
            expected.push((DiffKind::Fold, Some(trailing - CONTEXT_LINES)));
        } else {
            expected.extend(std::iter::repeat_n((DiffKind::Ctx, None), trailing));
        }

        assert_eq!(
            kinds(&lines),
            expected.iter().map(|e| e.0).collect::<Vec<_>>()
        );
        for (line, (_, folded_count)) in lines.iter().zip(&expected) {
            if let Some(n) = folded_count {
                assert_eq!(line.text, format!("{n} unchanged lines"));
            }
        }
    }

    #[test]
    fn folded_keeps_close_hunks_together() {
        let old: Vec<String> = (0..14).map(|i| format!("line{i}")).collect();
        let mut new = old.clone();
        new[2] = "x".to_string();
        new[8] = "y".to_string();
        let lines = diff_lines_folded(&old.join("\n"), &new.join("\n"));
        let folds = lines.iter().filter(|l| l.kind == DiffKind::Fold).count();
        let ctx = lines.iter().filter(|l| l.kind == DiffKind::Ctx).count();
        assert_eq!(
            folds, 1,
            "only the trailing run past the context window folds"
        );
        assert_eq!(ctx, 10, "gap of 5 ctx lines between hunks stays unfolded");
    }

    #[test]
    fn folded_identical_text_is_single_fold() {
        let lines = diff_lines_folded("a\nb\nc\nd", "a\nb\nc\nd");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].kind, DiffKind::Fold);
        assert_eq!(lines[0].text, "4 unchanged lines");
    }
}
