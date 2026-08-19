mod r#async;
pub(crate) mod autocmd;
pub(crate) mod base64;
pub(crate) mod env;
pub(crate) mod r#fn;
pub(crate) mod fs;
pub(crate) mod hooks;
pub(crate) mod image;
pub(crate) mod json;
pub(crate) mod keymap;
pub(crate) mod log;
pub(crate) mod net;
pub(crate) mod options;
pub(crate) mod session;
pub(crate) mod slot;
pub(crate) mod split;
pub(crate) mod text;
pub(crate) mod tool;
pub(crate) mod treesitter;
pub(crate) mod ui;
pub(crate) mod util;
pub(crate) mod uv;
pub(crate) mod yaml;

pub(crate) mod embed;

use std::sync::Arc;

use mlua::{Lua, Result as LuaResult, Table};

use crate::api::options::PluginOpts;
use crate::api::tool::{PendingRules, PendingTools};
use crate::api::util::command::UiAction;
use crate::plugin_permissions::PluginPermissions;

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_craft_global(
    lua: &Lua,
    pending: PendingTools,
    pending_rules: PendingRules,
    plugin: Arc<str>,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    permissions: &PluginPermissions,
    opts: PluginOpts,
    embed_tx: Option<crate::api::embed::EmbedChannel>,
) -> LuaResult<Table> {
    let craft = lua.create_table()?;

    let api = tool::create_api_table(
        lua,
        pending,
        pending_rules,
        Arc::clone(&plugin),
        opts,
        ui_action_tx.clone(),
    )?;
    autocmd::add_autocmd_methods(&api, lua, Arc::clone(&plugin))?;
    slot::add_slot_methods(&api, lua, Arc::clone(&plugin))?;
    craft.set("api", api)?;
    craft.set("env", env::create_env_table(lua, permissions)?)?;
    craft.set("fs", fs::create_fs_table(lua, permissions)?)?;
    craft.set("log", log::create_log_table(lua, Arc::clone(&plugin))?)?;
    craft.set("treesitter", treesitter::create_treesitter_table(lua)?)?;
    craft.set("uv", uv::create_uv_table(lua, permissions)?)?;
    craft.set("base64", base64::create_base64_table(lua)?)?;
    craft.set("image", image::create_image_table(lua)?)?;
    craft.set("json", json::create_json_table(lua)?)?;
    craft.set("yaml", yaml::create_yaml_table(lua)?)?;
    craft.set("net", net::create_net_table(lua, permissions)?)?;
    craft.set("text", text::create_text_table(lua)?)?;
    craft.set(
        "session",
        session::create_session_table(lua, ui_action_tx.clone())?,
    )?;
    craft.set(
        "ui",
        ui::create_ui_table(lua, ui_action_tx.clone(), Arc::clone(&plugin))?,
    )?;
    craft.set(
        "fn",
        r#fn::create_fn_table(lua, Arc::clone(&plugin), permissions, ui_action_tx)?,
    )?;
    split::register(&craft, lua)?;
    craft.set("async", r#async::create_async_table(lua)?)?;
    craft.set(
        "builtin_skills",
        lua.create_function(|_lua, ()| {
            let t = _lua.create_table()?;
            for (i, (name, content)) in craft_agent::builtin_skills::BUILTIN_SKILLS
                .iter()
                .enumerate()
            {
                let entry = _lua.create_table()?;
                entry.set("name", *name)?;
                entry.set("content", *content)?;
                t.set(i + 1, entry)?;
            }
            Ok(t)
        })?,
    )?;
    craft.set(
        "keymap",
        keymap::create_keymap_table(lua, Arc::clone(&plugin))?,
    )?;

    if let Some(tx) = embed_tx {
        craft.set("embed", crate::api::embed::create_embed_table(lua, tx)?)?;
    }

    Ok(craft)
}
