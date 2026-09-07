use mlua::{Lua, LuaSerdeExt, Result as LuaResult, Value};
use serde_json::Value as JsonValue;

/// How many nulls an array encoding may invent for keys the table does not
/// hold. A JSON null arrives as an absent Lua key, so a round trip has to be
/// able to put a few back, but nothing bounds the largest key a table carries.
/// Past this the table is a sparse map, not an array, and the object encoding
/// keeps every key while allocating per entry.
const MAX_ARRAY_HOLES: usize = 4096;

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

/// [`json_to_lua`]'s symmetric counterpart, guided by the JSON that built
/// `val`: whatever the Lua side produced wins, and a null the Lua side left
/// absent is restored from `template`.
///
/// A JSON null arrives as a Lua nil, and assigning nil to a table key is a
/// no-op, so a layer never sees a null and cannot hand one back. They have to
/// be carried across instead. Guiding stops wherever the two sides stop being
/// the same container kind, so a layer that swapped a subtree for a scalar owns
/// it outright. The price: a layer cannot delete a key whose value is null, it
/// comes back, while deleting a key holding a real value works.
pub(crate) fn lua_to_json_within(
    lua: &Lua,
    val: &Value,
    template: &JsonValue,
) -> LuaResult<JsonValue> {
    within_template(lua, val, Some(template))
}

fn within_template(lua: &Lua, val: &Value, template: Option<&JsonValue>) -> LuaResult<JsonValue> {
    Ok(match val {
        Value::Nil => JsonValue::Null,
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(i) => (*i).into(),
        Value::Number(f) => (*f).into(),
        Value::String(s) => JsonValue::String(s.to_str()?.to_owned()),
        Value::Table(tbl) => {
            // An untagged table serializes as a JSON array only when every key
            // is a positive integer and they are dense from 1 (count == max),
            // so no string key silently disappears and sparse tables like
            // `{ [1] = "a", [3] = "c" }` deterministically become objects
            // (`lua_rawlen` borders are implementation-defined for those).
            // The array metatable outranks that, since `json_to_lua` writes it
            // on every JSON array and a null element leaves an absent key:
            // density cannot be re-derived, and the gaps are holes to fill with
            // null. Keys an array cannot express still fall back to the object
            // encoding, which keeps all of them.
            let mut has_non_int = false;
            let mut int_count = 0usize;
            let mut max_int = 0i64;
            let mut entries: Vec<(Value, Value)> = Vec::new();
            for pair in tbl.pairs::<Value, Value>() {
                let (k, v) = pair?;
                match k {
                    Value::Integer(i) if i > 0 => {
                        int_count += 1;
                        max_int = max_int.max(i);
                    }
                    Value::String(_) => {}
                    _ => has_non_int = true,
                }
                entries.push((k, v));
            }

            let tagged = tbl.metatable().as_ref() == Some(&lua.array_metatable());
            // Slots the entries do not pay for. The array encoding has to
            // materialize every one of them, and the largest key alone decides
            // how many, so this is the only thing standing between
            // `decoded[os.time()] = 1` and an allocation the size of a clock.
            let holes = max_int.saturating_sub(int_count as i64);
            let is_array = !has_non_int
                && if tagged {
                    holes <= MAX_ARRAY_HOLES as i64
                } else {
                    int_count > 0 && holes == 0
                };
            if is_array {
                let template = template.and_then(JsonValue::as_array);
                // A trailing null never moved `max_int`, so only the template
                // knows the array ran on past the last key the Lua side kept.
                // A trailing non-null there was a real value the layer dropped.
                let mut len = max_int as usize;
                while template.is_some_and(|t| t.get(len).is_some_and(JsonValue::is_null)) {
                    len += 1;
                }
                let mut arr = vec![JsonValue::Null; len];
                for (k, v) in entries {
                    let Value::Integer(i) = k else { unreachable!() };
                    let idx = i as usize - 1;
                    arr[idx] = within_template(lua, &v, template.and_then(|t| t.get(idx)))?;
                }
                return Ok(JsonValue::Array(arr));
            }

            let template = template.and_then(JsonValue::as_object);
            let mut map = serde_json::Map::new();
            for (k, v) in entries {
                let key = match k {
                    Value::String(s) => s.to_str()?.to_owned(),
                    Value::Integer(i) => i.to_string(),
                    Value::Boolean(b) => b.to_string(),
                    _ => continue,
                };
                let child = template.and_then(|t| t.get(&key));
                map.insert(key.clone(), within_template(lua, &v, child)?);
            }
            for (key, _) in template.into_iter().flatten().filter(|(_, v)| v.is_null()) {
                map.entry(key.as_str()).or_insert(JsonValue::Null);
            }
            JsonValue::Object(map)
        }
        _ => JsonValue::Null,
    })
}

#[cfg(test)]
mod tests {
    use mlua::Lua;
    use serde_json::Value as JsonValue;
    use test_case::test_case;

    use super::{MAX_ARRAY_HOLES, json_to_lua, lua_to_json_within};

    /// The name `LAYER_CASES` snippets edit through, standing in for the
    /// `value` argument a real hook layer is handed.
    const LAYER_GLOBAL: &str = "value";

    /// The array encoding materializes every hole, and the largest key alone
    /// says how many, so a table used as a sparse map has to leave the encoding
    /// rather than allocate up to its key. Nothing is lost: the object keeps
    /// every entry.
    #[test_case(MAX_ARRAY_HOLES,     true  ; "holes_the_entries_can_carry")]
    #[test_case(MAX_ARRAY_HOLES + 1, false ; "a_key_too_far_falls_back_to_object")]
    fn lua_to_json_array_encoding_bounds_the_holes_it_invents(holes: usize, array: bool) {
        let lua = Lua::new();
        let template = serde_json::json!([1]);
        let value = json_to_lua(&lua, &template).unwrap();
        let key = holes + 2;
        value.as_table().unwrap().set(key, 2).unwrap();

        let result = lua_to_json_within(&lua, &value, &template).unwrap();
        assert_eq!(result.is_array(), array);
        assert_eq!(
            result.get(key - 1).or_else(|| result.get(key.to_string())),
            Some(&serde_json::json!(2)),
            "either encoding keeps the entry"
        );
    }

    const ROUNDTRIP_CASES: &[&str] = &[
        "null",
        "true",
        "7",
        r#""x""#,
        "[]",
        r#"{}"#,
        r#"{"a":1,"b":[true,"x"]}"#,
        "[1,null,3]",
        "[[],{}]",
        r#"{"a":[1,2,null,3]}"#,
    ];

    #[test_case(0 ; "null")]
    #[test_case(1 ; "bool")]
    #[test_case(2 ; "int")]
    #[test_case(3 ; "string")]
    #[test_case(4 ; "empty_array")]
    #[test_case(5 ; "empty_object")]
    #[test_case(6 ; "nested_object")]
    #[test_case(7 ; "array_with_interior_null")]
    #[test_case(8 ; "nested_empty_containers")]
    #[test_case(9 ; "object_holding_array_with_null")]
    fn lua_to_json_within_roundtrips(idx: usize) {
        let original: JsonValue = serde_json::from_str(ROUNDTRIP_CASES[idx]).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &original).unwrap();
        let back = lua_to_json_within(&lua, &lua_val, &original).unwrap();
        assert_eq!(back, original);
    }

    /// A layer sees no null at all, so a pass-through of any input has to come
    /// back exactly as it went in.
    const TEMPLATE_IDENTITY_CASES: &[&str] = &[
        r#"{"a":null,"b":1}"#,
        "[1,null]",
        "[null]",
        r#"{"a":[1,null]}"#,
        "[1,null,3]",
        "{}",
        "[]",
        r#"{"a":{"b":null,"c":[null,{"d":null},null]},"e":[[null],null],"f":null}"#,
    ];

    #[test_case(0 ; "object_with_null")]
    #[test_case(1 ; "array_with_trailing_null")]
    #[test_case(2 ; "array_of_one_null")]
    #[test_case(3 ; "nested_array_with_trailing_null")]
    #[test_case(4 ; "array_with_interior_null")]
    #[test_case(5 ; "empty_object")]
    #[test_case(6 ; "empty_array")]
    #[test_case(7 ; "deeply_nested_mix")]
    fn lua_to_json_within_template_roundtrips_nulls(idx: usize) {
        let original: JsonValue = serde_json::from_str(TEMPLATE_IDENTITY_CASES[idx]).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &original).unwrap();

        let back = lua_to_json_within(&lua, &lua_val, &original).unwrap();
        assert_eq!(back, original);
    }

    /// `(template, what the layer does to it, what the caller must get back)`.
    const LAYER_CASES: &[(&str, &str, &str)] = &[
        ("[1,2,3]", "value[3] = nil", "[1,2]"),
        (
            r#"{"a":null,"b":1}"#,
            r#"value.a = "x""#,
            r#"{"a":"x","b":1}"#,
        ),
        (
            r#"{"a":{"b":null},"c":2}"#,
            "value.a = 1",
            r#"{"a":1,"c":2}"#,
        ),
        (r#"{"a":null,"b":1}"#, "value.b = nil", r#"{"a":null}"#),
    ];

    #[test_case(0 ; "truncating_a_real_value_shortens_the_array")]
    #[test_case(1 ; "a_null_replaced_by_a_value_keeps_the_value")]
    #[test_case(2 ; "an_object_replaced_by_a_scalar_ends_the_template")]
    #[test_case(3 ; "deleting_a_non_null_key_still_deletes_it")]
    fn lua_to_json_within_template_lets_the_layer_win(idx: usize) {
        let (template, edit, expected) = LAYER_CASES[idx];
        let template: JsonValue = serde_json::from_str(template).unwrap();
        let lua = Lua::new();
        let lua_val = json_to_lua(&lua, &template).unwrap();
        lua.globals().set(LAYER_GLOBAL, lua_val.clone()).unwrap();
        lua.load(edit).exec().unwrap();

        let result = lua_to_json_within(&lua, &lua_val, &template).unwrap();
        assert_eq!(result, serde_json::from_str::<JsonValue>(expected).unwrap());
    }
}
