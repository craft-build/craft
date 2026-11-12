use monty_types::{MontyObject, ObjectRef};
use serde_json::{Number, Value};

/// Converts a Monty value into JSON, using its representation when no JSON type fits.
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

/// Converts JSON into a Monty value.
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn assert_roundtrip(value: Value) {
        let monty = from_json(&value);
        assert_eq!(to_json(monty.as_ref()), value);
    }

    #[test]
    fn scalars_roundtrip() {
        for value in [
            Value::Null,
            json!(true),
            json!(false),
            json!(0),
            json!(-42),
            json!(1.25),
            json!("hello\n世界"),
        ] {
            assert_roundtrip(value);
        }
    }

    #[test]
    fn nested_arrays_and_objects_roundtrip() {
        assert_roundtrip(json!({
            "array": [null, true, -7, 3.5, "text"],
            "object": {"empty_array": [], "empty_object": {}},
            "unicode": "🙂"
        }));
    }

    #[test]
    fn non_finite_monty_floats_become_null() {
        assert_eq!(to_json(MontyObject::float(f64::NAN).as_ref()), Value::Null);
        assert_eq!(
            to_json(MontyObject::float(f64::INFINITY).as_ref()),
            Value::Null
        );
    }
}
