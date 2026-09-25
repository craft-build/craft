use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, Window, div, px, rgb};

use crate::app::{App, SessionConfigControl};
use crate::chrome;
use crate::theme;

// ---------------------------------------------------------------- top bar

pub(super) fn top_bar(
    app: &mut App,
    window: &mut Window,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let name = app
        .active_project
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let path = app
        .active_project
        .as_ref()
        .map(|p| p.path.clone())
        .unwrap_or_default();
    let checkpoint_count = app.checkpoints_for().len();

    chrome::draggable(
        div()
            .h(px(36.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .pl(px(chrome::leading_inset()))
            .pr(px(8.))
            .bg(rgb(theme::PANEL_BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER)),
    )
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(icon_button("all-workspaces", "←", cx, |app, cx| {
                app.go_projects(cx)
            }))
            .child(icon_button("toggle-sidebar", "☰", cx, |app, cx| {
                app.toggle_sidebar(cx)
            }))
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(path),
            ),
    )
    .child(
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(
                div()
                    .id("toggle-checkpoints")
                    .px(px(9.))
                    .py(px(4.))
                    .bg(rgb(theme::INPUT_BG))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .rounded(px(5.))
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_SECONDARY))
                    .cursor_pointer()
                    .child(format!("Checkpoints ({checkpoint_count})"))
                    .on_click(cx.listener(|app, _, _, cx| app.toggle_checkpoints(cx))),
            )
            .child(
                div()
                    .id("goto-settings")
                    .px(px(9.))
                    .py(px(4.))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .rounded(px(5.))
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_SECONDARY))
                    .cursor_pointer()
                    .child("Settings")
                    .on_click(cx.listener(|app, _, _, cx| app.go_settings(cx))),
            )
            .child(icon_button("toggle-file-tree", "☰", cx, |app, cx| {
                app.toggle_file_tree(cx)
            }))
            .child(chrome::window_controls(window, cx)),
    )
}

fn icon_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .text_size(px(14.))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .cursor_pointer()
        .px(px(5.))
        .py(px(3.))
        .rounded(px(4.))
        .hover(|style| style.bg(rgb(theme::HOVER_BG)))
        .child(label)
        .on_click(cx.listener(move |app, _, _, cx| on_click(app, cx)))
}

// ---------------------------------------------------------------- footer

pub(super) fn footer_bar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let percent = app.context_usage.unwrap_or(0);
    let context_label = app
        .context_usage
        .map(|percent| format!("{percent}% context"))
        .unwrap_or_else(|| "Context unavailable".into());
    let status_label = app.connection_status.clone();
    let status_color = if app.thinking {
        theme::ACCENT
    } else {
        theme::TEXT_MUTED
    };
    let config_controls = app.session_config_controls.clone();
    let open_config_menu = app.open_config_menu.clone();
    let config_search_input = app.config_search_input.clone();
    let config_search_query = app
        .config_search_input
        .read(cx)
        .content
        .trim()
        .to_lowercase();
    let agent_profiles = app.agent_profiles.clone();
    let selected_agent = app.active_agent_profile_id().map(str::to_string);
    let can_select_agent = app.can_select_agent_for_session();
    let agent_menu_open = app.agent_menu_open;

    div()
        .h(px(28.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_between()
        .px(px(12.))
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::FOOTER_BG))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .child(agent_selector(
                    agent_profiles,
                    selected_agent,
                    can_select_agent,
                    agent_menu_open,
                    cx,
                ))
                .children(config_controls.into_iter().map(|control| {
                    let is_open = open_config_menu.as_deref() == Some(control.id.as_str());
                    config_selector(
                        control,
                        is_open,
                        config_search_input.clone(),
                        config_search_query.clone(),
                        cx,
                    )
                })),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .w(px(14.))
                                .h(px(14.))
                                .rounded_full()
                                .bg(rgb(theme::ACCENT))
                                .opacity((percent as f32 / 100.0).clamp(0.15, 1.0)),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(context_label),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(
                            div()
                                .w(px(6.))
                                .h(px(6.))
                                .rounded_full()
                                .bg(rgb(status_color)),
                        )
                        .child(status_label),
                ),
        )
}

fn agent_selector(
    profiles: Vec<crate::config::AgentProfile>,
    selected_id: Option<String>,
    can_select: bool,
    menu_open: bool,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let selected_name = selected_id
        .as_ref()
        .and_then(|selected| {
            profiles
                .iter()
                .find(|profile| &profile.id == selected)
                .map(|profile| profile.name.clone())
        })
        .unwrap_or_else(|| "Select agent".into());
    let has_profiles = !profiles.is_empty();

    div()
        .relative()
        .child(
            div()
                .id("agent-menu-toggle")
                .flex()
                .items_center()
                .gap(px(5.))
                .text_size(px(11.))
                .text_color(rgb(if selected_id.is_some() {
                    theme::TEXT_SECONDARY
                } else {
                    theme::ACCENT
                }))
                .when(can_select && has_profiles, |d| {
                    d.cursor_pointer()
                        .on_click(cx.listener(|app, _, _, cx| app.toggle_agent_menu(cx)))
                })
                .child(format!("Agent: {selected_name}"))
                .when(can_select && has_profiles, |d| {
                    d.child(div().text_color(rgb(theme::TEXT_MUTED)).child("▾"))
                }),
        )
        .when(menu_open && can_select && has_profiles, |d| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(28.))
                    .left(px(0.))
                    .bg(rgb(theme::INPUT_BG))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .min_w(px(170.))
                    .children(profiles.into_iter().map(|profile| {
                        let profile_id = profile.id.clone();
                        div()
                            .id(SharedString::from(format!(
                                "session-agent-profile-{}",
                                profile.id
                            )))
                            .px(px(10.))
                            .py(px(8.))
                            .text_size(px(12.))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                            .child(profile.name)
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.select_agent_profile(&profile_id, cx)
                            }))
                    })),
            )
        })
        .into_any_element()
}

fn config_selector(
    control: SessionConfigControl,
    is_open: bool,
    search_input: gpui::Entity<crate::text_input::TextInput>,
    search_query: String,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let toggle_id = control.id.clone();
    let has_choices = !control.choices.is_empty();
    let searchable = control.searchable;
    let choices = control
        .choices
        .iter()
        .filter(|choice| !searchable || choice.name.to_lowercase().contains(&search_query))
        .cloned()
        .collect::<Vec<_>>();
    let no_matches = choices.is_empty();

    div()
        .relative()
        .child(
            div()
                .id(SharedString::from(format!(
                    "config-menu-toggle-{}",
                    control.id
                )))
                .flex()
                .items_center()
                .gap(px(5.))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .when(has_choices, |d| d.cursor_pointer())
                .child(format!("{}: {}", control.name, control.selected_name))
                .when(has_choices, |d| {
                    d.child(div().text_color(rgb(theme::TEXT_MUTED)).child("▾"))
                        .on_click(
                            cx.listener(move |app, _, _, cx| {
                                app.toggle_config_menu(&toggle_id, cx)
                            }),
                        )
                }),
        )
        .when(is_open && has_choices, |d| {
            d.child(
                div()
                    .absolute()
                    .bottom(px(28.))
                    .left(px(0.))
                    .bg(rgb(theme::INPUT_BG))
                    .border_1()
                    .border_color(rgb(theme::BORDER))
                    .w(px(280.))
                    .max_h(px(320.))
                    .flex()
                    .flex_col()
                    .when(searchable, |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .p(px(8.))
                                .border_b_1()
                                .border_color(rgb(theme::BORDER))
                                .child(
                                    div()
                                        .bg(rgb(theme::PANEL_BG))
                                        .border_1()
                                        .border_color(rgb(theme::BORDER))
                                        .px(px(8.))
                                        .py(px(6.))
                                        .text_size(px(11.))
                                        .child(search_input),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "config-options-scroll-{}",
                                control.id
                            )))
                            .max_h(px(if searchable { 264. } else { 318. }))
                            .overflow_y_scroll()
                            .when(no_matches, |d| {
                                d.child(
                                    div()
                                        .px(px(10.))
                                        .py(px(12.))
                                        .text_size(px(11.))
                                        .text_color(rgb(theme::TEXT_MUTED))
                                        .child("No matching models"),
                                )
                            })
                            .children(choices.into_iter().map(|choice| {
                                let config_id = control.id.clone();
                                let value = choice.value.clone();
                                div()
                                    .id(SharedString::from(format!(
                                        "footer-config-{}-{}",
                                        control.id, choice.name
                                    )))
                                    .px(px(10.))
                                    .py(px(8.))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                                    .child(choice.name)
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        app.select_session_config(
                                            config_id.clone(),
                                            value.clone(),
                                            cx,
                                        )
                                    }))
                            })),
                    ),
            )
        })
        .into_any_element()
}
