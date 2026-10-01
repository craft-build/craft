//! `/help`: the keybinding sheet overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;

use super::super::theme;
use super::usage::render_usage_sheet;
use crate::tui::app::App;
use crate::tui::modals::Modal;

/// `/help`: keybinding sheet, generated from the data-driven `KEYBINDS`
/// table grouped by context (F.1); user overrides show their effective
/// chords, disabled actions are omitted.
pub fn render_help(f: &mut Frame, app: &mut App, area: Rect) {
    let t = theme::current();

    if !matches!(app.overlays.modal, Modal::Help) {
        return;
    }
    let (lines, _) = crate::tui::keybindings::help_lines(&app.overlays.keybinds);
    let scroll = app.overlays.help_scroll;
    app.overlays.help_scroll_max = render_usage_sheet(
        f,
        area,
        "Keybindings",
        &lines,
        vec![Span::styled(
            "↑↓ scroll · esc to close",
            Style::default().fg(t.text_tertiary),
        )],
        scroll,
    );
}
