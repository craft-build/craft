use std::io;
use std::path::Path;

use mlua::Lua;
use strum::EnumIter;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter)]
pub enum Permission {
    FsRead,
    FsWrite,
    Net,
    Run,
    Env,
}

const MANIFEST_FILE: &str = "plugin.toml";

const PERM_KEYS: [&str; 5] = ["fs_read", "fs_write", "net", "run", "env"];

#[derive(Debug, Clone)]
pub struct PluginPermissions {
    allowed: [bool; 5],
}

impl PluginPermissions {
    pub fn trusted() -> Self {
        Self { allowed: [true; 5] }
    }

    pub fn denied() -> Self {
        Self {
            allowed: [false; 5],
        }
    }

    pub fn is_allowed(&self, perm: Permission) -> bool {
        self.allowed[perm as usize]
    }

    pub fn set(&mut self, perm: Permission, val: bool) {
        self.allowed[perm as usize] = val;
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let table = manifest.get("permissions").and_then(|v| v.as_table());
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
    let msg = format!(
        "Permission denied: {perm:?}. Add '{perm_key}' to [permissions] in {MANIFEST_FILE}"
    );
    tracing::warn!(permission = perm_key, "{msg}");
    mlua::Error::RuntimeError(msg)
}

impl Permission {
    pub const ALL: &[Permission] = &[
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Net,
        Permission::Run,
        Permission::Env,
    ];

    pub fn manifest_key(&self) -> &'static str {
        PERM_KEYS[*self as usize]
    }
}

/// What a package's `plugin.toml` asks for.
///
/// Deny by default: an omitted key is *not requested*. The two parsers are
/// separate types so an effective grant can never be built by accident from a
/// request alone.
#[derive(Debug, Clone)]
pub struct Requested(PluginPermissions);

impl Requested {
    pub fn none() -> Self {
        Self(PluginPermissions::denied())
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let perms = manifest.get("permissions");
        let mut allowed = [false; 5];
        for perm in Permission::ALL {
            allowed[*perm as usize] = perms
                .and_then(|p| p.get(perm.manifest_key()))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
        }
        Self(PluginPermissions { allowed })
    }

    pub fn is_requested(&self, perm: Permission) -> bool {
        self.0.is_allowed(perm)
    }

    /// A package the user installed by hand gets what it asks for. They placed
    /// the files, which is the same trust already given to a local `init.lua`.
    /// Only a package craft fetched has to be intersected with an approval.
    pub fn granted_for_manual_install(self) -> PluginPermissions {
        self.0
    }

    /// Effective permissions for a managed package: the request and the user's
    /// approval must agree.
    pub fn intersect(&self, approved: &PluginPermissions) -> PluginPermissions {
        let mut out = PluginPermissions::denied();
        for perm in Permission::ALL {
            out.set(
                *perm,
                self.0.is_allowed(*perm) && approved.is_allowed(*perm),
            );
        }
        out
    }
}

/// Reads a package's requested permissions.
///
/// Only an absent manifest means "requests nothing". A manifest that exists but
/// cannot be read or parsed is an error, because silently treating it as empty
/// would load the package and then fail every guarded call it makes, which
/// reports the typo as a permission problem instead of a syntax one.
pub(crate) fn load_requested_permissions(
    plugin_dir: &Path,
) -> Result<Requested, crate::error::PluginError> {
    let manifest_path = plugin_dir.join(MANIFEST_FILE);
    let content = match std::fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Requested::none()),
        Err(source) => {
            return Err(crate::error::PluginError::Io {
                path: manifest_path,
                source,
            });
        }
    };
    toml::from_str::<toml::Value>(&content)
        .map(|value| Requested::from_manifest(&value))
        .map_err(|e| crate::error::PluginError::PackageManifest {
            path: manifest_path,
            message: e.to_string(),
        })
}

pub fn load_plugin_permissions(plugin_dir: Option<&Path>) -> PluginPermissions {
    let Some(dir) = plugin_dir else {
        return PluginPermissions::trusted();
    };
    let toml_path = dir.join(MANIFEST_FILE);
    match std::fs::read_to_string(&toml_path) {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(value) => PluginPermissions::from_manifest(&value),
            Err(e) => {
                tracing::warn!(
                    path = %toml_path.display(),
                    error = %e,
                    "cannot read {MANIFEST_FILE}, denying all permissions"
                );
                PluginPermissions::denied()
            }
        },
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                tracing::warn!(
                    dir = %dir.display(),
                    "no {MANIFEST_FILE} next to plugin; all permissions denied. Create one next \n                     to it and list the permissions under [permissions] to grant them"
                );
            } else {
                tracing::warn!(
                    path = %toml_path.display(),
                    error = %e,
                    "cannot read {MANIFEST_FILE}, denying all permissions"
                );
            }
            PluginPermissions::denied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Permission, PluginPermissions, Requested};

    #[test]
    fn requested_denies_omitted_keys() {
        let val: toml::Value = toml::from_str("[permissions]\nnet = true\n").unwrap();
        let req = Requested::from_manifest(&val);
        assert!(req.is_requested(Permission::Net));
        for perm in Permission::ALL {
            if *perm != Permission::Net {
                assert!(!req.is_requested(*perm), "{perm:?} must not be requested");
            }
        }
    }

    #[test]
    fn requested_agrees_with_the_legacy_parser_on_explicit_keys() {
        let val: toml::Value = toml::from_str("[permissions]\nnet = true\n").unwrap();
        let legacy = PluginPermissions::from_manifest(&val);
        let requested = Requested::from_manifest(&val);
        for perm in Permission::ALL {
            assert_eq!(
                legacy.is_allowed(*perm),
                requested.is_requested(*perm),
                "{perm:?}"
            );
        }
    }

    #[test]
    fn intersect_needs_both_request_and_approval() {
        let val: toml::Value = toml::from_str("[permissions]\nnet = true\nrun = true\n").unwrap();
        let requested = Requested::from_manifest(&val);

        let mut approved = PluginPermissions::denied();
        approved.set(Permission::Net, true);
        approved.set(Permission::FsRead, true);

        let effective = requested.intersect(&approved);
        assert!(
            effective.is_allowed(Permission::Net),
            "requested + approved"
        );
        assert!(!effective.is_allowed(Permission::Run), "not approved");
        assert!(!effective.is_allowed(Permission::FsRead), "not requested");
        assert!(!effective.is_allowed(Permission::Env), "neither");
    }

    #[test]
    fn manual_install_grants_what_it_requests() {
        let val: toml::Value = toml::from_str("[permissions]\nfs_read = true\n").unwrap();
        let granted = Requested::from_manifest(&val).granted_for_manual_install();
        assert!(granted.is_allowed(Permission::FsRead));
        assert!(!granted.is_allowed(Permission::Net));
    }
}
