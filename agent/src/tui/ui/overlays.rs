//! Overlays: slash menu, model menu, command palette, confirm dialog.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::messages::wrap_text;
use super::theme;
use crate::tui::app::App;
use crate::tui::modals::Modal;

fn dim(f: &mut Frame, area: Rect) {
    // Clear first: a bg-only Block would leave the underlying characters visible.
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(theme::BG_SUNKEN)),
        area,
    );
}

/// Borderless raised-surface block shared by all overlays.
fn boxed(_area: Rect) -> Block<'static> {
    Block::default().style(Style::default().bg(theme::BG_RAISED))
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn render_rows(f: &mut Frame, rows: &[Vec<Span<'static>>], selected: usize, area: Rect) {
    for (i, spans) in rows.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let bg = if i == selected {
            theme::BG_OVERLAY
        } else {
            theme::BG_RAISED
        };
        let mut spans = spans.clone();
        let w: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        if w < area.width as usize {
            spans.push(Span::styled(
                " ".repeat(area.width as usize - w),
                Style::default().bg(bg),
            ));
        }
        let style = Style::default().bg(bg);
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(style),
            Rect {
                x: area.x,
                y: area.y + i as u16,
                width: area.width,
                height: 1,
            },
        );
    }
}

/// Slash-command popup above the composer. `chat` is the whole chat column and
/// `bottom` the 7-row composer/status/footer region it sits above.
pub fn render_slash(f: &mut Frame, app: &App, chat: Rect, bottom: Rect) {
    let items = app.slash_matches();
    if items.is_empty() {
        return;
    }
    let n = items.len().min(6) as u16;
    let width = chat.width.saturating_sub(4).min(64);
    let area = Rect {
        x: chat.x + 2,
        y: bottom.y.saturating_sub(n),
        width,
        height: n,
    };
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(theme::BG_RAISED)),
        area,
    );
    let rows: Vec<Vec<Span<'static>>> = items
        .iter()
        .take(6)
        .map(|(cmd, desc)| {
            vec![
                Span::styled(format!(" {cmd}"), Style::default().fg(theme::CYAN)),
                Span::styled(
                    format!("  {desc}"),
                    Style::default().fg(theme::TEXT_TERTIARY),
                ),
            ]
        })
        .collect();
    render_rows(f, &rows, app.slash_selected.min(items.len() - 1), area);
}

/// Model picker, opened with ctrl+l / /model / palette.
pub fn render_model_menu(f: &mut Frame, app: &App, chat: Rect, bottom: Rect) {
    let Modal::ModelMenu(selected) = app.modal else {
        return;
    };
    if app.session.models.is_empty() {
        return;
    }
    // Large real catalogs would otherwise paint over the chat and composer:
    // cap the menu to the rows above the composer and window it around the
    // selected row.
    let available = bottom.y.saturating_sub(chat.y + 2) as usize;
    let visible = app.session.models.len().min(available.max(1));
    let start = selected.min(app.session.models.len().saturating_sub(visible));
    let n = visible as u16;
    let area = Rect {
        x: chat.x + 2,
        y: bottom.y.saturating_sub(n + 2),
        width: 44.min(chat.width.saturating_sub(4)),
        height: n + 2,
    };
    f.render_widget(Clear, area);
    let block = boxed(area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows: Vec<Vec<Span<'static>>> = app
        .session
        .models
        .iter()
        .enumerate()
        .map(|(i, choice)| {
            let marker = if i == app.session.model_idx {
                "● "
            } else {
                "  "
            };
            vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(if i == app.session.model_idx {
                        theme::ACCENT
                    } else {
                        theme::BG_RAISED
                    }),
                ),
                Span::styled(
                    choice.label.clone(),
                    Style::default().fg(theme::TEXT_PRIMARY),
                ),
                Span::styled(
                    format!("  {}", choice.provider_label),
                    Style::default().fg(theme::TEXT_TERTIARY),
                ),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows[start..start + visible],
        selected - start,
        Rect {
            y: inner.y + 1,
            ..inner
        },
    );
}

/// ctrl+p command palette, centered near the top.
pub fn render_palette(f: &mut Frame, app: &App, area: Rect) {
    let Modal::Palette { query, selected } = &app.modal else {
        return;
    };
    let (query, selected) = (query.clone(), *selected);
    dim(f, area);

    let items = app.palette_items();
    let n = items.len().min(8) as u16;
    let width = 60.min(area.width.saturating_sub(4));
    let height = n + 4; // top pad + query row + divider + items + bottom pad
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + area.height / 6 + 2, // placed two rows below center-top anchor
        width,
        height: height.min(area.height),
    };
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // query row (after the top padding row)
    let qrow = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme::CYAN)),
            Span::styled(
                if query.is_empty() {
                    "Type a command…".to_string()
                } else {
                    query.clone()
                },
                Style::default().fg(if query.is_empty() {
                    theme::TEXT_TERTIARY
                } else {
                    theme::TEXT_PRIMARY
                }),
            ),
        ])),
        qrow,
    );
    f.set_cursor_position((qrow.x + 2 + query.chars().count() as u16, qrow.y));

    // Content inset by one blank column on each side of the popup.
    let content = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    };

    // divider
    let div_y = qrow.y + 1;
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(content.width as usize),
            Style::default().fg(theme::BORDER_SUBTLE),
        ))),
        Rect {
            x: content.x,
            y: div_y,
            width: content.width,
            height: 1,
        },
    );

    let rows: Vec<Vec<Span<'static>>> = items
        .iter()
        .take(8)
        .map(|(_, label, hint)| {
            let w = content.width as usize;
            let label = format!(" {label}");
            let hint = format!("{hint} ");
            let gap = w.saturating_sub(label.chars().count() + hint.chars().count());
            vec![
                Span::styled(label, Style::default().fg(theme::TEXT_PRIMARY)),
                Span::raw(" ".repeat(gap)),
                Span::styled(hint, Style::default().fg(theme::TEXT_TERTIARY)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        selected.min(items.len().saturating_sub(1)),
        Rect {
            x: content.x,
            y: div_y + 1,
            width: content.width,
            height: n,
        },
    );
}

/// "Reject this diff?" confirmation dialog.
pub fn render_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Modal::ConfirmReject(id) = &app.modal else {
        return;
    };
    let file = app
        .conversation
        .messages
        .iter()
        .find_map(|m| match m {
            crate::tui::app::Message::Tool {
                id: mid,
                kind: crate::tui::provider::ToolKind::Edit { path },
                ..
            } if mid == id => Some(path.clone()),
            _ => None,
        })
        .unwrap_or_default();

    dim(f, area);
    let mut rect = centered(48, 8, area);
    // Two rows below center.
    rect.y = (rect.y + 2).min(area.y + area.height.saturating_sub(rect.height));
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut y = inner.y + 1;
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Reject this diff?",
            Style::default()
                .fg(theme::TEXT_PRIMARY)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );
    y += 2;
    let body = format!("This discards Craft's proposed changes to {file}.");
    let w = inner.width.saturating_sub(2) as usize;
    for (i, chunk) in wrap_text(&body, w).into_iter().enumerate().take(2) {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                chunk,
                Style::default().fg(theme::TEXT_SECONDARY),
            ))),
            Rect {
                x: inner.x + 1,
                y: y + i as u16,
                width: w as u16,
                height: 1,
            },
        );
    }
    // actions, right-aligned on the last row
    let actions = Line::from(vec![
        Span::styled("[ esc cancel ]", Style::default().fg(theme::TEXT_SECONDARY)),
        Span::raw("  "),
        Span::styled("[ y reject ]", Style::default().fg(theme::DANGER)),
    ]);
    let aw = actions.width() as u16 + 1;
    f.render_widget(
        Paragraph::new(actions),
        Rect {
            x: inner.x + inner.width.saturating_sub(aw),
            y: inner.y + inner.height.saturating_sub(1),
            width: aw,
            height: 1,
        },
    );
}
