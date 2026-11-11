use monty_types::{MontyObject, ObjectRef};
use serde_json::{Map, Number, Value};

pub fn to_json(value: ObjectRef<'_>) -> Value {
    if let Some(value) = value.as_bool() {
        return Value::Bool(value);
    }
    if let Some(value) = value.as_int() {
        return Value::from(value);
    }
    if let Some(value) = value.as_str() {
        return Value::String(value.to_owned());
    }
    if let Some(value) = value.as_float() {
        return Number::from_f64(value).map_or(Value::Null, Value::Number);
    }
    if value.type_name() == "NoneType" {
        return Value::Null;
    }
    if let Some(items) = value.items() {
        return Value::Array(items.iter().map(|item| to_json(*item)).collect());
    }
    if let Some(pairs) = value.pairs() {
        return Value::Object(
            pairs
                .into_iter()
                .map(|(key, value)| {
                    let key = key.as_str().map_or_else(|| key.py_repr(), str::to_owned);
                    (key, to_json(value))
                })
                .collect(),
        );
    }
    Value::String(value.py_repr())
}

pub fn from_json(value: &Value) -> MontyObject {
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(value) => MontyObject::bool(*value),
        Value::Number(value) => value.as_i64().map_or_else(
            || MontyObject::float(value.as_f64().expect("JSON number")),
            MontyObject::int,
        ),
        Value::String(value) => MontyObject::string(value),
        Value::Array(values) => MontyObject::list(values.iter().map(from_json)),
        Value::Object(values) => MontyObject::dict(
            values
                .iter()
                .map(|(key, value)| (MontyObject::string(key), from_json(value))),
        ),
    }
}

pub fn object(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Value {
    Value::Object(Map::from_iter(
        entries.into_iter().map(|(key, value)| (key.into(), value)),
    ))
}
