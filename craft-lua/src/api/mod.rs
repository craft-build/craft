mod async_api;
pub(crate) mod buf;
pub(crate) mod command;
pub(crate) mod ctx;
pub(crate) mod env;
pub(crate) mod embed;
pub(crate) mod fn_api;
pub(crate) mod fs;
pub(crate) mod json;
pub(crate) mod log;
pub(crate) mod net;
pub(crate) mod setup;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod treesitter;
pub(crate) mod ui;
pub(crate) mod uv;
pub(crate) mod win;
pub(crate) mod yaml;

use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table, Value};
use serde_json::Value as JsonValue;

use crate::api::command::UiAction;
use crate::api::tool::PendingTools;
use crate::plugin_permissions::PluginPermissions;

pub(crate) fn create_craft_global(
    lua: &Lua,
    pending: PendingTools,
    plugin: Arc<str>,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    permissions: &PluginPermissions,
    embed_tx: Option<crate::api::embed::EmbedChannel>,
) -> LuaResult<Table> {
    let craft = lua.create_table()?;

    craft.set(
        "api",
        tool::create_api_table(lua, pending, Arc::clone(&plugin))?,
    )?;
    craft.set("env", env::create_env_table(lua, permissions)?)?;
    craft.set("fs", fs::create_fs_table(lua, permissions)?)?;
    craft.set("log", log::create_log_table(lua, plugin)?)?;
    craft.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    craft.set("uv", uv::create_uv_table(lua, permissions)?)?;
    craft.set("json", json::create_json_table(lua)?)?;
    craft.set("yaml", yaml::create_yaml_table(lua)?)?;
    craft.set("net", net::create_net_table(lua, permissions)?)?;
    craft.set("text", text::create_text_table(lua)?)?;
    craft.set("ui", ui::create_ui_table(lua, ui_action_tx)?)?;
    craft.set("fn", fn_api::create_fn_table(lua, permissions)?)?;
    craft.set("async", async_api::create_async_table(lua)?)?;

    #[cfg(feature = "onnx")]
    if let Some(tx) = embed_tx {
        craft.set("embed", crate::api::embed::create_embed_table(lua, tx)?)?;
    }

    #[cfg(not(feature = "onnx"))]
    let _ = embed_tx;

    Ok(craft)
}

pub(crate) fn err_pair(lua: &Lua, e: impl std::fmt::Display) -> LuaResult<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
}

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
