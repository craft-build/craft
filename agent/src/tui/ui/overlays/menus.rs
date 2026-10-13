//! Composer-adjacent menus: the slash-command popup, the model menu, and
//! the command palette.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Clear, Paragraph};

use super::super::theme;
use super::{boxed, dim, render_rows};
use crate::tui::app::App;
use crate::tui::compat;
use crate::tui::modals::Modal;
use crate::tui::style::Style;
use crate::tui::text::{Line, Span};

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
        Block::default().style(compat::rat_style(Style::default().bg(t.bg_raised))),
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
        Paragraph::new(compat::rat_line(&Line::from(vec![
            Span::styled(" Enter select", Style::default().fg(t.cyan)),
            Span::styled("  ! strong", Style::default().fg(t.accent)),
            Span::styled("  @ medium", Style::default().fg(t.accent)),
            Span::styled("  # weak", Style::default().fg(t.accent)),
            Span::styled("  $ compaction", Style::default().fg(t.accent)),
        ]))),
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
        Paragraph::new(compat::rat_line(&Line::from(vec![
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
        ]))),
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
        Paragraph::new(compat::rat_line(&Line::from(Span::styled(
            "─".repeat(content.width as usize),
            Style::default().fg(t.border_subtle),
        )))),
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

// ─────────────────────────────────────────────────────────────────────────
// Wave 2a: the slt overlay sheets
// ─────────────────────────────────────────────────────────────────────────

use crate::tui::style::{Modifier, StyleExt};

use super::super::composer;
use super::{SheetRow, boxed_slt, clip_spans, row_spans, sheet_slt};

/// The chat-column and composer-block coordinates the legacy `draw()`
/// splits produce — the slash popup and model menu anchor against them.
/// Returns `(chat_x, chat_w, bottom_y)`.
fn chat_bottom(app: &App, term_w: u32, term_h: u32) -> (u32, u32, u32) {
    const SIDEBAR_MIN_WIDTH: u32 = 80; // legacy gate (frame.rs)
    const SIDEBAR_WIDTH: u32 = 34; // legacy `ui/mod.rs` SIDEBAR_WIDTH
    const CHAT_MIN_WIDTH: u32 = 60; // legacy `Constraint::Min(60)`
    let show_sidebar = app.session.sidebar_open && term_w >= SIDEBAR_MIN_WIDTH;
    let chat_w = if show_sidebar {
        (term_w - SIDEBAR_WIDTH).max(CHAT_MIN_WIDTH)
    } else {
        term_w
    };
    let text_w = (chat_w as usize)
        .saturating_sub(2 + composer::TEXT_LEFT_PAD + composer::TEXT_RIGHT_PAD)
        .max(1);
    let row_count = crate::tui::ui::messages::wrap_rows(&app.composer.text, text_w).len();
    let composer_h = row_count.min(composer::MAX_TEXT_ROWS) as u32 + 3;
    (0, chat_w, term_h.saturating_sub(composer_h + 3))
}

/// Slash-command popup above the composer (no dim — it floats over the
/// chat like the legacy `Clear` + raised block).
pub(super) fn render_slash_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let items = app.slash_matches();
    if items.is_empty() {
        return;
    }
    let (sw, sh) = (ui.width(), ui.height());
    let (_, chat_w, bottom_y) = chat_bottom(app, sw, sh);
    let n = items.len().min(6) as u32;
    let width = chat_w.saturating_sub(4).min(64);
    let (x, y) = (2, bottom_y.saturating_sub(n));
    // Scroll the window so the highlighted row stays visible past the edge.
    let window = n as usize;
    let start = app
        .overlays
        .slash_selected
        .saturating_sub(window.saturating_sub(1))
        .min(items.len().saturating_sub(window));
    let selected = app.overlays.slash_selected.saturating_sub(start);
    let rows: Vec<SheetRow> = items
        .iter()
        .skip(start)
        .take(window)
        .enumerate()
        .map(|(i, (cmd, desc))| {
            let spans = vec![
                Span::styled(format!(" {cmd}"), Style::default().fg(t.cyan)),
                Span::styled(format!("  {desc}"), Style::default().fg(t.text_tertiary)),
            ];
            SheetRow {
                dx: 0,
                dy: i as u32,
                spans: row_spans(spans, i == selected, width as usize),
            }
        })
        .collect();
    boxed_slt(ui, x, y, width, n, rows);
}

/// Model picker (no dim; windowed around the selection as legacy).
pub(super) fn render_model_menu_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let Modal::ModelMenu(selected) = app.overlays.modal else {
        return;
    };
    if app.session.models.is_empty() {
        return;
    }
    let (sw, sh) = (ui.width(), ui.height());
    let (_, chat_w, bottom_y) = chat_bottom(app, sw, sh);
    let available = bottom_y.saturating_sub(2) as usize;
    let visible = app.session.models.len().min(available.max(1));
    let start = selected.min(app.session.models.len().saturating_sub(visible));
    let n = visible as u32;
    let width = 56.min(chat_w.saturating_sub(4));
    let (x, y) = (2, bottom_y.saturating_sub(n + 3));
    let all_rows: Vec<Vec<Span<'static>>> = app
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
    let mut rows: Vec<SheetRow> = all_rows[start..start + visible]
        .iter()
        .enumerate()
        .map(|(i, spans)| SheetRow {
            dx: 0,
            dy: 1 + i as u32,
            spans: row_spans(spans.clone(), i == selected - start, width as usize),
        })
        .collect();
    // Keybind footer: Enter selects; !@#$ (or 1-4) toggle tier assignment.
    rows.push(SheetRow {
        dx: 0,
        dy: 1 + n,
        spans: vec![
            Span::styled(" Enter select", Style::default().fg(t.cyan)),
            Span::styled("  ! strong", Style::default().fg(t.accent)),
            Span::styled("  @ medium", Style::default().fg(t.accent)),
            Span::styled("  # weak", Style::default().fg(t.accent)),
            Span::styled("  $ compaction", Style::default().fg(t.accent)),
        ],
    });
    boxed_slt(ui, x, y, width, n + 3, rows);
}

/// ctrl+p command palette, centered near the top.
pub(super) fn render_palette_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let Modal::Palette { query, selected } = &app.overlays.modal else {
        return;
    };
    let (query, selected) = (query.clone(), *selected);
    let (sw, sh) = (ui.width(), ui.height());

    let items = app.palette_items();
    let n = items.len().min(8) as u32;
    let width = 60.min(sw.saturating_sub(4));
    let height = (n + 4).min(sh); // top pad + query row + divider + items + bottom pad
    let (x, y) = ((sw - width) / 2, sh / 6 + 2); // two rows below the center-top anchor

    // Content inset by one blank column on each side of the popup.
    let content_w = width.saturating_sub(2) as usize;

    // Scroll the 8-row window so the highlighted row stays visible once
    // the selection moves past the bottom edge.
    let window = n as usize;
    let start = selected
        .saturating_sub(window.saturating_sub(1))
        .min(items.len().saturating_sub(window));

    // Query row: `> ` prompt, the query (or placeholder), and the
    // spoofed caret one cell past its last char — the cell the legacy
    // hardware caret occupied.
    let qspans = clip_spans(
        vec![
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
            Span::styled(
                " ",
                Style::default()
                    .bg(t.bg_raised)
                    .add_modifier(Modifier::REVERSED),
            ),
        ],
        content_w,
    );

    let mut rows = vec![
        // query row (after the top padding row)
        SheetRow {
            dx: 1,
            dy: 1,
            spans: qspans,
        },
        // divider
        SheetRow {
            dx: 1,
            dy: 2,
            spans: vec![Span::styled(
                "─".repeat(content_w),
                Style::default().fg(t.border_subtle),
            )],
        },
    ];
    rows.extend(
        items
            .iter()
            .skip(start)
            .take(window)
            .enumerate()
            .map(|(i, (_, label, hint))| {
                let label = format!(" {label}");
                let hint = format!("{hint} ");
                let gap = content_w.saturating_sub(label.chars().count() + hint.chars().count());
                let spans = vec![
                    Span::styled(label, Style::default().fg(t.text_primary)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(hint, Style::default().fg(t.text_tertiary)),
                ];
                SheetRow {
                    dx: 1,
                    dy: 3 + i as u32,
                    spans: row_spans(spans, i == selected.saturating_sub(start), content_w),
                }
            }),
    );
    sheet_slt(ui, x, y, width, height, rows);
}
