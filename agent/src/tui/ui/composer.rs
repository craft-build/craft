//! Composer (input box), status bar and footer hint row.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use crate::tui::app::App;
use crate::tui::provider::Status;

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
    f.render_widget(
        Block::default().style(Style::default().bg(theme::BG_SURFACE)),
        inset,
    );

    if inset.height < 4 || inset.width < 9 {
        return;
    }
    let bar_x = inset.x + 1;
    let text_x = inset.x + TEXT_LEFT_PAD as u16;
    let text_w = inset
        .width
        .saturating_sub((TEXT_LEFT_PAD + TEXT_RIGHT_PAD) as u16) as usize;
    let view_rows = (inset.height - 3) as usize; // padding top/bottom + info row

    // Accent bar spanning the full box height, like posted user messages.
    for i in 0..inset.height {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "▎",
                Style::default().fg(theme::ACCENT).bg(theme::BG_SURFACE),
            ))),
            Rect {
                x: bar_x,
                y: inset.y + i,
                width: 1,
                height: 1,
            },
        );
    }

    let chars: Vec<char> = app.composer.text.chars().collect();
    let cursor = app.composer.cursor.min(chars.len());
    let rows = super::messages::wrap_rows(&app.composer.text, text_w);

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

    let text_style = Style::default()
        .fg(theme::TEXT_PRIMARY)
        .bg(theme::BG_SURFACE);
    for (i, &(s, e)) in rows.iter().enumerate().skip(offset).take(view_rows) {
        let y = inset.y + 1 + (i - offset) as u16;
        let line: String = chars[s..e].iter().collect();
        let w = line.chars().count();
        let mut spans = vec![Span::styled(line, text_style)];
        if w < text_w {
            spans.push(Span::styled(
                " ".repeat(text_w - w),
                Style::default().bg(theme::BG_SURFACE),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: text_x,
                y,
                width: text_w as u16,
                height: 1,
            },
        );
    }

    // Placeholder on the first visible row.
    if offset == 0 && app.composer.text.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Message Craft…",
                Style::default()
                    .fg(theme::TEXT_TERTIARY)
                    .bg(theme::BG_SURFACE),
            ))),
            Rect {
                x: text_x,
                y: inset.y + 1,
                width: text_w as u16,
                height: 1,
            },
        );
    }

    if !matches!(app.modal, crate::tui::modals::Modal::Palette { .. }) {
        f.set_cursor_position((
            text_x + cursor_col.min(text_w.saturating_sub(1)) as u16,
            inset.y + 1 + (cursor_row - offset) as u16,
        ));
    }

    // Fourth line of the input box: `model · provider · thinking` on the
    // left, context usage right-aligned. Starts after the accent bar so
    // the box's left border keeps running through this row.
    let info = Rect {
        x: text_x,
        y: inset.y + inset.height - 1,
        width: inset.width.saturating_sub(TEXT_LEFT_PAD as u16 + 1),
        height: 1,
    };
    let surf = Style::default().bg(theme::BG_SURFACE);
    let tertiary = Style::default()
        .fg(theme::TEXT_TERTIARY)
        .bg(theme::BG_SURFACE);
    let sep = || Span::styled(" · ", tertiary);
    let (model, provider) = app.model();
    let left = vec![
        Span::styled(
            model,
            Style::default().fg(theme::BLUE_400).bg(theme::BG_SURFACE),
        ),
        sep(),
        Span::styled(provider, tertiary),
        sep(),
        Span::styled(
            app.effort(),
            Style::default().fg(theme::WARNING).bg(theme::BG_SURFACE),
        ),
    ];
    let right = format!("{}  ", app.session.token_label);
    let width = info.width as usize;
    let right_w = right.chars().count();
    let (left, left_w) = truncate_spans(left, width.saturating_sub(right_w + 1));
    let gap = width.saturating_sub(left_w + right_w);
    let mut spans = left;
    spans.push(Span::styled(" ".repeat(gap), surf));
    spans.push(Span::styled(right, tertiary));
    f.render_widget(Paragraph::new(Line::from(spans)), info);
}

/// Spinner / status word with the running timer and interrupt hint.
fn status_indicator(app: &App) -> Vec<Span<'static>> {
    let tertiary = Style::default().fg(theme::TEXT_TERTIARY);
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    // Prompt-progress: how long the current turn has been running, so a
    // silent model reads as slow rather than stuck.
    let elapsed = app
        .turn_started
        .map(|at| at.elapsed().as_secs())
        .unwrap_or(0);
    match app.status {
        Status::Thinking | Status::Running | Status::WaitingApproval => {
            let frame = SPINNER[app.status_tick % SPINNER.len()];
            let word = match app.status {
                Status::Thinking => "thinking",
                Status::Running => "running",
                _ => "awaiting approval",
            };
            vec![
                Span::styled(format!("{frame} "), Style::default().fg(theme::ACCENT)),
                Span::styled(format!("{word} {elapsed}s  esc interrupt"), tertiary),
            ]
        }
        Status::Failed => vec![
            Span::styled("✖ ", Style::default().fg(theme::DANGER)),
            Span::styled("failed  esc interrupt", tertiary),
        ],
        Status::Done => vec![Span::styled("········  esc interrupt", tertiary)],
    }
}

/// Single bottom row: active status indicator (spinner, elapsed timer,
/// interrupt hint) and cwd on the left, a live flash toast or
/// `ctrl+p commands` right-aligned. The model/provider/thinking level and
/// context usage moved up into the composer's info line.
pub fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let tertiary = Style::default().fg(theme::TEXT_TERTIARY);
    let sep = || Span::styled(" · ", tertiary);
    // Two leading spaces line the indicator up under the composer's accent
    // bar, which sits two cells in from the row's left edge.
    let mut left = vec![Span::raw("  ")];
    left.extend(status_indicator(app));
    left.extend([
        sep(),
        Span::styled(abbreviated_cwd(&app.session.cwd), tertiary),
    ]);
    let right = if let Some(flash) = app.flash_text() {
        format!("  {flash}  ")
    } else {
        "ctrl+p commands  ".to_string()
    };
    let width = area.width as usize;
    let right_w = right.chars().count();
    let max_left = width.saturating_sub(right_w + 1);
    let (left, left_w) = truncate_spans(left, max_left);
    let gap = width.saturating_sub(left_w + right_w);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    let right_style = if app.flash_text().is_some() {
        Style::default().fg(theme::ACCENT)
    } else {
        tertiary
    };
    spans.push(Span::styled(right, right_style));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Abbreviate a cwd for the status row: keep the last two components, `…`
/// prefix when something was cut (reference `cwd_branch_label` shape).
fn abbreviated_cwd(cwd: &str) -> String {
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => "/".into(),
        1 => parts[0].into(),
        _ => format!("…/{}", parts[parts.len() - 1]),
    }
}

/// Clip a span list to `max` display cells, ending with an ellipsis if cut.
fn truncate_spans<'a>(spans: Vec<Span<'a>>, max: usize) -> (Vec<Span<'a>>, usize) {
    let mut out: Vec<Span<'a>> = Vec::new();
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
