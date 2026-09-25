use agent_client_protocol::schema::v1::{
    ElicitationContentValue, ElicitationMode, ElicitationPropertySchema,
};
use gpui::{Context, prelude::*};

use crate::acp::{ElicitationDecision, PendingElicitation};
use crate::text_input::TextInput;

use super::App;

/// The input shape of an elicitation form field, with its default value.
/// Shared by the form preparation and accept paths so both agree on which
/// strings are free text versus enum/choice values.
pub(crate) enum FieldShape<'a> {
    /// A plain string with no enum/one-of constraint — rendered as text input.
    Text {
        default: &'a str,
    },
    /// A string constrained by enum/one-of — chosen from preset values.
    Choice {
        default: Option<&'a str>,
    },
    Number {
        default: Option<f64>,
    },
    Integer {
        default: Option<i64>,
    },
    Boolean {
        default: Option<bool>,
    },
    Array {
        default: Option<&'a [String]>,
    },
    Unsupported,
}

pub(crate) fn field_shape(property: &ElicitationPropertySchema) -> FieldShape<'_> {
    match property {
        ElicitationPropertySchema::String(schema) => {
            if schema.enum_values.is_none() && schema.one_of.is_none() {
                FieldShape::Text {
                    default: schema.default.as_deref().unwrap_or_default(),
                }
            } else {
                FieldShape::Choice {
                    default: schema.default.as_deref(),
                }
            }
        }
        ElicitationPropertySchema::Number(schema) => FieldShape::Number {
            default: schema.default,
        },
        ElicitationPropertySchema::Integer(schema) => FieldShape::Integer {
            default: schema.default,
        },
        ElicitationPropertySchema::Boolean(schema) => FieldShape::Boolean {
            default: schema.default,
        },
        ElicitationPropertySchema::Array(schema) => FieldShape::Array {
            default: schema.default.as_deref(),
        },
        _ => FieldShape::Unsupported,
    }
}

impl App {
    /// Drop all elicitation form state. Used when a form is answered,
    /// declined, cancelled, or the session is disconnected.
    pub(crate) fn clear_elicitation(&mut self) {
        self.elicitation_inputs.clear();
        self.elicitation_values.clear();
    }

    pub fn decline_elicitation(&mut self, cx: &mut Context<Self>) {
        if let Some(elicitation) = self.pending_elicitation.take() {
            elicitation.respond(ElicitationDecision::Decline);
        }
        self.clear_elicitation();
        cx.notify();
    }

    pub(crate) fn prepare_elicitation_form(
        &mut self,
        elicitation: &PendingElicitation,
        cx: &mut Context<Self>,
    ) {
        self.clear_elicitation();
        let ElicitationMode::Form(form) = &elicitation.request.mode else {
            return;
        };

        for (key, property) in &form.requested_schema.properties {
            match field_shape(property) {
                FieldShape::Text { default } => {
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "Enter a response");
                        input.set_content(default.to_string());
                        input
                    });
                    self.elicitation_inputs.insert(key.clone(), input);
                }
                FieldShape::Choice {
                    default: Some(default),
                } => {
                    self.elicitation_values
                        .insert(key.clone(), serde_json::Value::String(default.to_string()));
                }
                FieldShape::Number { default } => {
                    let default = default.map(|value| value.to_string()).unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "Enter a number");
                        input.set_content(default);
                        input
                    });
                    self.elicitation_inputs.insert(key.clone(), input);
                }
                FieldShape::Integer { default } => {
                    let default = default.map(|value| value.to_string()).unwrap_or_default();
                    let input = cx.new(|cx| {
                        let mut input = TextInput::new(cx, "Enter a whole number");
                        input.set_content(default);
                        input
                    });
                    self.elicitation_inputs.insert(key.clone(), input);
                }
                FieldShape::Boolean {
                    default: Some(default),
                } => {
                    self.elicitation_values
                        .insert(key.clone(), serde_json::Value::Bool(default));
                }
                FieldShape::Array {
                    default: Some(default),
                } => {
                    self.elicitation_values.insert(
                        key.clone(),
                        serde_json::Value::Array(
                            default
                                .iter()
                                .cloned()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
                _ => {}
            }
        }
    }

    pub fn set_elicitation_value(
        &mut self,
        key: String,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.elicitation_values.insert(key, value);
        cx.notify();
    }

    pub fn toggle_elicitation_value(&mut self, key: String, value: String, cx: &mut Context<Self>) {
        let selected = self
            .elicitation_values
            .entry(key)
            .or_insert_with(|| serde_json::Value::Array(vec![]));
        let serde_json::Value::Array(values) = selected else {
            return;
        };
        if let Some(index) = values
            .iter()
            .position(|selected| selected.as_str() == Some(value.as_str()))
        {
            values.remove(index);
        } else {
            values.push(serde_json::Value::String(value));
        }
        cx.notify();
    }

    pub fn accept_elicitation(&mut self, cx: &mut Context<Self>) {
        let Some(elicitation) = self.pending_elicitation.as_ref() else {
            return;
        };
        let ElicitationMode::Form(form) = &elicitation.request.mode else {
            self.toast = Some("This elicitation mode is not supported".into());
            cx.notify();
            return;
        };
        let schema = form.requested_schema.clone();
        let required = schema.required.unwrap_or_default();
        let mut content = std::collections::BTreeMap::new();

        for (key, property) in schema.properties {
            let value = match field_shape(&property) {
                FieldShape::Text { .. } => {
                    let value = self
                        .elicitation_inputs
                        .get(&key)
                        .map(|input| input.read(cx).content.trim().to_string())
                        .unwrap_or_default();
                    if value.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::String(value))
                    }
                }
                FieldShape::Number { .. } => {
                    let raw = self
                        .elicitation_inputs
                        .get(&key)
                        .map(|input| input.read(cx).content.trim().to_string())
                        .unwrap_or_default();
                    if raw.is_empty() {
                        None
                    } else {
                        match raw.parse::<f64>() {
                            Ok(value) => {
                                serde_json::Number::from_f64(value).map(serde_json::Value::Number)
                            }
                            Err(_) => {
                                self.toast = Some(format!("{key} must be a number"));
                                cx.notify();
                                return;
                            }
                        }
                    }
                }
                FieldShape::Integer { .. } => {
                    let raw = self
                        .elicitation_inputs
                        .get(&key)
                        .map(|input| input.read(cx).content.trim().to_string())
                        .unwrap_or_default();
                    if raw.is_empty() {
                        None
                    } else {
                        match raw.parse::<i64>() {
                            Ok(value) => Some(serde_json::Value::Number(value.into())),
                            Err(_) => {
                                self.toast = Some(format!("{key} must be a whole number"));
                                cx.notify();
                                return;
                            }
                        }
                    }
                }
                FieldShape::Unsupported => {
                    self.toast = Some(format!("{key} uses an unsupported field type"));
                    cx.notify();
                    return;
                }
                _ => self.elicitation_values.get(&key).cloned(),
            };

            let missing = value.as_ref().is_none_or(|value| {
                value.as_str().is_some_and(str::is_empty)
                    || value.as_array().is_some_and(Vec::is_empty)
            });
            if required.contains(&key) && missing {
                self.toast = Some(format!("{key} is required"));
                cx.notify();
                return;
            }
            if let Some(value) = value {
                let Some(value) = json_elicitation_value(value) else {
                    self.toast = Some(format!("{key} has an unsupported value"));
                    cx.notify();
                    return;
                };
                content.insert(key, value);
            }
        }

        if let Some(elicitation) = self.pending_elicitation.take() {
            elicitation.respond(ElicitationDecision::Accept(content));
        }
        self.clear_elicitation();
        cx.notify();
    }
}

pub(crate) fn json_elicitation_value(
    value: serde_json::Value,
) -> Option<agent_client_protocol::schema::v1::ElicitationContentValue> {
    match value {
        serde_json::Value::String(value) => Some(ElicitationContentValue::String(value)),
        serde_json::Value::Bool(value) => Some(ElicitationContentValue::Boolean(value)),
        serde_json::Value::Number(value) if value.is_i64() => {
            Some(ElicitationContentValue::Integer(value.as_i64()?))
        }
        serde_json::Value::Number(value) => Some(ElicitationContentValue::Number(value.as_f64()?)),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .map(ElicitationContentValue::StringArray),
        _ => None,
    }
}
