use agent_client_protocol::schema::v1::{
    ElicitationMode, ElicitationPropertySchema, MultiSelectItems,
};
use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb};

use crate::app::App;
use crate::theme;

use super::permission::action_button;

pub(super) fn elicitation_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
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
