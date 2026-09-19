//! Message list rendering: user / assistant / tool blocks, plus scrolling.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::theme;
use crate::tui::app::{App, DiffState, Message};
use crate::tui::provider::{LineKind, ToolKind};

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
        ToolKind::Edit { path } => format!("Edit {path}"),
    }
}

fn tool_summary(kind: &ToolKind) -> String {
    match kind {
        ToolKind::Read { summary, .. } | ToolKind::Grep { summary, .. } => summary.clone(),
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

#[allow(clippy::too_many_arguments)]
fn tool_block(
    kind: &ToolKind,
    _id: &str,
    body: &[crate::tui::provider::ToolLine],
    diff: Option<DiffState>,
    focused: bool,
    collapsed: bool,
    hovered: bool,
    width: usize,
) -> Vec<Line<'static>> {
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
            header.push(Span::styled("  ", surf));
        }
    }
    header.push(Span::styled(
        tool_label(kind),
        Style::default().fg(theme::TEXT_SECONDARY).bg(card_bg),
    ));
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
        return lines;
    }

    // --- body rows ---
    let indent = " ".repeat(BODY_INDENT);
    for ln in body {
        let row = match ln.kind {
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
    lines
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

    let mut lines: Vec<Line<'static>> = Vec::new();

    if app.conversation.messages.is_empty() {
        lines.push(Line::from(Span::styled(
            "No messages yet.",
            Style::default().fg(theme::TEXT_TERTIARY),
        )));
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
    }

    // Blank spacer carrying the user-message accent bar, so the block's
    // left border reads as one continuous line.
    let bar_blank = |width: usize| {
        user_line(
            vec![Span::styled("▎", Style::default().fg(theme::ACCENT))],
            width,
        )
    };

    let mut msg_starts = Vec::with_capacity(app.conversation.messages.len());
    let mut tool_ranges: Vec<(usize, usize, usize)> = Vec::new(); // (msg idx, start, end)
    for (idx, msg) in app.conversation.messages.iter().enumerate() {
        msg_starts.push(lines.len());
        let is_user = matches!(msg, Message::User(_));
        if is_user {
            lines.push(bar_blank(width));
        }
        match msg {
            Message::User(text) => lines.extend(user_block(text, width)),
            Message::Assistant(text) => lines.extend(assistant_block(text, width)),
            Message::Thinking(text) => lines.extend(thinking_block(text, width)),
            Message::Tool {
                id,
                kind,
                lines: body,
                diff,
            } => {
                let start = lines.len();
                let collapsed = app.conversation.collapsed.iter().any(|c| c == id);
                let focused = app.conversation.focused == Some(idx);
                let hovered = app.view.hover_tool == Some(idx);
                lines.extend(tool_block(
                    kind, id, body, *diff, focused, collapsed, hovered, width,
                ));
                if kind.collapsible() {
                    tool_ranges.push((idx, start, lines.len()));
                }
            }
        }
        if is_user {
            lines.push(bar_blank(width));
        }
        lines.push(Line::default());
    }
    app.view.msg_starts = msg_starts;

    let total = lines.len();
    let visible = inner.height as usize;
    let max = total.saturating_sub(visible).min(u16::MAX as usize) as u16;
    if app.view.follow {
        app.view.scroll = max;
    }
    app.view.scroll = app.view.scroll.min(max);
    app.view.max_scroll = max;
    app.view.view_height = inner.height;

    // Visible screen rects of collapsible tool cards (for hover/click).
    let scroll = app.view.scroll as i32;
    app.view.tool_regions = tool_ranges
        .iter()
        .filter_map(|(idx, start, end)| {
            let vis_start = (*start as i32 - scroll).max(0);
            let vis_end = (*end as i32 - scroll).min(inner.height as i32);
            if vis_end <= vis_start {
                None
            } else {
                Some((
                    *idx,
                    Rect {
                        x: inner.x,
                        y: inner.y + vis_start as u16,
                        width: inner.width,
                        height: (vis_end - vis_start) as u16,
                    },
                ))
            }
        })
        .collect();

    let para = Paragraph::new(lines).scroll((app.view.scroll, 0));
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::wrap_rows;

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
