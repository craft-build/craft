use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, Window, div, px, rgb};

use crate::app::{App, ReviewTab, SessionConfigControl};
use crate::chrome;
use crate::theme;

use super::{SIDEBAR_WIDTH, active_session_ref};

// ---------------------------------------------------------------- top bar

pub(super) fn top_bar(
    app: &mut App,
    window: &mut Window,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let project_name = app
        .active_project
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let path = app
        .active_project
        .as_ref()
        .map(|p| p.path.clone())
        .unwrap_or_default();
    let session_name = active_session_ref(app)
        .map(|session| session.name.clone())
        .unwrap_or_else(|| "New session".into());
    let checkpoint_count = app.checkpoints_for().len();
    let pending_count = app.pending_comments().len();
    let status_color = if app.thinking || app.acp_client.is_some() {
        theme::ACCENT
    } else {
        theme::TEXT_FAINT
    };
    let review_open = app.file_tree_visible && app.review_tab == ReviewTab::Comments;

    chrome::draggable(
        div()
            .h(px(40.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(rgb(theme::PANEL_BG))
            .border_b_1()
            .border_color(rgb(theme::BORDER)),
    )
    .child(
        div()
            .h_full()
            .flex_shrink_0()
            .when(app.sidebar_visible, |d| {
                d.w(px(SIDEBAR_WIDTH))
                    .border_r_1()
                    .border_color(rgb(theme::BORDER))
            })
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(chrome::leading_inset()))
            .pr(px(14.))
            .child(icon_button("toggle-sidebar", "◧", cx, |app, cx| {
                app.toggle_sidebar(cx)
            }))
            .child(icon_button("all-workspaces", "⌂", cx, |app, cx| {
                app.go_projects(cx)
            }))
            .child(
                div()
                    .ml_auto()
                    .pl(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .font_family(theme::MONO_FONT_FAMILY)
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(
                        div()
                            .size(px(16.))
                            .rounded(px(4.))
                            .bg(rgb(theme::ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(10.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(theme::ACCENT_DARK_TEXT))
                            .child("C"),
                    )
                    .child("craft"),
            ),
    )
    .child(
        div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(16.))
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(320.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::TEXT_PRIMARY))
                    .child(session_name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .font_family(theme::MONO_FONT_FAMILY)
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child(format!("{project_name} / {path}")),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.))
                            .max_w(px(260.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font_family(theme::MONO_FONT_FAMILY)
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(div().text_color(rgb(status_color)).child("●"))
                            .child(app.connection_status.clone()),
                    )
                    .child(div().w(px(1.)).h(px(16.)).bg(rgb(theme::DIVIDER)))
                    .child(
                        bar_button("toggle-checkpoints", "⟲", "Checkpoints", cx, |app, cx| {
                            app.toggle_checkpoints(cx)
                        })
                        .child(count_badge(
                            checkpoint_count,
                            theme::DIVIDER,
                            theme::TEXT_SECONDARY,
                        )),
                    )
                    .child(
                        bar_button("toggle-review", "☰", "Review", cx, move |app, cx| {
                            if review_open {
                                app.toggle_file_tree(cx)
                            } else {
                                app.set_review_tab(ReviewTab::Comments, cx)
                            }
                        })
                        .when(pending_count > 0, |d| {
                            d.child(count_badge(
                                pending_count,
                                theme::USER,
                                theme::ACCENT_DARK_TEXT,
                            ))
                        }),
                    )
                    .child(icon_button("goto-settings", "⚙", cx, |app, cx| {
                        app.go_settings(cx)
                    }))
                    .child(icon_button("toggle-file-tree", "◨", cx, |app, cx| {
                        app.toggle_file_tree(cx)
                    })),
            ),
    )
    .child(chrome::window_controls(window, cx))
}

fn bar_button(
    id: &'static str,
    glyph: &'static str,
    label: &'static str,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(6.))
        .py(px(3.))
        .rounded(px(4.))
        .text_size(px(12.))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .cursor_pointer()
        .hover(|style| {
            style
                .bg(rgb(theme::HOVER_BG))
                .text_color(rgb(theme::TEXT_PRIMARY))
        })
        .child(div().text_color(rgb(theme::TEXT_MUTED)).child(glyph))
        .child(label)
        .on_click(cx.listener(move |app, _, _, cx| on_click(app, cx)))
}

fn count_badge(count: usize, bg: u32, fg: u32) -> impl IntoElement {
    div()
        .font_family(theme::MONO_FONT_FAMILY)
        .text_size(px(10.))
        .text_color(rgb(fg))
        .bg(rgb(bg))
        .px(px(5.))
        .rounded(px(3.))
        .child(count.to_string())
}

fn icon_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(14.))
        .text_color(rgb(theme::TEXT_MUTED))
        .cursor_pointer()
        .rounded(px(4.))
        .hover(|style| {
            style
                .bg(rgb(theme::HOVER_BG))
                .text_color(rgb(theme::TEXT_PRIMARY))
        })
        .child(label)
        .on_click(cx.listener(move |app, _, _, cx| on_click(app, cx)))
}

pub(super) fn agent_selector(
    profiles: Vec<crate::config::AgentProfile>,
    selected_id: Option<String>,
    can_select: bool,
    menu_open: bool,
    connected: bool,
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
    let interactive = can_select && has_profiles;

    div()
        .relative()
        .child(
            div()
                .id("agent-menu-toggle")
                .flex()
                .items_center()
                .gap(px(6.))
                .text_color(rgb(if selected_id.is_some() {
                    theme::TEXT_SECONDARY
                } else {
                    theme::USER
                }))
                .when(interactive, |d| {
                    d.cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                        .on_click(cx.listener(|app, _, _, cx| app.toggle_agent_menu(cx)))
                })
                .child(
                    div()
                        .text_color(rgb(if connected {
                            theme::ACCENT
                        } else {
                            theme::TEXT_FAINT
                        }))
                        .child("●"),
                )
                .child(selected_name)
                .when(interactive, |d| {
                    d.child(div().text_color(rgb(theme::TEXT_MUTED)).child("▾"))
                }),
        )
        .when(menu_open && interactive, |d| {
            d.child(
                menu_panel(px(200.)).children(profiles.into_iter().map(|profile| {
                    let profile_id = profile.id.clone();
                    let is_selected = selected_id.as_deref() == Some(profile.id.as_str());
                    menu_item(
                        SharedString::from(format!("session-agent-profile-{}", profile.id)),
                        profile.name,
                        is_selected,
                    )
                    .on_click(
                        cx.listener(move |app, _, _, cx| app.select_agent_profile(&profile_id, cx)),
                    )
                })),
            )
        })
        .into_any_element()
}

/// Option sets this small are shown inline as a segmented toggle.
const SEGMENTED_MAX_CHOICES: usize = 3;

pub(super) fn config_selector(
    control: SessionConfigControl,
    is_open: bool,
    search_input: gpui::Entity<crate::text_input::TextInput>,
    search_query: String,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    if !control.searchable && (2..=SEGMENTED_MAX_CHOICES).contains(&control.choices.len()) {
        return segmented_selector(control, cx);
    }
    let toggle_id = control.id.clone();
    let has_choices = !control.choices.is_empty();
    let searchable = control.searchable;
    let selected_name = control.selected_name.clone();
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
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(
                    div()
                        .text_color(rgb(theme::TEXT_FAINT))
                        .child(control.name.to_lowercase()),
                )
                .child(control.selected_name.clone())
                .when(has_choices, |d| {
                    d.cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                        .child(div().text_color(rgb(theme::TEXT_MUTED)).child("▾"))
                        .on_click(
                            cx.listener(move |app, _, _, cx| {
                                app.toggle_config_menu(&toggle_id, cx)
                            }),
                        )
                }),
        )
        .when(is_open && has_choices, |d| {
            d.child(
                menu_panel(px(280.))
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
                                        .flex()
                                        .gap(px(8.))
                                        .bg(rgb(theme::INPUT_BG))
                                        .border_l_3()
                                        .border_color(rgb(theme::ACCENT))
                                        .px(px(8.))
                                        .py(px(6.))
                                        .text_size(px(11.))
                                        .child(
                                            div()
                                                .text_color(rgb(theme::ACCENT))
                                                .font_weight(FontWeight::BOLD)
                                                .child("/"),
                                        )
                                        .child(div().flex_1().child(search_input)),
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
                                        .text_color(rgb(theme::TEXT_FAINT))
                                        .child("No matching models"),
                                )
                            })
                            .children(choices.into_iter().map(|choice| {
                                let config_id = control.id.clone();
                                let value = choice.value.clone();
                                let is_selected = choice.name == selected_name;
                                menu_item(
                                    SharedString::from(format!(
                                        "footer-config-{}-{}",
                                        control.id, choice.name
                                    )),
                                    choice.name,
                                    is_selected,
                                )
                                .on_click(cx.listener(
                                    move |app, _, _, cx| {
                                        app.select_session_config(
                                            config_id.clone(),
                                            value.clone(),
                                            cx,
                                        )
                                    },
                                ))
                            })),
                    ),
            )
        })
        .into_any_element()
}

fn segmented_selector(control: SessionConfigControl, cx: &mut Context<App>) -> gpui::AnyElement {
    div()
        .flex()
        .bg(rgb(theme::INPUT_BG))
        .children(control.choices.into_iter().map(|choice| {
            let is_selected = choice.name == control.selected_name;
            let config_id = control.id.clone();
            let value = choice.value.clone();
            div()
                .id(SharedString::from(format!(
                    "footer-config-{}-{}",
                    control.id, choice.name
                )))
                .px(px(9.))
                .py(px(1.))
                .when(is_selected, |d| {
                    d.bg(rgb(theme::ACCENT))
                        .text_color(rgb(theme::ACCENT_DARK_TEXT))
                        .font_weight(FontWeight::BOLD)
                        .child(choice.name.to_uppercase())
                })
                .when(!is_selected, |d| {
                    d.text_color(rgb(theme::TEXT_MUTED))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                        .child(choice.name.to_lowercase())
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.select_session_config(config_id.clone(), value.clone(), cx)
                        }))
                })
        }))
        .into_any_element()
}

/// Popup that opens upward from a control in the composer's bottom row.
fn menu_panel(width: gpui::Pixels) -> gpui::Div {
    div()
        .absolute()
        .bottom(px(24.))
        .left(px(-10.))
        .w(width)
        .bg(rgb(theme::PANEL_BG))
        .border_1()
        .border_color(rgb(theme::DIVIDER))
        .py(px(4.))
        .font_family(theme::MONO_FONT_FAMILY)
        .text_size(px(12.))
}

fn menu_item(id: SharedString, label: String, selected: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .gap(px(8.))
        .px(px(10.))
        .py(px(6.))
        .cursor_pointer()
        .text_color(rgb(if selected {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_SECONDARY
        }))
        .hover(|style| {
            style
                .bg(rgb(theme::HOVER_BG))
                .text_color(rgb(theme::TEXT_PRIMARY))
        })
        .child(
            div()
                .w(px(10.))
                .text_color(rgb(theme::ACCENT))
                .child(if selected { "▸" } else { "" }),
        )
        .child(label)
}
