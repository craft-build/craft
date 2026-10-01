//! Overlays: slash menu, model menu, command palette, confirm dialog.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::theme;

mod help;
mod mcp;
mod menus;
mod pickers;
mod search_file;
mod usage;

pub(super) use help::render_help;
pub(super) use mcp::render_mcp;
pub(super) use menus::{render_model_menu, render_palette, render_slash};
pub(super) use pickers::{
    render_recipes, render_sessions, render_task_picker, render_theme_picker,
};
pub(super) use search_file::{render_file_picker, render_search};
pub(super) use usage::{render_stats, render_usage};

fn dim(f: &mut Frame, area: Rect) {
    // Clear first: a bg-only Block would leave the underlying characters visible.
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(theme::current().bg_sunken)),
        area,
    );
}

/// Borderless raised-surface block shared by all overlays.
fn boxed(_area: Rect) -> Block<'static> {
    Block::default().style(Style::default().bg(theme::current().bg_raised))
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
            theme::current().bg_overlay
        } else {
            theme::current().bg_raised
        };
        let mut spans = spans.clone();
        let w: usize = spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
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
