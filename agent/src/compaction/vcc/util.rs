//! Breadcrumb helpers for the craft VCC extractor (trimmed in 08 to what
//! the extractor uses; the sections/merge machinery went with the craft
//! VCC algorithm).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

static PIPE_TAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\s*\|\s*(?:head|tail|sort|wc|column|tr|cut|awk|uniq|python3|node|bun)(?:\s[^|]*)?$",
    )
    .unwrap()
});

static CD_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^cd\s+\S+\s*&&\s*").unwrap());

const BASH_CAP: usize = 120;

/// Semantic compression of a bash command: flatten to first meaningful line,
/// strip `cd` prefix and pipe tails, cap length.
pub(crate) fn compress_bash(raw: &str) -> String {
    let cmd = raw
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or(raw);
    let mut cmd = CD_PREFIX_RE.replace(cmd, "").to_string();
    for _ in 0..3 {
        let stripped = PIPE_TAIL_RE.replace(&cmd, "").to_string();
        if stripped == cmd {
            break;
        }
        cmd = stripped;
    }
    if cmd.len() > BASH_CAP {
        let cut = cmd.floor_char_boundary(BASH_CAP - 3);
        format!("{}...", &cmd[..cut])
    } else {
        cmd
    }
}

/// Clip text to `max` chars, preferring a word boundary.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let max = text.floor_char_boundary(max);
    let cut = text[..max].rfind(' ').unwrap_or(max);
    let end = if cut > max * 6 / 10 { cut } else { max };
    let end = end.min(bytes.len());
    let end = text.floor_char_boundary(end);
    text[..end].to_string()
}

pub(crate) fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    clip(line, max)
}

const PATH_KEYS: &[&str] = &["path", "file_path", "filePath", "file"];

pub(crate) fn extract_path(args: &Value) -> Option<String> {
    for key in PATH_KEYS {
        if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_prefers_word_boundary() {
        assert_eq!(clip("one two three", 8), "one two");
        assert_eq!(clip("short", 20), "short");
    }

    #[test]
    fn clip_returns_short_unchanged() {
        assert_eq!(clip("exact", 5), "exact");
    }

    #[test]
    fn compress_bash_strips_cd_and_pipe_tails() {
        assert_eq!(
            compress_bash("cd /ws && cargo test | tail -5"),
            "cargo test"
        );
    }

    #[test]
    fn compress_bash_takes_first_meaningful_line() {
        assert_eq!(compress_bash("\n\n  ls -la  \nunused"), "ls -la");
    }

    #[test]
    fn extract_path_checks_every_spelling() {
        for key in ["path", "file_path", "filePath", "file"] {
            let args = serde_json::json!({ key: "a.rs" });
            assert_eq!(extract_path(&args).as_deref(), Some("a.rs"));
        }
        assert_eq!(extract_path(&serde_json::json!({})), None);
    }

    #[test]
    fn clip_does_not_panic_on_multibyte_boundary() {
        // CJK text: byte boundaries land inside characters.
        let text = "日本語のテキスト".repeat(20);
        let clipped = clip(&text, 33);
        assert!(!clipped.is_empty());
    }

    #[test]
    fn compress_bash_does_not_panic_on_multibyte_command() {
        let command = format!("echo '{}'", "日本語".repeat(60));
        let compressed = compress_bash(&command);
        assert!(!compressed.is_empty());
    }

    #[test]
    fn first_line_clips() {
        // The only space sits exactly at the 60% mark, where clip keeps the
        // hard boundary instead.
        assert_eq!(
            first_line("a very long first line\nsecond", 10),
            "a very lon"
        );
        assert_eq!(first_line("one two three\nsecond", 7), "one two");
    }
}
