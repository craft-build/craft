//! Centered list pickers: sessions, task chats, recipes, and themes.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::theme;
use super::{boxed, centered, dim, render_rows};
use crate::tui::app::App;
use crate::tui::modals::Modal;

/// `/sessions`: persisted-session picker; Enter loads, Esc closes.
pub fn render_sessions(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::Sessions { entries, selected } = &app.overlays.modal else {
        return;
    };
    dim(f, area);
    let n = entries.len().clamp(1, 8) as u16;
    let width = 60.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Sessions — enter to resume, esc to close",
            Style::default()
                .fg(t.text_primary)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );
    let content_w = inner.width.saturating_sub(2) as usize;
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
    render_rows(
        f,
        &spans,
        if entries.is_empty() {
            usize::MAX
        } else {
            *selected
        },
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}

/// Ctrl-N task-chat picker (task 96): row 0 is the main chat, then one row
/// per task chat — name on the left, status color-coded on the right — with
/// the active chat marked. Enter mounts the selection, Esc closes.
pub fn render_task_picker(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::TaskPicker { selected } = &app.overlays.modal else {
        return;
    };
    let active = app.active_task.map_or(0, |i| i + 1);
    dim(f, area);
    let n = app.task_chats.len().clamp(1, 10) as u16 + 1;
    let width = 60.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Task chats — enter to open, esc to close",
            Style::default()
                .fg(t.text_primary)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );
    let content_w = inner.width.saturating_sub(2) as usize;
    let mut rows: Vec<Vec<Span<'static>>> = vec![picker_row(
        "Main chat",
        "",
        active == 0,
        content_w,
        t.text_tertiary,
    )];
    rows.extend(app.task_chats.iter().enumerate().map(|(i, chat)| {
        let (status, color) = match chat.status() {
            crate::tui::app::TaskStatus::Working => ("working", t.warning),
            crate::tui::app::TaskStatus::Done => ("done", t.success),
            crate::tui::app::TaskStatus::Error => ("error", t.danger),
        };
        picker_row(&chat.name, status, active == i + 1, content_w, color)
    }));
    render_rows(
        f,
        &rows,
        *selected,
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}

/// One picker row: an active-chat marker, the name truncated with an
/// ellipsis, and a right-aligned status in `status_color`.
fn picker_row(
    name: &str,
    status: &str,
    is_active: bool,
    content_w: usize,
    status_color: ratatui::style::Color,
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

/// `/recipe`: recipe picker (J.5); Enter runs the selection or prefills
/// `key=` parameter stubs, Esc closes.
pub fn render_recipes(f: &mut Frame, app: &App, area: Rect) {
    use crate::tui::modals::Modal;
    let t = theme::current();

    let Modal::Recipes { entries, selected } = &app.overlays.modal else {
        return;
    };
    dim(f, area);
    let n = entries.len().clamp(1, 8) as u16;
    let width = 60.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Recipes — enter to run, esc to close",
            Style::default()
                .fg(t.text_primary)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );
    let content_w = inner.width.saturating_sub(2) as usize;
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
    render_rows(
        f,
        &spans,
        if entries.is_empty() {
            usize::MAX
        } else {
            *selected
        },
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}

/// `/theme`: bundled-theme picker. Arrows preview live, Enter applies +
/// persists, Esc restores the pre-open theme.
pub fn render_theme_picker(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::ThemePicker {
        entries,
        selected,
        original,
    } = &app.overlays.modal
    else {
        return;
    };
    dim(f, area);
    if entries.is_empty() {
        return;
    }
    const MAX_VISIBLE: usize = 15;
    let visible = entries.len().clamp(1, MAX_VISIBLE);
    let start = (*selected).min(entries.len().saturating_sub(visible));
    let n = visible as u16;
    let width = 48.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Themes — ↑↓ preview, enter apply, esc cancel",
            Style::default()
                .fg(t.text_primary)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );
    let rows: Vec<Vec<Span<'static>>> = entries[start..start + visible]
        .iter()
        .map(|name| {
            let marker = if name == original { "● " } else { "  " };
            vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(if name == original {
                        t.accent
                    } else {
                        t.bg_raised
                    }),
                ),
                Span::styled(name.clone(), Style::default().fg(t.text_primary)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        *selected - start,
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}
