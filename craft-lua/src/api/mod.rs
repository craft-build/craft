mod async_api;
pub(crate) mod buf;
pub(crate) mod command;
pub(crate) mod ctx;
pub(crate) mod env;
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

use crate::api::command::UiAction;
use crate::api::tool::PendingTools;

pub(crate) fn create_craft_global(
    lua: &Lua,
    pending: PendingTools,
    plugin: Arc<str>,
    ui_action_tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Table> {
    let craft = lua.create_table()?;

    craft.set(
        "api",
        tool::create_api_table(lua, pending, Arc::clone(&plugin))?,
    )?;
    craft.set("env", env::create_env_table(lua)?)?;
    craft.set("fs", fs::create_fs_table(lua)?)?;
    craft.set("log", log::create_log_table(lua, plugin)?)?;
    craft.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    craft.set("uv", uv::create_uv_table(lua)?)?;
    craft.set("json", json::create_json_table(lua)?)?;
    craft.set("yaml", yaml::create_yaml_table(lua)?)?;
    craft.set("net", net::create_net_table(lua)?)?;
    craft.set("text", text::create_text_table(lua)?)?;
    craft.set("ui", ui::create_ui_table(lua, ui_action_tx)?)?;
    craft.set("fn", fn_api::create_fn_table(lua)?)?;
    craft.set("async", async_api::create_async_table(lua)?)?;

    Ok(craft)
}

pub(crate) fn err_pair(lua: &Lua, e: impl std::fmt::Display) -> LuaResult<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?)))
}
