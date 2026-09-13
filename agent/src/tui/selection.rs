//! App-side text selection: geometry, frame-text extraction, and clipboard
//! copy. Terminal-native selection is disabled by mouse capture, so the app
//! highlights and copies text itself, like opencode's TUI.

use ratatui::layout::Rect;

/// Glyphs that are UI chrome rather than content: accent bars, focus markers,
/// box drawing. Never highlighted or copied as text.
pub(crate) const DECORATION_CHARS: [char; 8] = ['▎', '▌', '│', '─', '┌', '└', '┐', '┘'];

#[derive(Clone, Copy)]
pub struct Selection {
    pub anchor: (u16, u16), // (row, col) where the drag started
    pub head: (u16, u16),   // (row, col) of the current drag position
    /// The region the drag started in — the selection can never leave it, so
    /// a chat selection can't roll into the composer and vice versa.
    pub region: Rect,
}

impl Selection {
    /// Corners normalized so `top` precedes `bottom` in reading order.
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if (self.anchor.0, self.anchor.1) <= (self.head.0, self.head.1) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

pub(crate) fn rect_contains(r: Rect, row: u16, col: u16) -> bool {
    r.width > 0
        && r.height > 0
        && row >= r.y
        && row < r.y + r.height
        && col >= r.x
        && col < r.x + r.width
}

pub(crate) fn clamp_to(r: Rect, row: u16, col: u16) -> (u16, u16) {
    (
        row.clamp(r.y, r.y + r.height.saturating_sub(1)),
        col.clamp(r.x, r.x + r.width.saturating_sub(1)),
    )
}

/// First/last column (absolute, inclusive) of real text in a frame row,
/// within `region`. Whitespace padding and decoration glyphs are not text.
pub(crate) fn text_extent(row: &str, region: Rect) -> Option<(usize, usize)> {
    let x0 = region.x as usize;
    let x1 = (region.x + region.width) as usize;
    let mut first = None;
    let mut last = 0;
    for (i, ch) in row.chars().enumerate().take(x1).skip(x0) {
        if ch != ' ' && !DECORATION_CHARS.contains(&ch) {
            if first.is_none() {
                first = Some(i);
            }
            last = i;
        }
    }
    first.map(|f| (f, last))
}

/// Extract the selected text from a rendered frame. Rows are clamped to the
/// selection's region and to their real text bounds, decoration glyphs are
/// dropped, and rows without text are skipped — the clipboard gets clean
/// content with no padding, sidebar text, or accent bars.
pub(crate) fn extract_selection_text(frame_text: &[String], sel: Selection) -> String {
    let ((r1, c1), (r2, c2)) = sel.normalized();
    let region = sel.region;
    let mut parts: Vec<String> = Vec::new();
    for r in r1..=r2 {
        if r < region.y || r >= region.y + region.height {
            continue;
        }
        let Some(row) = frame_text.get(r as usize) else {
            continue;
        };
        let chars: Vec<char> = row.chars().collect();
        let Some((first, last)) = text_extent(row, region) else {
            continue;
        };
        let row_from = if r == r1 {
            c1 as usize
        } else {
            region.x as usize
        };
        let row_to = if r == r2 {
            c2 as usize
        } else {
            region.x as usize + region.width as usize - 1
        };
        let from = row_from.max(first);
        let to = row_to.min(last);
        if from > to || to >= chars.len() {
            continue;
        }
        let text: String = chars[from..=to]
            .iter()
            .filter(|ch| !DECORATION_CHARS.contains(ch))
            .collect::<String>()
            .trim()
            .to_string();
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join("\n")
}

/// Copy text to the system clipboard (macOS `pbcopy`).
pub(crate) fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    fn sel(anchor: (u16, u16), head: (u16, u16), region: Rect) -> Selection {
        Selection {
            anchor,
            head,
            region,
        }
    }

    #[test]
    fn extraction_strips_bars_padding_and_sidebar() {
        let region = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 4,
        };
        let ft = frame(&[
            "  ▎ > fix the flaky refresh     SIDEBAR-NOT-COPIED",
            "  Looking at the refresh path.  more sidebar",
            "  ▎                             even more sidebar",
            "                                trailing sidebar",
        ]);
        // Full-region select (rows 0..3, all region columns).
        let text = extract_selection_text(&ft, sel((0, 0), (3, 29), region));
        assert_eq!(
            text,
            "> fix the flaky refresh\nLooking at the refresh path."
        );
    }

    #[test]
    fn extraction_partial_row_binds_to_text() {
        let region = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 1,
        };
        let ft = frame(&["  Looking at the refresh path first.    "]);
        // Drag inside the text, right-to-left.
        let text = extract_selection_text(&ft, sel((0, 15), (0, 8), region));
        assert_eq!(text, "g at the");
    }

    #[test]
    fn extraction_skips_whitespace_only_rows() {
        let region = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 3,
        };
        let ft = frame(&[
            "first line          ",
            "                    ",
            "last line           ",
        ]);
        let text = extract_selection_text(&ft, sel((0, 0), (2, 19), region));
        assert_eq!(text, "first line\nlast line");
    }

    #[test]
    fn drag_positions_clamp_to_selection_region() {
        let region = Rect {
            x: 2,
            y: 1,
            width: 10,
            height: 5,
        };
        assert_eq!(clamp_to(region, 0, 0), (1, 2));
        assert_eq!(clamp_to(region, 99, 99), (5, 11));
        assert!(rect_contains(region, 3, 3));
        assert!(!rect_contains(region, 6, 3));
        assert!(!rect_contains(region, 3, 12));
    }
}
