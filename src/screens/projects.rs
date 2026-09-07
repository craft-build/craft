use gpui::prelude::*;
use gpui::{Context, FontWeight, Window, div, px, rgb};

use crate::app::App;
use crate::chrome;
use crate::theme;

pub fn render(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(top_bar(window, cx))
        .child(
            div()
                .flex_1()
                .id("projects-scroll")
                .overflow_y_scroll()
                .p(px(28.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .w(px(640.))
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child("RECENT SESSIONS"),
                        )
                        .children(
                            app.projects
                                .clone()
                                .into_iter()
                                .map(|p| project_card(p, cx)),
                        ),
                ),
        )
}

fn top_bar(window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
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
            .gap(px(8.))
            .child(div().w(px(8.)).h(px(8.)).bg(rgb(theme::ACCENT)))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::BOLD)
                    .child("FORGE"),
            ),
    )
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(
                div()
                    .id("new-session")
                    .px(px(12.))
                    .py(px(6.))
                    .bg(rgb(theme::ACCENT))
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::ACCENT_DARK_TEXT))
                    .cursor_pointer()
                    .child("+ New session")
                    .on_click(cx.listener(|app, _, _, cx| app.new_session(cx))),
            )
            .child(
                div()
                    .id("settings-from-projects")
                    .px(px(10.))
                    .py(px(6.))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .text_size(px(12.))
                    .text_color(rgb(theme::TEXT_SECONDARY))
                    .cursor_pointer()
                    .child("Settings")
                    .on_click(cx.listener(|app, _, _, cx| app.go_settings(cx))),
            )
            .child(chrome::window_controls(window, cx)),
    )
}

fn project_card(p: crate::state::Project, cx: &mut Context<App>) -> gpui::AnyElement {
    let id = p.id.clone();
    div()
        .id(gpui::SharedString::from(format!("project-card-{}", p.id)))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::PANEL_BG))
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .cursor_pointer()
        .hover(|s| s.border_color(rgb(theme::ACCENT)))
        .child(
            div()
                .flex()
                .justify_between()
                .items_baseline()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(p.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(p.updated.clone()),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(p.path.clone()),
        )
        .child(
            div()
                .mt(px(2.))
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(p.desc.clone()),
        )
        .child(
            div()
                .mt(px(6.))
                .flex()
                .gap(px(14.))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(p.checkpoint_label.clone())
                .child(p.model.clone()),
        )
        .on_click(cx.listener(move |app, _, _, cx| app.open_project(&id, cx)))
        .into_any_element()
}
