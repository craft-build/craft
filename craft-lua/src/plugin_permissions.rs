use std::path::Path;

use mlua::Lua;
use strum::EnumIter;

#[derive(Clone, Copy, Debug, EnumIter)]
pub enum Permission {
    FsRead,
    FsWrite,
    Net,
    Run,
    Env,
}

const PERM_KEYS: [&str; 5] = ["fs_read", "fs_write", "net", "run", "env"];

#[derive(Clone)]
pub struct PluginPermissions {
    allowed: [bool; 5],
}

impl PluginPermissions {
    pub fn trusted() -> Self {
        Self { allowed: [true; 5] }
    }

    pub fn denied() -> Self {
        Self { allowed: [false; 5] }
    }

    pub fn is_allowed(&self, perm: Permission) -> bool {
        self.allowed[perm as usize]
    }

    pub fn set(&mut self, perm: Permission, val: bool) {
        self.allowed[perm as usize] = val;
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let table = manifest
            .get("permissions")
            .and_then(|v| v.as_table());
        let Some(table) = table else {
            return Self::denied();
        };
        let mut perms = Self::denied();
        for (i, key) in PERM_KEYS.iter().enumerate() {
            if let Some(enabled) = table.get(*key).and_then(|v| v.as_bool()) {
                perms.allowed[i] = enabled;
            }
        }
        perms
    }

    pub fn guard<R>(
        &self,
        perm: Permission,
        lua: &Lua,
        f: impl FnOnce(&Lua) -> mlua::Result<R>,
    ) -> mlua::Result<R> {
        if !self.is_allowed(perm) {
            return Err(denied_error(perm));
        }
        f(lua)
    }

    pub async fn guard_async<R>(
        &self,
        perm: Permission,
        lua: &Lua,
        f: impl AsyncFnOnce(&Lua) -> mlua::Result<R>,
    ) -> mlua::Result<R> {
        if !self.is_allowed(perm) {
            return Err(denied_error(perm));
        }
        f(lua).await
    }
}

pub fn denied_error(perm: Permission) -> mlua::Error {
    let perm_key = PERM_KEYS[perm as usize];
    mlua::Error::RuntimeError(format!(
        "Permission denied: {perm:?}. Add '{perm_key}' to [permissions] in plugin.toml"
    ))
}

pub fn load_plugin_permissions(plugin_dir: Option<&Path>) -> PluginPermissions {
    let Some(dir) = plugin_dir else {
        return PluginPermissions::trusted();
    };
    let toml_path = dir.join("plugin.toml");
    let Ok(contents) = std::fs::read_to_string(&toml_path) else {
        return PluginPermissions::denied();
    };
    match toml::from_str(&contents) {
        Ok(value) => PluginPermissions::from_manifest(&value),
        Err(_) => PluginPermissions::denied(),
    }
}
