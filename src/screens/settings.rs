use gpui::prelude::*;
use gpui::{Context, FontWeight, Window, div, px, rgb};

use crate::app::App;
use crate::chrome;
use crate::text_input::TextInput;
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
                        .when(app.active_project.is_some(), |d| {
                            d.child(agent_section(app, cx))
                                .child(model_section(app, cx))
                        })
                        .when(app.active_project.is_none(), |d| {
                            d.child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child("Open a workspace to configure its ACP agent."),
                            )
                        })
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
    let models = app.available_models.clone();
    div()
        .flex()
        .flex_col()
        .child(section_label("AGENT MODELS"))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .when(models.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("The connected agent has not advertised model choices."),
                    )
                })
                .children(models.into_iter().map(|name| {
                    let selected = app.selected_model == name;
                    let click_name = name.clone();
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
                        .on_click(
                            cx.listener(move |app, _, _, cx| app.select_model(&click_name, cx)),
                        )
                })),
        )
}

fn agent_section(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let remote = app.config_remote;
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(section_label("ACP AGENT"))
        .child(input_row("Agent command", app.agent_command_input.clone()))
        .child(
            div()
                .id("transport-toggle")
                .flex()
                .justify_between()
                .px(px(10.))
                .py(px(8.))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .cursor_pointer()
                .child(div().text_size(px(12.)).child("Transport"))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(theme::ACCENT))
                        .child(if remote { "SSH" } else { "Local" }),
                )
                .on_click(cx.listener(|app, _, _, cx| app.toggle_config_remote(cx))),
        )
        .when(remote, |d| {
            d.child(input_row(
                "Remote workspace",
                app.remote_workspace_input.clone(),
            ))
            .child(input_row("SSH host", app.ssh_host_input.clone()))
            .child(input_row("SSH user", app.ssh_user_input.clone()))
            .child(input_row("Identity file", app.ssh_key_input.clone()))
        })
        .child(
            div()
                .id("save-agent-config")
                .px(px(12.))
                .py(px(7.))
                .bg(rgb(theme::SELECTION))
                .text_size(px(12.))
                .cursor_pointer()
                .child("Save & connect")
                .on_click(cx.listener(|app, _, _, cx| app.save_agent_config(cx))),
        )
}

fn input_row(label: &'static str, input: gpui::Entity<TextInput>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .px(px(10.))
        .py(px(8.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .w(px(100.))
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(input),
        )
}

fn about_section() -> impl IntoElement {
    div().flex().flex_col().child(section_label("ABOUT")).child(
        div()
            .text_size(px(12.))
            .text_color(rgb(theme::TEXT_SECONDARY))
            .child(format!("Forge {}", env!("CARGO_PKG_VERSION"))),
    )
}
