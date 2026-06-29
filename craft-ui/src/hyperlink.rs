//! OSC-8 clickable hyperlinks, injected post-hoc into rendered buffer cells.
//!
//! OSC-8 (`ESC ]8;;<URI>ESC \ <text> ESC ]8;;ESC \`) makes arbitrary text
//! clickable in terminals that support it. The escape bytes are invisible
//! to the user but, critically, **not** zero-width to `unicode-width` — so
//! baking them into `Span` content would corrupt the wrap math that
//! `Paragraph` and `append_right_info` depend on.
//!
//! Instead we keep spans as plain text (widths stay correct) and rewrite
//! the target `Cell::symbol` *after* `Paragraph` has laid out the frame.
//! The escape bytes live in the cell symbol; the crossterm backend flushes
//! them verbatim. This mirrors how spinner-frame updates work.

use std::path::{Path, PathBuf};

/// OSC-8 link wrapper. `\x1b]8;;` opens, the URI follows, `ST` (`ESC \`)
/// terminates the parameter, then the visible text, then the closer.
const OSC8_OPEN: &str = "\u{1b}]8;;";
const OSC8_CLOSE: &str = "\u{1b}\\";
const OSC8_EMPTY: &str = "\u{1b}]8;;\u{1b}\\";

/// A hyperlink target: which segment-local cell range to wrap, and the URI.
/// Column ranges are in display columns within the logical line (before
/// `Paragraph` wrapping; each entry targets a single `Line`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hyperlink {
    pub line: usize,
    pub col_start: u16,
    pub col_end: u16,
    pub uri: String,
}

impl Hyperlink {
    pub fn new(line: usize, col_start: u16, col_end: u16, uri: String) -> Self {
        Self {
            line,
            col_start,
            col_end,
            uri,
        }
    }
}

/// Wraps a cell's existing symbol in an OSC-8 hyperlink to `uri`.
/// The visible text is preserved; only escape bytes are added.
pub fn apply_to_cell(cell: &mut ratatui::buffer::Cell, uri: &str) {
    let prev = cell.symbol();
    let wrapped = format!("{OSC8_OPEN}{uri}{OSC8_CLOSE}{prev}{OSC8_EMPTY}");
    cell.set_symbol(&wrapped);
}

/// Builds a `file://` URI from a path string, resolving `~` and
/// cwd-relative paths to absolute. Returns `None` if the path cannot be
/// made absolute (e.g. empty, or a bare filename with no cwd context).
pub fn file_uri(path: &str) -> Option<String> {
    let resolved = resolve_path(path)?;
    let canonical = canonicalize(&resolved).unwrap_or(resolved);
    Some(uri_from_path(&canonical))
}

fn resolve_path(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = craft_storage::paths::home()?;
        return Some(home.join(rest));
    }
    if trimmed == "~" {
        return craft_storage::paths::home();
    }
    if trimmed.starts_with('/') || (cfg!(windows) && trimmed.get(1..3) == Some(":\\")) {
        return Some(PathBuf::from(trimmed));
    }
    let cwd = std::env::current_dir().ok()?;
    Some(cwd.join(trimmed))
}

/// Canonicalize but tolerate non-existent paths (read tool may show a path
/// before it exists, or a removed file in a diff). Falls back to the input.
fn canonicalize(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Encodes an absolute path into a `file://` URI, percent-encoding
/// bytes that are not unreserved per RFC 3986.
fn uri_from_path(path: &Path) -> String {
    let mut out = String::from("file://");
    for &byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

/// Extracts a path-like substring from an image caption.
/// `read` produces captions like `[image ./foo.png]` or `[image /abs/bar.jpg]`;
/// browser screenshots contain a URL and are skipped (return `None`).
pub fn caption_path(caption: &str) -> Option<&str> {
    let inner = caption
        .strip_prefix("[image ")
        .and_then(|s| s.strip_suffix(']'))?;
    if inner.is_empty() {
        return None;
    }
    Some(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Cell;
    use test_case::test_case;

    #[test_case("/abs/file.rs"      ; "absolute")]
    #[test_case("~/notes.txt"       ; "tilde")]
    fn file_uri_starts_with_scheme(path: &str) {
        let uri = file_uri(path).expect("uri");
        assert!(uri.starts_with("file://"), "{uri}");
    }

    #[test]
    fn file_uri_percent_encodes_space() {
        let uri = file_uri("/abs/with space.rs").expect("uri");
        assert!(uri.contains("with%20space.rs"), "{uri}");
        assert!(!uri.contains(' '), "{uri}");
    }

    #[test]
    fn file_uri_none_for_empty() {
        assert!(file_uri("").is_none());
        assert!(file_uri("   ").is_none());
    }

    #[test_case("[image ./foo.png]",  Some("./foo.png")  ; "relative")]
    #[test_case("[image /a/b.jpg]",   Some("/a/b.jpg")   ; "absolute")]
    #[test_case("[image ]",           None               ; "empty_inner")]
    #[test_case("[screenshot of x]",  None               ; "browser_caption")]
    #[test_case("plain text",         None               ; "not_a_caption")]
    fn caption_path_cases(input: &str, expected: Option<&str>) {
        assert_eq!(caption_path(input), expected);
    }

    #[test]
    fn apply_to_cell_preserves_visible_text() {
        let mut cell = Cell::new("x");
        apply_to_cell(&mut cell, "file:///foo");
        let sym = cell.symbol();
        assert!(sym.contains("file:///foo"), "{sym}");
        assert!(sym.contains('x'), "{sym}");
        assert!(sym.starts_with("\u{1b}]8;;"), "{sym}");
    }

    #[test]
    fn apply_to_cell_wraps_empty_cell() {
        let mut cell = Cell::new("");
        apply_to_cell(&mut cell, "file:///x");
        let sym = cell.symbol();
        assert!(sym.contains("file:///x"));
        assert!(sym.contains(OSC8_EMPTY));
    }

    #[test]
    fn hyperlink_new_roundtrips() {
        let h = Hyperlink::new(2, 4, 8, "file:///a".into());
        assert_eq!(h.line, 2);
        assert_eq!(h.col_start, 4);
        assert_eq!(h.col_end, 8);
        assert_eq!(h.uri, "file:///a");
    }
}
