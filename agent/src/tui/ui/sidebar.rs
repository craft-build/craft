//! Right sidebar: cwd·branch, Plan checklist, Files touched.

use super::theme;
use crate::tui::app::App;
use crate::tui::style::{Modifier, Style, StyleExt};
use crate::tui::text::{Line, Span};

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

fn divider(width: usize) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width),
        Style::default().fg(theme::current().border_subtle),
    ))
}

fn heading(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_uppercase(),
        Style::default()
            .fg(theme::current().text_primary)
            .add_modifier(Modifier::BOLD),
    ))
}

// ─────────────────────────────────────────────────────────────────────────
// The slt sidebar panel
// ─────────────────────────────────────────────────────────────────────────

/// Right-column width (legacy `SIDEBAR_WIDTH`, owned by `ui/mod.rs` on the
/// ratatui side; the slt frame declares its own copy too).
const SIDEBAR_WIDTH: u32 = 34;
/// The legacy chat column never shrinks below this (`Constraint::Min(60)`),
/// so at narrow terminal widths the sidebar is the one that squeezes.
const CHAT_MIN_WIDTH: u32 = 60;

/// Render the right sidebar column into the slt frame: cwd·branch, Plan
/// checklist, Files touched — same rows, styles and 2-cell side padding
/// as the ratatui `render`. The width mirrors the legacy column solver:
/// `min(SIDEBAR_WIDTH, terminal - CHAT_MIN_WIDTH)` (80-col terminal → 20).
pub(crate) fn render_slt(ui: &mut slt::Context, app: &App) {
    let t = theme::current();
    let width = SIDEBAR_WIDTH.min(ui.width().saturating_sub(CHAT_MIN_WIDTH));
    let w = (width as usize).saturating_sub(4); // 2-cell padding each side

    let _ = ui
        .container()
        .bg(t.bg_surface)
        .w(width)
        .px(2)
        .pt(1)
        .col(|ui| {
            let put = |ui: &mut slt::Context, line: Line<'static>| {
                ui.line(|ui| {
                    for span in line.spans {
                        ui.styled(span.content.as_ref(), span.style);
                    }
                });
            };
            let blank = |ui: &mut slt::Context| {
                ui.line(|ui| {
                    ui.styled(" ", Style::default().bg(t.bg_surface));
                });
            };

            // cwd · branch
            put(
                ui,
                Line::from(vec![Span::styled(
                    truncate(
                        if app.session.cwd.is_empty() {
                            "—"
                        } else {
                            &app.session.cwd
                        },
                        w,
                    ),
                    Style::default().fg(t.text_tertiary),
                )]),
            );
            put(
                ui,
                Line::from(vec![Span::styled(
                    truncate(
                        if app.session.branch.is_empty() {
                            "—"
                        } else {
                            &app.session.branch
                        },
                        w,
                    ),
                    Style::default().fg(t.text_tertiary),
                )]),
            );
            blank(ui);
            put(ui, divider(w));
            blank(ui);

            // Plan
            put(ui, heading("Plan"));
            blank(ui);
            for p in &app.plan {
                let (mark, mark_color, text_color) = if p.done {
                    ("[x]", t.text_tertiary, t.text_tertiary)
                } else if p.active {
                    ("[>]", t.accent, t.text_primary)
                } else {
                    ("[ ]", t.border_strong, t.text_secondary)
                };
                let label_style = Style::default().fg(text_color).add_modifier(if p.done {
                    Modifier::STRIKETHROUGH
                } else {
                    Modifier::NONE
                });
                put(
                    ui,
                    Line::from(vec![
                        Span::styled(format!("{mark} "), Style::default().fg(mark_color)),
                        Span::styled(truncate(&p.label, w.saturating_sub(4)), label_style),
                    ]),
                );
            }
            blank(ui);
            put(ui, divider(w));
            blank(ui);

            // Files touched
            put(ui, heading("Files touched"));
            blank(ui);
            for file in &app.files {
                let badge = format!("[{}]", file.status);
                let path_w = w.saturating_sub(badge.chars().count() + 1);
                let gap =
                    w.saturating_sub(path_w.min(file.path.chars().count()) + badge.chars().count());
                put(
                    ui,
                    Line::from(vec![
                        Span::styled(
                            truncate(&file.path, path_w),
                            Style::default().fg(t.text_secondary),
                        ),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(badge, Style::default().fg(t.tone_color(file.tone))),
                    ]),
                );
            }
        });
}
