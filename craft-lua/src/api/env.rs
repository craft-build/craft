use mlua::{Lua, Result as LuaResult, Table};

use crate::plugin_permissions::{Permission::Env, PluginPermissions};

pub(crate) fn create_env_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "state_dir",
        lua.create_function({
            let perms = perms.clone();
            move |lua, ()| {
                perms.guard(Env, lua, |_| {
                    Ok(craft_storage::paths::state_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from)))
                })
            }
        })?,
    )?;

    t.set(
        "config_dir",
        lua.create_function({
            let perms = perms.clone();
            move |lua, ()| {
                perms.guard(Env, lua, |_| {
                    Ok(craft_storage::paths::config_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from)))
                })
            }
        })?,
    )?;

    t.set(
        "legacy_dir",
        lua.create_function({
            let perms = perms.clone();
            move |lua, ()| {
                perms.guard(Env, lua, |_| {
                    Ok(craft_storage::paths::legacy_home_dir()
                        .and_then(|p| p.to_str().map(String::from)))
                })
            }
        })?,
    )?;

    Ok(t)
}
