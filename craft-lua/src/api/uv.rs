use mlua::{Lua, Result as LuaResult, Table};

use crate::plugin_permissions::{Permission::Env, PluginPermissions};

pub(crate) fn create_uv_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let t = lua.create_table()?;
    let perms = perms.clone();

    let p = perms.clone();
    t.set(
        "cwd",
        lua.create_function(move |lua, ()| {
            p.guard(Env, lua, |_| {
                Ok(std::env::current_dir()
                    .ok()
                    .and_then(|p| p.to_str().map(String::from)))
            })
        })?,
    )?;

    let p = perms;
    t.set(
        "os_homedir",
        lua.create_function(move |lua, ()| {
            p.guard(Env, lua, |_| {
                Ok(craft_storage::paths::home().and_then(|p| p.to_str().map(String::from)))
            })
        })?,
    )?;

    Ok(t)
}
