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
                                .child(session_options_section(app, cx))
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

fn session_options_section(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let controls = app.session_config_controls.clone();
    div()
        .flex()
        .flex_col()
        .child(section_label("SESSION OPTIONS"))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(12.))
                .when(controls.is_empty(), |d| {
                    d.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("The connected agent has not advertised session options."),
                    )
                })
                .children(controls.into_iter().map(|control| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .px(px(10.))
                        .py(px(8.))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(control.name.clone()),
                        )
                        .child(div().flex().flex_wrap().gap(px(6.)).children(
                            control.choices.into_iter().map(|choice| {
                                let selected = choice.name == control.selected_name;
                                let config_id = control.id.clone();
                                let value = choice.value.clone();
                                div()
                                    .id(gpui::SharedString::from(format!(
                                        "session-option-{}-{}",
                                        control.id, choice.name
                                    )))
                                    .px(px(8.))
                                    .py(px(5.))
                                    .text_size(px(11.))
                                    .border_1()
                                    .border_color(rgb(theme::BORDER))
                                    .cursor_pointer()
                                    .when(selected, |d| d.bg(rgb(theme::INPUT_BG)))
                                    .child(choice.name)
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        app.select_session_config(
                                            config_id.clone(),
                                            value.clone(),
                                            cx,
                                        )
                                    }))
                            }),
                        ))
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
