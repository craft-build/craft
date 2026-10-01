//! Width measurement and the three line-wrap algorithms (offset rows,
//! plain text, styled spans) shared by every message block.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width in cells (CJK/emoji count as double-width). Used for
/// measurement only; slicing elsewhere stays char-index based.
pub(super) fn cell_len(s: &str) -> usize {
    s.width()
}

pub(super) fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| cell_len(&s.content)).sum()
}

/// Pad a row with background-filled spaces out to `width` cells.
pub(super) fn pad_row(mut spans: Vec<Span<'static>>, width: usize, bg: Style) -> Line<'static> {
    let w = spans_width(&spans);
    if w < width {
        spans.push(Span::styled(" ".repeat(width - w), bg));
    }
    Line::from(spans)
}

/// Word-wrap with exact char offsets: returns (start, end) char indices of
/// each display row (end exclusive, breaking spaces dropped). Used by the
/// multi-line composer for both rendering and cursor placement.
pub(crate) fn wrap_rows(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut offset = 0;
    for para in text.split('\n') {
        // (char, display width) pairs: breaks are chosen in cells but
        // reported as char indices for slicing.
        let chars: Vec<(char, usize)> = para.chars().map(|c| (c, c.width().unwrap_or(0))).collect();
        let mut cum = Vec::with_capacity(chars.len() + 1);
        cum.push(0usize);
        for &(_, w) in &chars {
            cum.push(cum.last().unwrap() + w);
        }
        let plen = chars.len();
        let total = cum[plen];
        let mut start = 0;
        while total - cum[start] > width {
            // Hard limit: the most chars that fit in `width` cells.
            let hard_end = (start + 1..=plen)
                .find(|&e| cum[e] - cum[start] > width)
                .map(|e| e - 1)
                .unwrap_or(plen);
            // Prefer breaking after the last space inside the row.
            let space = (start + 1..=hard_end)
                .rev()
                .find(|&i| chars[i - 1].0 == ' ');
            let (row_end, next) = match space {
                Some(i) => (i - 1, i),
                None => (hard_end, hard_end),
            };
            rows.push((offset + start, offset + row_end));
            start = next;
        }
        rows.push((offset + start, offset + plen));
        offset += plen + 1; // account for the '\n'
    }
    rows
}

/// Simple greedy word wrap with hard-splitting of overlong words.
pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut cur = String::new();
        for word in para.split(' ') {
            let cur_len = cell_len(&cur);
            let wlen = cell_len(word);
            if cur_len > 0 && cur_len + 1 + wlen > width {
                out.push(std::mem::take(&mut cur));
                cur.push_str(word);
            } else {
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(word);
            }
        }
        // Hard-split in cells: take the longest char prefix whose display
        // width fits `width`, so double-width glyphs never overflow a row.
        while cell_len(&cur) > width {
            let mut head = String::new();
            let mut head_w = 0;
            for c in cur.chars() {
                let cw = c.width().unwrap_or(0);
                if head_w + cw > width {
                    break;
                }
                head_w += cw;
                head.push(c);
            }
            let tail_start = cur.chars().count().saturating_sub(head.chars().count());
            cur = cur.chars().skip(tail_start).collect();
            out.push(head);
        }
        out.push(cur);
    }
    out
}

/// Greedy word wrap over styled spans (hard-splitting overlong words),
/// preserving styles across wrapped rows. Mirrors `wrap_rows` semantics:
/// breaks after the last space that fits, otherwise hard-splits in cells.
pub(super) fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    if width == 0 {
        return vec![spans];
    }
    let cells: Vec<(char, Style)> = spans
        .iter()
        .flat_map(|s| s.content.chars().map(|c| (c, s.style)))
        .collect();
    let mut rows = Vec::new();
    let mut start = 0;
    while start < cells.len() {
        let (mut w, mut end) = (0, start);
        while end < cells.len() {
            let cw = cells[end].0.width().unwrap_or(0);
            if w + cw > width {
                break;
            }
            w += cw;
            end += 1;
        }
        let (mut row_end, mut next) = (end, end);
        if end < cells.len()
            && let Some(i) = (start + 1..=end).rev().find(|&i| cells[i - 1].0 == ' ')
        {
            row_end = i - 1;
            next = i;
        }
        rows.push(run_length_spans(&cells[start..row_end]));
        start = next;
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }
    rows
}

/// Merge consecutive same-styled chars back into spans.
fn run_length_spans(cells: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    for (c, st) in cells {
        match out.last_mut() {
            Some(last) if last.style == *st => last.content.to_mut().push(*c),
            _ => out.push(Span::styled(c.to_string(), *st)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{cell_len, pad_row, wrap_rows};

    #[test]
    fn double_width_glyphs_pad_to_cell_width() {
        use ratatui::style::Style;
        use ratatui::text::Span;
        use unicode_width::UnicodeWidthStr;
        // CJK chars are 2 cells each, the emoji 2 as well: 6+2 = 8 cells
        // from 4 chars. Padding must count cells, not chars, so an ASCII
        // row and a CJK+emoji row padded to the same width line up —
        // including the diff `+/-` sign column, which sits after the
        // fixed-width gutter and before `spans` content.
        assert_eq!(cell_len("日本語🎉"), 8);
        assert_eq!(cell_len("abcdef"), 6);
        let bg = Style::default();
        let ascii = pad_row(vec![Span::raw("abcdef")], 10, bg);
        let wide = pad_row(vec![Span::raw("日本語🎉")], 10, bg);
        for line in [ascii, wide] {
            let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            assert_eq!(text.width(), 10, "row must fill exactly 10 cells");
        }
    }

    /// F: `wrap_spans` keeps span styles across wrapped rows and breaks at
    /// spaces when possible.
    #[test]
    fn wrap_spans_preserves_styles_across_rows() {
        use ratatui::style::{Color, Style};
        use ratatui::text::Span;
        let spans = vec![
            Span::styled("aaaa ".to_string(), Style::default().fg(Color::Red)),
            Span::styled("bbbb".to_string(), Style::default().fg(Color::Blue)),
        ];
        let rows = super::wrap_spans(spans, 6);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0].content, "aaaa");
        assert_eq!(rows[0][0].style.fg, Some(Color::Red));
        assert_eq!(rows[1][0].content, "bbbb");
        assert_eq!(rows[1][0].style.fg, Some(Color::Blue));
        // A word longer than the width hard-splits in cells.
        let rows = super::wrap_spans(
            vec![Span::styled("0123456789".to_string(), Style::default())],
            4,
        );
        let joined: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(joined, vec!["0123", "4567", "89"]);
    }

    fn render(text: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        wrap_rows(text, width)
            .into_iter()
            .map(|(s, e)| chars[s..e].iter().collect())
            .collect()
    }

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(
            render("aaa bbb ccc", 5),
            vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()]
        );
    }

    #[test]
    fn hard_splits_long_words() {
        assert_eq!(render("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn respects_explicit_newlines() {
        assert_eq!(
            render("one\ntwo\n\nthree", 10),
            vec!["one", "two", "", "three"]
        );
    }

    #[test]
    fn short_text_single_row() {
        assert_eq!(render("hi", 10), vec!["hi"]);
        assert_eq!(render("", 10), vec![""]);
    }
}
