use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use craft_agent::permissions::PluginRuleStore;
use craft_agent::tools::ToolRegistry;
use craft_config::{PluginsConfig, RawConfig};
use include_dir::{Dir, include_dir};

use crate::api::keymap::KeymapReader;
use crate::api::options::{PluginOptionSpecs, PluginOpts};
use crate::api::util::command::{HintReader, LuaCommandReader, UiAction};
use crate::error::PluginError;
use crate::pack::DiscoveredPackage;
use crate::plugin_permissions::{
    PluginPermissions, check_plugin_compatibility, load_plugin_permissions,
};
use crate::runtime::{self, ConfigScope, LoadChunk, LoadContext, LuaThread, Request, RestoreItem};
use crate::terminal_backend::{LocalTerminal, TerminalBackend};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const USER_PLUGIN: &str = "user";
pub const SKIPPED_PLUGIN_WARNING: &str = "skipping plugin lua";

struct BundledPlugin {
    name: &'static str,
    dir: Dir<'static>,
}

/// `lib` is not a default builtin; it only exists so plugins can
/// `require()` shared modules across plugin boundaries.
static BUNDLED_PLUGINS: &[BundledPlugin] = &[
    BundledPlugin {
        name: "webfetch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/webfetch"),
    },
    BundledPlugin {
        name: "websearch",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/websearch"),
    },
    BundledPlugin {
        name: "bash",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/bash"),
    },
    BundledPlugin {
        name: "grep",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/grep"),
    },
    BundledPlugin {
        name: "glob",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/glob"),
    },
    BundledPlugin {
        name: "skill",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/skill"),
    },
    BundledPlugin {
        name: "memory",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/memory"),
    },
    BundledPlugin {
        name: "question",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/question"),
    },
    BundledPlugin {
        name: "todo_write",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/todo_write"),
    },
    BundledPlugin {
        name: "view_image",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/view_image"),
    },
    BundledPlugin {
        name: "sessions",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/sessions"),
    },
    BundledPlugin {
        name: "task",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/task"),
    },
    BundledPlugin {
        name: "lib",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
    },
];

/// Every bundled name, not just the default-enabled ones. An external package
/// sharing an owner name with any of them would let one package's unload tear
/// down the other's registrations.
pub(crate) fn is_bundled(name: &str) -> bool {
    BUNDLED_PLUGINS.iter().any(|p| p.name == name)
}

static BUNDLED_DIRS: LazyLock<&'static [&'static Dir<'static>]> = LazyLock::new(|| {
    let dirs: Vec<&'static Dir<'static>> = BUNDLED_PLUGINS.iter().map(|p| &p.dir).collect();
    Vec::leak(dirs)
});

/// A package's entrypoints: every `plugin/*.lua`, sorted by filename so load
/// order is deterministic across machines.
///
/// A repository can commit a symlink, so each entry is resolved and checked to
/// be inside the package before it is read.
fn package_entrypoints(root: &Path) -> Result<Vec<PathBuf>, PluginError> {
    let entrypoint_dir = root.join("plugin");
    let entries = match fs::read_dir(&entrypoint_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(PluginError::Io {
                path: entrypoint_dir,
                source,
            });
        }
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| PluginError::Io {
            path: entrypoint_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let resolved = path.canonicalize().map_err(|e| PluginError::Io {
            path: path.clone(),
            source: e,
        })?;
        if !resolved.starts_with(root) {
            return Err(PluginError::PackageEscape { path });
        }
        if resolved.is_file() {
            let Some(file_name) = path.file_name().map(std::ffi::OsString::from) else {
                continue;
            };
            files.push((file_name, resolved));
        }
    }
    // By file name, not by the resolved path: a package may symlink an entry
    // elsewhere inside itself, and load order must still be the order a user
    // sees in `plugin/`.
    files.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

pub struct PluginHost {
    inner: LuaThread,
    plugin_rules: Arc<PluginRuleStore>,
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(handle) = self.inner.join.take() else {
            return;
        };
        // Start the shutdown first, or the join below waits for all
        // queued bulk work to drain.
        self.begin_shutdown();
        let (done_tx, done_rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(handle.join().is_err());
        });
        match done_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(true) => tracing::warn!("lua thread panicked on shutdown"),
            Err(_) => tracing::warn!("lua thread did not stop within timeout, detaching"),
            Ok(false) => {}
        }
    }
}

impl PluginHost {
    pub fn new(
        registry: Arc<ToolRegistry>,
        embed_tx: Option<crate::api::embed::EmbedChannel>,
    ) -> Result<Self, PluginError> {
        Self::with_jit(registry, embed_tx, true)
    }

    /// `jit: false` (the `--no-jit` flag) runs plugin Lua on the O1
    /// interpreter with full debug info. Applied at VM creation, so
    /// every chunk gets it, init.lua files included.
    pub fn with_jit(
        registry: Arc<ToolRegistry>,
        embed_tx: Option<crate::api::embed::EmbedChannel>,
        jit: bool,
    ) -> Result<Self, PluginError> {
        Self::with_terminal_backend(registry, embed_tx, Arc::new(LocalTerminal), jit)
    }

    pub fn with_terminal_backend(
        registry: Arc<ToolRegistry>,
        embed_tx: Option<crate::api::embed::EmbedChannel>,
        terminal_backend: Arc<dyn TerminalBackend>,
        jit: bool,
    ) -> Result<Self, PluginError> {
        let plugin_rules = Arc::new(PluginRuleStore::default());
        let lua = runtime::spawn(
            registry,
            *BUNDLED_DIRS,
            embed_tx,
            terminal_backend,
            jit,
            Arc::clone(&plugin_rules),
        )?;
        Ok(Self {
            inner: lua,
            plugin_rules,
        })
    }

    /// The store that `craft.api.register_permission_rule` writes into. Hand
    /// it to every [`craft_agent::permissions::PermissionManager`] so plugin
    /// rules apply to all sessions.
    pub fn plugin_rules(&self) -> Arc<PluginRuleStore> {
        Arc::clone(&self.plugin_rules)
    }

    /// Stop the Lua thread from taking new work without joining it, so the
    /// caller can rebuild shared state (like the tool registry) while the
    /// old VM winds down on its own. The shutdown flag makes the watchdog
    /// abort in-flight callbacks, `Shutdown` on the channel skips ahead of
    /// queued bulk work, and swapping the sender for a disconnected one
    /// makes every later host call fail right at the send; `&mut self`
    /// rules out a call racing the swap. `Drop` still joins the thread.
    pub fn begin_shutdown(&mut self) {
        self.inner.shutdown.store(true, Ordering::Release);
        let _ = self.inner.tx.send(Request::Shutdown);
        self.inner.tx = flume::unbounded().0;
    }

    /// Boots the runtime and loads every default bundled plugin into `registry`.
    /// For callers like tests and docgen that want the full builtin set
    /// without building a config.
    pub fn with_all_builtins(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let mut host = Self::new(registry, None)?;
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))?;
        Ok(host)
    }

    /// `warnings` collects non-fatal startup problems (an incompatible
    /// `plugin.toml` skips that directory's Lua) for the caller to surface.
    pub fn load_init_files(
        &self,
        cwd: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let mut merged: Option<RawConfig> = None;

        for global_dir in craft_config::global_config_dirs() {
            self.run_init_file(
                &global_dir.join("init.lua"),
                ConfigScope::Global,
                &mut merged,
                warnings,
            )?;
        }
        self.run_init_file(
            &cwd.join(".craft/init.lua"),
            ConfigScope::Project,
            &mut merged,
            warnings,
        )?;

        Ok(merged)
    }

    /// `--no-plugins` recovery path: skip every user `init.lua` while the
    /// host and builtin plugins stay live. Centralized so every entry point
    /// (TUI, outline, acp, prompt, headless, desktop) honors the flag
    /// identically.
    pub fn load_init_files_or_skip(
        &self,
        no_plugins: bool,
        cwd: &Path,
        warnings: &mut Vec<String>,
    ) -> Result<Option<RawConfig>, PluginError> {
        if no_plugins {
            return Ok(None);
        }
        self.load_init_files(cwd, warnings)
    }

    fn run_init_file(
        &self,
        path: &Path,
        scope: ConfigScope,
        merged: &mut Option<RawConfig>,
        warnings: &mut Vec<String>,
    ) -> Result<(), PluginError> {
        if !path.is_file() {
            return Ok(());
        }
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        if let Err(e) = check_plugin_compatibility(scope.label(), plugin_dir.as_deref()) {
            warnings.push(format!("{SKIPPED_PLUGIN_WARNING}: {e}"));
            return Ok(());
        }
        if let Some(raw) = self.send_config_lua(source, scope, plugin_dir)? {
            match merged {
                Some(existing) => existing.merge(raw),
                None => *merged = Some(raw),
            }
        }
        Ok(())
    }

    pub fn load_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        let result = self.send_builtin_loads(config);
        // Armed even when a load failed, so a caller that only warns
        // about the error is not left interpreting for the rest of
        // the session.
        let _ = self.inner.tx.send(Request::WarmJit);
        result
    }

    fn send_builtin_loads(&self, config: &PluginsConfig) -> Result<(), PluginError> {
        for (plugin, opts) in &config.opts {
            // An enabled package takes its options when the package itself
            // loads, and an enabled builtin takes them in the loop below.
            if config.packages.contains(plugin) || config.names.contains(plugin) {
                continue;
            }
            // What is left is a name that exists but is not loading: a builtin
            // or a package the config disabled, or one discovery refused. It
            // cannot be a typo, because the config layer validated every
            // `plugins.<name>` key against the same names before this ran.
            let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
            tracing::warn!(
                plugin = plugin.as_str(),
                keys = keys.join(", "),
                "nothing named {} is loading; its plugins.{} options are ignored",
                plugin,
                plugin
            );
        }
        for builtin in &config.names {
            let dir = match BUNDLED_PLUGINS.iter().find(|p| p.name == builtin.as_str()) {
                Some(p) => &p.dir,
                None => {
                    return Err(PluginError::UnknownPlugin {
                        plugin: builtin.clone(),
                    });
                }
            };
            let init = dir
                .get_file("init.lua")
                .and_then(|f| f.contents_utf8())
                .ok_or_else(|| PluginError::Lua {
                    plugin: builtin.clone(),
                    source: mlua::Error::runtime("bundled plugin missing init.lua"),
                })?;
            let name: Arc<str> = Arc::from(builtin.as_str());
            let opts = config
                .opts
                .get(builtin.as_str())
                .cloned()
                .map(Arc::new)
                .unwrap_or_default();
            self.send_load(
                Arc::clone(&name),
                vec![LoadChunk::new(name.as_ref(), init)],
                LoadContext {
                    opts,
                    ..LoadContext::plain(None, PluginPermissions::trusted())
                },
            )?;
        }
        Ok(())
    }

    pub fn set_terminal_backend(
        &self,
        backend: Arc<dyn TerminalBackend>,
    ) -> Result<(), PluginError> {
        self.inner
            .tx
            .send(Request::SetTerminalBackend { backend })
            .map_err(|_| PluginError::HostDead)
    }

    pub fn set_sandbox_config(&self, config: craft_config::SandboxConfig) {
        let _ = self.inner.tx.send(Request::SetSandboxConfig { config });
    }

    fn send_load(
        &self,
        name: Arc<str>,
        chunks: Vec<LoadChunk>,
        context: LoadContext,
    ) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::LoadSource {
                name,
                chunks,
                context,
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    /// Option specs declared by loaded plugins via `craft.api.register_options`,
    /// keyed by plugin name. Used by docgen.
    pub fn plugin_options(&self) -> Result<PluginOptionSpecs, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::CollectPluginOptions { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Runs a source as the global `init.lua`.
    ///
    /// The one scope where `craft.pack.add` may declare packages, so it is its
    /// own method: deriving the privilege from a source name would let any
    /// caller reach it by spelling the name the right way.
    pub fn send_global_init_lua(
        &self,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.send_config_lua(source, ConfigScope::Global, plugin_dir)
    }

    /// Runs a source as a config chunk named after itself. It gets the
    /// read-only `craft.pack` table.
    pub fn send_run_init_lua(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        self.send_config_lua(source, ConfigScope::Named(source_name), plugin_dir)
    }

    fn send_config_lua(
        &self,
        source: String,
        scope: ConfigScope,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::RunInitLua {
                source,
                scope,
                plugin_dir,
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::ClearPlugin {
                plugin: Arc::from(plugin),
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?;
        Ok(())
    }

    pub fn load_source(&self, name: &str, source: &str) -> Result<(), PluginError> {
        self.load_source_with_opts(name, source, serde_json::Map::new())
    }

    pub fn load_source_with_opts(
        &self,
        name: &str,
        source: &str,
        opts: serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source)],
            LoadContext {
                opts: Arc::new(opts),
                ..LoadContext::plain(None, PluginPermissions::trusted())
            },
        )
    }

    pub fn load_source_with_permissions(
        &self,
        name: &str,
        source: &str,
        permissions: PluginPermissions,
    ) -> Result<(), PluginError> {
        self.send_load(
            Arc::from(name),
            vec![LoadChunk::new(name, source)],
            LoadContext::plain(None, permissions),
        )
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        check_plugin_compatibility(USER_PLUGIN, plugin_dir.as_deref())?;
        let permissions = load_plugin_permissions(plugin_dir.as_deref());
        let name: Arc<str> = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map_or_else(|| Arc::from(USER_PLUGIN), Arc::from);
        // Test-only path today. Once user plugin dirs exist: derive a real
        // plugin name, since the hardcoded "user" would collide across files,
        // pass the `plugins.<name>` opts through, and teach the
        // unknown-plugin guards about user plugin names.
        self.send_load(
            name,
            vec![LoadChunk::new(path.display().to_string(), source)],
            LoadContext::plain(plugin_dir, permissions),
        )
    }

    /// Packages declared by `craft.pack.add` in `init.lua`.
    ///
    /// Read after the init files have run, which is when the declared set is
    /// complete and before anything is installed.
    pub fn declared_packages(&self) -> Result<Vec<crate::api::pack::Declared>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::CollectPackages { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    fn run_pack_loader(
        &self,
        declared: crate::api::pack::Declared,
        package: &DiscoveredPackage,
        permissions: PluginPermissions,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::RunPackLoader {
                declared,
                context: LoadContext {
                    plugin_dir: Some(package.dir.clone()),
                    permissions,
                    opts,
                    revision_guard: package.revision_guard.clone(),
                    package: true,
                },
                reply: reply_tx,
            })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    /// Loads one external package directory as a single owner.
    ///
    /// Every `plugin/*.lua` becomes a chunk, and the chunks share one
    /// environment, so what one file registers the next can use. The whole set
    /// commits or none of it does.
    pub fn load_package(
        &self,
        name: &str,
        dir: &Path,
        permissions: PluginPermissions,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        self.load_package_with_guard(name, dir, permissions, opts, None)
    }

    fn load_package_with_guard(
        &self,
        name: &str,
        dir: &Path,
        permissions: PluginPermissions,
        opts: PluginOpts,
        revision_guard: Option<Arc<craft_pack::lock::Lock>>,
    ) -> Result<(), PluginError> {
        // Refused here and not only in discovery, because loading an owner
        // drops that owner's existing registrations first. A package named
        // after a bundled plugin would unload the builtin before its own
        // entrypoint ever ran, so every caller has to be gated, not just the
        // one that walks the site directory.
        if is_bundled(name) {
            return Err(PluginError::PackageNameConflict {
                name: name.to_owned(),
                path: dir.to_path_buf(),
            });
        }
        // Resolved once here, so the manifest, the entrypoints, and later
        // `require` calls all agree on one directory even if the path they came
        // from changes underneath us.
        let root = dir.canonicalize().map_err(|e| PluginError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;
        let files = package_entrypoints(&root)?;
        if files.is_empty() {
            return Err(PluginError::PackageEmpty {
                name: name.to_owned(),
                path: root,
            });
        }

        let mut chunks = Vec::with_capacity(files.len());
        for path in files {
            let source = fs::read_to_string(&path).map_err(|e| PluginError::Io {
                path: path.clone(),
                source: e,
            })?;
            chunks.push(LoadChunk::new(path.display().to_string(), source));
        }
        self.send_load(
            Arc::from(name),
            chunks,
            LoadContext {
                plugin_dir: Some(root),
                permissions,
                opts,
                revision_guard,
                package: true,
            },
        )
    }

    /// Refuses further `craft.packadd` calls, and returns anything the queue
    /// still holds.
    ///
    /// One call and not a read followed by a close, because a Lua task can
    /// record an activation between the two and closing would strand it.
    pub fn seal_pack_ops(&self) -> Result<Vec<crate::api::pack::PackOp>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::SealPackOps { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Takes the package operations Lua recorded, leaving the queue empty.
    ///
    /// Called by the host after the initiating task has exited, which keeps a
    /// load off the thread that requested it.
    fn take_pending_pack_ops(&self) -> Result<Vec<crate::api::pack::PackOp>, PluginError> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .tx
            .send(Request::TakePackOps { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    /// Loads every package that should be loaded now: the `start/` ones, and
    /// the `opt/` ones that `craft.packadd` named.
    ///
    /// Activation names are collected here rather than acted on inside
    /// `packadd`, because a load waits on a reply from the runtime thread that
    /// `packadd` is called on. They are collected after each round as well as
    /// before the first, so a package that activates another one still has it
    /// loaded in this startup rather than the next.
    pub fn load_packages(
        &self,
        packages: &[DiscoveredPackage],
        config: &PluginsConfig,
    ) -> Vec<String> {
        self.load_declared_packages(packages, &[], config)
    }

    /// As `load_packages`, with the declarations that may carry a custom
    /// loader. A package with no matching declaration loads its `plugin/*.lua`.
    pub fn load_declared_packages(
        &self,
        packages: &[DiscoveredPackage],
        declared: &[crate::api::pack::Declared],
        config: &PluginsConfig,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        let mut loaded: Vec<&str> = Vec::new();
        let mut round: Vec<&DiscoveredPackage> = packages
            .iter()
            .filter(|pkg| pkg.eager && config.packages.iter().any(|n| n == &pkg.name))
            .collect();

        // A `loop` and not `while !round.is_empty()`: with no `start` package
        // installed the first round is empty, and the names `init.lua` already
        // recorded still have to be collected.
        loop {
            for pkg in round {
                let opts = config
                    .opts
                    .get(&pkg.name)
                    .cloned()
                    .map(Arc::new)
                    .unwrap_or_default();
                // A package the user installed by hand is granted what it asks
                // for. Only a package craft fetched needs an approval as well.
                let permissions = crate::pack::effective_permissions(pkg);
                loaded.push(&pkg.name);
                let custom = declared
                    .iter()
                    .find(|declaration| declaration.spec.name == pkg.name)
                    .filter(|declaration| {
                        matches!(declaration.load, crate::api::pack::LoadMode::Custom(_))
                            && matches!(
                                &pkg.origin,
                                crate::pack::Origin::Fetched { src }
                                    if src == &declaration.spec.src
                            )
                    });
                let result = match custom {
                    Some(declaration) => {
                        self.run_pack_loader(declaration.clone(), pkg, permissions, opts)
                    }
                    None => self.load_package_with_guard(
                        &pkg.name,
                        &pkg.dir,
                        permissions,
                        opts,
                        pkg.revision_guard.clone(),
                    ),
                };
                if let Err(e) = result {
                    tracing::error!(
                        package = %pkg.name,
                        path = %pkg.dir.display(),
                        error = %e,
                        "failed to load package"
                    );
                    failures.push(format!("{}: failed to load: {e}", pkg.name));
                }
            }

            let ops = match self.take_pending_pack_ops() {
                Ok(ops) => ops,
                Err(e) => {
                    failures.push(format!("could not read package activations: {e}"));
                    break;
                }
            };
            round = Vec::new();
            for op in ops {
                let crate::api::pack::PackOp::Activate { name } = op;
                if loaded.contains(&name.as_str()) {
                    continue;
                }
                // Refused rather than loaded when the config disabled it, so
                // `packadd` cannot be a way around `plugins.<name>.enabled`.
                let found = packages
                    .iter()
                    .find(|pkg| pkg.name == name && config.packages.iter().any(|n| n == &pkg.name));
                match found {
                    Some(pkg) => round.push(pkg),
                    None => failures.push(format!(
                        "packadd {name:?}: no package with that name is installed"
                    )),
                }
            }
            if round.is_empty() {
                break;
            }
        }
        // Nothing drains the queue after this, so `packadd` is closed rather
        // than left accepting names no one will read. Closing returns whatever
        // arrived since the last round, which is reported rather than dropped:
        // that request was going to be honoured a moment earlier.
        match self.seal_pack_ops() {
            Ok(leftover) => {
                for op in leftover {
                    let crate::api::pack::PackOp::Activate { name } = op;
                    failures.push(format!(
                        "packadd {name:?}: arrived after the packages had loaded"
                    ));
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not close the package activation queue");
            }
        }
        failures
    }

    pub fn event_handle(&self) -> EventHandle {
        EventHandle {
            tx: self.inner.tx.clone(),
        }
    }

    pub fn command_reader(&self) -> LuaCommandReader {
        self.inner.command_reader.clone()
    }

    pub fn keymap_reader(&self) -> KeymapReader {
        self.inner.keymap_reader.clone()
    }

    pub fn hint_reader(&self) -> HintReader {
        self.inner.hint_reader.clone()
    }

    pub fn ui_action_rx(&self) -> flume::Receiver<UiAction> {
        self.inner.ui_action_rx.clone()
    }
}

#[derive(Clone)]
pub struct EventHandle {
    tx: flume::Sender<Request>,
}

impl EventHandle {
    pub(crate) fn tx(&self) -> &flume::Sender<Request> {
        &self.tx
    }

    /// Test constructor that wraps an arbitrary request channel.
    pub(crate) fn from_tx(tx: flume::Sender<Request>) -> Self {
        Self { tx }
    }

    /// Test handle whose every dispatch fails, simulating a dead lua host.
    pub fn disconnected_for_test() -> Self {
        let (tx, rx) = flume::bounded(0);
        drop(rx);
        Self { tx }
    }

    /// True when no runtime is draining requests. Production handles stay
    /// connected for the host's lifetime; the `disconnected_for_test`
    /// handle and a host whose thread has shut down both report true.
    /// Callers use this to skip async side effects (e.g. a restore-complete
    /// flip) that no live consumer would ever observe.
    pub fn is_disconnected(&self) -> bool {
        self.tx.is_disconnected()
    }

    pub fn set_sandbox_config(&self, config: craft_config::SandboxConfig) {
        let _ = self.tx.send(Request::SetSandboxConfig { config });
    }

    pub fn run_command(&self, plugin: Arc<str>, command: Arc<str>, args: String, depth: u8) {
        let _ = self.tx.try_send(Request::RunCommand {
            plugin,
            command,
            args,
            depth,
        });
    }

    pub fn collect_prompt_slots(&self) -> craft_agent::prompt::ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots { reply: tx });
        rx.recv().unwrap_or_default()
    }

    pub fn collect_recency(&self) -> craft_agent::prompt::RecencyFacts {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectRecency { reply: tx });
        rx.recv().unwrap_or_default()
    }

    pub async fn collect_prompt_slots_async(&self) -> craft_agent::prompt::ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots { reply: tx });
        rx.recv_async().await.unwrap_or_default()
    }

    pub async fn collect_recency_async(&self) -> craft_agent::prompt::RecencyFacts {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectRecency { reply: tx });
        rx.recv_async().await.unwrap_or_default()
    }

    pub fn request_restore(&self, item: RestoreItem, event_tx: craft_agent::EventSender) {
        let _ = self
            .tx
            .try_send(Request::RestoreToolAsync { item, event_tx });
    }

    pub fn send_restore_complete(&self, flag: Arc<AtomicBool>) {
        let _ = self.tx.send(Request::RestoreComplete { flag });
    }

    pub fn fire_autocmd(&self, event: &str, data: serde_json::Value) {
        let _ = self.tx.try_send(Request::FireAutocmd {
            event: event.to_owned(),
            data,
        });
    }

    pub fn run_keybind_callback(&self, id: u64) -> bool {
        self.tx.try_send(Request::RunKeybindCallback { id }).is_ok()
    }
}

/// Bridge from the agent's [`craft_agent::prompt::RecencySource`] trait to the
/// Lua plugin runtime. Held by the agent as `Option<Arc<dyn RecencySource>>`;
/// each turn the agent calls [`Self::collect`], which round-trips into the Lua
/// VM thread, evaluates every registered `register_recency_source` callback,
/// and returns the joined volatile facts. Built fresh per turn, never stored.
#[derive(Clone)]
pub struct LuaRecencySource {
    handle: EventHandle,
}

impl LuaRecencySource {
    pub fn new(handle: EventHandle) -> Self {
        Self { handle }
    }
}

impl craft_agent::prompt::RecencySource for LuaRecencySource {
    fn collect(&self, _ctx: &craft_agent::prompt::RecencyCtx) -> craft_agent::prompt::RecencyFacts {
        self.handle.collect_recency()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{LuaCommandInfo, LuaCommandWriter};
    use craft_agent::prompt::{PromptId, Slot};
    use craft_agent::tools::ToolRegistry;
    use std::time::Instant;

    /// Closing the queue and reading it are one message. A Lua task can record
    /// an activation between a separate read and close, and a close that threw
    /// the queue away would strand exactly the request that was about to be
    /// honoured.
    #[test]
    fn closing_the_activation_queue_hands_back_what_it_holds() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new()), None).unwrap();
        host.load_source("recorder", r#"craft.packadd("demo")"#)
            .expect("packadd is available to every plugin");

        let leftover = host.seal_pack_ops().expect("the host is running");

        assert_eq!(
            leftover,
            vec![crate::api::pack::PackOp::Activate {
                name: "demo".to_owned()
            }],
            "a recorded activation must come back, not be dropped"
        );
    }

    /// jit=true is exercised by the whole integration suite
    /// (`tests/plugin_host.rs` boots hosts via `new`); only the O1
    /// interpreter path needs its own coverage.
    #[test]
    fn with_jit_off_loads_builtins_and_registers_tools() {
        let reg = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::with_jit(Arc::clone(&reg), None, false).unwrap();
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .unwrap();
        assert!(reg.has("glob"));
    }

    /// The second call sends `Shutdown` on a sender that is already
    /// Every name in `craft_config::DEFAULT_BUILTINS` must be a bundled Lua
    /// plugin. Native tools are builtin via `NATIVE_TOOL_NAMES`, not this list;
    /// a native name here would make `with_all_builtins` (and docgen) fail at
    /// runtime. Catches that drift in the test suite instead of via docgen.
    #[test]
    fn default_builtins_are_all_bundled_plugins() {
        for name in craft_config::DEFAULT_BUILTINS {
            assert!(
                BUNDLED_PLUGINS.iter().any(|p| p.name == *name),
                "DEFAULT_BUILTINS entry {name:?} is not a bundled Lua plugin; native tools belong in register_tools!, not DEFAULT_BUILTINS"
            );
        }
    }

    /// disconnected; it must swallow that error and keep rejecting work.
    #[test]
    fn begin_shutdown_rejects_later_loads_and_is_idempotent() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new()), None).unwrap();
        host.begin_shutdown();
        assert!(host.load_source("late", "return {}").is_err());
        host.begin_shutdown();
        assert!(host.load_source("later", "return {}").is_err());
    }

    /// Regression for the exit drain in `runtime::spawn`. An `EventHandle`
    /// clone keeps queued requests alive after the Lua thread exits, and
    /// without the drain its reply sender lives forever and
    /// `collect_prompt_slots` blocks; with it, the call falls back to
    /// defaults right away.
    #[test]
    fn live_event_handle_does_not_hang_after_begin_shutdown() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new()), None).unwrap();
        host.load_source(
            "hinted",
            r#"craft.api.register_prompt_hint({ slot = "tool_usage", content = "live" })"#,
        )
        .unwrap();
        let handle = host.event_handle();
        host.begin_shutdown();

        let slots = handle.collect_prompt_slots();
        assert!(
            hint_contents(&slots, PromptId::System, Slot::ToolUsage).is_empty(),
            "dead host must yield defaults, not real slots"
        );

        drop(host);
        let slots = handle.collect_prompt_slots();
        assert!(hint_contents(&slots, PromptId::System, Slot::ToolUsage).is_empty());
    }

    #[test]
    fn command_writer_reader_pair_works() {
        let (writer, reader) = LuaCommandWriter::new();
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 0);

        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/test"),
            description: Arc::from("desc"),
            plugin: Arc::from("p"),
            max_args: 0,
        }]);
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 1);
        assert!(snap.generation > 0);
    }

    #[test]
    fn memory_builtin_registers_command() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
        let reader = host.command_reader();
        let snap = reader.load();
        let found = snap.commands.iter().any(|c| c.name.as_ref() == "/memory");
        assert!(
            found,
            "Expected /memory command, found: {:?}",
            snap.commands
                .iter()
                .map(|c| c.name.as_ref())
                .collect::<Vec<_>>()
        );
    }

    /// `/tasks` and `/sessions` used to be Rust commands. The plugins that took
    /// them over have to keep the names, or the palette quietly loses a row.
    #[test]
    fn builtin_plugins_register_their_commands() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
        let snap = host.command_reader().load();
        let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
        for command in ["/sessions", "/rename", "/tasks"] {
            assert!(
                names.contains(&command),
                "expected {command} command, got: {names:?}"
            );
        }
    }

    #[test]
    fn run_command_sends_correct_request() {
        let (tx, rx) = flume::bounded(8);
        let handle = EventHandle { tx };
        handle.run_command(
            Arc::from("myplugin"),
            Arc::from("/greet"),
            "world".into(),
            2,
        );
        let req = rx.try_recv().unwrap();
        match req {
            Request::RunCommand {
                plugin,
                command,
                args,
                depth,
            } => {
                assert_eq!(plugin.as_ref(), "myplugin");
                assert_eq!(command.as_ref(), "/greet");
                assert_eq!(args, "world");
                assert_eq!(depth, 2);
            }
            _ => panic!("expected RunCommand"),
        }
    }

    #[test]
    fn multiple_plugins_register_independent_commands() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "plugin_a",
            r#"
            craft.api.register_command({
                name = "/alpha",
                description = "from a",
                handler = function() end,
            })
            "#,
        )
        .unwrap();
        host.load_source(
            "plugin_b",
            r#"
            craft.api.register_command({
                name = "/beta",
                description = "from b",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 2);
        let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
        assert!(names.contains(&"/alpha"));
        assert!(names.contains(&"/beta"));
    }

    #[test]
    fn register_command_adds_missing_leading_slash() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(reg, None).unwrap();
        host.load_source(
            "noslash",
            r#"
            craft.api.register_command({
                name = "hello",
                description = "no slash",
                handler = function() end,
            })
            "#,
        )
        .unwrap();

        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 1);
        assert_eq!(snap.commands[0].name.as_ref(), "/hello");
    }

    #[test]
    fn command_reader_generation_increments_on_publish() {
        let (writer, reader) = LuaCommandWriter::new();
        assert_eq!(reader.load().generation, 0);
        writer.publish(vec![]);
        assert!(reader.load().generation > 0);
    }

    /// End-to-end: a plugin registers a keymap override, the override is published
    /// to the snapshot, EventHandle::run_keybind_callback dispatches the request,
    /// the runtime resolves the Function by id from the registry, and the callback
    /// executes with an observable side effect. This is the load-bearing path the
    /// dispatch reorder and the store hardening rest on; unit tests only cover the
    /// layers in isolation.
    #[test]
    fn keybind_callback_runs_end_to_end() {
        let host = PluginHost::new(Arc::new(ToolRegistry::new()), None).unwrap();
        host.load_source(
            "kb",
            r#"
            craft.keymap.set("n", "<C-g>", function()
                craft.api.register_command({
                    name = "/fired",
                    description = "callback ran",
                    handler = function() end,
                })
            end, { desc = "test override" })
            "#,
        )
        .unwrap();

        let snap = host.keymap_reader().load();
        assert_eq!(snap.entries.len(), 1, "override published to snapshot");
        let entry = &snap.entries[0];
        assert_eq!(entry.desc, "test override");
        assert!(
            host.command_reader().load().commands.is_empty(),
            "callback has not fired yet"
        );

        let handle = host.event_handle();
        handle.run_keybind_callback(entry.id);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let cmds = &host.command_reader().load().commands;
            if cmds.iter().any(|c| c.name.as_ref() == "/fired") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "keybind callback did not register /fired within 2s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `load_init_files_or_skip` is the single seam every entry point
    /// (TUI, outline, acp, prompt, headless, desktop) uses to honor
    /// `--no-plugins`. Verify both halves: the flag skips a broken
    /// init.lua, and absence runs it (so the skip path is not a tautology
    /// that hides a regression in the unconditional loader).
    #[test]
    fn load_init_files_or_skip_respects_flag() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".craft")).unwrap();
        fs::write(
            dir.path().join(".craft/init.lua"),
            "error('broken init lua must not run')",
        )
        .unwrap();

        let host = PluginHost::new(Arc::new(ToolRegistry::new()), None).unwrap();
        let mut warnings = Vec::new();

        let skipped = host
            .load_init_files_or_skip(true, dir.path(), &mut warnings)
            .expect("no-plugins skips broken init.lua");
        assert!(
            skipped.is_none(),
            "--no-plugins must skip user init.lua entirely"
        );

        let ran = host.load_init_files_or_skip(false, dir.path(), &mut warnings);
        assert!(
            ran.is_err(),
            "without --no-plugins the broken init.lua must surface as an error"
        );
    }

    #[test]
    fn callback_string_lands_in_targeted_prompt_only() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "test_hint",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = "general",
                content = function()
                    return "ONLY_GENERAL"
                end,
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        let slots = handle.collect_prompt_slots();
        let general = slots.get(
            craft_agent::prompt::PromptId::General,
            craft_agent::prompt::Slot::ToolUsage,
        );
        let system = slots.get(
            craft_agent::prompt::PromptId::System,
            craft_agent::prompt::Slot::ToolUsage,
        );
        assert_eq!(general.len(), 1);
        assert_eq!(general[0].content, "ONLY_GENERAL");
        assert!(system.is_empty());
    }

    #[test]
    fn callback_returning_nil_contributes_nothing() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "nil_hint",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                content = function()
                    return nil
                end,
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        assert!(
            handle
                .collect_prompt_slots()
                .get(
                    craft_agent::prompt::PromptId::System,
                    craft_agent::prompt::Slot::ToolUsage,
                )
                .is_empty()
        );
    }

    #[test]
    fn static_no_prompt_lands_on_all_prompts_with_slot() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "broad_hint",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                content = "BROAD",
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        let slots = handle.collect_prompt_slots();
        for &pid in craft_agent::prompt::PromptId::ALL {
            if pid.has_slot(craft_agent::prompt::Slot::ToolUsage) {
                assert_eq!(
                    slots.get(pid, craft_agent::prompt::Slot::ToolUsage).len(),
                    1,
                    "tool_usage hint should land on {:?}",
                    pid
                );
            }
        }
    }

    #[test]
    fn default_hint_skips_prompts_lacking_the_slot() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "conv_hint",
            r#"
            craft.api.register_prompt_hint({
                slot = "conventions",
                content = "SHOULD_SKIP_RESEARCH",
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        let slots = handle.collect_prompt_slots();
        assert!(
            slots
                .get(
                    craft_agent::prompt::PromptId::Research,
                    craft_agent::prompt::Slot::Conventions,
                )
                .is_empty()
        );
        assert_eq!(
            slots
                .get(
                    craft_agent::prompt::PromptId::System,
                    craft_agent::prompt::Slot::Conventions,
                )
                .len(),
            1
        );
    }

    #[test]
    fn register_prompt_hint_rejects_incompatible_slot_prompt() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "bad_target",
            r#"
            craft.api.register_prompt_hint({
                slot = "after_instructions",
                prompt = "research",
                content = "DROPPED",
            })
            "#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn prompt_list_targets_each_listed_prompt() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "multi_prompt",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                prompt = { "system", "research" },
                content = "MULTI",
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        let slots = handle.collect_prompt_slots();
        assert_eq!(
            slots
                .get(
                    craft_agent::prompt::PromptId::System,
                    craft_agent::prompt::Slot::ToolUsage,
                )
                .len(),
            1
        );
        assert_eq!(
            slots
                .get(
                    craft_agent::prompt::PromptId::Research,
                    craft_agent::prompt::Slot::ToolUsage,
                )
                .len(),
            1
        );
        assert!(
            slots
                .get(
                    craft_agent::prompt::PromptId::General,
                    craft_agent::prompt::Slot::ToolUsage,
                )
                .is_empty()
        );
    }

    #[test]
    fn multiple_plugins_sorted_by_plugin_name() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "zzz_plugin",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                content = "from_zzz",
            })
            "#,
        )
        .unwrap();
        host.load_source(
            "aaa_plugin",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                content = "from_aaa",
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        let slots = handle.collect_prompt_slots();
        let entries = slots.get(
            craft_agent::prompt::PromptId::System,
            craft_agent::prompt::Slot::ToolUsage,
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].content, "from_aaa",
            "BTreeMap should sort by plugin name"
        );
        assert_eq!(entries[1].content, "from_zzz");
    }

    #[test]
    fn unload_clears_all_hints_from_plugin() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "temp_plugin",
            r#"
            craft.api.register_prompt_hint({
                slot = "tool_usage",
                content = "temporary",
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle();
        assert_eq!(
            handle
                .collect_prompt_slots()
                .get(
                    craft_agent::prompt::PromptId::System,
                    craft_agent::prompt::Slot::ToolUsage,
                )
                .len(),
            1
        );

        host.unload("temp_plugin").unwrap();
        assert!(
            handle
                .collect_prompt_slots()
                .get(
                    craft_agent::prompt::PromptId::System,
                    craft_agent::prompt::Slot::ToolUsage,
                )
                .is_empty()
        );
    }

    #[test_case::test_case(
        r#"craft.api.register_prompt_hint({ slot = "bad_slot", content = "x" })"#,
        "invalid slot" ; "invalid_slot"
    )]
    #[test_case::test_case(
        r#"craft.api.register_prompt_hint({ slot = "tool_usage", prompt = "bad_prompt", content = "x" })"#,
        "invalid prompt" ; "invalid_prompt"
    )]
    #[test_case::test_case(
        r#"craft.api.register_prompt_hint({ slot = "tool_usage" })"#,
        "missing content" ; "missing_content"
    )]
    #[test_case::test_case(
        r#"craft.api.register_prompt_hint({ content = "x" })"#,
        "missing slot" ; "missing_slot"
    )]
    fn invalid_hint_spec_is_rejected(lua_code: &str, _label: &str) {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        assert!(host.load_source("bad_hint", lua_code).is_err());
    }

    fn hint_contents(
        slots: &craft_agent::prompt::ResolvedSlots,
        prompt: craft_agent::prompt::PromptId,
        slot: craft_agent::prompt::Slot,
    ) -> Vec<String> {
        slots
            .get(prompt, slot)
            .iter()
            .map(|e| e.content.clone())
            .collect()
    }

    #[test]
    fn identity_slot_lands_on_system_only() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "id",
            r#"
            craft.api.set_prompt({
                slot = "identity",
                content = "Custom identity",
            })
            "#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Identity),
            ["Custom identity".to_string()]
        );
        assert!(hint_contents(&slots, PromptId::Research, Slot::Identity).is_empty());
        assert!(hint_contents(&slots, PromptId::General, Slot::Identity).is_empty());
    }

    #[test]
    fn tone_slot_lands_on_system_only() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "tone",
            r#"
            craft.api.set_prompt({
                slot = "tone",
                content = "Custom tone",
            })
            "#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Tone),
            ["Custom tone".to_string()]
        );
        assert!(hint_contents(&slots, PromptId::Research, Slot::Tone).is_empty());
        assert!(hint_contents(&slots, PromptId::General, Slot::Tone).is_empty());
    }

    #[test]
    fn singleton_last_wins_across_plugins() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "aaa",
            r#"craft.api.set_prompt({ slot = "identity", content = "AAA" })"#,
        )
        .unwrap();
        host.load_source(
            "zzz",
            r#"craft.api.set_prompt({ slot = "identity", content = "ZZZ" })"#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        let entries = slots.get(PromptId::System, Slot::Identity);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries.last().unwrap().content, "ZZZ");
    }

    #[test]
    fn set_prompt_content_required() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source("bad", r#"craft.api.set_prompt({ slot = "identity" })"#);
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("'content' is required"));
    }

    #[test]
    fn set_prompt_sets_identity() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "setter",
            r#"
            craft.api.set_prompt({
                slot = "identity",
                content = "New identity",
            })
            "#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Identity),
            ["New identity".to_string()]
        );
    }

    #[test]
    fn set_prompt_explicit_system_prompt() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "setter",
            r#"
            craft.api.set_prompt({
                slot = "identity",
                prompt = "system",
                content = "Explicit identity",
            })
            "#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Identity),
            ["Explicit identity".to_string()]
        );
    }

    #[test]
    fn set_prompt_invalid_prompt_rejected() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "bad",
            r#"craft.api.set_prompt({ slot = "identity", prompt = "nope", content = "x" })"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn set_prompt_and_register_prompt_hint_coexist() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "hint",
            r#"craft.api.register_prompt_hint({ slot = "tool_usage", content = "HINT" })"#,
        )
        .unwrap();
        host.load_source(
            "setter",
            r#"craft.api.set_prompt({ slot = "identity", content = "SET" })"#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::ToolUsage),
            ["HINT".to_string()]
        );
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Identity),
            ["SET".to_string()]
        );
    }

    #[test]
    fn set_prompt_rejects_aggregate_slot() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "bad",
            r#"craft.api.set_prompt({ slot = "tool_usage", content = "nope" })"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn set_prompt_rejects_incompatible_slot_prompt() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "bad",
            r#"craft.api.set_prompt({ slot = "identity", prompt = "research", content = "x" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("not available"));
    }

    #[test]
    fn empty_prompt_table_is_rejected() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "bad",
            r#"craft.api.set_prompt({ slot = "identity", prompt = {}, content = "x" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("no sequence entries"));
    }

    #[test]
    fn set_prompt_content_must_not_be_empty() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "bad",
            r#"craft.api.set_prompt({ slot = "identity", content = "" })"#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("empty"));
    }

    #[test]
    fn set_prompt_with_callback() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "setter_cb",
            r#"
            craft.api.set_prompt({
                slot = "identity",
                content = function() return "Dyn identity" end,
            })
            "#,
        )
        .unwrap();
        let slots = host.event_handle().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Identity),
            ["Dyn identity".to_string()]
        );
    }

    #[test]
    fn register_recency_source_collects_rendered_block() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "git_state",
            r#"
            craft.api.register_recency_source({
                name = "git-status",
                callback = function() return "git: clean" end,
            })
            "#,
        )
        .unwrap();
        let facts = host.event_handle().collect_recency();
        let rendered = facts.render();
        assert!(rendered.starts_with("<turn-context>"));
        assert!(rendered.contains("git: clean"));
    }

    #[test]
    fn register_recency_source_requires_name() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "no_name",
            r#"
            craft.api.register_recency_source({
                callback = function() return "x" end,
            })
            "#,
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("'name' is required"));
    }

    #[test]
    fn register_recency_source_requires_callback() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let r = host.load_source(
            "no_cb",
            r#"
            craft.api.register_recency_source({ name = "x" })
            "#,
        );
        assert!(r.is_err());
        assert!(
            r.unwrap_err()
                .to_string()
                .contains("'callback' is required")
        );
    }

    #[test]
    fn recency_source_nil_and_multiple_sources_combine() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        host.load_source(
            "multi",
            r#"
            craft.api.register_recency_source({
                name = "nil-one",
                callback = function() return nil end,
            })
            craft.api.register_recency_source({
                name = "real-one",
                callback = function() return "todos: 3" end,
            })
            "#,
        )
        .unwrap();
        let facts = host.event_handle().collect_recency();
        let rendered = facts.render();
        assert!(!rendered.contains("nil"));
        assert!(rendered.contains("todos: 3"));
    }

    #[test]
    fn no_recency_sources_registered_is_empty() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg), None).unwrap();
        let facts = host.event_handle().collect_recency();
        assert!(facts.is_empty());
        assert_eq!(facts.render(), "");
    }
}
