use std::io;
use std::path::Path;

use mlua::Lua;
use semver::Version;

use crate::error::PluginError;

pub use craft_config::Permission;

pub(crate) const MANIFEST_FILE: &str = "plugin.toml";
const MIN_CRAFT_VERSION: &str = "min_craft_version";
const RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct PluginPermissions {
    allowed: [bool; Permission::COUNT],
}

impl PluginPermissions {
    pub fn trusted() -> Self {
        Self {
            allowed: [true; Permission::COUNT],
        }
    }

    pub fn denied() -> Self {
        Self {
            allowed: [false; Permission::COUNT],
        }
    }

    pub fn is_allowed(&self, perm: Permission) -> bool {
        self.allowed[perm as usize]
    }

    pub fn set(&mut self, perm: Permission, val: bool) {
        self.allowed[perm as usize] = val;
    }

    /// Builds a set from the names an approval records.
    ///
    /// A name this build does not know is ignored rather than treated as a
    /// grant, so an approval file written by a newer craft cannot widen what an
    /// older one allows.
    pub fn from_approved<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::denied();
        for name in names {
            if let Some(perm) = Permission::from_key(name) {
                out.set(perm, true);
            }
        }
        out
    }

    pub fn from_manifest(manifest: &toml::Value) -> Self {
        let table = manifest.get("permissions").and_then(|v| v.as_table());
        let Some(table) = table else {
            return Self::denied();
        };
        let mut perms = Self::denied();
        for perm in Permission::ALL {
            if let Some(enabled) = table.get(perm.manifest_key()).and_then(|v| v.as_bool()) {
                perms.allowed[*perm as usize] = enabled;
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
    let perm_key = perm.manifest_key();
    let msg = format!(
        "Permission denied: {perm:?}. Add '{perm_key}' to [permissions] in {MANIFEST_FILE}"
    );
    tracing::warn!(permission = perm_key, "{msg}");
    mlua::Error::RuntimeError(msg)
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
        let mut allowed = [false; Permission::COUNT];
        for &perm in Permission::ALL {
            allowed[perm as usize] = perms
                .and_then(|p| p.get(perm.manifest_key()))
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
        }
        Self(PluginPermissions { allowed })
    }

    pub fn is_requested(&self, perm: Permission) -> bool {
        self.0.is_allowed(perm)
    }

    pub fn names(&self) -> Vec<String> {
        Permission::ALL
            .iter()
            .copied()
            .filter(|permission| self.is_requested(*permission))
            .map(|permission| permission.manifest_key().to_owned())
            .collect()
    }

    /// Code whose files nobody fetched gets what it asks for: a package the
    /// user installed by hand, or a plugin bundled into the binary. Only a
    /// package craft downloaded has to be intersected with an approval.
    pub fn granted(self) -> PluginPermissions {
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
    load_plugin_manifest(plugin_dir).map_or_else(PluginPermissions::denied, |manifest| {
        PluginPermissions::from_manifest(&manifest)
    })
}

/// Host-side gate, run before any Lua from `plugin_dir` reaches the runtime.
/// An `Err` means the directory is refused; the startup path turns it into a
/// warning and skips the plugin, so one bad `min_craft_version` cannot keep
/// craft from booting.
pub(crate) fn check_plugin_compatibility(
    plugin: &str,
    plugin_dir: Option<&Path>,
) -> Result<(), PluginError> {
    let Some(manifest) = load_plugin_manifest(plugin_dir) else {
        return Ok(());
    };
    let Some(required) = manifest.get(MIN_CRAFT_VERSION) else {
        return Ok(());
    };
    check_minimum_version(plugin, required, RUNTIME_VERSION)
}

fn load_plugin_manifest(plugin_dir: Option<&Path>) -> Option<toml::Value> {
    let dir = plugin_dir?;
    let toml_path = dir.join(MANIFEST_FILE);
    match std::fs::read_to_string(&toml_path) {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(manifest) => Some(manifest),
            Err(e) => {
                tracing::warn!(
                    path = %toml_path.display(),
                    error = %e,
                    "cannot read {MANIFEST_FILE}, denying all permissions"
                );
                None
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
            None
        }
    }
}

fn check_minimum_version(
    plugin: &str,
    required: &toml::Value,
    running: &str,
) -> Result<(), PluginError> {
    let required = required
        .as_str()
        .ok_or_else(|| PluginError::InvalidMinimumVersionType {
            plugin: plugin.to_owned(),
        })?;
    let required =
        Version::parse(required).map_err(|source| PluginError::InvalidMinimumVersion {
            plugin: plugin.to_owned(),
            version: required.to_owned(),
            source,
        })?;
    let running = Version::parse(running).map_err(|source| PluginError::InvalidRuntimeVersion {
        version: running.to_owned(),
        source,
    })?;
    if required > running {
        return Err(PluginError::CraftVersionTooOld {
            plugin: plugin.to_owned(),
            required,
            running,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use test_case::test_case;

    use super::{
        MANIFEST_FILE, MIN_CRAFT_VERSION, Permission, PluginError, PluginPermissions,
        RUNTIME_VERSION, Requested, check_minimum_version, check_plugin_compatibility,
        load_plugin_permissions,
    };

    const PLUGIN: &str = "test-plugin";

    fn assert_denied(permissions: &PluginPermissions) {
        for permission in Permission::ALL {
            assert!(
                !permissions.is_allowed(*permission),
                "{:?} should be denied",
                permission
            );
        }
    }

    #[test]
    fn requested_names_are_the_approval_keys() {
        let value: toml::Value = toml::from_str(
            r#"
            [permissions]
            fs_read = true
            run = true
            "#,
        )
        .unwrap();

        assert_eq!(
            Requested::from_manifest(&value).names(),
            ["fs_read".to_owned(), "run".to_owned()]
        );
    }

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
        let granted = Requested::from_manifest(&val).granted();
        assert!(granted.is_allowed(Permission::FsRead));
        assert!(!granted.is_allowed(Permission::Net));
    }

    #[test_case("1.2.2", "1.2.3", true; "lower")]
    #[test_case("1.2.3", "1.2.3", true; "equal")]
    #[test_case("1.2.3-alpha.1", "1.2.3-alpha.2", true; "older_prerelease")]
    #[test_case("1.2.3-alpha.2", "1.2.3-alpha.1", false; "newer_prerelease")]
    #[test_case("1.2.4", "1.2.3", false; "higher")]
    fn minimum_version_uses_semver_precedence(required: &str, running: &str, compatible: bool) {
        let required = toml::Value::String(required.to_owned());
        let result = check_minimum_version(PLUGIN, &required, running);
        assert_eq!(result.is_ok(), compatible);
        if !compatible {
            assert!(matches!(
                result,
                Err(PluginError::CraftVersionTooOld { .. })
            ));
        }
    }

    #[test]
    fn minimum_version_requires_a_plain_semver_string() {
        let wrong_type = check_minimum_version(PLUGIN, &toml::Value::Integer(1), RUNTIME_VERSION);
        assert!(matches!(
            wrong_type,
            Err(PluginError::InvalidMinimumVersionType { .. })
        ));

        for version in ["not-a-version", "v1.2.3"] {
            let value = toml::Value::String(version.to_owned());
            assert!(matches!(
                check_minimum_version(PLUGIN, &value, RUNTIME_VERSION),
                Err(PluginError::InvalidMinimumVersion { .. })
            ));
        }

        let value = toml::Value::String("1.2.3".to_owned());
        assert!(matches!(
            check_minimum_version(PLUGIN, &value, "invalid"),
            Err(PluginError::InvalidRuntimeVersion { .. })
        ));
    }

    #[test]
    fn manifest_rejects_an_invalid_declared_minimum() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_CRAFT_VERSION} = 1\n"),
        )
        .unwrap();
        assert!(matches!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())),
            Err(PluginError::InvalidMinimumVersionType { .. })
        ));

        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!("{MIN_CRAFT_VERSION} = \"v1.2.3\"\n"),
        )
        .unwrap();
        assert!(matches!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())),
            Err(PluginError::InvalidMinimumVersion { .. })
        ));
    }

    #[test]
    fn missing_valid_and_malformed_manifests_keep_existing_defaults() {
        // A missing plugin dir fails closed: nothing is granted.
        assert_denied(&load_plugin_permissions(None));

        let dir = tempfile::tempdir().unwrap();
        assert_denied(&load_plugin_permissions(Some(dir.path())));

        fs::write(dir.path().join(MANIFEST_FILE), "").unwrap();
        // craft denies by default: an empty manifest grants nothing.
        assert_denied(&load_plugin_permissions(Some(dir.path())));

        fs::write(dir.path().join(MANIFEST_FILE), "not = [valid").unwrap();
        assert_denied(&load_plugin_permissions(Some(dir.path())));
        assert!(
            check_plugin_compatibility(PLUGIN, Some(dir.path())).is_ok(),
            "an unparseable manifest has no floor to enforce"
        );
    }

    #[test]
    fn one_manifest_provides_compatibility_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            format!(
                "{MIN_CRAFT_VERSION} = {RUNTIME_VERSION:?}\n\n[permissions]\nfs_read = true\nnet = false\n"
            ),
        )
        .unwrap();

        check_plugin_compatibility(PLUGIN, Some(dir.path())).unwrap();
        let permissions = load_plugin_permissions(Some(dir.path()));
        assert!(permissions.is_allowed(Permission::FsRead));
        assert!(!permissions.is_allowed(Permission::Net));
    }
}
