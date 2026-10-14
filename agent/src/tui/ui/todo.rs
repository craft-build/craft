//! Todo widget: the collapsible panel above the composer that replaced the
//! right sidebar. Two sections share one bar-edged panel:
//!
//! ```text
//! ▎
//! ▎ ▾ todo 2/4                                                  ctrl+t
//! ▎   [✓] add raylib dependency
//! ▎   [•] rebuild in release mode
//! ▎   [ ] run craft test
//! ▎ ▸ files 3                                                   ctrl+b
//! ▎
//! ```
//!
//! The plan checklist (`todo_write`) folds with Ctrl-T (Build mode) and the
//! files-touched list with Ctrl-B; clicking a section header folds it too.
//! A folded todo header carries the in-progress item so the current step
//! stays visible. The panel is absent while both lists are empty.

use super::composer::SIDE_MARGIN;
use super::theme;
use crate::tui::app::App;
use crate::tui::style::{Modifier, Style, StyleExt};
use crate::tui::text::Span;

/// Most checklist rows shown at once; longer plans window around the
/// in-progress item and summarize the rest.
const MAX_TODO_ROWS: usize = 8;
/// Most files-touched rows shown at once.
const MAX_FILE_ROWS: usize = 6;

fn truncate(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width {
        s.to_string()
    } else if width > 1 {
        let head: String = s.chars().take(width - 1).collect();
        format!("{head}…")
    } else {
        String::new()
    }
}

/// One widget row: bar, content, padded to `width` on the panel surface.
/// The content is clipped to the row (minus the right pad).
fn row(
    content: Vec<Span<'static>>,
    right: Option<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let t = theme::current();
    let surf = Style::default().bg(t.bg_surface);
    let mut spans = vec![
        Span::styled("▎", Style::default().fg(t.border_strong).bg(t.bg_surface)),
        Span::styled(" ", surf),
    ];
    let right_w = right.as_ref().map_or(0, |r| r.content.chars().count() + 1);
    let mut room = width.saturating_sub(2 + right_w);
    for span in content {
        if room == 0 {
            break;
        }
        let text = truncate(&span.content, room);
        room -= text.chars().count();
        spans.push(Span::styled(text, span.style.bg(t.bg_surface)));
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let mut pad = width.saturating_sub(used);
    if let Some(r) = right
        && pad >= right_w
    {
        pad -= right_w;
        spans.push(Span::styled(" ".repeat(pad), surf));
        spans.push(Span::styled(
            r.content.into_owned(),
            r.style.bg(t.bg_surface),
        ));
        pad = 1;
    }
    spans.push(Span::styled(" ".repeat(pad), surf));
    spans
}

/// Rows of the plan section (header first). Empty when there is no plan.
fn todo_rows(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let t = theme::current();
    if app.plan.is_empty() {
        return Vec::new();
    }
    let done = app.plan.iter().filter(|p| p.done).count();
    let open = app.session.todo_open;
    let caret = Style::default().fg(t.text_tertiary);
    let mut header = vec![
        Span::styled(if open { "▾ " } else { "▸ " }, caret),
        Span::styled("todo", Style::default().fg(t.text_primary).bold()),
        Span::styled(
            format!(" {done}/{}", app.plan.len()),
            Style::default().fg(t.text_tertiary),
        ),
    ];
    if !open && let Some(active) = app.plan.iter().find(|p| p.active) {
        header.push(Span::styled(" · ", Style::default().fg(t.text_disabled)));
        header.push(Span::styled(
            active.label.trim_start().to_string(),
            Style::default().fg(t.text_secondary),
        ));
    }
    let hint = Span::styled("ctrl+t", Style::default().fg(t.text_disabled));
    let mut rows = vec![row(header, Some(hint), width)];
    if !open {
        return rows;
    }

    // Window the checklist so the in-progress item stays on screen.
    let total = app.plan.len();
    let shown = total.min(MAX_TODO_ROWS);
    let active = app.plan.iter().position(|p| p.active).unwrap_or(0);
    let start = active
        .saturating_sub(shown / 2)
        .min(total.saturating_sub(shown));
    let (above, below) = (start, total - start - shown);
    let more = |n: usize, dir: &str| {
        row(
            vec![Span::styled(
                format!("  … {n} {dir}"),
                Style::default().fg(t.text_tertiary),
            )],
            None,
            width,
        )
    };
    if above > 0 {
        rows.push(more(above, "above"));
    }
    for p in &app.plan[start..start + shown] {
        let (mark, mark_fg, text_style) = if p.done {
            (
                "[✓]",
                t.accent,
                Style::default()
                    .fg(t.text_disabled)
                    .add_modifier(Modifier::STRIKETHROUGH),
            )
        } else if p.active {
            ("[•]", t.accent, Style::default().fg(t.text_primary))
        } else {
            ("[ ]", t.text_tertiary, Style::default().fg(t.text_tertiary))
        };
        rows.push(row(
            vec![
                Span::raw("  "),
                Span::styled(mark, Style::default().fg(mark_fg)),
                Span::raw(" "),
                Span::styled(p.label.clone(), text_style),
            ],
            None,
            width,
        ));
    }
    if below > 0 {
        rows.push(more(below, "more"));
    }
    rows
}

/// Rows of the files-touched section (header first). Empty when no file
/// has been touched.
fn file_rows(app: &App, width: usize) -> Vec<Vec<Span<'static>>> {
    let t = theme::current();
    if app.files.is_empty() {
        return Vec::new();
    }
    let open = app.session.files_open;
    let header = vec![
        Span::styled(
            if open { "▾ " } else { "▸ " },
            Style::default().fg(t.text_tertiary),
        ),
        Span::styled("files", Style::default().fg(t.text_primary).bold()),
        Span::styled(
            format!(" {}", app.files.len()),
            Style::default().fg(t.text_tertiary),
        ),
    ];
    let hint = Span::styled("ctrl+b", Style::default().fg(t.text_disabled));
    let mut rows = vec![row(header, Some(hint), width)];
    if !open {
        return rows;
    }
    for file in app.files.iter().take(MAX_FILE_ROWS) {
        let badge = Span::styled(
            format!("[{}]", file.status),
            Style::default().fg(t.tone_color(file.tone)),
        );
        rows.push(row(
            vec![
                Span::raw("  "),
                Span::styled(file.path.clone(), Style::default().fg(t.text_secondary)),
            ],
            Some(badge),
            width,
        ));
    }
    let hidden = app.files.len().saturating_sub(MAX_FILE_ROWS);
    if hidden > 0 {
        rows.push(row(
            vec![Span::styled(
                format!("  … {hidden} more"),
                Style::default().fg(t.text_tertiary),
            )],
            None,
            width,
        ));
    }
    rows
}

/// Height the whole widget occupies in the chat column (0 when both
/// lists are empty): the two padding rows, the section rows, and the
/// trailing blank separator row. Pure in app state so the transcript
/// painter can budget for it synchronously (see `messages::render_slt`)
/// instead of trusting the one-frame-stale area handoff — a ctrl+b /
/// ctrl+t toggle must not let stale-height bands overflow into the
/// sections and composer below.
pub(crate) fn block_height(app: &App, width: usize) -> u32 {
    let todo = todo_rows(app, width).len();
    let files = file_rows(app, width).len();
    if todo == 0 && files == 0 {
        0
    } else {
        (todo + files + 3) as u32
    }
}

/// Render the widget above the composer (nothing while both lists are
/// empty), followed by a one-row gap. Section header rects are published
/// for click-to-fold, one frame stale like every other hit region.
pub(crate) fn render_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();
    let width = (ui.width().saturating_sub(SIDE_MARGIN * 2) as usize).max(1);
    let todo = todo_rows(app, width);
    let files = file_rows(app, width);

    let mut todo_rect: Option<slt::Rect> = None;
    let mut files_rect: Option<slt::Rect> = None;
    if !todo.is_empty() || !files.is_empty() {
        let paint_section = |ui: &mut slt::Context, rows: Vec<Vec<Span<'static>>>| {
            let mut header: Option<slt::Rect> = None;
            for (i, spans) in rows.into_iter().enumerate() {
                // Fixed-width wrappers: their rects predict reliably frame
                // over frame (the header's is the click target).
                let resp = ui
                    .container()
                    .ml(SIDE_MARGIN)
                    .w(width as u32)
                    .h(1)
                    .col(|ui| {
                        ui.line(|ui| {
                            for span in &spans {
                                ui.styled(span.content.as_ref(), span.style);
                            }
                        });
                    });
                if i == 0 {
                    header = Some(resp.rect);
                }
            }
            header
        };
        // Blank padding rows (bar included) open and close the panel.
        let pad = || vec![row(Vec::new(), None, width)];
        let _ = ui.container().grow(0).col(|ui| {
            paint_section(ui, pad());
            todo_rect = paint_section(ui, todo);
            files_rect = paint_section(ui, files);
            paint_section(ui, pad());
            ui.styled(" ", Style::default().bg(t.bg_app));
        });
    }

    let paint = app.view.paint.clone();
    let as_rect = |r: Option<slt::Rect>| {
        r.filter(|r| r.width > 0 && r.height > 0)
            .map(crate::tui::geom::Rect::from_slt)
            .unwrap_or_default()
    };
    let (todo_rect, files_rect) = (as_rect(todo_rect), as_rect(files_rect));
    let _ = ui.overlay(|ui| {
        ui.container().w(1).h(1).draw(move |_buf, _area| {
            if let Ok(mut p) = paint.lock() {
                p.todo_header = todo_rect;
                p.files_header = files_rect;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::{PlanItem, Tone, TouchedFile};

    fn text(rows: &[Vec<Span<'static>>]) -> Vec<String> {
        rows.iter()
            .map(|r| r.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn item(label: &str, done: bool, active: bool) -> PlanItem {
        PlanItem {
            label: label.into(),
            done,
            active,
        }
    }

    #[test]
    fn todo_section_lists_items_with_marks_and_count() {
        let mut app = App::new();
        app.plan = vec![
            item("add dep", true, false),
            item("rebuild", false, true),
            item("test", false, false),
        ];
        let rows = text(&todo_rows(&app, 60));
        assert!(rows[0].contains("▾ todo 1/3"), "{rows:?}");
        assert!(rows[0].trim_end().ends_with("ctrl+t"), "{rows:?}");
        assert!(rows[1].contains("[✓] add dep"));
        assert!(rows[2].contains("[•] rebuild"));
        assert!(rows[3].contains("[ ] test"));
        assert!(rows.iter().all(|r| r.chars().count() == 60), "{rows:?}");
    }

    #[test]
    fn folded_todo_is_one_row_naming_the_active_item() {
        let mut app = App::new();
        app.plan = vec![item("add dep", true, false), item("rebuild", false, true)];
        app.session.todo_open = false;
        let rows = text(&todo_rows(&app, 60));
        assert_eq!(rows.len(), 1);
        assert!(rows[0].contains("▸ todo 1/2 · rebuild"), "{rows:?}");
    }

    #[test]
    fn long_plans_window_around_the_active_item() {
        let mut app = App::new();
        app.plan = (0..20)
            .map(|i| item(&format!("step {i}"), i < 12, i == 12))
            .collect();
        let rows = text(&todo_rows(&app, 60));
        assert!(rows.iter().any(|r| r.contains("[•] step 12")), "{rows:?}");
        assert!(rows[1].contains("… 8 above"), "{rows:?}");
        assert!(rows.last().unwrap().contains("… 4 more"), "{rows:?}");
    }

    #[test]
    fn files_section_folds_to_its_count() {
        let mut app = App::new();
        app.files = vec![TouchedFile {
            path: "src/a.rs".into(),
            status: "M".into(),
            tone: Tone::Warning,
        }];
        assert_eq!(text(&file_rows(&app, 50)).len(), 1, "folded by default");
        app.session.files_open = true;
        let rows = text(&file_rows(&app, 50));
        assert!(rows[0].contains("▾ files 1"));
        assert!(
            rows[1].contains("src/a.rs") && rows[1].contains("[M]"),
            "{rows:?}"
        );
    }

    #[test]
    fn empty_lists_render_nothing() {
        let app = App::new();
        assert!(todo_rows(&app, 40).is_empty());
        assert!(file_rows(&app, 40).is_empty());
    }
}
