//! `/help`: the keybinding sheet overlay.

use super::super::theme;
use crate::tui::app::App;
use crate::tui::modals::Modal;
use crate::tui::style::Style;
use crate::tui::text::Span;

/// `/help` on the slt frame — the shared sheet renderer with the
/// keybinding table and scroll footer.
pub(super) fn render_help_slt(ui: &mut slt::Context, app: &mut App) {
    let t = theme::current();

    if !matches!(app.overlays.modal, Modal::Help) {
        return;
    }
    let (lines, _) = crate::tui::keybindings::help_lines(&app.overlays.keybinds);
    let scroll = app.overlays.help_scroll;
    app.overlays.help_scroll_max = super::usage::render_usage_sheet_slt(
        ui,
        "Keybindings",
        &lines,
        vec![Span::styled(
            "↑↓ scroll · esc to close",
            Style::default().fg(t.text_tertiary),
        )],
        scroll,
    );
}
