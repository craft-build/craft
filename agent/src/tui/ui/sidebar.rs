//! Right sidebar: cwd·branch, Plan checklist, Files touched.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use crate::tui::app::App;

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
        Style::default().fg(theme::BORDER_SUBTLE),
    ))
}

fn heading(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_uppercase(),
        Style::default()
            .fg(theme::TEXT_PRIMARY)
            .add_modifier(Modifier::BOLD),
    ))
}

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().style(Style::default().bg(theme::BG_SURFACE));
    let inner = area;
    f.render_widget(block, area);

    let w = inner.width.saturating_sub(4) as usize; // 2-cell padding each side
    let x = inner.x + 2;
    let mut y = inner.y + 1;
    if inner.height < 4 || w == 0 {
        return;
    }

    let put = |f: &mut Frame, line: Line<'static>, y: u16| {
        f.render_widget(
            Paragraph::new(line),
            Rect {
                x,
                y,
                width: w as u16,
                height: 1,
            },
        );
    };

    // cwd · branch
    put(
        f,
        Line::from(vec![Span::styled(
            truncate(&app.cwd, w),
            Style::default().fg(theme::TEXT_TERTIARY),
        )]),
        y,
    );
    y += 1;
    put(
        f,
        Line::from(vec![Span::styled(
            truncate(&app.branch, w),
            Style::default().fg(theme::TEXT_TERTIARY),
        )]),
        y,
    );
    y += 2;
    if y >= inner.y + inner.height {
        return;
    }
    put(f, divider(w), y);
    y += 2;

    // Plan
    put(f, heading("Plan"), y);
    y += 2;
    for p in &app.plan {
        if y >= inner.y + inner.height {
            break;
        }
        let (mark, mark_color, text_color) = if p.done {
            ("[x]", theme::TEXT_TERTIARY, theme::TEXT_TERTIARY)
        } else if p.active {
            ("[>]", theme::ACCENT, theme::TEXT_PRIMARY)
        } else {
            ("[ ]", theme::BORDER_STRONG, theme::TEXT_SECONDARY)
        };
        let label_style = Style::default().fg(text_color).add_modifier(if p.done {
            Modifier::CROSSED_OUT
        } else {
            Modifier::empty()
        });
        put(
            f,
            Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(mark_color)),
                Span::styled(truncate(&p.label, w.saturating_sub(4)), label_style),
            ]),
            y,
        );
        y += 1;
    }
    y += 1;
    if y >= inner.y + inner.height {
        return;
    }
    put(f, divider(w), y);
    y += 2;

    // Files touched
    put(f, heading("Files touched"), y);
    y += 2;
    for file in &app.files {
        if y >= inner.y + inner.height {
            break;
        }
        let badge = format!("[{}]", file.status);
        let path_w = w.saturating_sub(badge.chars().count() + 1);
        let gap = w.saturating_sub(path_w.min(file.path.chars().count()) + badge.chars().count());
        put(
            f,
            Line::from(vec![
                Span::styled(
                    truncate(&file.path, path_w),
                    Style::default().fg(theme::TEXT_SECONDARY),
                ),
                Span::raw(" ".repeat(gap)),
                Span::styled(badge, Style::default().fg(theme::tone_color(file.tone))),
            ]),
            y,
        );
        y += 1;
    }
}
