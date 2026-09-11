use std::collections::HashSet;
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

static STOP_WORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "shall", "can",
        "need", "must", "to", "of", "in", "for", "on", "with", "at", "by", "from", "as", "into",
        "through", "during", "before", "after", "above", "below", "between", "under", "over",
        "and", "but", "or", "nor", "not", "so", "yet", "both", "either", "neither", "each",
        "every", "all", "any", "few", "more", "most", "other", "some", "such", "no", "that",
        "this", "these", "those", "it", "its", "i", "me", "my", "we", "our", "you", "your", "he",
        "him", "his", "she", "her", "they", "them", "their", "who", "which", "what", "if", "then",
        "than", "when", "where", "how", "just", "also",
    ]
    .into_iter()
    .collect()
});

static CONTENT_WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{L}[\p{L}\p{N}]*|\p{N}+").unwrap());

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

/// Word-aware truncation: count content words (skipping stop words), cut at
/// `limit` content words and append `...(truncated)`.
pub(crate) fn truncate_tokens(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut count = 0;
    let mut cut_idx = flat.len();
    for m in CONTENT_WORD_RE.find_iter(&flat) {
        let word = m.as_str();
        if !STOP_WORDS.contains(&word.to_lowercase().as_str()) {
            count += 1;
            if count > limit {
                cut_idx = m.start();
                break;
            }
        }
        cut_idx = m.end();
    }
    if count <= limit {
        return flat;
    }
    let cut_idx = flat.floor_char_boundary(cut_idx);
    format!("{}...(truncated)", flat[..cut_idx].trim_end())
}

pub(crate) fn sanitize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(|c: char| c.is_control() && c != '\n' && c != '\t', "")
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

/// Clip to the last sentence boundary at or before `max`, else word boundary.
pub(crate) fn clip_sentence(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let window = &text[..text.floor_char_boundary(max)];
    if let Some(last) = window.rfind(['.', '!', '?']) {
        let after = &window[last + 1..];
        if after.chars().all(|c| c.is_whitespace()) || last + 1 >= max * 5 / 10 {
            return text[..last + 1].trim_end().to_string();
        }
    }
    clip(text, max)
}

pub(crate) fn non_empty_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
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

#[expect(dead_code)]
pub(crate) fn summarize_tool_args(args: &Value) -> String {
    if let Some(p) = extract_path(args) {
        return format!("path={p}");
    }
    if let Some(c) = args.get("command").and_then(|v| v.as_str()) {
        return format!("command={c}");
    }
    if let Some(q) = args.get("query").and_then(|v| v.as_str()) {
        return format!("query={q}");
    }
    let keys: Vec<&str> = args
        .as_object()
        .map(|o| o.keys().map(|k| k.as_str()).collect())
        .unwrap_or_default();
    keys.join(", ")
}

/// Collapse `<skill name="X" ...>...</skill>` blocks into `[skill: X]` markers.
pub(crate) fn collapse_skill_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<skill ") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some(ns) = after.find("name=\"") {
            let name_start = ns + 6;
            if let Some(ne) = after[name_start..].find('"') {
                let name = &after[name_start..name_start + ne];
                out.push_str(&format!("[skill: {name}]"));
                if let Some(close_rel) = after.find("</skill>") {
                    rest = &after[close_rel + "</skill>".len()..];
                    continue;
                }
                return out;
            }
        }
        out.push('<');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn collapse_skill_lines(lines: &[String]) -> Vec<String> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut inside = false;
    for line in lines {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("<skill ") {
            inside = true;
            if let Some(ns) = rest.find("name=\"") {
                let after = &rest[ns + 6..];
                if let Some(ne) = after.find('"') {
                    let name = &after[..ne];
                    if seen.insert(name.to_string()) {
                        result.push(format!("[skill: {name}]"));
                    }
                    continue;
                }
            }
            continue;
        }
        if inside {
            if trimmed.starts_with("</skill>") {
                inside = false;
            }
            continue;
        }
        result.push(line.clone());
    }
    result
}

static KEY_STOPS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "the",
        "a",
        "an",
        "this",
        "that",
        "these",
        "those",
        "it",
        "its",
        "is",
        "was",
        "are",
        "were",
        "been",
        "being",
        "has",
        "have",
        "had",
        "does",
        "do",
        "did",
        "will",
        "would",
        "could",
        "should",
        "may",
        "might",
        "shall",
        "can",
        "to",
        "of",
        "in",
        "for",
        "on",
        "with",
        "at",
        "by",
        "from",
        "as",
        "into",
        "through",
        "before",
        "after",
        "above",
        "below",
        "and",
        "but",
        "or",
        "not",
        "so",
        "yet",
        "when",
        "where",
        "while",
        "during",
        "against",
        "all",
        "added",
        "adding",
        "created",
        "creating",
        "applied",
        "applying",
        "inserted",
        "inserting",
        "implemented",
        "implementing",
        "introduced",
        "introducing",
        "using",
        "swapped",
        "swapping",
        "split",
        "splitting",
        "migrated",
        "migrating",
        "isolated",
        "isolating",
        "removed",
        "removing",
        "extracted",
        "extracting",
        "replaced",
        "replacing",
        "refactored",
        "refactoring",
        "wrapped",
        "wrapping",
        "guarded",
        "guarding",
        "moved",
        "moving",
        "updated",
        "configured",
        "enabled",
        "switched",
    ]
    .into_iter()
    .collect()
});

const KEY_MAX_WORDS: usize = 3;

/// Build a compact breadcrumb key: up to `KEY_MAX_WORDS` content words (skipping
/// stop words), joined with "-".
pub(crate) fn refine_breadcrumb_key(fragment: &str) -> String {
    let words: Vec<&str> = fragment.split_whitespace().collect();
    let mut content: Vec<&str> = Vec::new();
    for w in words {
        if KEY_STOPS.contains(w.to_lowercase().as_str()) {
            continue;
        }
        content.push(w);
        if content.len() >= KEY_MAX_WORDS {
            break;
        }
    }
    if content.is_empty() {
        return fragment.chars().take(40).collect();
    }
    content.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_prefers_word_boundary() {
        assert_eq!(clip("hello world foo", 10), "hello worl");
    }

    #[test]
    fn clip_uses_word_boundary_in_latter_range() {
        assert_eq!(clip("hello world foo bar", 14), "hello world");
    }

    #[test]
    fn clip_returns_short_unchanged() {
        assert_eq!(clip("short", 100), "short");
    }

    #[test]
    fn clip_sentence_keeps_punctuation() {
        assert_eq!(
            clip_sentence("Fix the bug. Then move on.", 12),
            "Fix the bug."
        );
    }

    #[test]
    fn clip_does_not_panic_on_multibyte_boundary() {
        // 'é' is 2 bytes; a slice ending at an odd byte index would split it.
        let s = "é".repeat(50);
        let out = clip(&s, 81);
        assert!(out.len() <= s.len());
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn clip_sentence_does_not_panic_on_multibyte_boundary() {
        // 'é' is 2 bytes; byte 80 lands mid-codepoint.
        let s = "é".repeat(50) + ". tail";
        let out = clip_sentence(&s, 80);
        assert!(out.len() <= s.len());
        assert!(out.chars().all(|c| c == 'é' || c == '.' || c == ' '));
    }

    #[test]
    fn compress_bash_does_not_panic_on_multibyte_command() {
        let cmd = "echo ".to_string() + &"界".repeat(60);
        let out = compress_bash(&cmd);
        assert!(out.ends_with("...") || out.len() <= BASH_CAP);
    }

    #[test]
    fn collapse_skill_text_dedups() {
        let text = "<skill name=\"audit\">body1</skill> mid <skill name=\"audit\">body2</skill>";
        assert_eq!(
            collapse_skill_text(text),
            "[skill: audit] mid [skill: audit]"
        );
    }

    #[test]
    fn refine_breadcrumb_key_skips_stops() {
        assert_eq!(
            refine_breadcrumb_key("the session check added"),
            "session-check"
        );
    }

    #[test]
    fn extract_path_finds_file_path() {
        let v = serde_json::json!({"file_path": "/a/b.rs"});
        assert_eq!(extract_path(&v), Some("/a/b.rs".into()));
    }
}
