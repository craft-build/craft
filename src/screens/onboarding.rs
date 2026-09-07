use gpui::prelude::*;
use gpui::{Context, Window, div, px, rgb};

use crate::app::App;
use crate::chrome;
use crate::theme;

pub fn render(_app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    div()
        .size_full()
        .relative()
        .child(
            chrome::draggable(div().absolute().top_0().left_0().right_0().h(px(44.)))
                .flex()
                .items_center()
                .justify_end()
                .px(px(12.))
                .child(chrome::window_controls(window, cx)),
        )
        .child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
            div()
                .w(px(420.))
                .flex()
                .flex_col()
                .gap(px(22.))
                .items_start()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().w(px(9.)).h(px(9.)).bg(rgb(theme::ACCENT)))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child("FORGE"),
                        ),
                )
                .child(
                    div()
                        .text_size(px(20.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .line_height(px(28.))
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child("A singleplayer environment for coding with AI."),
                )
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(22.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(
                            "No teammates, no shared threads. Just you, the agent, and every diff, checkpoint, and comment kept on your machine.",
                        ),
                )
                .child(
                    div()
                        .id("go-projects")
                        .mt(px(8.))
                        .px(px(16.))
                        .py(px(10.))
                        .bg(rgb(theme::ACCENT))
                        .text_size(px(13.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::ACCENT_DARK_TEXT))
                        .cursor_pointer()
                        .child("Set up local workspace →")
                        .on_click(cx.listener(|app, _, _, cx| app.go_projects(cx))),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Runs fully offline. Nothing leaves this machine unless you say so."),
                ),
        )
        )
}
