use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Value};
use serde_json::Value as JsonValue;

pub(crate) fn err_pair(lua: &Lua, e: impl std::fmt::Display) -> LuaResult<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
}

/// Convert a [`serde_json::Value`] into a Lua value by hand.
///
/// mlua's `to_value` looks like the easy path, but monty turns on serde_json's
/// `arbitrary_precision` feature for the whole workspace. With it, a number
/// serializes as a little tagged struct instead of a plain scalar, so plugins
/// end up with a Lua table where they asked for a number. We walk the tree
/// ourselves to keep numbers as numbers.
pub(crate) fn json_to_lua(lua: &Lua, value: &JsonValue) -> LuaResult<Value> {
    Ok(match value {
        JsonValue::Null => Value::Nil,
        JsonValue::Bool(b) => Value::Boolean(*b),
        JsonValue::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Value::Integer(i),
            (_, Some(f)) => Value::Number(f),
            _ => Value::Nil,
        },
        JsonValue::String(s) => Value::String(lua.create_string(s)?),
        JsonValue::Array(items) => {
            let table = lua.create_table_with_capacity(items.len(), 0)?;
            for (idx, item) in items.iter().enumerate() {
                table.set(idx + 1, json_to_lua(lua, item)?)?;
            }
            table.set_metatable(Some(lua.array_metatable()))?;
            Value::Table(table)
        }
        JsonValue::Object(map) => {
            let table = lua.create_table_with_capacity(0, map.len())?;
            for (key, val) in map {
                table.set(key.as_str(), json_to_lua(lua, val)?)?;
            }
            Value::Table(table)
        }
    })
}
