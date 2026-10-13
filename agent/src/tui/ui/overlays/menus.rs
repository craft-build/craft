//! Composer-adjacent menus: the slash-command popup, the model menu, and
//! the command palette.

use super::super::theme;
use crate::tui::app::App;
use crate::tui::modals::Modal;
use crate::tui::style::Style;
use crate::tui::text::Span;

// ─────────────────────────────────────────────────────────────────────────
// The slt overlay sheets
// ─────────────────────────────────────────────────────────────────────────

use super::super::composer;
use super::{PANEL_INSET, SheetRow, boxed_slt, clip_spans, row_spans, sheet_slt};

/// Drop-up geometry: the content box's x (so the panel's bar lines up
/// with the composer's), the widest content the column allows, and the
/// composer box's top row — the drop-up's bottom padding row lands on the
/// box's own top padding row, so the two read as one attached panel.
/// Returns `(x, max_w, bottom_y)`.
fn drop_up(app: &App, term_w: u32, term_h: u32) -> (u32, u32, u32) {
    let x = composer::SIDE_MARGIN + PANEL_INSET;
    let max_w = (composer::box_width(term_w) as u32).saturating_sub(PANEL_INSET);
    (x, max_w, composer::box_top(app, term_w, term_h))
}

/// Slash-command drop-up above the composer (no dim — it floats over the
/// chat, attached to the input).
pub(super) fn render_slash_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let items = app.slash_matches();
    if items.is_empty() {
        return;
    }
    let (sw, sh) = (ui.width(), ui.height());
    let (x, max_w, bottom_y) = drop_up(app, sw, sh);
    let n = items.len().min(6) as u32;
    let width = max_w.min(64);
    let y = bottom_y.saturating_sub(n);
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
                Span::styled(format!(" {cmd}"), Style::default().fg(t.accent)),
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
    let (x, max_w, bottom_y) = drop_up(app, sw, sh);
    // Rows left above the menu for its chrome (title/hint rows + the
    // panel's top padding row).
    let available = bottom_y.saturating_sub(4) as usize;
    let visible = app.session.models.len().min(available.max(1));
    let start = selected.min(app.session.models.len().saturating_sub(visible));
    let n = visible as u32;
    let width = 56.min(max_w);
    // Rows, a blank, the key hints; the panel pads above and below.
    let height = n + 2;
    let y = bottom_y.saturating_sub(height);
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
                        t.bg_surface
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
            dy: i as u32,
            spans: row_spans(spans.clone(), i == selected - start, width as usize),
        })
        .collect();
    // Keybind footer: Enter selects; !@#$ (or 1-4) toggle tier assignment.
    // Keys in the accent, actions muted — the forms' hint-row style.
    let key = Style::default().fg(t.accent);
    let desc = Style::default().fg(t.text_tertiary);
    let mut hints = Vec::new();
    for (i, (k, d)) in [
        ("Enter", "select"),
        ("!", "strong"),
        ("@", "medium"),
        ("#", "weak"),
        ("$", "compaction"),
    ]
    .into_iter()
    .enumerate()
    {
        hints.push(Span::styled(if i == 0 { " " } else { "  " }, desc));
        hints.push(Span::styled(k, key));
        hints.push(Span::styled(format!(" {d}"), desc));
    }
    rows.push(SheetRow {
        dx: 0,
        dy: n + 1,
        spans: hints,
    });
    boxed_slt(ui, x, y, width, height, rows);
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

    // Query row: `> ` prompt, the query (or placeholder), and the caret.
    // Wave 2b: the `▎` glyph matches the composer/editor widget carets
    // (the app owns the query — filtering, list navigation — so this row
    // is painted, not a TextInputState; see docs/tui-slt-migration.md).
    let qspans = clip_spans(
        vec![
            Span::styled("> ", Style::default().fg(t.accent).bold()),
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
            Span::styled("▎", Style::default().fg(t.text_primary)),
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
