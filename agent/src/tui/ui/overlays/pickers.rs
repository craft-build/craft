//! Centered list pickers: sessions, task chats, recipes, and themes.

use super::super::theme;
use crate::tui::app::App;
use crate::tui::modals::Modal;
use crate::tui::style::{Color, Style};
use crate::tui::text::Span;

/// One picker row: an active-chat marker, the name truncated with an
/// ellipsis, and a right-aligned status in `status_color`.
fn picker_row(
    name: &str,
    status: &str,
    is_active: bool,
    content_w: usize,
    status_color: Color,
) -> Vec<Span<'static>> {
    let t = theme::current();
    let marker = if is_active { "●" } else { " " };
    let status_text = if status.is_empty() {
        String::new()
    } else {
        format!("{status} ")
    };
    // Two columns for the marker and its trailing space.
    let name_w = content_w.saturating_sub(status_text.chars().count() + 2);
    let mut label: String = name.chars().take(name_w).collect();
    if name.chars().count() > name_w && name_w > 1 {
        // Byte-truncate at the last full char so the ellipsis fits.
        let bytes: usize = label.chars().map(char::len_utf8).take(name_w - 1).sum();
        label.truncate(bytes);
        label.push('…');
    }
    let gap = content_w.saturating_sub(2 + label.chars().count() + status_text.chars().count());
    vec![
        Span::styled(
            format!("{marker} "),
            Style::default().fg(if is_active { t.accent } else { t.bg_raised }),
        ),
        Span::styled(label, Style::default().fg(t.text_primary)),
        Span::raw(" ".repeat(gap)),
        Span::styled(status_text, Style::default().fg(status_color)),
    ]
}

// ─────────────────────────────────────────────────────────────────────────
// The slt picker sheets
// ─────────────────────────────────────────────────────────────────────────

use super::{SheetRow, centered_slt, row_spans, sheet_slt};

/// The bold sheet title row (one column in from the box edge).
pub(super) fn title_span(title: &str) -> Span<'static> {
    use crate::tui::style::{Modifier, StyleExt};

    Span::styled(
        title.to_string(),
        Style::default()
            .fg(theme::current().text_primary)
            .add_modifier(Modifier::BOLD),
    )
}

/// `/thinking` on the slt frame.
pub(super) fn render_thinking_picker_slt(ui: &mut slt::Context, app: &App) {
    let Modal::ThinkingPicker { selected } = app.overlays.modal else {
        return;
    };
    let (sw, sh) = (ui.width(), ui.height());
    let choices = app.thinking_choices();
    let (x, y, w, h) = centered_slt(62, choices.len() as u32 + 5, sw, sh);
    if h < 5 || w < 3 {
        return;
    }
    let content_w = w.saturating_sub(2) as usize;
    let visible = choices.len().min(h.saturating_sub(4) as usize);
    let start = selected.saturating_sub(visible.saturating_sub(1));
    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![title_span("Thinking — enter to apply, esc to cancel")],
    }];
    rows.extend(
        choices
            .iter()
            .skip(start)
            .take(visible)
            .enumerate()
            .map(|(i, choice)| {
                let marker = if *choice == app.session.thinking {
                    "●"
                } else {
                    " "
                };
                let spans = vec![Span::styled(
                    format!(" {marker} {choice}"),
                    Style::default().fg(theme::current().text_primary),
                )];
                SheetRow {
                    dx: 1,
                    dy: 2 + i as u32,
                    spans: row_spans(spans, i == selected.saturating_sub(start), content_w),
                }
            }),
    );
    rows.push(SheetRow {
        dx: 1,
        dy: 2 + visible as u32,
        spans: vec![Span::styled(
            "Custom token budget: /thinking <tokens>",
            Style::default().fg(theme::current().text_tertiary),
        )],
    });
    sheet_slt(ui, x, y, w, h, rows);
}

/// `/sessions` on the slt frame.
pub(super) fn render_sessions_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let Modal::Sessions { entries, selected } = &app.overlays.modal else {
        return;
    };
    let (entries, selected) = (entries, *selected);
    let (sw, sh) = (ui.width(), ui.height());
    let n = entries.len().clamp(1, 8) as u32;
    let width = 60.min(sw.saturating_sub(4));
    let (x, y, w, h) = centered_slt(width, n + 4, sw, sh);
    let content_w = w.saturating_sub(2) as usize;
    let spans: Vec<Vec<Span<'static>>> = if entries.is_empty() {
        vec![vec![Span::styled(
            " no sessions yet".to_string(),
            Style::default().fg(t.text_tertiary),
        )]]
    } else {
        entries
            .iter()
            .map(|entry| {
                let updated = format!("{} ", entry.updated);
                let title_w = content_w.saturating_sub(updated.chars().count() + 1);
                let mut title: String = entry.title.chars().take(title_w).collect();
                if entry.title.chars().count() > title_w && title_w > 1 {
                    title.truncate(title_w - 1);
                    title.push('…');
                }
                let title = format!(" {title}");
                let gap = content_w.saturating_sub(title.chars().count() + updated.chars().count());
                vec![
                    Span::styled(title, Style::default().fg(t.text_primary)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(updated, Style::default().fg(t.text_tertiary)),
                ]
            })
            .collect()
    };
    let selected = if entries.is_empty() {
        usize::MAX
    } else {
        selected
    };
    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![title_span("Sessions — enter to resume, esc to close")],
    }];
    rows.extend(spans.iter().enumerate().map(|(i, spans)| SheetRow {
        dx: 1,
        dy: 2 + i as u32,
        spans: row_spans(spans.clone(), i == selected, content_w),
    }));
    sheet_slt(ui, x, y, w, h, rows);
}

/// Ctrl-N task-chat picker on the slt frame.
pub(super) fn render_task_picker_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let Modal::TaskPicker { selected } = &app.overlays.modal else {
        return;
    };
    let selected = *selected;
    let active = app.active_task.map_or(0, |i| i + 1);
    let (sw, sh) = (ui.width(), ui.height());
    let n = app.task_chats.len().clamp(1, 10) as u32 + 1;
    let width = 60.min(sw.saturating_sub(4));
    let (x, y, w, h) = centered_slt(width, n + 4, sw, sh);
    let content_w = w.saturating_sub(2) as usize;
    let mut all_rows: Vec<Vec<Span<'static>>> = vec![picker_row(
        "Main chat",
        "",
        active == 0,
        content_w,
        t.text_tertiary,
    )];
    all_rows.extend(app.task_chats.iter().enumerate().map(|(i, chat)| {
        let (status, color) = match chat.status() {
            crate::tui::app::TaskStatus::Working => ("working", t.warning),
            crate::tui::app::TaskStatus::Done => ("done", t.success),
            crate::tui::app::TaskStatus::Error => ("error", t.danger),
        };
        picker_row(&chat.name, status, active == i + 1, content_w, color)
    }));
    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![title_span("Task chats — enter to open, esc to close")],
    }];
    rows.extend(all_rows.iter().enumerate().map(|(i, spans)| SheetRow {
        dx: 1,
        dy: 2 + i as u32,
        spans: row_spans(spans.clone(), i == selected, content_w),
    }));
    sheet_slt(ui, x, y, w, h, rows);
}

/// `/recipe` picker on the slt frame.
pub(super) fn render_recipes_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let Modal::Recipes { entries, selected } = &app.overlays.modal else {
        return;
    };
    let (entries, selected) = (entries, *selected);
    let (sw, sh) = (ui.width(), ui.height());
    let n = entries.len().clamp(1, 8) as u32;
    let width = 60.min(sw.saturating_sub(4));
    let (x, y, w, h) = centered_slt(width, n + 4, sw, sh);
    let content_w = w.saturating_sub(2) as usize;
    let spans: Vec<Vec<Span<'static>>> = if entries.is_empty() {
        vec![vec![Span::styled(
            " no recipes found (.craft/recipes/*.yaml)".to_string(),
            Style::default().fg(t.text_tertiary),
        )]]
    } else {
        entries
            .iter()
            .map(|entry| {
                let params = if entry.params.is_empty() {
                    String::new()
                } else {
                    format!(" {}", entry.params.join(", "))
                };
                let hint = format!("{params} ");
                let name_w = content_w.saturating_sub(hint.chars().count() + 1);
                let mut name: String = entry.name.chars().take(name_w).collect();
                if entry.name.chars().count() > name_w && name_w > 1 {
                    name.truncate(name_w - 1);
                    name.push('…');
                }
                let name = format!(" {name}");
                let gap = content_w.saturating_sub(name.chars().count() + hint.chars().count());
                vec![
                    Span::styled(name, Style::default().fg(t.text_primary)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(hint, Style::default().fg(t.text_tertiary)),
                ]
            })
            .collect()
    };
    let selected = if entries.is_empty() {
        usize::MAX
    } else {
        selected
    };
    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![title_span("Recipes — enter to run, esc to close")],
    }];
    rows.extend(spans.iter().enumerate().map(|(i, spans)| SheetRow {
        dx: 1,
        dy: 2 + i as u32,
        spans: row_spans(spans.clone(), i == selected, content_w),
    }));
    sheet_slt(ui, x, y, w, h, rows);
}

/// `/theme` picker on the slt frame.
pub(super) fn render_theme_picker_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();

    let Modal::ThemePicker {
        entries,
        selected,
        original,
    } = &app.overlays.modal
    else {
        return;
    };
    let (entries, selected, original) = (entries, *selected, original);
    let (sw, sh) = (ui.width(), ui.height());
    if entries.is_empty() {
        super::dim_slt(ui);
        return;
    }
    const MAX_VISIBLE: usize = 15;
    let visible = entries.len().clamp(1, MAX_VISIBLE);
    let start = selected.min(entries.len().saturating_sub(visible));
    let n = visible as u32;
    let width = 48.min(sw.saturating_sub(4));
    let (x, y, w, h) = centered_slt(width, n + 4, sw, sh);
    let content_w = w.saturating_sub(2) as usize;
    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![title_span("Themes — ↑↓ preview, enter apply, esc cancel")],
    }];
    rows.extend(
        entries[start..start + visible]
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let marker = if name == original { "● " } else { "  " };
                let spans = vec![
                    Span::styled(
                        marker.to_string(),
                        Style::default().fg(if name == original {
                            t.accent
                        } else {
                            t.bg_raised
                        }),
                    ),
                    Span::styled(name.clone(), Style::default().fg(t.text_primary)),
                ];
                SheetRow {
                    dx: 1,
                    dy: 2 + i as u32,
                    spans: row_spans(spans, i == selected - start, content_w),
                }
            }),
    );
    sheet_slt(ui, x, y, w, h, rows);
}
