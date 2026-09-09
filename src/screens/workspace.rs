use agent_client_protocol::schema::v1::{
    ElicitationMode, ElicitationPropertySchema, MultiSelectItems,
};
use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, Window, div, px, rgb, rgba};

use crate::app::{App, CommentDraft, SessionConfigControl};
use crate::chrome;
use crate::markdown::markdown_view;
use crate::selectable_text::{CommentTarget, SelectableText};
use crate::state::{Comment, Diff, DiffLine, DiffLineKind, Message, Role, Steps, Terminal};
use crate::theme;

pub fn render(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .relative()
        .child(top_bar(app, window, cx))
        .child(
            div()
                .flex_1()
                .flex()
                .overflow_x_hidden()
                .overflow_y_hidden()
                .when(app.sidebar_visible, |d| d.child(sidebar(app, cx)))
                .child(main_column(app, window, cx))
                .when(app.file_tree_visible, |d| d.child(right_panels(app, cx))),
        )
        .child(footer_bar(app, cx))
        .when(app.show_checkpoints, |d| {
            d.child(checkpoints_overlay(app, cx))
        })
}

// ---------------------------------------------------------------- top bar

fn top_bar(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
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

// ---------------------------------------------------------------- sidebar

fn sidebar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_project_id = app.active_project.as_ref().map(|p| p.id.clone());
    let active_session_id = app.active_session_id.clone();

    div()
        .w(px(210.))
        .flex_shrink_0()
        .bg(rgb(theme::PANEL_BG))
        .border_r_1()
        .border_color(rgb(theme::BORDER))
        .id("sidebar-scroll")
        .overflow_y_scroll()
        .py(px(8.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .px(px(12.))
                .pb(px(6.))
                .child("Projects"),
        )
        .children(app.projects.clone().into_iter().map(|p| {
            let expanded = !app.collapsed_projects.contains(&p.id);
            let is_active = active_project_id.as_deref() == Some(p.id.as_str());
            let sessions = app
                .sessions_by_project
                .get(&p.id)
                .cloned()
                .unwrap_or_default();
            let active_sessions = sessions
                .iter()
                .filter(|session| !session.archived)
                .cloned()
                .collect::<Vec<_>>();
            let archived_sessions = sessions
                .into_iter()
                .filter(|session| session.archived)
                .collect::<Vec<_>>();
            let archives_expanded = app.expanded_archives.contains(&p.id);
            let pending_delete = app.pending_session_delete.clone();
            let toggle_id = p.id.clone();
            let add_id = p.id.clone();

            div()
                .child(
                    div()
                        .id(SharedString::from(format!("proj-{}", p.id)))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(12.))
                        .py(px(4.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                        .child(
                            div()
                                .w(px(10.))
                                .text_size(px(10.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(if expanded { "▾" } else { "▸" }),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(if is_active {
                                    theme::TEXT_PRIMARY
                                } else {
                                    theme::TEXT_SECONDARY
                                }))
                                .child(p.name.clone()),
                        )
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.toggle_project_collapse(&toggle_id, cx)
                        })),
                )
                .when(expanded, |d| {
                    d.child(div().children(active_sessions.into_iter().map(|s| {
                        let session_active =
                            is_active && active_session_id.as_deref() == Some(s.id.as_str());
                        let project_id = p.id.clone();
                        let archive_project_id = p.id.clone();
                        let session_id = s.id.clone();
                        let archive_session_id = s.id.clone();
                        div()
                            .id(SharedString::from(format!("sess-{}-{}", p.id, s.id)))
                            .flex()
                            .items_center()
                            .py(px(4.))
                            .pl(px(28.))
                            .pr(px(8.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(if session_active {
                                theme::TEXT_PRIMARY
                            } else {
                                theme::TEXT_SECONDARY
                            }))
                            .when(session_active, |d| d.bg(rgb(theme::SELECTION)))
                            .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(s.name.clone()),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("archive-session-{}", s.id)))
                                    .flex_shrink_0()
                                    .px(px(4.))
                                    .text_size(px(10.))
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .cursor_pointer()
                                    .hover(|style| style.text_color(rgb(theme::ACCENT)))
                                    .child("archive")
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        cx.stop_propagation();
                                        app.archive_session(
                                            &archive_project_id,
                                            &archive_session_id,
                                            cx,
                                        )
                                    })),
                            )
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.open_session(&project_id, &session_id, cx)
                            }))
                    })))
                    .child(
                        div()
                            .id(SharedString::from(format!("add-sess-{}", p.id)))
                            .py(px(5.))
                            .pl(px(28.))
                            .pr(px(12.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .hover(|s| s.text_color(rgb(theme::ACCENT)))
                            .child("+ new session")
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.add_session_to_project(&add_id, cx)
                            })),
                    )
                    .when(!archived_sessions.is_empty(), |d| {
                        let archive_toggle_id = p.id.clone();
                        d.child(
                            div()
                                .id(SharedString::from(format!("archived-sessions-{}", p.id)))
                                .flex()
                                .items_center()
                                .gap(px(5.))
                                .py(px(5.))
                                .pl(px(28.))
                                .pr(px(8.))
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(theme::TEXT_SECONDARY)))
                                .child(if archives_expanded { "▾" } else { "▸" })
                                .child(format!("Archived ({})", archived_sessions.len()))
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    app.toggle_archived_sessions(&archive_toggle_id, cx)
                                })),
                        )
                        .when(archives_expanded, |d| {
                            d.children(archived_sessions.into_iter().map(|session| {
                                let restore_project_id = p.id.clone();
                                let delete_project_id = p.id.clone();
                                let restore_session_id = session.id.clone();
                                let delete_session_id = session.id.clone();
                                let delete_armed =
                                    pending_delete.as_deref() == Some(session.id.as_str());
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .py(px(5.))
                                    .pl(px(34.))
                                    .pr(px(8.))
                                    .text_size(px(11.))
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .child(session.name),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "restore-session-{}",
                                                session.id
                                            )))
                                            .cursor_pointer()
                                            .text_color(rgb(theme::ACCENT))
                                            .child("restore")
                                            .on_click(cx.listener(move |app, _, _, cx| {
                                                app.restore_session(
                                                    &restore_project_id,
                                                    &restore_session_id,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "delete-session-{}",
                                                session.id
                                            )))
                                            .cursor_pointer()
                                            .text_color(rgb(if delete_armed {
                                                theme::DIFF_DEL_TEXT
                                            } else {
                                                theme::TEXT_MUTED
                                            }))
                                            .child(if delete_armed { "confirm" } else { "×" })
                                            .on_click(cx.listener(move |app, _, _, cx| {
                                                app.request_delete_session(
                                                    &delete_project_id,
                                                    &delete_session_id,
                                                    cx,
                                                )
                                            })),
                                    )
                            }))
                        })
                    })
                })
        }))
}

// ---------------------------------------------------------------- main column

fn main_column(app: &mut App, _window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let messages = app.active_messages();
    let last_index = messages.len().saturating_sub(1);
    let scroll_handle = app.thread_scroll.clone();

    div()
        .flex_1()
        .min_w(px(340.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(rgb(theme::BG))
        .child(
            div()
                .id("thread-scroll")
                .track_scroll(&scroll_handle)
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(18.))
                .py(px(14.))
                .flex()
                .items_start()
                .child(
                    // Shrink only horizontally: the row resolves the thread's width
                    // before measuring wrapped text. A max-width constraint can leave
                    // GPUI's card heights or scroll extent measured at the wrong width.
                    div()
                        .w(px(760.))
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(16.))
                        .children(messages.into_iter().enumerate().map(|(index, message)| {
                            let is_streaming = app.thinking
                                && index == last_index
                                && matches!(message.role, Role::Assistant);
                            message_view(message, is_streaming, app, cx)
                        }))
                        .when(app.pending_permission.is_some(), |d| {
                            d.child(permission_view(app, cx))
                        })
                        .when(app.pending_elicitation.is_some(), |d| {
                            d.child(elicitation_view(app, cx))
                        })
                        .child(composer_view(app, cx)),
                ),
        )
}

fn permission_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let (title, choices) = app
        .pending_permission
        .as_ref()
        .map(|permission| {
            (
                permission.title.clone(),
                permission
                    .options
                    .iter()
                    .map(|option| option.name.clone())
                    .collect::<Vec<_>>()
                    .join(" · "),
            )
        })
        .unwrap_or_default();
    div()
        .w_full()
        .border_1()
        .border_color(rgb(theme::ACCENT))
        .rounded(px(8.))
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Permission required · {title}")),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(choices),
        )
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(action_button(
                    "permission-reject",
                    "Reject",
                    cx,
                    |app, cx| app.decide_permission(false, cx),
                ))
                .child(action_button(
                    "permission-allow",
                    "Allow once",
                    cx,
                    |app, cx| app.decide_permission(true, cx),
                )),
        )
}

fn elicitation_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let (message, title, description, properties, required, form_supported) = app
        .pending_elicitation
        .as_ref()
        .map(|elicitation| {
            if let ElicitationMode::Form(form) = &elicitation.request.mode {
                (
                    elicitation.request.message.clone(),
                    form.requested_schema.title.clone(),
                    form.requested_schema.description.clone(),
                    form.requested_schema
                        .properties
                        .iter()
                        .map(|(key, property)| (key.clone(), property.clone()))
                        .collect(),
                    form.requested_schema.required.clone().unwrap_or_default(),
                    true,
                )
            } else {
                (
                    elicitation.request.message.clone(),
                    None,
                    None,
                    vec![],
                    vec![],
                    false,
                )
            }
        })
        .unwrap_or_else(|| (String::new(), None, None, vec![], vec![], false));
    div()
        .w_full()
        .border_1()
        .border_color(rgb(theme::ACCENT))
        .rounded(px(8.))
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title.unwrap_or_else(|| "Agent needs information".into())),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(12.))
                .line_height(px(18.))
                .child(message),
        )
        .when_some(description, |d, description| {
            d.child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .whitespace_normal()
                    .text_size(px(11.))
                    .line_height(px(16.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(description),
            )
        })
        .when(!form_supported, |d| {
            d.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(theme::DIFF_DEL_TEXT))
                    .child("This agent requested an unsupported elicitation mode."),
            )
        })
        .children(properties.into_iter().map(|(key, property)| {
            let is_required = required.contains(&key);
            elicitation_field(key, property, is_required, app, cx)
        }))
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(action_button(
                    "elicitation-decline",
                    "Decline",
                    cx,
                    |app, cx| app.decline_elicitation(cx),
                ))
                .when(form_supported, |d| {
                    d.child(action_button(
                        "elicitation-accept",
                        "Submit",
                        cx,
                        |app, cx| app.accept_elicitation(cx),
                    ))
                }),
        )
}

fn elicitation_field(
    key: String,
    property: ElicitationPropertySchema,
    required: bool,
    app: &mut App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let (title, description) = match &property {
        ElicitationPropertySchema::String(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Number(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Integer(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Boolean(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        ElicitationPropertySchema::Array(schema) => {
            (schema.title.clone(), schema.description.clone())
        }
        _ => (None, None),
    };
    let label = format!(
        "{}{}",
        title.unwrap_or_else(|| key.clone()),
        if required { " *" } else { "" }
    );

    let mut field = div()
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(label),
        )
        .when_some(description, |d, description| {
            d.child(
                div()
                    .w_full()
                    .min_w(px(0.))
                    .whitespace_normal()
                    .text_size(px(10.))
                    .line_height(px(15.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(description),
            )
        });

    match property {
        ElicitationPropertySchema::String(schema)
            if schema.enum_values.is_some() || schema.one_of.is_some() =>
        {
            let choices = schema
                .one_of
                .unwrap_or_default()
                .into_iter()
                .map(|option| (option.value, option.title))
                .chain(
                    schema
                        .enum_values
                        .unwrap_or_default()
                        .into_iter()
                        .map(|value| (value.clone(), value)),
                )
                .collect::<Vec<_>>();
            field = field.child(elicitation_choice_buttons(key, choices, false, app, cx));
        }
        ElicitationPropertySchema::Boolean(_) => {
            field = field.child(elicitation_choice_buttons(
                key,
                vec![("true".into(), "Yes".into()), ("false".into(), "No".into())],
                true,
                app,
                cx,
            ));
        }
        ElicitationPropertySchema::Array(schema) => {
            let choices = match schema.items {
                MultiSelectItems::String(items) => items
                    .values
                    .into_iter()
                    .map(|value| (value.clone(), value))
                    .collect(),
                MultiSelectItems::Titled(items) => items
                    .options
                    .into_iter()
                    .map(|option| (option.value, option.title))
                    .collect(),
                _ => vec![],
            };
            field = field.child(elicitation_multi_choice_buttons(key, choices, app, cx));
        }
        ElicitationPropertySchema::Other(_) => {
            field = field.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(theme::DIFF_DEL_TEXT))
                    .child("Unsupported field type"),
            );
        }
        _ => {
            if let Some(input) = app.elicitation_inputs.get(&key).cloned() {
                field = field.child(
                    div()
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .px(px(8.))
                        .py(px(6.))
                        .text_size(px(12.))
                        .child(input),
                );
            }
        }
    }
    field.into_any_element()
}

fn elicitation_choice_buttons(
    key: String,
    choices: Vec<(String, String)>,
    boolean: bool,
    app: &mut App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let selected = app.elicitation_values.get(&key).cloned();
    div()
        .flex()
        .flex_wrap()
        .gap(px(6.))
        .children(choices.into_iter().map(|(value, label)| {
            let is_selected = selected.as_ref().is_some_and(|selected| {
                selected.as_str() == Some(value.as_str())
                    || (boolean && selected.as_bool() == value.parse::<bool>().ok())
            });
            let selected_value = if boolean {
                serde_json::Value::Bool(value == "true")
            } else {
                serde_json::Value::String(value.clone())
            };
            let selected_key = key.clone();
            div()
                .id(SharedString::from(format!("elicitation-{key}-{value}")))
                .max_w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .px(px(9.))
                .py(px(5.))
                .border_1()
                .border_color(rgb(if is_selected {
                    theme::ACCENT
                } else {
                    theme::BORDER
                }))
                .when(is_selected, |d| d.bg(rgb(theme::INPUT_BG)))
                .text_size(px(11.))
                .line_height(px(16.))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                .child(label)
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.set_elicitation_value(selected_key.clone(), selected_value.clone(), cx)
                }))
        }))
}

fn elicitation_multi_choice_buttons(
    key: String,
    choices: Vec<(String, String)>,
    app: &mut App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let selected = app
        .elicitation_values
        .get(&key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    div()
        .flex()
        .flex_wrap()
        .gap(px(6.))
        .children(choices.into_iter().map(|(value, label)| {
            let is_selected = selected
                .iter()
                .any(|selected| selected.as_str() == Some(value.as_str()));
            let selected_key = key.clone();
            let selected_value = value.clone();
            div()
                .id(SharedString::from(format!("elicitation-{key}-{value}")))
                .max_w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .px(px(9.))
                .py(px(5.))
                .border_1()
                .border_color(rgb(if is_selected {
                    theme::ACCENT
                } else {
                    theme::BORDER
                }))
                .when(is_selected, |d| d.bg(rgb(theme::INPUT_BG)))
                .text_size(px(11.))
                .line_height(px(16.))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                .child(label)
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.toggle_elicitation_value(selected_key.clone(), selected_value.clone(), cx)
                }))
        }))
}

fn action_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<App>,
    action: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(10.))
        .py(px(5.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .text_size(px(11.))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER_BG)))
        .child(label)
        .on_click(cx.listener(move |app, _, _, cx| action(app, cx)))
}

fn running_indicator(cx: &mut Context<App>) -> impl IntoElement {
    div()
        .debug_selector(|| "running-indicator".into())
        .flex()
        .items_center()
        .justify_between()
        .border_t_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .pt(px(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::ACCENT)))
                .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::ACCENT)))
                .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::ACCENT)))
                .child(
                    div()
                        .ml(px(4.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Running"),
                ),
        )
        .child(
            div()
                .id("cancel-turn")
                .debug_selector(|| "cancel-turn".into())
                .px(px(8.))
                .py(px(3.))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .text_size(px(11.))
                .text_color(rgb(theme::ACCENT))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                .child("Stop")
                .on_click(cx.listener(|app, _, _, cx| app.cancel_turn(cx))),
        )
}

fn message_view(
    m: Message,
    is_streaming: bool,
    app: &mut App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    match m.role {
        Role::User => user_message(m, app, cx).into_any_element(),
        Role::Assistant => assistant_message(m, is_streaming, app, cx).into_any_element(),
    }
}

const COMMENT_PREVIEW_CHAR_LIMIT: usize = 120;

fn comment_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview = chars
        .by_ref()
        .take(COMMENT_PREVIEW_CHAR_LIMIT)
        .collect::<String>();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    preview
}

fn user_message(m: Message, app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let markdown_id = format!("user-markdown-{}", m.id);
    let comment_key = app.comment_key(&format!("msg_{}", m.id));
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::ACCENT))
                .child("you"),
        )
        .child(markdown_view(
            &m.text,
            markdown_id,
            CommentTarget {
                key: comment_key.clone(),
                label: "user message".to_string(),
                scroll_handle: app.thread_scroll.clone(),
                focus_handle: app.selection_focus.clone(),
            },
        ))
        .when_some(app.comments.get(&comment_key), |d, comments| {
            d.child(comments_list(comments))
        })
        .when_some(app.comment_drafts.get(&comment_key).cloned(), |d, draft| {
            d.child(comment_box(draft, comment_key, cx))
        })
        .when(!m.context.is_empty(), |d| {
            d.child(
                div()
                    .flex()
                    .gap(px(6.))
                    .flex_wrap()
                    .children(m.context.iter().map(|c| chip_view(c.clone()))),
            )
        })
        .when(!m.attached_comments.is_empty(), |d| {
            d.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .border_l_2()
                    .border_color(rgb(theme::BORDER))
                    .pl(px(8.))
                    .children(m.attached_comments.iter().map(|(label, text)| {
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_normal()
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(rgb(theme::TEXT_SECONDARY))
                            .child(format!("{label}: {}", comment_preview(text)))
                    })),
            )
        })
}

fn chip_view(text: String) -> impl IntoElement {
    div()
        .max_w_full()
        .min_w(px(0.))
        .whitespace_normal()
        .text_size(px(11.))
        .line_height(px(16.))
        .px(px(6.))
        .py(px(2.))
        .bg(rgb(theme::INPUT_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .rounded(px(5.))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .child(text)
}

fn assistant_message(
    m: Message,
    is_streaming: bool,
    app: &mut App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let msg_id = m.id.clone();
    let comment_key = app.comment_key(&format!("msg_{}", m.id));
    let steps_expanded = app.expanded_steps.contains(&msg_id);
    let toggle_steps_id = msg_id.clone();
    let toggle_comment_key = comment_key.clone();

    let mut col = div()
        .debug_selector(|| format!("assistant-card-{msg_id}"))
        .w_full()
        .min_w(px(0.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::PANEL_BG))
        .rounded(px(8.))
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child("assistant"),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("comment-toggle-{msg_id}")))
                        .px(px(4.))
                        .py(px(2.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_SECONDARY)))
                        .child("✎")
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.open_comment_box(
                                toggle_comment_key.clone(),
                                "assistant reply".to_string(),
                                cx,
                            )
                        })),
                ),
        );

    if let Some(steps) = &m.steps {
        col = col.child(steps_view(steps, steps_expanded, toggle_steps_id, cx));
    }
    if let Some(diff) = &m.diff {
        let key_prefix = msg_id.clone();
        let file = diff.file.clone();
        col = col.child(diff_view(
            diff,
            format!("inline-diff-{msg_id}"),
            move |idx| {
                (
                    format!("{key_prefix}_{idx}"),
                    format!("{file} line {}", idx + 1),
                )
            },
            app,
            cx,
        ));
    }
    if let Some(term) = &m.terminal {
        col = col.child(terminal_view(term, &msg_id));
    }
    if !m.text.is_empty() {
        col = col.child(markdown_view(
            &m.text,
            format!("assistant-markdown-{msg_id}"),
            CommentTarget {
                key: comment_key.clone(),
                label: "assistant reply".to_string(),
                scroll_handle: app.thread_scroll.clone(),
                focus_handle: app.selection_focus.clone(),
            },
        ));
    }
    if is_streaming {
        col = col.child(running_indicator(cx));
    }
    if let Some(cp) = &m.checkpoint_label {
        col = col.child(
            div()
                .debug_selector(|| format!("checkpoint-{msg_id}"))
                .min_h(px(25.))
                .flex()
                .items_end()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_MUTED))
                .border_t_1()
                .border_color(rgb(theme::TERMINAL_BORDER))
                .pt(px(7.))
                .pb(px(2.))
                .child(cp.clone()),
        );
    }
    if let Some(list) = app.comments.get(&comment_key).cloned()
        && !list.is_empty()
    {
        col = col.child(comments_list(&list));
    }
    if let Some(draft) = app.comment_drafts.get(&comment_key).cloned() {
        let submit_key = comment_key.clone();
        col = col.child(comment_box(draft, submit_key, cx));
    }
    col
}

fn steps_view(
    steps: &Steps,
    expanded: bool,
    msg_id: String,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let items = steps.items.clone();
    div()
        .child(
            div()
                .id(SharedString::from(format!("steps-{msg_id}")))
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(if expanded { "▾" } else { "▸" })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .child(steps.summary.clone()),
                )
                .on_click(cx.listener(move |app, _, _, cx| app.toggle_steps(&msg_id, cx))),
        )
        .when(expanded, |d| {
            d.child(
                div()
                    .mt(px(6.))
                    .pl(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .children(items.into_iter().map(|item| {
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_normal()
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(format!("· {item}"))
                    })),
            )
        })
}

fn terminal_view(term: &Terminal, message_id: &str) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("terminal-scroll-{message_id}")))
        .w_full()
        .min_w(px(0.))
        .overflow_x_scroll()
        .bg(rgb(theme::TERMINAL_BG))
        .border_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .rounded(px(6.))
        .font_family(theme::MONO_FONT_FAMILY)
        .px(px(10.))
        .py(px(8.))
        .text_size(px(12.))
        .flex()
        .flex_col()
        .child(
            div()
                .whitespace_nowrap()
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(format!("$ {}", term.cmd)),
        )
        .child(
            div()
                .mt(px(2.))
                .whitespace_nowrap()
                .text_color(rgb(theme::DIFF_ADD_TEXT))
                .child(term.output.clone()),
        )
}

fn comments_list(list: &[Comment]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .border_t_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .pt(px(6.))
        .children(list.iter().map(|c| {
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(format!("{}: {}", c.author, c.text))
        }))
}

fn comment_box(draft: CommentDraft, key: String, cx: &mut Context<App>) -> impl IntoElement {
    let reference = draft.reference_label();
    let input = draft.input;
    div()
        .id(SharedString::from(format!("comment-draft-{key}")))
        .anchor_scroll(draft.scroll_anchor)
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .whitespace_normal()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(comment_preview(&reference)),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .text_size(px(11.))
                        .px(px(6.))
                        .py(px(4.))
                        .child(input.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("add-comment-{key}")))
                        .bg(rgb(theme::SELECTION))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .px(px(8.))
                        .py(px(4.))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                        .child("Add")
                        .on_click(cx.listener(move |app, _, _, cx| {
                            // This listener is already updating App. Calling
                            // TextInput::submit here would recursively update App.
                            let text = input.update(cx, |input, _| input.take_content());
                            app.submit_comment(key.clone(), text, cx);
                        })),
                ),
        )
}

// ---------------------------------------------------------------- diff rendering (shared)

fn diff_view(
    diff: &Diff,
    scroll_id: String,
    key_fn: impl Fn(usize) -> (String, String) + 'static,
    app: &mut App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div()
        .id(SharedString::from(scroll_id))
        .w_full()
        .font_family(theme::MONO_FONT_FAMILY)
        .min_w(px(0.))
        .overflow_x_scroll()
        .border_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(10.))
                .py(px(6.))
                .bg(rgb(theme::INPUT_BG))
                .border_b_1()
                .border_color(rgb(theme::TERMINAL_BORDER))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .child(diff.file.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(diff.stat.clone()),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .px(px(10.))
                .py(px(4.))
                .child(diff.hunk_header.clone()),
        )
        .children(diff.lines.iter().enumerate().map(|(idx, line)| {
            let (key, label) = key_fn(idx);
            diff_line_view(line, key, label, app.thread_scroll.clone(), app, cx)
        }))
}

fn diff_line_view(
    line: &DiffLine,
    key: String,
    label: String,
    scroll_handle: gpui::ScrollHandle,
    app: &mut App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let (gutter, gutter_color, bg): (&str, u32, Option<u32>) = match line.kind {
        DiffLineKind::Add => ("+", theme::DIFF_ADD_TEXT, Some(theme::DIFF_ADD_BG)),
        DiffLineKind::Del => ("-", theme::DIFF_DEL_TEXT, Some(theme::DIFF_DEL_BG)),
        DiffLineKind::Ctx => (" ", theme::TEXT_MUTED, None),
    };

    let key = app.comment_key(&key);
    let comments = app.comments.get(&key).cloned().unwrap_or_default();
    let toggle_key = key.clone();
    let toggle_label = label.clone();

    let row = div()
        .flex()
        .when_some(bg, |d, bg| d.bg(rgba(bg)))
        .px(px(10.))
        .py(px(1.))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(rgb(gutter_color))
                .child(gutter),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_size(px(12.))
                .whitespace_nowrap()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .debug_selector(|| format!("diff-text-{key}"))
                .child(
                    SelectableText::new(
                        format!("diff-text-{key}"),
                        gpui::StyledText::new(line.text.clone()),
                        line.text.clone(),
                    )
                    .comment_target(CommentTarget {
                        key: key.clone(),
                        label: label.clone(),
                        scroll_handle,
                        focus_handle: app.selection_focus.clone(),
                    }),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("edit-{key}")))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .cursor_pointer()
                .child("✎")
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.open_comment_box(toggle_key.clone(), toggle_label.clone(), cx)
                })),
        );

    let mut wrapper = div().child(row);
    if !comments.is_empty() {
        wrapper = wrapper.child(
            div()
                .bg(rgb(theme::INPUT_BG))
                .pl(px(26.))
                .pr(px(10.))
                .py(px(4.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .children(comments.iter().map(|c| {
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(format!("{}: {}", c.author, c.text))
                })),
        );
    }
    if let Some(draft) = app.comment_drafts.get(&key).cloned() {
        let submit_key = key.clone();
        wrapper = wrapper.child(
            div()
                .bg(rgb(theme::INPUT_BG))
                .pl(px(26.))
                .pr(px(10.))
                .py(px(4.))
                .child(comment_box(draft, submit_key, cx)),
        );
    }
    wrapper.into_any_element()
}

// ---------------------------------------------------------------- composer

fn composer_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let pending = app.pending_comments();
    let has_chips = !app.context_chips.is_empty();
    let chips = app.context_chips.clone();
    let composer = app.composer.clone();

    div()
        .debug_selector(|| "composer".into())
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .when(has_chips, |d| {
            d.child(div().flex().gap(px(6.)).flex_wrap().mb(px(4.)).children(
                chips.into_iter().enumerate().map(|(idx, path)| {
                    div()
                        .max_w_full()
                        .min_w(px(0.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .px(px(6.))
                        .py(px(2.))
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .rounded(px(5.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .whitespace_normal()
                                .line_height(px(16.))
                                .child(path),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("remove-chip-{idx}")))
                                .cursor_pointer()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child("×")
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    app.context_chips.remove(idx);
                                    cx.notify();
                                })),
                        )
                }),
            ))
        })
        .when(!pending.is_empty(), |d| {
            d.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .mb(px(4.))
                    .children(pending.into_iter().map(|p| {
                        let key = p.key.clone();
                        let idx = p.idx;
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .px(px(6.))
                            .py(px(2.))
                            .bg(rgba(theme::PENDING_CHIP_BG))
                            .border_1()
                            .border_color(rgba(theme::PENDING_CHIP_BORDER))
                            .rounded(px(5.))
                            .text_color(rgb(theme::ACCENT))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .whitespace_normal()
                                    .line_height(px(16.))
                                    .child(format!("{}: {}", p.label, comment_preview(&p.text))),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-pending-{key}-{idx}")))
                                    .flex_shrink_0()
                                    .cursor_pointer()
                                    .child("×")
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        app.remove_pending_comment(&key, idx, cx)
                                    })),
                            )
                    })),
            )
        })
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::ACCENT))
                .child("you"),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(composer),
        )
}

// ---------------------------------------------------------------- right panels

fn right_panels(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_diff = app.active_diff_file.clone();
    let files = app.changed_files().to_vec();

    div()
        .flex()
        .flex_shrink_0()
        .when_some(active_diff.clone(), |d, path| {
            let Some(diff) = app.file_diffs.get(&path).cloned() else {
                return d;
            };
            d.child(
                div()
                    .w(px(360.))
                    .flex_shrink_0()
                    .bg(rgb(theme::BG))
                    .border_l_1()
                    .border_color(rgb(theme::BORDER))
                    .id("active-diff-scroll")
                    .track_scroll(&app.diff_scroll)
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .px(px(10.))
                            .py(px(8.))
                            .bg(rgb(theme::INPUT_BG))
                            .border_b_1()
                            .border_color(rgb(theme::TERMINAL_BORDER))
                            .child(div().text_size(px(12.)).child(diff.file.clone()))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .child(diff.stat.clone()),
                                    )
                                    .child(
                                        div()
                                            .id("close-diff")
                                            .cursor_pointer()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .child("×")
                                            .on_click(cx.listener(|app, _, _, cx| {
                                                app.active_diff_file = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .px(px(10.))
                            .py(px(4.))
                            .child(diff.hunk_header.clone()),
                    )
                    .children(diff.lines.iter().enumerate().map(|(idx, line)| {
                        let key = format!("f_{path}_{idx}");
                        let label = format!("{path} line {}", idx + 1);
                        diff_line_view(line, key, label, app.diff_scroll.clone(), app, cx)
                    })),
            )
        })
        .child(
            div()
                .w(px(200.))
                .flex_shrink_0()
                .overflow_hidden()
                .bg(rgb(theme::PANEL_BG))
                .border_l_1()
                .border_color(rgb(theme::BORDER))
                .id("changed-files-scroll")
                .overflow_y_scroll()
                .py(px(10.))
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .px(px(12.))
                        .pb(px(8.))
                        .child("Changed files"),
                )
                .when(files.is_empty(), |d| {
                    d.child(
                        div()
                            .px(px(12.))
                            .py(px(6.))
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("Working tree clean"),
                    )
                })
                .children(files.iter().map(|f| {
                    let path = f.path.clone();
                    let supports_text_diff = f.supports_text_diff;
                    let is_active = active_diff.as_deref() == Some(f.path.as_str());
                    let dot = if f.status.as_deref() == Some("added") {
                        theme::DIFF_ADD_TEXT
                    } else {
                        theme::ACCENT
                    };
                    let label = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
                    let click_path = path.clone();
                    let attach_path = path.clone();
                    div()
                        .id(SharedString::from(format!("changed-file-{path}")))
                        .flex()
                        .items_center()
                        .w_full()
                        .overflow_hidden()
                        .gap(px(6.))
                        .px(px(12.))
                        .py(px(4.))
                        .when(supports_text_diff, |d| d.cursor_pointer())
                        .when(is_active, |d| d.bg(rgb(theme::INPUT_BG)))
                        .when(supports_text_diff, |d| {
                            d.hover(|s| s.bg(rgb(theme::HOVER_BG)))
                        })
                        .child(div().w(px(6.)).h(px(6.)).flex_shrink_0().bg(rgb(dot)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_size(px(12.))
                                .text_color(rgb(if supports_text_diff {
                                    theme::TEXT_PRIMARY
                                } else {
                                    theme::TEXT_MUTED
                                }))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(label),
                        )
                        .when(!supports_text_diff, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(9.))
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child("binary"),
                            )
                        })
                        .child(
                            div()
                                .id(SharedString::from(format!("attach-file-{path}")))
                                .ml_auto()
                                .flex_shrink_0()
                                .px(px(2.))
                                .text_size(px(11.))
                                .text_color(rgb(theme::ACCENT))
                                .cursor_pointer()
                                .child("+")
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    cx.stop_propagation();
                                    app.add_context_file(&attach_path, cx);
                                })),
                        )
                        .when(supports_text_diff, |d| {
                            d.on_click(cx.listener(move |app, _, _, cx| {
                                app.toggle_diff_file(&click_path, cx)
                            }))
                        })
                })),
        )
}

// ---------------------------------------------------------------- footer

fn footer_bar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
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

// ---------------------------------------------------------------- checkpoints & toast

fn checkpoints_overlay(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let mut checkpoints = app.checkpoints_for();
    checkpoints.reverse();

    div()
        .absolute()
        .inset_0()
        .child(
            div()
                .id("checkpoints-backdrop")
                .absolute()
                .inset_0()
                .bg(rgba(theme::OVERLAY))
                .on_click(cx.listener(|app, _, _, cx| app.toggle_checkpoints(cx))),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(300.))
                .bg(rgb(theme::PANEL_BG))
                .border_l_1()
                .border_color(rgb(theme::BORDER))
                .id("checkpoints-scroll")
                .overflow_y_scroll()
                .p(px(14.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .mb(px(12.))
                        .child("Checkpoint history"),
                )
                .children(checkpoints.into_iter().map(|(label, time)| {
                    let restore_label = label.clone();
                    div()
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .p(px(10.))
                        .mb(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(time),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("restore-{restore_label}")))
                                .mt(px(4.))
                                .px(px(8.))
                                .py(px(4.))
                                .border_1()
                                .border_color(rgb(theme::BORDER))
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_SECONDARY))
                                .cursor_pointer()
                                .child("Restore")
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    app.restore_checkpoint(&restore_label, cx)
                                })),
                        )
                })),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_comment_previews_are_unchanged() {
        assert_eq!(comment_preview("Keep this branch"), "Keep this branch");
        assert_eq!(
            comment_preview(&"a".repeat(COMMENT_PREVIEW_CHAR_LIMIT)),
            "a".repeat(COMMENT_PREVIEW_CHAR_LIMIT)
        );
    }

    #[test]
    fn long_comment_previews_are_capped_without_splitting_unicode() {
        let preview = comment_preview(&"é".repeat(COMMENT_PREVIEW_CHAR_LIMIT + 1));

        assert_eq!(preview.chars().count(), COMMENT_PREVIEW_CHAR_LIMIT);
        assert_eq!(
            preview,
            format!("{}…", "é".repeat(COMMENT_PREVIEW_CHAR_LIMIT - 1))
        );
    }
}
