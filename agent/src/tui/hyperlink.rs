//! OSC-8 clickable hyperlinks, injected post-hoc into rendered buffer cells.
//!
//! OSC-8 (`ESC ]8;;<URI>ESC \ <text> ESC ]8;;ESC \`) makes arbitrary text
//! clickable in terminals that support it. The escape bytes are invisible
//! to the user but, critically, **not** zero-width to `unicode-width` — so
//! baking them into `Span` content would corrupt the wrap math that the
//! pre-wrapped message rows depend on.
//!
//! Instead we keep spans as plain text (widths stay correct) and rewrite
//! the target `Cell::symbol` *after* the `Paragraph` has laid out the
//! frame. The escape bytes live in the cell symbol; the crossterm backend
//! flushes them verbatim.
//!
//! Ported from the reference's `craft-ui/src/hyperlink.rs` (dropping
//! `caption_path`: this repo has no image captions yet).

use std::path::{Path, PathBuf};

/// OSC-8 link wrapper. `\x1b]8;;` opens, the URI follows, `ST` (`ESC \`)
/// terminates the parameter, then the visible text, then the closer.
const OSC8_OPEN: &str = "\u{1b}]8;;";
const OSC8_CLOSE: &str = "\u{1b}\\";
const OSC8_EMPTY: &str = "\u{1b}]8;;\u{1b}\\";

/// A hyperlink target: which cells of one display row to wrap, and the URI.
/// `row` addresses the message document's rows (the same coordinate space
/// the scrollback engine's `doc_row` produces); column ranges are within
/// that row. Each entry targets a single row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hyperlink {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub uri: String,
}

impl Hyperlink {
    pub fn new(row: u16, col_start: u16, col_end: u16, uri: String) -> Self {
        Self {
            row,
            col_start,
            col_end,
            uri,
        }
    }
}

/// Wraps a cell's existing symbol in an OSC-8 hyperlink to `uri`.
/// The visible text is preserved; only escape bytes are added.
///
/// The symbol's computed width would be the URI's length, which the
/// buffer diff treats as a multi-width glyph — skipping (and never
/// painting) the rest of the row, leaving terminal-default background.
/// `ForcedWidth(1)` keeps the diff advancing one cell per cell.
pub fn apply_to_cell(cell: &mut ratatui::buffer::Cell, uri: &str) {
    let prev = cell.symbol();
    let wrapped = format!("{OSC8_OPEN}{uri}{OSC8_CLOSE}{prev}{OSC8_EMPTY}");
    cell.set_symbol(&wrapped);
    cell.set_diff_option(ratatui::buffer::CellDiffOption::ForcedWidth(
        std::num::NonZeroU16::new(1).expect("nonzero"),
    ));
}

/// Builds a `file://` URI from a path string, resolving `~` and
/// cwd-relative paths to absolute. Returns `None` if the path cannot be
/// made absolute (e.g. empty, or a bare filename with no cwd context).
pub fn file_uri(path: &str) -> Option<String> {
    let resolved = resolve_path(path)?;
    let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    Some(uri_from_path(&canonical))
}

fn resolve_path(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = crate::paths::home()?;
        return Some(home.join(rest));
    }
    if trimmed == "~" {
        return crate::paths::home();
    }
    if trimmed.starts_with('/') || (cfg!(windows) && trimmed.get(1..3) == Some(":\\")) {
        return Some(PathBuf::from(trimmed));
    }
    let cwd = std::env::current_dir().ok()?;
    Some(cwd.join(trimmed))
}

/// Encodes an absolute path into a `file://` URI, percent-encoding
/// bytes that are not unreserved per RFC 3986. Non-existent paths pass
/// through uncanonicalized (a diff may show a not-yet-written file).
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

/// Whether the terminal multiplexer would corrupt OSC-8 passthrough.
/// tmux re-escapes sequences it does not recognize; keep the reference's
/// conservative gate and skip injection entirely under it.
pub fn is_muxed() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Removes OSC-8 hyperlink sequences (`ESC ]8;;…ST`) from a cell symbol,
/// leaving the visible text. Used when snapshotting the frame as plain
/// text so selection extents and clipboard copies stay clean.
pub fn strip_osc8(symbol: &str) -> String {
    let mut out = String::with_capacity(symbol.len());
    let mut rest = symbol;
    while let Some(i) = rest.find(OSC8_OPEN) {
        out.push_str(&rest[..i]);
        // Skip through the ST terminator that closes the URI params.
        match rest[i..].find(OSC8_CLOSE) {
            Some(end) => rest = &rest[i + end + OSC8_CLOSE.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Cell;

    #[test]
    fn file_uri_starts_with_scheme_for_absolute() {
        let uri = file_uri("/abs/file.rs").expect("uri");
        assert!(uri.starts_with("file:///abs/file.rs"), "{uri}");
    }

    #[test]
    fn file_uri_resolves_tilde() {
        let uri = file_uri("~/notes.txt").expect("uri");
        let home = crate::paths::home().expect("home");
        let expect = format!("file://{}", home.join("notes.txt").display());
        assert_eq!(uri, expect);
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
        assert_eq!(
            h,
            Hyperlink {
                row: 2,
                col_start: 4,
                col_end: 8,
                uri: "file:///a".into()
            }
        );
    }
}
