//! Custom window chrome: the app draws its own top bars (see
//! `screens::workspace::top_bar` and friends) instead of using a native
//! white macOS titlebar. `main.rs` sets `appears_transparent: true` (and
//! `window_decorations: Client` on Linux) so the OS draws no titlebar
//! background at all; what's left is:
//!
//! - macOS: the native traffic lights still exist (we can't draw those
//!   ourselves), just repositioned via `TitlebarOptions::traffic_light_position`
//!   to sit inside our bar. Callers must reserve `leading_inset()` of left
//!   padding so their own content starts after them.
//! - Windows/Linux: there's no native decoration left at all once the
//!   titlebar is transparent, so this module draws minimize/maximize/close
//!   buttons for those platforms (mac gets nothing here — the OS's own
//!   traffic lights cover it).
//!
//! Every top bar should also mark its empty background with
//! `.window_control_area(WindowControlArea::Drag)` so the window can still
//! be dragged by its custom bar, the way a native titlebar would be.

use gpui::prelude::*;
use gpui::{Context, MouseButton, Window, WindowControlArea, div, px, rgb};

use crate::app::App;
use crate::theme;

/// Left padding a bar needs so its own content starts after the window
/// controls: on macOS that's clearance for the native traffic lights, on
/// other platforms just a normal inset (their controls go on the right).
pub const fn leading_inset() -> f32 {
    if cfg!(target_os = "macos") { 88. } else { 12. }
}

/// Double-click-to-zoom on a drag area, matching native titlebar behavior
/// (zoom on mac/Linux; Windows handles this itself via its own buttons).
fn handle_titlebar_double_click()
-> impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static {
    |event, window, _| {
        if event.click_count() == 2 {
            if cfg!(target_os = "macos") {
                window.titlebar_double_click();
            } else {
                window.zoom_window();
            }
        }
    }
}

/// Marks a bar container as a drag region (double-click to zoom, like a
/// native titlebar). Apply directly to the bar's outer `div()` — interactive
/// children (buttons) still win hit-testing over their ancestor's drag area,
/// so this is safe to put on the whole bar rather than just empty gaps.
pub fn draggable(el: gpui::Div) -> gpui::Stateful<gpui::Div> {
    el.id("chrome-drag-area")
        .window_control_area(WindowControlArea::Drag)
        .on_click(handle_titlebar_double_click())
}

/// Minimize/maximize/close buttons for non-macOS platforms. Renders as
/// nothing on macOS, where the native traffic lights (repositioned via
/// `traffic_light_position`) already do this job.
pub fn window_controls(window: &mut Window, _cx: &mut Context<App>) -> impl IntoElement {
    if cfg!(target_os = "macos") {
        return div();
    }

    let maximized = window.is_maximized();

    div()
        .flex()
        .items_center()
        .h_full()
        .child(control_button(
            "win-minimize",
            "−",
            WindowControlArea::Min,
            |window, _| {
                window.minimize_window();
            },
        ))
        .child(control_button(
            "win-maximize",
            if maximized { "❐" } else { "□" },
            WindowControlArea::Max,
            |window, _| window.zoom_window(),
        ))
        .child(control_button(
            "win-close",
            "✕",
            WindowControlArea::Close,
            |window, _| {
                window.remove_window();
            },
        ))
}

fn control_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .window_control_area(area)
        .w(px(36.))
        .h_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(12.))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::HOVER_BG)).rounded(px(4.)))
        .child(glyph)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            on_click(window, cx);
        })
}
