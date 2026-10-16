//! Small building blocks shared by the workspace views: barred cards, badges
//! and the two button styles.

use gpui::prelude::*;
use gpui::{Context, Div, FontWeight, SharedString, Stateful, div, px, rgb};

use crate::app::App;
use crate::theme;

/// Braille spinner frame used for anything running.
pub(super) const SPINNER: &str = "⠹";

/// A flat card marked by a 3px bar on its left edge.
pub(super) fn bar_card(bar: u32, bg: u32) -> Div {
    div()
        .w_full()
        .min_w(px(0.))
        .bg(rgb(bg))
        .border_l_3()
        .border_color(rgb(bar))
}

/// Solid label on a colored background (`PENDING`, counts).
pub(super) fn badge(text: impl Into<SharedString>, bg: u32) -> Div {
    div()
        .flex_shrink_0()
        .font_family(theme::MONO_FONT_FAMILY)
        .text_size(px(10.))
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(theme::ACCENT_DARK_TEXT))
        .bg(rgb(bg))
        .px(px(5.))
        .child(text.into())
}

#[derive(Clone, Copy)]
pub(super) enum ButtonKind {
    /// Filled with a highlight color.
    Primary(u32),
    Secondary,
}

pub(super) fn button(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> Stateful<Div> {
    let (bg, fg, hover) = match kind {
        ButtonKind::Primary(color) => (color, theme::ACCENT_DARK_TEXT, color),
        ButtonKind::Secondary => (theme::DIVIDER, theme::TEXT_SECONDARY, theme::TRACK),
    };
    div()
        .id(id.into())
        .flex_shrink_0()
        .px(px(10.))
        .py(px(5.))
        .bg(rgb(bg))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(fg))
        .cursor_pointer()
        .hover(move |style| style.bg(rgb(hover)).opacity(0.9))
        .child(label.into())
        .on_click(cx.listener(move |app, _, _, cx| on_click(app, cx)))
}

/// Text-only control; callers pick its colors and hover style.
pub(super) fn ghost(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex_shrink_0()
        .cursor_pointer()
        .child(label.into())
}

/// Segmented meter like `▰▰▰▰▱▱▱`, `filled` of `total` cells lit.
pub(super) fn meter(filled: usize, total: usize, color: u32) -> Div {
    let filled = filled.min(total);
    div()
        .flex()
        .child(div().text_color(rgb(color)).child("▰".repeat(filled)))
        .child(
            div()
                .text_color(rgb(theme::TRACK))
                .child("▰".repeat(total - filled)),
        )
}

/// Two-letter initials for an avatar circle.
pub(super) fn initials(name: &str) -> String {
    let mut words = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty());
    let first = words.next().and_then(|w| w.chars().next());
    let second = words.next().and_then(|w| w.chars().next());
    match (first, second) {
        (Some(a), Some(b)) => format!("{a}{b}").to_uppercase(),
        (Some(a), None) => a.to_uppercase().to_string(),
        _ => "?".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn initials_take_the_first_letters_of_the_first_two_words() {
        assert_eq!(initials("raylib-game"), "RG");
        assert_eq!(initials("craft"), "C");
        assert_eq!(initials(""), "?");
    }
}
