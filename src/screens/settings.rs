use gpui::prelude::*;
use gpui::{Context, FontWeight, Window, div, px, rgb};

use crate::app::App;
use crate::chrome;
use crate::state::MODEL_NAMES;
use crate::theme;

pub fn render(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            chrome::draggable(
                div()
                    .h(px(44.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pl(px(chrome::leading_inset()))
                    .pr(px(16.))
                    .border_b_1()
                    .border_color(rgb(theme::BORDER)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .id("settings-back")
                            .text_size(px(14.))
                            .text_color(rgb(theme::TEXT_SECONDARY))
                            .cursor_pointer()
                            .child("←")
                            .on_click(cx.listener(|app, _, _, cx| app.go_back_from_settings(cx))),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Settings"),
                    ),
            )
            .child(chrome::window_controls(window, cx)),
        )
        .child(
            div()
                .flex_1()
                .id("settings-scroll")
                .overflow_y_scroll()
                .p(px(28.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(520.))
                        .flex()
                        .flex_col()
                        .gap(px(28.))
                        .child(model_section(app, cx))
                        .child(editor_section())
                        .child(shortcuts_section())
                        .child(about_section()),
                ),
        )
}

fn section_label(text: &'static str) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .text_color(rgb(theme::TEXT_MUTED))
        .mb(px(10.))
        .child(text)
}

fn model_section(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    div().flex().flex_col().child(section_label("DEFAULT MODEL")).child(
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(MODEL_NAMES.iter().map(|&name| {
                let selected = app.selected_model == name;
                div()
                    .id(gpui::SharedString::from(format!("model-{name}")))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .py(px(8.))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(theme::INPUT_BG)))
                    .child(
                        div()
                            .w(px(8.))
                            .h(px(8.))
                            .rounded_full()
                            .border_1()
                            .border_color(rgb(theme::ACCENT))
                            .when(selected, |d| d.bg(rgb(theme::ACCENT))),
                    )
                    .child(div().text_size(px(12.)).child(name))
                    .on_click(cx.listener(move |app, _, _, cx| app.select_model(name, cx)))
            })),
    )
}

fn settings_row(label: &'static str, value: &'static str) -> impl IntoElement {
    div()
        .flex()
        .justify_between()
        .px(px(10.))
        .py(px(8.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(label),
        )
        .child(div().text_size(px(12.)).child(value))
}

fn editor_section() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(section_label("EDITOR"))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(settings_row("Font size", "13px"))
                .child(settings_row("Theme", "Charcoal / Cyan"))
                .child(settings_row("Diff style", "Inline")),
        )
}

fn shortcut_row(label: &'static str, keys: &'static str) -> impl IntoElement {
    div()
        .flex()
        .justify_between()
        .px(px(10.))
        .py(px(6.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(label),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(keys),
        )
}

fn shortcuts_section() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(section_label("KEYBOARD SHORTCUTS"))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(shortcut_row("Send message", "Enter"))
                .child(shortcut_row("Checkpoint history", "⌘ H"))
                .child(shortcut_row("Switch model", "⌘ M"))
                .child(shortcut_row("Toggle file tree", "⌘ B")),
        )
}

fn about_section() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(section_label("ABOUT"))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child("Forge 0.9.2 · build 2026.09.04"),
        )
}
