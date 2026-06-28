//! Hashline-style line anchors for `edit`/`multiedit`.
//!
//! The model points at a short content-hash of the target line(s) instead of
//! (or in addition to) retyping them. The applier verifies the anchor against
//! the *current* file content before applying; a stale anchor is rejected
//! before any write, and the current line content is returned so the model can
//! self-correct in one retry. This mirrors omp's hashline approach but reuses
//! craft's existing diff/apply plumbing.
//!
//! Anchor format: a 12-char hex prefix of a stable hash over the joined target
//! lines (trim-normalized so whitespace-only drift does not invalidate anchors,
//! but semantic changes do).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::fuzzy_replace;

pub(super) const ANCHOR_LEN: usize = 12;

const STALE_ANCHOR_PREFIX: &str =
    "stale line anchor: line(s) changed since read. Current content:\n";

/// An anchor-guided replacement request.
#[derive(Debug, Clone)]
pub(super) struct AnchoredEdit {
    pub line_anchor_hash: String,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug)]
pub(super) enum AnchorOutcome {
    Applied {
        content: String,
        pass: fuzzy_replace::Pass,
    },
    Stale {
        current: String,
    },
    NotFound,
}

/// Compute the anchor hash for a set of lines (already joined by `\n`).
pub fn anchor_hash(text: &str) -> String {
    let mut h = DefaultHasher::new();
    normalize_for_anchor(text).hash(&mut h);
    let full = format!("{:016x}", h.finish());
    full[..ANCHOR_LEN].to_string()
}

fn normalize_for_anchor(text: &str) -> String {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Verify the anchor against the file content and apply the replacement.
///
/// `content` is the current file text. We find the candidate location by
/// running the normal fuzzy matcher on `old_text`; once located we hash the
/// matched lines and compare to the anchor. On match we apply; on mismatch we
/// return the current matched lines so the model can retry.
pub(super) fn apply_anchored(
    content: &str,
    edit: &AnchoredEdit,
    replace_all: bool,
) -> AnchorOutcome {
    let matched = match fuzzy_replace::locate(content, &edit.old_text) {
        Some(m) => m,
        None => return AnchorOutcome::NotFound,
    };

    if !anchor_hash(&matched).eq_ignore_ascii_case(&edit.line_anchor_hash) {
        return AnchorOutcome::Stale { current: matched };
    }

    match fuzzy_replace::replace(content, &edit.old_text, &edit.new_text, replace_all, None) {
        Ok(result) => AnchorOutcome::Applied {
            content: result.content,
            pass: result.pass,
        },
        Err(e) if e == fuzzy_replace::NO_MATCH => AnchorOutcome::NotFound,
        Err(_) => AnchorOutcome::NotFound,
    }
}

pub(super) fn stale_message(current: &str) -> String {
    format!("{STALE_ANCHOR_PREFIX}{current}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn empty_edit() -> AnchoredEdit {
        AnchoredEdit {
            line_anchor_hash: String::new(),
            old_text: String::new(),
            new_text: String::new(),
        }
    }

    fn anchor_for(text: &str) -> String {
        anchor_hash(text)
    }

    #[test]
    fn anchor_hash_is_stable_and_trim_insensitive() {
        let h1 = anchor_hash("fn foo() {\n    bar();\n}");
        let h2 = anchor_hash("fn foo() {\nbar();\n}");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), ANCHOR_LEN);
        let h3 = anchor_hash("fn foo() {\n    baz();\n}");
        assert_ne!(h1, h3);
    }

    #[test]
    fn anchor_hash_ignores_blank_lines() {
        let h1 = anchor_hash("a\n\n\nb");
        let h2 = anchor_hash("a\nb");
        assert_eq!(h1, h2);
    }

    #[test]
    fn apply_with_valid_anchor_succeeds() {
        let content = "fn foo() {\n    bar();\n}";
        let mut edit = empty_edit();
        edit.line_anchor_hash = anchor_for("fn foo() {\n    bar();\n}");
        edit.old_text = "fn foo() {\n    bar();\n}".into();
        edit.new_text = "fn foo() {\n    baz();\n}".into();
        match apply_anchored(content, &edit, false) {
            AnchorOutcome::Applied { content, .. } => assert!(content.contains("baz")),
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn stale_anchor_rejected_with_current_content() {
        let content = "fn foo() {\n    CHANGED();\n}";
        let mut edit = empty_edit();
        edit.line_anchor_hash = anchor_for("fn foo() {\n    bar();\n}");
        edit.old_text = "fn foo() {\n    CHANGED();\n}".into();
        edit.new_text = "fn foo() {\n    baz();\n}".into();
        match apply_anchored(content, &edit, false) {
            AnchorOutcome::Stale { current } => {
                assert!(
                    !current.is_empty(),
                    "stale outcome should return current content"
                );
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn not_found_when_old_text_absent() {
        let mut edit = empty_edit();
        edit.line_anchor_hash = anchor_for("fn foo() {}");
        edit.old_text = "MISSING".into();
        edit.new_text = "x".into();
        match apply_anchored("fn foo() {}", &edit, false) {
            AnchorOutcome::NotFound => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn stale_message_includes_current() {
        let msg = stale_message("fn real() {}");
        assert!(msg.starts_with(STALE_ANCHOR_PREFIX));
        assert!(msg.contains("fn real() {}"));
    }

    #[test]
    fn anchor_case_insensitive_compare() {
        let content = "fn foo() {\n    bar();\n}";
        let mut edit = empty_edit();
        let lower = anchor_for("fn foo() {\n    bar();\n}");
        edit.line_anchor_hash = lower.to_uppercase();
        edit.old_text = "fn foo() {\n    bar();\n}".into();
        edit.new_text = "fn foo() {\n    baz();\n}".into();
        assert!(matches!(
            apply_anchored(content, &edit, false),
            AnchorOutcome::Applied { .. }
        ));
    }

    /// The per-line anchor the `read` tool surfaces (`anchor_hash(line)`) must
    /// be usable directly as `line_anchor_hash` for a single-line edit: the
    /// applier locates the line and recomputes the same hash.
    #[test]
    fn single_line_anchor_round_trips_through_apply() {
        let content = "let a = 1;\nlet b = 2;\nlet c = 3;\n";
        let target_line = "let b = 2;";
        let displayed_anchor = anchor_hash(target_line);

        let mut edit = empty_edit();
        edit.line_anchor_hash = displayed_anchor;
        edit.old_text = target_line.into();
        edit.new_text = "let b = 20;".into();
        match apply_anchored(content, &edit, false) {
            AnchorOutcome::Applied { content, .. } => {
                assert!(content.contains("let b = 20;"));
                assert!(!content.contains("let b = 2;\n"));
            }
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    /// Anchors are robust to the line having different leading/trailing
    /// whitespace in the file versus the display (trim-normalized).
    #[test]
    fn single_line_anchor_matches_when_file_line_is_indented() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let displayed = "let x = 1;";
        let mut edit = empty_edit();
        edit.line_anchor_hash = anchor_hash(displayed);
        edit.old_text = displayed.into();
        edit.new_text = "let x = 2;".into();
        assert!(matches!(
            apply_anchored(content, &edit, false),
            AnchorOutcome::Applied { .. }
        ));
    }
}
