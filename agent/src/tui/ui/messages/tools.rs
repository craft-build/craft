//! Tool metadata renderers: header labels, summaries, diff badges, and
//! diff-body span splitting with syntax colors.

use ratatui::style::Style;
use ratatui::text::Span;

use super::super::theme;
use crate::markdown::highlight::{SegmentColor, StyledSegment};
use crate::tui::app::DiffState;
use crate::tui::provider::ToolKind;

pub(super) fn tool_label(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Read { path, .. } => format!("Read {path}"),
        ToolKind::Grep { pattern, .. } => format!("Grep \"{pattern}\""),
        ToolKind::Bash { cmd } => cmd.clone(),
        ToolKind::Card { cmd, .. } => cmd.clone(),
        ToolKind::Edit { path, .. } => format!("Edit {path}"),
    }
}

pub(super) fn tool_summary(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Read { summary, .. }
        | ToolKind::Grep { summary, .. }
        | ToolKind::Edit { summary, .. }
        | ToolKind::Card { summary, .. } => summary.clone(),
        _ => String::new(),
    }
}

pub(super) fn diff_badge(diff: Option<DiffState>) -> Option<(&'static str, ratatui::style::Color)> {
    let t = theme::current();

    match diff {
        Some(DiffState::Pending) => Some(("[needs approval]", t.warning)),
        None => None,
    }
}

pub(super) fn seg_color(c: SegmentColor) -> Option<ratatui::style::Color> {
    match c {
        SegmentColor::Rgb(r, g, b) => Some(ratatui::style::Color::Rgb(r, g, b)),
        SegmentColor::Ansi(i) => Some(ratatui::style::Color::Indexed(i)),
        SegmentColor::Default => None,
    }
}

/// Split diff-line content into spans: syntax-highlight colors (when
/// available) are patched under the diff base style, and char ranges in
/// `emph` get the emphasized style (bold) on top.
pub(super) fn styled_diff_spans(
    text: &str,
    segs: Option<&[StyledSegment]>,
    emph: &[(usize, usize)],
    base: Style,
    emph_style: Style,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    fn plain(chars: &[char], start: usize, end: usize, style: Style, out: &mut Vec<Span<'static>>) {
        if end > start {
            out.push(Span::styled(
                chars[start..end].iter().collect::<String>(),
                style,
            ));
        }
    }
    let Some(segs) = segs else {
        let mut cursor = 0usize;
        for &(es, ee) in emph {
            plain(&chars, cursor, es, base, &mut out);
            plain(&chars, es, ee, emph_style, &mut out);
            cursor = cursor.max(ee);
        }
        plain(&chars, cursor, chars.len(), base, &mut out);
        return out;
    };
    let mut cursor = 0usize; // char offset consumed so far in `text`
    for seg in segs {
        let seg_len = seg.text.chars().count();
        let seg_start = cursor;
        let seg_end = seg_start + seg_len;
        let mut fg = seg_color(seg.fg).map_or(base, |c| base.fg(c));
        if seg.bold {
            fg = fg.bold();
        }
        if seg.italic {
            fg = fg.italic();
        }
        let mut piece_start = seg_start;
        for &(es, ee) in emph {
            let (s, e) = (es.max(seg_start), ee.min(seg_end));
            if e <= s {
                continue;
            }
            plain(&chars, piece_start, s, fg, &mut out);
            plain(&chars, s, e, emph_style, &mut out);
            piece_start = e;
        }
        plain(&chars, piece_start, seg_end, fg, &mut out);
        cursor = seg_end;
    }
    out
}
