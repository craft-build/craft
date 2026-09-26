//! Message list rendering: user / assistant / tool blocks, plus scrolling.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::markdown::Emphasis;
use crate::markdown::highlight::{Highlighter, SegmentColor, StyledSegment};
use crate::markdown::render::{self, StyleToken};
use crate::tui::app::{App, DiffState, Message};
use crate::tui::hyperlink;
use crate::tui::provider::{LineKind, Tone, ToolKind};
use crate::tui::ui::scrollback::{Layout, ScrollPos, Segment};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MARGIN: u16 = 2;
const BODY_INDENT: usize = 4;

/// Display width in cells (CJK/emoji count as double-width). Used for
/// measurement only; slicing elsewhere stays char-index based.
fn cell_len(s: &str) -> usize {
    s.width()
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| cell_len(&s.content)).sum()
}

/// Pad a row with background-filled spaces out to `width` cells.
fn pad_row(mut spans: Vec<Span<'static>>, width: usize, bg: Style) -> Line<'static> {
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
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
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

fn surface_style() -> Style {
    Style::default().bg(theme::current().bg_surface)
}

fn surface_style_with(bg: ratatui::style::Color) -> Style {
    Style::default().bg(bg)
}

/// User-message row: `▎` on the app background, then the tool-card surface
/// background from the cell right of the bar to one cell before the edge.
fn user_line(mut spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let target = width.saturating_sub(1);
    let w = spans_width(&spans);
    if w < target {
        spans.push(Span::styled(" ".repeat(target - w), surface_style()));
    }
    Line::from(spans)
}

fn user_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let surf = surface_style();
    let mut lines = Vec::new();
    for chunk in wrap_text(text, width.saturating_sub(3)) {
        lines.push(user_line(
            vec![
                Span::styled("▎", Style::default().fg(theme::current().accent)),
                Span::styled(
                    format!(" {chunk}"),
                    Style::default()
                        .fg(theme::current().text_primary)
                        .patch(surf),
                ),
            ],
            width,
        ));
    }
    lines
}

/// Map the markdown engine's semantic tokens onto the TUI theme.
fn md_style(token: &StyleToken, emph: &Emphasis) -> Style {
    let t = theme::current();

    let mut style = match token {
        StyleToken::Text => Style::default().fg(t.text_primary),
        StyleToken::InlineCode => Style::default().fg(t.cyan),
        StyleToken::Highlight {
            fg,
            bold,
            italic,
            underline,
        } => {
            let mut s = Style::default().fg(seg_color(*fg).unwrap_or(t.text_primary));
            if *bold {
                s = s.bold();
            }
            if *italic {
                s = s.italic();
            }
            if *underline {
                s = s.underlined();
            }
            s
        }
        StyleToken::Heading => Style::default().fg(t.accent).bold(),
        StyleToken::CodeBar | StyleToken::ListMarker => Style::default().fg(t.text_tertiary),
        StyleToken::TableBorder | StyleToken::HorizontalRule => {
            Style::default().fg(t.text_tertiary)
        }
    };
    if emph.bold {
        style = style.bold();
    }
    if emph.italic {
        style = style.italic();
    }
    if emph.strike {
        style = style.crossed_out();
    }
    if emph.underline {
        style = style.underlined();
    }
    style
}

/// Agent text: parsed and rendered through the markdown engine (blocks,
/// inline styles, highlighted code fences) at the wrap width. Code-block
/// highlighting hits the global block cache, so re-rendering each frame
/// stays cheap.
fn assistant_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.min(88) as u16;
    render::render(text, wrap_w)
        .into_iter()
        .map(|line| {
            Line::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.text, md_style(&s.style, &s.emphasis)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn thinking_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.min(88);
    wrap_text(text, wrap_w)
        .into_iter()
        .map(|chunk| {
            Line::from(Span::styled(
                chunk,
                Style::default().fg(theme::current().text_secondary),
            ))
        })
        .collect()
}

/// A system notice: one muted line (wrapped when long) with a tone-colored
/// prefix glyph, without card chrome.
fn notice_block(tone: Tone, text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.saturating_sub(4).clamp(1, 88);
    wrap_text(text, wrap_w)
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            Line::from(vec![
                Span::styled(
                    if i == 0 { "◆ " } else { "  " },
                    Style::default().fg(theme::current().tone_color(tone)),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(chunk, Style::default().fg(theme::current().text_tertiary)),
            ])
        })
        .collect()
}

fn tool_label(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Read { path, .. } => format!("Read {path}"),
        ToolKind::Grep { pattern, .. } => format!("Grep \"{pattern}\""),
        ToolKind::Bash { cmd } => cmd.clone(),
        ToolKind::Edit { path, .. } => format!("Edit {path}"),
    }
}

fn tool_summary(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Read { summary, .. }
        | ToolKind::Grep { summary, .. }
        | ToolKind::Edit { summary, .. } => summary.clone(),
        _ => String::new(),
    }
}

fn diff_badge(diff: Option<DiffState>) -> Option<(&'static str, ratatui::style::Color)> {
    let t = theme::current();

    match diff {
        Some(DiffState::Pending) => Some(("[needs approval]", t.warning)),
        Some(DiffState::Approved) => Some(("[approved]", t.success)),
        Some(DiffState::Rejected) => Some(("[rejected]", t.danger)),
        None => None,
    }
}

fn seg_color(c: SegmentColor) -> Option<ratatui::style::Color> {
    match c {
        SegmentColor::Rgb(r, g, b) => Some(ratatui::style::Color::Rgb(r, g, b)),
        SegmentColor::Ansi(i) => Some(ratatui::style::Color::Indexed(i)),
        SegmentColor::Default => None,
    }
}

/// Split diff-line content into spans: syntax-highlight colors (when
/// available) are patched under the diff base style, and char ranges in
/// `emph` get the emphasized style (bold) on top.
fn styled_diff_spans(
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

/// Bodies hiding fewer lines than this render in full; a notice for a
/// couple of hidden lines buys nothing.
const MIN_HIDDEN_LINES: usize = 5;

#[allow(clippy::too_many_arguments)]
fn tool_block(
    kind: &ToolKind,
    _id: &str,
    body: &[crate::tui::provider::ToolLine],
    diff: Option<DiffState>,
    focused: bool,
    collapsed: bool,
    hovered: bool,
    body_expanded: bool,
    width: usize,
) -> (
    Vec<Line<'static>>,
    Option<usize>,
    Option<hyperlink::Hyperlink>,
) {
    let t = theme::current();

    // Hovered collapsible cards lift to a slightly lighter background.
    let card_bg = if hovered { t.bg_overlay } else { t.bg_surface };
    let surf = surface_style_with(card_bg);
    let marker = if focused { "▌" } else { " " };
    let marker_style = if focused {
        Style::default().fg(t.accent).bg(card_bg)
    } else {
        surf
    };

    // --- header row ---
    let mut header: Vec<Span<'static>> = vec![Span::styled(marker, marker_style)];
    match kind {
        ToolKind::Read { .. } | ToolKind::Grep { .. } => {
            header.push(Span::styled(
                if collapsed { "▸ " } else { "▾ " }.to_string(),
                Style::default().fg(t.text_tertiary).bg(card_bg),
            ));
        }
        ToolKind::Bash { .. } => {
            header.push(Span::styled(
                "$ ",
                Style::default().fg(t.text_tertiary).bg(card_bg),
            ));
        }
        ToolKind::Edit { .. } => {
            header.push(Span::styled(
                if collapsed { "▸ " } else { "▾ " }.to_string(),
                Style::default().fg(t.text_tertiary).bg(card_bg),
            ));
        }
    }
    let prefix_w = spans_width(&header);
    header.push(Span::styled(
        tool_label(kind),
        Style::default().fg(t.text_secondary).bg(card_bg),
    ));
    // OSC-8 link target: the path text inside the header label. Columns
    // count from the row start (marker + caret + label prefix). The row
    // itself is filled in by the renderer once the segment is placed.
    // Edit labels are verb summaries (`deleted: a.txt`), so the path is
    // located by search rather than a fixed prefix.
    let link = match kind {
        ToolKind::Read { path, .. } | ToolKind::Edit { path, .. } => {
            let label = tool_label(kind);
            let path_off = if matches!(kind, ToolKind::Read { .. }) {
                Some(cell_len("Read "))
            } else {
                label
                    .find(path.as_str())
                    .map(|byte| label[..byte].chars().count())
            };
            hyperlink::file_uri(path).zip(path_off).map(|(uri, off)| {
                let start = prefix_w + off;
                hyperlink::Hyperlink::new(0, start as u16, (start + cell_len(path)) as u16, uri)
            })
        }
        ToolKind::Grep { .. } | ToolKind::Bash { .. } => None,
    };
    let blank = |lines: &mut Vec<Line<'static>>| lines.push(pad_row(Vec::new(), width, surf));

    // right-aligned badge / summary
    let badge = diff_badge(diff);
    let right = match (badge, kind) {
        (Some((text, color)), _) => Some((text.to_string(), color)),
        (None, k) if k.collapsible() => Some((tool_summary(k), t.text_tertiary)),
        _ => None,
    };
    if let Some((text, color)) = right {
        let used = spans_width(&header) + cell_len(&text) + 2;
        if used < width {
            header.push(Span::styled(" ".repeat(width - used), surf));
        }
        header.push(Span::styled(
            format!("{text} "),
            Style::default().fg(color).bg(card_bg),
        ));
    }

    let mut lines = Vec::new();
    blank(&mut lines); // top padding
    lines.push(pad_row(header, width, surf));

    let expanded = !kind.collapsible() || !collapsed;
    if !expanded {
        blank(&mut lines); // bottom padding
        return (lines, None, link);
    }

    fn gutter_span(nr: usize, w: usize, bg: ratatui::style::Color) -> Span<'static> {
        let t = theme::current();
        let text = if nr == 0 {
            " ".repeat(w)
        } else {
            format!("{nr:>w$}")
        };
        Span::styled(
            format!("{text} "),
            Style::default().fg(t.text_tertiary).bg(bg),
        )
    }

    /// One row of a diff body: line-number gutter, +/- sign, and the content
    /// split into word-level emphasis with syntax highlighting patched under
    /// the diff colors.
    #[allow(clippy::too_many_arguments)]
    fn diff_row(
        ln: &crate::tui::provider::ToolLine,
        is_add: bool,
        gutter_w: usize,
        hl: &mut Option<Highlighter>,
        width: usize,
        card_bg: ratatui::style::Color,
        surf: Style,
    ) -> Line<'static> {
        let t = theme::current();
        let (sign, fg, bg) = if is_add {
            ("+ ", t.diff_add_text, t.diff_add_bg)
        } else {
            ("- ", t.diff_del_text, t.diff_del_bg)
        };
        let base = Style::default().fg(fg).bg(bg);
        let emph_style = base.bold();

        let segs = hl
            .as_mut()
            .map(|h| h.highlight_line(&ln.text))
            .filter(|s| !s.is_empty());
        let content = styled_diff_spans(&ln.text, segs.as_deref(), &ln.emph, base, emph_style);

        let mut spans = vec![Span::styled(" ".repeat(BODY_INDENT), surf)];
        spans.push(gutter_span(ln.nr, gutter_w, card_bg));
        spans.push(Span::styled(sign, base));
        spans.extend(content);
        let w = spans_width(&spans);
        if w < width {
            spans.push(Span::styled(" ".repeat(width - w), Style::default().bg(bg)));
        }
        Line::from(spans)
    }

    // --- body truncation (ported from the reference's tool display) ---
    let indent = " ".repeat(BODY_INDENT);
    let (cap, keep) = kind.body_hints();
    let hidden = body.len().saturating_sub(cap);
    let truncating = !body_expanded && hidden >= MIN_HIDDEN_LINES;
    let (shown, notice_before) = if truncating {
        match keep {
            crate::tui::provider::Keep::Head => (&body[..cap], false),
            crate::tui::provider::Keep::Tail => (&body[body.len() - cap..], true),
        }
    } else {
        (body, false)
    };
    let notice_text = if truncating {
        Some(format!("\u{2026} ({hidden} lines) click to expand"))
    } else if body_expanded && body.len() > cap {
        // The expanded body keeps a row to fold it back down.
        Some("\u{2026} click to collapse".to_string())
    } else {
        None
    };
    let mut notice_row: Option<usize> = None;
    let mut push_notice = |lines: &mut Vec<Line<'static>>| {
        if let Some(text) = &notice_text {
            notice_row = Some(lines.len());
            lines.push(pad_row(
                vec![
                    Span::styled(indent.clone(), surf),
                    Span::styled(
                        text.clone(),
                        Style::default().fg(t.text_tertiary).bg(card_bg),
                    ),
                ],
                width,
                surf,
            ));
        }
    };
    if notice_before {
        push_notice(&mut lines);
    }

    // --- body rows ---
    let is_edit = matches!(kind, ToolKind::Edit { .. });
    let gutter_w = if is_edit {
        body.iter()
            .map(|ln| ln.nr)
            .max()
            .unwrap_or(0)
            .max(1)
            .ilog10() as usize
            + 1
    } else {
        0
    };
    let mut hl = match kind {
        // Edit diffs and Read file bodies both carry source code; one
        // stateful highlighter walks the card's lines top to bottom.
        ToolKind::Edit { path, .. } | ToolKind::Read { path, .. } => {
            Some(Highlighter::for_path(path))
        }
        _ => None,
    };
    for ln in shown {
        let row = match ln.kind {
            LineKind::Gap if is_edit => pad_row(
                vec![
                    Span::styled(indent.clone(), surf),
                    Span::styled(
                        " ...".to_string(),
                        Style::default().fg(t.text_tertiary).bg(card_bg),
                    ),
                ],
                width,
                surf,
            ),
            LineKind::Add if is_edit => diff_row(ln, true, gutter_w, &mut hl, width, card_bg, surf),
            LineKind::Del if is_edit => {
                diff_row(ln, false, gutter_w, &mut hl, width, card_bg, surf)
            }
            LineKind::Add | LineKind::Del => {
                let (sign, fg, bg) = if ln.kind == LineKind::Add {
                    ("+ ", t.diff_add_text, t.diff_add_bg)
                } else {
                    ("- ", t.diff_del_text, t.diff_del_bg)
                };
                let text = format!("{sign}{}", ln.text);
                let mut spans = vec![
                    Span::styled(indent.clone(), surf),
                    Span::styled(text, Style::default().fg(fg).bg(bg)),
                ];
                let w = spans_width(&spans);
                if w < width {
                    spans.push(Span::styled(" ".repeat(width - w), Style::default().bg(bg)));
                }
                Line::from(spans)
            }
            LineKind::Context => {
                // Source-bearing Context rows (Read cards, edit context)
                // get syntax colors patched over the card surface.
                let base = Style::default().fg(t.text_secondary).bg(card_bg);
                let segs = hl
                    .as_mut()
                    .map(|h| h.highlight_line(&ln.text))
                    .filter(|s| !s.is_empty());
                let mut spans = vec![Span::styled(indent.clone(), surf)];
                if is_edit {
                    spans.push(gutter_span(ln.nr, gutter_w, card_bg));
                    spans.push(Span::styled("  ".to_string(), surf));
                }
                spans.extend(styled_diff_spans(
                    &ln.text,
                    segs.as_deref(),
                    &[],
                    base,
                    base,
                ));
                pad_row(spans, width, surf)
            }
            kind => {
                let (prefix, fg) = match kind {
                    LineKind::Cmd => ("$ ", t.text_primary),
                    LineKind::Muted => ("", t.text_tertiary),
                    LineKind::Success => ("", t.success),
                    _ => ("", t.text_secondary),
                };
                pad_row(
                    vec![
                        Span::styled(indent.clone(), surf),
                        Span::styled(
                            format!("{prefix}{}", ln.text),
                            Style::default().fg(fg).bg(card_bg),
                        ),
                    ],
                    width,
                    surf,
                )
            }
        };
        lines.push(row);
    }

    if !notice_before {
        push_notice(&mut lines);
    }

    // --- approve / reject actions for pending diffs ---
    if diff == Some(DiffState::Pending) {
        let hint_fg = if focused { t.success } else { t.text_tertiary };
        let rej_fg = if focused { t.danger } else { t.text_tertiary };
        lines.push(pad_row(
            vec![
                Span::styled(indent.clone(), surf),
                Span::styled("[ y approve ]", Style::default().fg(hint_fg).bg(card_bg)),
                Span::styled("   ", surf),
                Span::styled("[ Y always ]", Style::default().fg(hint_fg).bg(card_bg)),
                Span::styled("   ", surf),
                Span::styled("[ n reject ]", Style::default().fg(rej_fg).bg(card_bg)),
            ],
            width,
            surf,
        ));
    }

    blank(&mut lines); // bottom padding
    (lines, notice_row, link)
}

pub fn render(f: &mut Frame, app: &mut App, area: Rect) {
    let t = theme::current();

    let inner = Rect {
        x: area.x + MARGIN,
        y: area.y + 1,
        width: area.width.saturating_sub(MARGIN * 2),
        height: area.height.saturating_sub(1),
    };
    let width = inner.width as usize;
    if width == 0 {
        return;
    }

    // Build the frame's segment document: one segment per block, in
    // deterministic message order, so stored scroll positions survive the
    // refill and appended messages.
    app.view.segments.clear();

    // Blank spacer carrying the user-message accent bar, so the block's
    // left border reads as one continuous line.
    let bar_blank = |width: usize| {
        user_line(
            vec![Span::styled("▎", Style::default().fg(t.accent))],
            width,
        )
    };

    if app.conversation.messages.is_empty() {
        let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
            "No messages yet.",
            Style::default().fg(t.text_tertiary),
        ))];
        lines.push(Line::default());
        #[cfg(test)]
        lines.push(Line::from(Span::styled(
            "Send a message — the mock provider will replay the scripted \"session refresh\" scenario.",
            Style::default().fg(t.text_disabled),
        )));
        #[cfg(not(test))]
        lines.push(Line::from(Span::styled(
            "Send a message to get started.",
            Style::default().fg(t.text_disabled),
        )));
        app.view.segments.push(Segment::with_lines(lines));
    }

    let mut msg_seg_start = Vec::with_capacity(app.conversation.messages.len());
    let mut tool_seg_ranges: Vec<(usize, usize, usize)> = Vec::new(); // (msg idx, start, end)
    let mut notice_seg_rows: Vec<(usize, usize, usize)> = Vec::new(); // (msg idx, seg, row)
    // (segment index, link) for card headers carrying an OSC-8 target;
    // injected into the buffer after layout, once the row is known.
    let mut card_links: Vec<(usize, hyperlink::Hyperlink)> = Vec::new();
    for (idx, msg) in app.conversation.messages.iter().enumerate() {
        msg_seg_start.push(app.view.segments.len());
        let is_user = matches!(msg, Message::User(_));
        if is_user {
            app.view
                .segments
                .push(Segment::with_lines(vec![bar_blank(width)]));
        }
        match msg {
            Message::User(text) => app
                .view
                .segments
                .push(Segment::with_lines(user_block(text, width))),
            Message::Assistant(text) => app
                .view
                .segments
                .push(Segment::with_lines(assistant_block(text, width))),
            Message::Thinking(text) => app
                .view
                .segments
                .push(Segment::with_lines(thinking_block(text, width))),
            Message::Notice { tone, text } => app
                .view
                .segments
                .push(Segment::with_lines(notice_block(*tone, text, width))),
            Message::Tool {
                id,
                kind,
                lines: body,
                diff,
                review,
            } => {
                let collapsed = app.conversation.collapsed.iter().any(|c| c == id);
                let body_expanded = app.conversation.expanded_bodies.iter().any(|c| c == id);
                let focused = app.conversation.focused == Some(idx);
                let hovered = app.view.hover_tool == Some(idx);
                let (lines, notice_row, link) = tool_block(
                    kind,
                    id,
                    body,
                    *diff,
                    focused,
                    collapsed,
                    hovered,
                    body_expanded,
                    width,
                );
                let seg = app.view.segments.len();
                if let Some(row) = notice_row {
                    notice_seg_rows.push((idx, seg, row));
                }
                if let Some(hl) = link {
                    card_links.push((seg, hl));
                }
                if kind.collapsible() {
                    tool_seg_ranges.push((
                        idx,
                        app.view.segments.len(),
                        app.view.segments.len() + 1,
                    ));
                }
                app.view.segments.push(Segment::with_lines(lines));
                // Auto-review rides just below the card, outside its chrome,
                // so the card itself stays focused on the tool's output.
                if let Some(review) = review {
                    app.view.segments.push(Segment::with_lines(notice_block(
                        review.tone,
                        &review.text,
                        width,
                    )));
                }
            }
        }
        if is_user {
            app.view
                .segments
                .push(Segment::with_lines(vec![bar_blank(width)]));
        }
        app.view
            .segments
            .push(Segment::with_lines(vec![Line::default()]));
    }

    // Resolve the viewport through the scrollback engine: follow pins to
    // the bottom, anything else is clamped back inside its segment.
    let layout = Layout::new(&app.view.segments, inner.width);
    if app.view.follow {
        app.view.scroll = layout.bottom(inner.height);
    } else {
        app.view.scroll = layout.clamp(app.view.scroll);
    }
    let top_row = layout.doc_row(app.view.scroll);
    app.view.view_height = inner.height;
    app.view.view_width = inner.width;
    app.view.msg_starts = msg_seg_start
        .iter()
        .map(|&s| layout.doc_row(ScrollPos { seg: s, row: 0 }) as usize)
        .collect();

    // Slice the visible rows out of the segments starting at the scroll
    // position. Lines are pre-wrapped (one display row each), so slicing
    // rows is slicing lines.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(inner.height as usize);
    let mut pos = app.view.scroll;
    while lines.len() < inner.height as usize && pos.seg < app.view.segments.len() {
        let seg = app
            .view
            .segments
            .get(pos.seg)
            .expect("bounds checked above");
        let h = seg.height(inner.width);
        let take = (h.saturating_sub(pos.row)).min(inner.height - lines.len() as u16);
        lines.extend(
            seg.lines()[pos.row as usize..(pos.row + take) as usize]
                .iter()
                .cloned(),
        );
        pos.seg += 1;
        pos.row = 0;
    }

    // Visible screen rects of collapsible tool cards (for hover/click),
    // addressed in doc rows so they track the scroll position.
    app.view.tool_regions = tool_seg_ranges
        .iter()
        .filter_map(|&(idx, s_start, s_end)| {
            let vis_start = layout
                .doc_row(ScrollPos {
                    seg: s_start,
                    row: 0,
                })
                .saturating_sub(top_row) as u16;
            let vis_end = (layout
                .doc_row(ScrollPos { seg: s_end, row: 0 })
                .saturating_sub(top_row) as u16)
                .min(inner.height);
            if vis_end <= vis_start || vis_start >= inner.height {
                None
            } else {
                Some((
                    idx,
                    Rect {
                        x: inner.x,
                        y: inner.y + vis_start,
                        width: inner.width,
                        height: vis_end - vis_start,
                    },
                ))
            }
        })
        .collect();

    // Visible one-row rects of "click to expand" notice rows, addressed the
    // same way as card regions so they track the scroll position.
    app.view.notice_regions = notice_seg_rows
        .iter()
        .filter_map(|&(idx, seg, row)| {
            let vis = layout
                .doc_row(ScrollPos {
                    seg,
                    row: row as u16,
                })
                .saturating_sub(top_row) as u16;
            (vis < inner.height).then_some((
                idx,
                Rect {
                    x: inner.x,
                    y: inner.y + vis,
                    width: inner.width,
                    height: 1,
                },
            ))
        })
        .collect();

    let para = Paragraph::new(lines);
    f.render_widget(para, inner);

    // Rewrite the linked header cells in place: spans stayed plain text
    // during layout, so wrap math is unaffected. Skipped under tmux, whose
    // passthrough mangles OSC-8. The single-row guard mirrors the
    // reference: a header is one pre-wrapped row, and column bounds keep
    // any overflow from writing outside the viewport.
    if !card_links.is_empty() && !hyperlink::is_muxed() {
        for (seg, hl) in card_links {
            // The header sits one row under the card's top-padding blank.
            let doc = layout.doc_row(ScrollPos { seg, row: 1 });
            // Skip rows scrolled out above (plain subtraction, not
            // saturating) or pushed out below the viewport.
            if doc < top_row {
                continue;
            }
            let vis = (doc - top_row) as u16;
            if vis >= inner.height {
                continue;
            }
            if hl.col_start >= inner.width || hl.col_end > inner.width {
                continue;
            }
            for col in hl.col_start..hl.col_end {
                let cell = f
                    .buffer_mut()
                    .cell_mut((inner.x + col, inner.y + vis))
                    .expect("col bounded by inner.width, vis by inner.height");
                hyperlink::apply_to_cell(cell, &hl.uri);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::wrap_rows;
    use super::{Line, cell_len, pad_row, tool_block};
    use crate::tui::provider::ToolLine;
    use ratatui::style::Modifier;

    /// W1: a notice is one muted line with a tone-colored prefix glyph.
    #[test]
    fn notice_renders_tone_glyph_and_muted_text() {
        let t = theme::current();

        use super::{notice_block, theme};
        use crate::tui::app::{App, Message};
        use crate::tui::provider::{AgentEvent, Tone};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Unit shape: glyph + muted text, continuation lines keep the indent.
        let lines = notice_block(Tone::Warning, "retrying (attempt 1)", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "◆ ");
        assert_eq!(lines[0].spans[0].style.fg, Some(t.warning));
        assert_eq!(lines[0].spans[2].style.fg, Some(t.text_tertiary));

        // App level: AgentEvent::Notice lands as a Message::Notice and
        // renders into the transcript.
        let mut app = App::new();
        app.handle_event(AgentEvent::Notice {
            tone: Tone::Danger,
            text: "agent looks stuck in a loop".into(),
        });
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::Notice {
                tone: Tone::Danger,
                ..
            })
        ));
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let y = rows
            .iter()
            .position(|r| r.contains("stuck in a loop"))
            .expect("notice text rendered");
        assert!(rows[y].contains('◆'), "tone glyph rendered: {:?}", rows[y]);
        let gx = rows[y].find('◆').unwrap() as u16;
        assert_eq!(buf[(gx, y as u16)].fg, t.danger);
    }

    /// Auto-review renders as its own line *under* the tool card; the card
    /// keeps showing the tool's own header and output.
    #[test]
    fn auto_review_renders_under_the_card_not_inside_it() {
        use crate::tui::app::App;
        use crate::tui::provider::{AgentEvent, Tone, ToolCallData, ToolKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: Vec::new(),
            awaiting_approval: false,
        }));
        app.handle_event(AgentEvent::AutoReview {
            id: "t1".into(),
            tone: Tone::Success,
            text: "auto-review allow: low — in-project edit".into(),
        });

        let mut terminal = Terminal::new(TestBackend::new(70, 16)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let cmd_row = rows
            .iter()
            .position(|r| r.contains("cargo test"))
            .expect("tool header still shown");
        let review_row = rows
            .iter()
            .position(|r| r.contains("auto-review allow"))
            .expect("review line rendered");
        assert!(
            review_row > cmd_row,
            "review must sit under the card, not inside it: {rows:?}"
        );
    }

    #[test]
    fn assistant_markdown_read_highlight_and_painted_card_header() {
        let t = theme::current();

        use super::theme;
        use crate::tui::app::{App, Message};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;

        let mut app = App::new();
        app.conversation.messages.push(Message::Assistant(
            "# Title\n\nsome **bold** and `code`\n\n```rust\nfn main() {}\n```".into(),
        ));
        let path = std::env::temp_dir().join("probe.rs");
        app.conversation.messages.push(Message::Tool {
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: path.display().to_string(),
                summary: "2 lines".into(),
            },
            lines: vec![
                ToolLine::new(crate::tui::provider::LineKind::Context, "fn main() {"),
                ToolLine::new(crate::tui::provider::LineKind::Context, "}"),
            ],
            diff: None,
            review: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();

        // Agent text is markdown: heading rendered in accent bold.
        let hy = rows.iter().position(|r| r.contains("Title")).unwrap();
        let hx = rows[hy].find("Title").unwrap() as u16;
        let c = &buf[(hx, hy as u16)];
        assert_eq!(c.fg, t.accent, "heading fg");
        assert!(c.modifier.contains(Modifier::BOLD), "heading bold");

        // Read card body is syntax highlighted: some source cell carries a
        // color other than the plain secondary/tertiary card text.
        let cy = rows.iter().position(|r| r.contains("fn main() {")).unwrap();
        let hl_found = (0..buf.area.width).any(|x| {
            let c = &buf[(x, cy as u16)];
            matches!(c.symbol(), "f" | "m" | "(" | ")")
                && c.fg != Color::Reset
                && c.fg != t.text_secondary
                && c.fg != t.text_tertiary
        });
        assert!(
            hl_found,
            "read card code should be syntax colored; row={:?}",
            rows[cy]
        );

        // Card header: OSC-8 link present, full path text survives, and no
        // cell inside the message area falls back to terminal-default bg
        // (the pre-fix symptom: black sections after the linked cell).
        let ry = rows.iter().position(|r| r.contains("Read ")).unwrap();
        assert!(rows[ry].contains("\u{1b}]8;;file://"));
        assert!(rows[ry].contains("probe.rs"));
        for x in 2..78u16 {
            assert_ne!(
                buf[(x, ry as u16)].bg,
                Color::Reset,
                "unpainted cell at x={x}"
            );
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.clone()).collect()
    }

    #[test]
    fn edit_card_renders_gutter_signs_gap_and_emphasis() {
        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![
            ToolLine {
                kind: crate::tui::provider::LineKind::Del,
                text: "fn a() { one }".into(),
                nr: 2,
                emph: vec![(9, 12)],
            },
            ToolLine {
                kind: crate::tui::provider::LineKind::Add,
                text: "fn a() { two }".into(),
                nr: 0,
                emph: vec![(9, 12)],
            },
            ToolLine::new(crate::tui::provider::LineKind::Gap, "..."),
            ToolLine {
                kind: crate::tui::provider::LineKind::Context,
                text: "}".into(),
                nr: 4,
                ..Default::default()
            },
        ];
        let lines = tool_block(&kind, "t1", &body, None, false, false, false, false, 80).0;
        let joined: Vec<String> = lines.iter().map(line_text).collect();
        // Numbered gutter on removed/context lines, blank on added.
        assert!(
            joined.iter().any(|l| l.contains(" 2 - fn a() { one }")),
            "{joined:?}"
        );
        assert!(
            joined.iter().any(|l| l.contains("   + fn a() { two }")),
            "{joined:?}"
        );
        assert!(joined.iter().any(|l| l.contains(" ...")), "{joined:?}");
        assert!(joined.iter().any(|l| l.contains(" 4   }")), "{joined:?}");
        // Word-level emphasis renders bold.
        let del_line = lines
            .iter()
            .find(|l| line_text(l).contains("- fn a() { one }"))
            .unwrap();
        assert!(del_line.spans.iter().any(|s| {
            s.content.contains("one") && s.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    /// Unchanged (context) rows inside an edit diff are syntax highlighted like
    /// the added/removed rows, not left as plain secondary text.
    #[test]
    fn edit_card_highlights_context_lines() {
        use super::theme;

        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![
            ToolLine::new(crate::tui::provider::LineKind::Context, "let x = 1;"),
            ToolLine {
                kind: crate::tui::provider::LineKind::Add,
                text: "let y = 2;".into(),
                ..Default::default()
            },
        ];
        let lines = tool_block(&kind, "t1", &body, None, false, false, false, false, 80).0;
        let ctx = lines
            .iter()
            .find(|l| line_text(l).contains("let x = 1;"))
            .expect("context row rendered");
        // The `let` keyword carries a syntax color, not the plain secondary fg.
        assert!(
            ctx.spans.iter().any(|s| {
                s.content.contains("let")
                    && s.style.fg.is_some()
                    && s.style.fg != Some(theme::current().text_secondary)
            }),
            "context row is not syntax highlighted: {:?}",
            ctx.spans
        );
    }

    #[test]
    fn edit_card_collapses_to_header_only() {
        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![ToolLine::new(crate::tui::provider::LineKind::Add, "x")];
        let expanded = tool_block(&kind, "t1", &body, None, false, false, false, false, 80).0;
        let collapsed = tool_block(&kind, "t1", &body, None, false, true, false, false, 80).0;
        assert!(expanded.len() > collapsed.len());
        let header: String = collapsed[1]
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert!(header.contains("▸"), "collapsed header shows ▸: {header:?}");
        assert!(!header.contains("+ x"));
        let collapsed_text: String = collapsed.iter().map(line_text).collect();
        assert!(!collapsed_text.contains("+ x"));
    }

    #[test]
    fn long_head_kept_body_truncates_with_notice() {
        let kind = crate::tui::provider::ToolKind::Read {
            path: "big.txt".into(),
            summary: String::new(),
        };
        let body: Vec<ToolLine> = (0..60)
            .map(|i| ToolLine::new(crate::tui::provider::LineKind::Context, format!("line {i}")))
            .collect();
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        // 40 head lines kept; the notice names the hidden count and sits
        // just above the bottom padding row.
        assert_eq!(notice, Some(lines.len() - 2));
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("line 39")), "head kept");
        assert!(
            !text.iter().any(|l| l.contains("line 40")),
            "line past the cap hidden"
        );
        assert!(
            text.iter()
                .any(|l| l.contains("\u{2026} (20 lines) click to expand")),
            "{text:?}"
        );
        // Expanded body renders everything, with a fold-back notice.
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, true, 80);
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            text.iter().any(|l| l.contains("line 59")),
            "full body rendered"
        );
        assert!(text[notice.unwrap()].contains("click to collapse"));
    }

    #[test]
    fn bash_body_keeps_the_tail() {
        let kind = crate::tui::provider::ToolKind::Bash {
            cmd: "cargo test".into(),
        };
        let body: Vec<ToolLine> = (0..60)
            .map(|i| ToolLine::new(crate::tui::provider::LineKind::Context, format!("out {i}")))
            .collect();
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        assert!(notice.is_some(), "tail-kept bodies still carry a notice");
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert!(text.iter().any(|l| l.contains("out 59")), "tail kept");
        assert!(!text.iter().any(|l| l.contains("out 0")), "head hidden");
        // The notice sits above the kept tail.
        let notice_idx = notice.unwrap();
        assert!(text[notice_idx].contains("click to expand"));
        assert!(text[notice_idx + 1].contains("out 30"));
    }

    #[test]
    fn bodies_hiding_too_few_lines_render_in_full() {
        let kind = crate::tui::provider::ToolKind::Read {
            path: "small.txt".into(),
            summary: String::new(),
        };
        let body: Vec<ToolLine> = (0..43)
            .map(|i| ToolLine::new(crate::tui::provider::LineKind::Context, format!("l {i}")))
            .collect();
        let (lines, notice, _link) =
            tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        assert_eq!(notice, None, "3 hidden lines buy no notice");
        let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("l 42"), "full body rendered");
    }

    #[test]
    fn read_and_edit_cards_carry_header_link() {
        let kind = crate::tui::provider::ToolKind::Read {
            path: "src/a.rs".into(),
            summary: String::new(),
        };
        let body = vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")];
        let (_, _, link) = tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
        let hl = link.expect("read card carries a link");
        assert!(hl.uri.starts_with("file://"), "{}", hl.uri);
        // marker (1) + caret (2) + "Read " (5) precede the path text.
        assert_eq!(hl.col_start, 8);
        assert_eq!(hl.col_end, 8 + "src/a.rs".len() as u16);

        let kind = crate::tui::provider::ToolKind::Edit {
            path: "src/b.rs".into(),
            summary: String::new(),
        };
        let (_, _, link) = tool_block(&kind, "t1", &body, None, false, true, false, false, 80);
        assert!(link.is_some(), "collapsed edit card still links");
    }

    #[test]
    fn bash_and_grep_cards_carry_no_link() {
        for kind in [
            crate::tui::provider::ToolKind::Bash { cmd: "ls".into() },
            crate::tui::provider::ToolKind::Grep {
                pattern: "x".into(),
                summary: String::new(),
            },
        ] {
            let body = vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")];
            let (_, _, link) = tool_block(&kind, "t1", &body, None, false, false, false, false, 80);
            assert!(link.is_none());
        }
    }

    #[test]
    fn render_injects_osc8_into_visible_header_and_skips_scrolled_out() {
        use crate::tui::app::{App, Message};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new();
        let path = std::env::temp_dir().join("osc8_probe.rs");
        app.conversation.messages.push(Message::Tool {
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: path.display().to_string(),
                summary: String::new(),
            },
            lines: vec![ToolLine::new(
                crate::tui::provider::LineKind::Context,
                "body",
            )],
            diff: None,
            review: None,
        });
        // Fill the document past the viewport so a scroll below the card
        // survives the layout clamp instead of snapping back to the top.
        for i in 0..15 {
            app.conversation
                .messages
                .push(Message::Assistant(format!("filler {i}")));
        }

        app.view.follow = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row_text: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect();
        let header_row = row_text
            .iter()
            .find(|r| r.contains("Read "))
            .expect("header rendered");
        assert!(
            header_row.contains("\u{1b}]8;;"),
            "header row wrapped: {header_row:?}"
        );
        assert!(header_row.contains("file://"));
        assert!(header_row.contains("\u{1b}]8;;\u{1b}\\"));

        // Scroll the card fully above the viewport: no escapes anywhere.
        // A fresh terminal, so stale cells from the first draw can't leak
        // into the assertion (Paragraph only rewrites the rows it fills).
        app.view.follow = false;
        app.view.scroll = crate::tui::ui::scrollback::ScrollPos { seg: 1, row: 0 };
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| super::render(f, &mut app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let any_osc8 = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .any(|(x, y)| buf[(x, y)].symbol().contains("\u{1b}]8;;"));
        assert!(!any_osc8, "scrolled-out link must not be injected");

        // A link whose column range runs past the viewport is skipped, but
        // its visible text must survive untouched (spans render first;
        // injection only adds escapes to cells it wraps).
        let mut narrow_app = App::new();
        let long = std::env::temp_dir().join("osc8_probe.rs");
        narrow_app.conversation.messages.push(Message::Tool {
            id: "t1".into(),
            kind: crate::tui::provider::ToolKind::Read {
                path: format!("{}?padpadpadpadpad", long.display()),
                summary: String::new(),
            },
            lines: vec![ToolLine::new(crate::tui::provider::LineKind::Context, "x")],
            diff: None,
            review: None,
        });
        narrow_app.view.follow = false;
        let mut narrow = Terminal::new(TestBackend::new(24, 12)).unwrap();
        narrow
            .draw(|f| super::render(f, &mut narrow_app, f.area()))
            .unwrap();
        let buf = narrow.backend().buffer();
        let screen: String = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert!(
            screen.contains("Read ") && screen.contains("/var/folders"),
            "off-screen link's visible text dropped: {screen:?}"
        );
        assert!(!screen.contains("\u{1b}]8;;"));
    }

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
