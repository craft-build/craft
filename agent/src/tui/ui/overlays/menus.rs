//! Composer-adjacent menus: the slash-command popup, the model menu, and
//! the command palette.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::super::theme;
use super::{boxed, dim, render_rows};
use crate::tui::app::App;
use crate::tui::modals::Modal;

/// Slash-command popup above the composer. `chat` is the whole chat column and
/// `bottom` the 7-row composer/status/footer region it sits above.
pub fn render_slash(f: &mut Frame, app: &App, chat: Rect, bottom: Rect) {
    let t = theme::current();

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
        Block::default().style(Style::default().bg(t.bg_raised)),
        area,
    );
    // Scroll the window so the highlighted row stays visible past the edge.
    let window = n as usize;
    let start = app
        .overlays
        .slash_selected
        .saturating_sub(window.saturating_sub(1))
        .min(items.len().saturating_sub(window));
    let rows: Vec<Vec<Span<'static>>> = items
        .iter()
        .skip(start)
        .take(window)
        .map(|(cmd, desc)| {
            vec![
                Span::styled(format!(" {cmd}"), Style::default().fg(t.cyan)),
                Span::styled(format!("  {desc}"), Style::default().fg(t.text_tertiary)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        app.overlays.slash_selected.saturating_sub(start),
        area,
    );
}

/// Model picker, opened with ctrl+l / /model / palette.
pub fn render_model_menu(f: &mut Frame, app: &App, chat: Rect, bottom: Rect) {
    let t = theme::current();

    let Modal::ModelMenu(selected) = app.overlays.modal else {
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
        y: bottom.y.saturating_sub(n + 3),
        width: 56.min(chat.width.saturating_sub(4)),
        height: n + 3,
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
            // Held tiers from the global registry; overrides are tier-keyed
            // so a model may hold several ("strong/weak").
            let spec = format!("{}/{}", choice.provider, choice.model);
            let tiers = crate::model_registry::override_tiers(&spec);
            let tier_label = tiers
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join("/");
            let label_style = if tiers.is_empty() {
                Style::default().fg(t.text_primary)
            } else {
                // Highlight rows the user has explicitly assigned.
                Style::default().fg(t.accent)
            };
            vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(if i == app.session.model_idx {
                        t.accent
                    } else {
                        t.bg_raised
                    }),
                ),
                Span::styled(choice.label.clone(), label_style),
                Span::styled(
                    format!("  {}", choice.provider_label),
                    Style::default().fg(t.text_tertiary),
                ),
                Span::styled(format!("  {tier_label}"), Style::default().fg(t.accent)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows[start..start + visible],
        selected - start,
        Rect {
            y: inner.y + 1,
            height: inner.height.saturating_sub(2),
            ..inner
        },
    );
    // Keybind footer: Enter selects; !@#$ (or 1-4) toggle tier assignment.
    let footer = Rect {
        y: inner.y + 1 + visible as u16,
        height: 1,
        ..inner
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Enter select", Style::default().fg(t.cyan)),
            Span::styled("  ! strong", Style::default().fg(t.accent)),
            Span::styled("  @ medium", Style::default().fg(t.accent)),
            Span::styled("  # weak", Style::default().fg(t.accent)),
            Span::styled("  $ compaction", Style::default().fg(t.accent)),
        ])),
        footer,
    );
}

/// ctrl+p command palette, centered near the top.
pub fn render_palette(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::Palette { query, selected } = &app.overlays.modal else {
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
            Span::styled("> ", Style::default().fg(t.cyan)),
            Span::styled(
                if query.is_empty() {
                    "Type a command…".to_string()
                } else {
                    query.clone()
                },
                Style::default().fg(if query.is_empty() {
                    t.text_tertiary
                } else {
                    t.text_primary
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
            Style::default().fg(t.border_subtle),
        ))),
        Rect {
            x: content.x,
            y: div_y,
            width: content.width,
            height: 1,
        },
    );

    // Scroll the 8-row window so the highlighted row stays visible once the
    // selection moves past the bottom edge.
    let window = n as usize;
    let start = selected
        .saturating_sub(window.saturating_sub(1))
        .min(items.len().saturating_sub(window));
    let rows: Vec<Vec<Span<'static>>> = items
        .iter()
        .skip(start)
        .take(window)
        .map(|(_, label, hint)| {
            let w = content.width as usize;
            let label = format!(" {label}");
            let hint = format!("{hint} ");
            let gap = w.saturating_sub(label.chars().count() + hint.chars().count());
            vec![
                Span::styled(label, Style::default().fg(t.text_primary)),
                Span::raw(" ".repeat(gap)),
                Span::styled(hint, Style::default().fg(t.text_tertiary)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        selected.saturating_sub(start),
        Rect {
            x: content.x,
            y: div_y + 1,
            width: content.width,
            height: n,
        },
    );
}
