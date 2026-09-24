//! Message list rendering: user / assistant / tool blocks, plus scrolling.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::markdown::highlight::{Highlighter, SegmentColor, StyledSegment};
use crate::tui::app::{App, DiffState, Message};
use crate::tui::hyperlink;
use crate::tui::provider::{LineKind, ToolKind};
use crate::tui::ui::scrollback::{Layout, ScrollPos, Segment};

const MARGIN: u16 = 2;
const BODY_INDENT: usize = 4;

/// Display width in cells (all glyphs we use are single-width).
fn cell_len(s: &str) -> usize {
    s.chars().count()
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
        let chars: Vec<char> = para.chars().collect();
        let plen = chars.len();
        let mut start = 0;
        while plen - start > width {
            let hard_end = start + width;
            // Prefer breaking after the last space inside the row.
            let space = (start + 1..=hard_end).rev().find(|&i| chars[i - 1] == ' ');
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
        while cell_len(&cur) > width {
            let head: String = cur.chars().take(width).collect();
            cur = cur.chars().skip(width).collect();
            out.push(head);
        }
        out.push(cur);
    }
    out
}

fn surface_style() -> Style {
    Style::default().bg(theme::BG_SURFACE)
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
                Span::styled("▎", Style::default().fg(theme::ACCENT)),
                Span::styled(
                    format!(" {chunk}"),
                    Style::default().fg(theme::TEXT_PRIMARY).patch(surf),
                ),
            ],
            width,
        ));
    }
    lines
}

fn assistant_block(text: &str, width: usize) -> Vec<Line<'static>> {
    let wrap_w = width.min(88);
    wrap_text(text, wrap_w)
        .into_iter()
        .map(|chunk| {
            Line::from(Span::styled(
                chunk,
                Style::default().fg(theme::TEXT_PRIMARY),
            ))
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
                Style::default().fg(theme::TEXT_SECONDARY),
            ))
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
    match diff {
        Some(DiffState::Pending) => Some(("[needs approval]", theme::WARNING)),
        Some(DiffState::Approved) => Some(("[approved]", theme::SUCCESS)),
        Some(DiffState::Rejected) => Some(("[rejected]", theme::DANGER)),
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
    // Hovered collapsible cards lift to a slightly lighter background.
    let card_bg = if hovered {
        theme::BG_OVERLAY
    } else {
        theme::BG_SURFACE
    };
    let surf = surface_style_with(card_bg);
    let marker = if focused { "▌" } else { " " };
    let marker_style = if focused {
        Style::default().fg(theme::ACCENT).bg(card_bg)
    } else {
        surf
    };

    // --- header row ---
    let mut header: Vec<Span<'static>> = vec![Span::styled(marker, marker_style)];
    match kind {
        ToolKind::Read { .. } | ToolKind::Grep { .. } => {
            header.push(Span::styled(
                if collapsed { "▸ " } else { "▾ " }.to_string(),
                Style::default().fg(theme::TEXT_TERTIARY).bg(card_bg),
            ));
        }
        ToolKind::Bash { .. } => {
            header.push(Span::styled(
                "$ ",
                Style::default().fg(theme::TEXT_TERTIARY).bg(card_bg),
            ));
        }
        ToolKind::Edit { .. } => {
            header.push(Span::styled(
                if collapsed { "▸ " } else { "▾ " }.to_string(),
                Style::default().fg(theme::TEXT_TERTIARY).bg(card_bg),
            ));
        }
    }
    let prefix_w = spans_width(&header);
    header.push(Span::styled(
        tool_label(kind),
        Style::default().fg(theme::TEXT_SECONDARY).bg(card_bg),
    ));
    // OSC-8 link target: the path text inside the header label. Columns
    // count from the row start (marker + caret + label prefix). The row
    // itself is filled in by the renderer once the segment is placed.
    let link = match kind {
        ToolKind::Read { path, .. } | ToolKind::Edit { path, .. } => {
            hyperlink::file_uri(path).map(|uri| {
                let prefix = prefix_w
                    + if matches!(kind, ToolKind::Read { .. }) {
                        cell_len("Read ")
                    } else {
                        cell_len("Edit ")
                    };
                hyperlink::Hyperlink::new(0, prefix as u16, (prefix + cell_len(path)) as u16, uri)
            })
        }
        ToolKind::Grep { .. } | ToolKind::Bash { .. } => None,
    };
    let blank = |lines: &mut Vec<Line<'static>>| lines.push(pad_row(Vec::new(), width, surf));

    // right-aligned badge / summary
    let badge = diff_badge(diff);
    let right = match (badge, kind) {
        (Some((text, color)), _) => Some((text.to_string(), color)),
        (None, k) if k.collapsible() => Some((tool_summary(k), theme::TEXT_TERTIARY)),
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
        let text = if nr == 0 {
            " ".repeat(w)
        } else {
            format!("{nr:>w$}")
        };
        Span::styled(
            format!("{text} "),
            Style::default().fg(theme::TEXT_TERTIARY).bg(bg),
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
        let (sign, fg, bg) = if is_add {
            ("+ ", theme::DIFF_ADD_TEXT, theme::DIFF_ADD_BG)
        } else {
            ("- ", theme::DIFF_DEL_TEXT, theme::DIFF_DEL_BG)
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
                        Style::default().fg(theme::TEXT_TERTIARY).bg(card_bg),
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
    let mut hl = if is_edit {
        match kind {
            ToolKind::Edit { path, .. } => Some(Highlighter::for_path(path)),
            _ => None,
        }
    } else {
        None
    };
    for ln in shown {
        let row = match ln.kind {
            LineKind::Gap if is_edit => pad_row(
                vec![
                    Span::styled(indent.clone(), surf),
                    Span::styled(
                        " ...".to_string(),
                        Style::default().fg(theme::TEXT_TERTIARY).bg(card_bg),
                    ),
                ],
                width,
                surf,
            ),
            LineKind::Add if is_edit => diff_row(ln, true, gutter_w, &mut hl, width, card_bg, surf),
            LineKind::Del if is_edit => {
                diff_row(ln, false, gutter_w, &mut hl, width, card_bg, surf)
            }
            LineKind::Context if is_edit => {
                let mut spans = vec![
                    Span::styled(indent.clone(), surf),
                    gutter_span(ln.nr, gutter_w, card_bg),
                    Span::styled("  ".to_string(), surf),
                    Span::styled(
                        ln.text.clone(),
                        Style::default().fg(theme::TEXT_SECONDARY).bg(card_bg),
                    ),
                ];
                let _ = &mut spans;
                pad_row(spans, width, surf)
            }
            LineKind::Add | LineKind::Del => {
                let (sign, fg, bg) = if ln.kind == LineKind::Add {
                    ("+ ", theme::DIFF_ADD_TEXT, theme::DIFF_ADD_BG)
                } else {
                    ("- ", theme::DIFF_DEL_TEXT, theme::DIFF_DEL_BG)
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
            kind => {
                let (prefix, fg) = match kind {
                    LineKind::Cmd => ("$ ", theme::TEXT_PRIMARY),
                    LineKind::Muted => ("", theme::TEXT_TERTIARY),
                    LineKind::Success => ("", theme::SUCCESS),
                    _ => ("", theme::TEXT_SECONDARY),
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
        let hint_fg = if focused {
            theme::SUCCESS
        } else {
            theme::TEXT_TERTIARY
        };
        let rej_fg = if focused {
            theme::DANGER
        } else {
            theme::TEXT_TERTIARY
        };
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
            vec![Span::styled("▎", Style::default().fg(theme::ACCENT))],
            width,
        )
    };

    if app.conversation.messages.is_empty() {
        let mut lines: Vec<Line<'static>> = vec![Line::from(Span::styled(
            "No messages yet.",
            Style::default().fg(theme::TEXT_TERTIARY),
        ))];
        lines.push(Line::default());
        #[cfg(test)]
        lines.push(Line::from(Span::styled(
            "Send a message — the mock provider will replay the scripted \"session refresh\" scenario.",
            Style::default().fg(theme::TEXT_DISABLED),
        )));
        #[cfg(not(test))]
        lines.push(Line::from(Span::styled(
            "Send a message to get started.",
            Style::default().fg(theme::TEXT_DISABLED),
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
            Message::Tool {
                id,
                kind,
                lines: body,
                diff,
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
    use super::{Line, tool_block};
    use crate::tui::provider::ToolLine;
    use ratatui::style::Modifier;

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
