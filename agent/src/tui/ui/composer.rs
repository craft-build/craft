//! Composer (input box), status bar and footer hint row.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::app::App;
use super::theme;

/// Max text rows the composer grows to before it scrolls instead of expanding.
pub const MAX_TEXT_ROWS: usize = 12;
/// Inner left/right padding of the text area (left includes the accent bar).
pub const TEXT_LEFT_PAD: usize = 3; // 1 pad + ▎ bar + 1 space
pub const TEXT_RIGHT_PAD: usize = 2;

/// The `> ` prompt input. Multi-line: content is word-wrapped and the box
/// (sized by the caller from [`wrap_rows`]) grows as text wraps, so all typed
/// text stays on screen.
pub fn render_input(f: &mut Frame, app: &App, area: Rect) {
    // 1-cell app-bg padding on each side of the surface box.
    let inset = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    f.render_widget(Block::default().style(Style::default().bg(theme::BG_SURFACE)), inset);

    if inset.height < 3 || inset.width < 9 {
        return;
    }
    let bar_x = inset.x + 1;
    let text_x = inset.x + TEXT_LEFT_PAD as u16;
    let text_w = inset.width.saturating_sub((TEXT_LEFT_PAD + TEXT_RIGHT_PAD) as u16) as usize;
    let view_rows = (inset.height - 2) as usize; // 1 padding row top and bottom

    // Accent bar spanning the full box height, like posted user messages.
    for i in 0..inset.height {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "▎",
                Style::default().fg(theme::ACCENT).bg(theme::BG_SURFACE),
            ))),
            Rect { x: bar_x, y: inset.y + i, width: 1, height: 1 },
        );
    }

    let chars: Vec<char> = app.composer.chars().collect();
    let cursor = app.composer_cursor.min(chars.len());
    let rows = super::messages::wrap_rows(&app.composer, text_w);

    // (row, col) of the cursor within the wrapped layout.
    let mut cursor_row = rows.len() - 1;
    let mut cursor_col = 0;
    for (i, &(s, e)) in rows.iter().enumerate() {
        if cursor <= e {
            cursor_row = i;
            cursor_col = cursor - s;
            break;
        }
        cursor_col = e - s;
    }

    // Scroll so the cursor row stays visible when there are more rows than fit.
    let offset = cursor_row
        .saturating_sub(view_rows.saturating_sub(1))
        .min(rows.len().saturating_sub(view_rows));

    let text_style = Style::default().fg(theme::TEXT_PRIMARY).bg(theme::BG_SURFACE);
    for (i, &(s, e)) in rows.iter().enumerate().skip(offset).take(view_rows) {
        let y = inset.y + 1 + (i - offset) as u16;
        let line: String = chars[s..e].iter().collect();
        let w = line.chars().count();
        let mut spans = vec![Span::styled(line, text_style)];
        if w < text_w {
            spans.push(Span::styled(" ".repeat(text_w - w), Style::default().bg(theme::BG_SURFACE)));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect { x: text_x, y, width: text_w as u16, height: 1 },
        );
    }

    // Placeholder on the first visible row.
    if offset == 0 && app.composer.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Message Craft…",
                Style::default().fg(theme::TEXT_TERTIARY).bg(theme::BG_SURFACE),
            ))),
            Rect { x: text_x, y: inset.y + 1, width: text_w as u16, height: 1 },
        );
    }

    if app.palette.is_none() {
        f.set_cursor_position((
            text_x + cursor_col.min(text_w.saturating_sub(1)) as u16,
            inset.y + 1 + (cursor_row - offset) as u16,
        ));
    }
}

/// Single bottom row: `········ esc interrupt · model · provider · effort`
/// on the left, `44.8K (4%)  ctrl+p commands` right-aligned. The left group
/// is truncated with an ellipsis on narrow terminals.
pub fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let tertiary = Style::default().fg(theme::TEXT_TERTIARY);
    let (model, provider) = app.model();
    let sep = || Span::styled(" · ", tertiary);
    let left = vec![
        Span::styled("  ········  esc interrupt", tertiary),
        sep(),
        Span::styled(model, Style::default().fg(theme::BLUE_400)),
        sep(),
        Span::styled(provider, tertiary),
        sep(),
        Span::styled(app.effort(), Style::default().fg(theme::WARNING)),
    ];
    let right = format!("{}  ctrl+p commands  ", app.token_label);
    let width = area.width as usize;
    let max_left = width.saturating_sub(right.chars().count() + 1);
    let (left, left_w) = truncate_spans(left, max_left);
    let gap = width.saturating_sub(left_w + right.chars().count());
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.push(Span::styled(right, tertiary));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Clip a span list to `max` display cells, ending with an ellipsis if cut.
fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> (Vec<Span<'static>>, usize) {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let len = span.content.chars().count();
        if used + len <= max {
            used += len;
            out.push(span);
        } else {
            let remaining = max.saturating_sub(used + 1);
            let text: String = span.content.chars().take(remaining).collect();
            out.push(Span::styled(format!("{text}…"), span.style));
            used += remaining + 1;
            break;
        }
    }
    (out, used)
}


