use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use craft_agent::tools::ToolRegistry;
use craft_config::{PluginsConfig, RawConfig};
use include_dir::{Dir, include_dir};

use crate::api::keymap::KeymapReader;
use crate::api::options::{PluginOptionSpecs, PluginOpts};
use crate::api::util::command::{HintReader, LuaCommandReader, UiAction};
use crate::error::PluginError;
use crate::plugin_permissions::{PluginPermissions, load_plugin_permissions};
use crate::runtime::{self, LuaThread, Request, RestoreItem};
use crate::terminal_backend::{LocalTerminal, TerminalBackend};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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
        name: "lib",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/lib"),
    },
];

static BUNDLED_DIRS: LazyLock<&'static [&'static Dir<'static>]> = LazyLock::new(|| {
    let dirs: Vec<&'static Dir<'static>> = BUNDLED_PLUGINS.iter().map(|p| &p.dir).collect();
    Vec::leak(dirs)
});

pub struct PluginHost {
    inner: Option<LuaThread>,
}

impl Drop for PluginHost {
    fn drop(&mut self) {
        let Some(ref mut inner) = self.inner else {
            return;
        };
        let Some(handle) = inner.join.take() else {
            return;
        };
        inner.shutdown.store(true, Ordering::Release);
        let _ = inner.tx.send(Request::Shutdown);
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
        let lua = runtime::spawn(registry, *BUNDLED_DIRS, embed_tx, terminal_backend, jit)?;
        Ok(Self { inner: Some(lua) })
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    /// Stop the Lua thread from taking new work without joining it, so the
    /// caller can rebuild shared state (like the tool registry) while the
    /// old VM winds down on its own. The shutdown flag makes the watchdog
    /// abort in-flight callbacks, `Shutdown` on the channel skips ahead of
    /// queued bulk work, and swapping the sender for a disconnected one
    /// makes every later host call fail right at the send; `&mut self`
    /// rules out a call racing the swap. `Drop` still joins the thread.
    pub fn begin_shutdown(&mut self) {
        if let Some(ref mut inner) = self.inner {
            inner.shutdown.store(true, Ordering::Release);
            let _ = inner.tx.send(Request::Shutdown);
            inner.tx = flume::unbounded().0;
        }
    }

    /// Boots the runtime and loads every default bundled plugin into `registry`.
    /// For callers like tests and docgen that want the full builtin set
    /// without building a config.
    pub fn with_all_builtins(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let mut host = Self::new(registry, None)?;
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))?;
        Ok(host)
    }

    pub fn load_init_files(&self, cwd: &Path) -> Result<Option<RawConfig>, PluginError> {
        if self.inner.is_none() {
            return Ok(None);
        }
        let mut merged: Option<RawConfig> = None;

        for global_dir in craft_config::global_config_dirs() {
            self.run_init_file(&global_dir.join("init.lua"), "global/init.lua", &mut merged)?;
        }
        self.run_init_file(
            &cwd.join(".craft/init.lua"),
            "project/init.lua",
            &mut merged,
        )?;

        Ok(merged)
    }

    fn run_init_file(
        &self,
        path: &Path,
        label: &str,
        merged: &mut Option<RawConfig>,
    ) -> Result<(), PluginError> {
        if !path.is_file() {
            return Ok(());
        }
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        if let Some(raw) = self.send_run_init_lua(source, label.to_owned(), plugin_dir)? {
            match merged {
                Some(existing) => existing.merge(raw),
                None => *merged = Some(raw),
            }
        }
        Ok(())
    }

    pub fn load_builtins(&mut self, config: &PluginsConfig) -> Result<(), PluginError> {
        if self.inner.is_none() {
            return Ok(());
        }
        for (plugin, opts) in &config.opts {
            let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
            if !BUNDLED_PLUGINS.iter().any(|p| p.name == plugin.as_str()) {
                return Err(PluginError::UnknownPluginOptions {
                    plugin: plugin.clone(),
                    keys: keys.join(", "),
                });
            }
            if !config.names.contains(plugin) {
                tracing::warn!(
                    plugin = plugin.as_str(),
                    keys = keys.join(", "),
                    "plugin is disabled; its plugins.{} options are ignored until re-enabled",
                    plugin
                );
            }
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
                name,
                init.to_owned(),
                None,
                PluginPermissions::trusted(),
                opts,
            )?;
        }
        Ok(())
    }

    fn tx(&self) -> Result<&flume::Sender<Request>, PluginError> {
        self.inner
            .as_ref()
            .map(|r| &r.tx)
            .ok_or(PluginError::HostDead)
    }

    pub fn set_terminal_backend(
        &self,
        backend: Arc<dyn TerminalBackend>,
    ) -> Result<(), PluginError> {
        let tx = self.tx()?;
        tx.send(Request::SetTerminalBackend { backend })
            .map_err(|_| PluginError::HostDead)
    }

    pub fn set_sandbox_config(&self, config: craft_config::SandboxConfig) {
        if let Ok(tx) = self.tx() {
            let _ = tx.send(Request::SetSandboxConfig { config });
        }
    }

    fn send_load(
        &self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
        permissions: PluginPermissions,
        opts: PluginOpts,
    ) -> Result<(), PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::LoadSource {
            name,
            source,
            plugin_dir,
            permissions,
            opts,
            reply: reply_tx,
        })
        .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    /// Option specs declared by loaded plugins via `craft.api.register_options`,
    /// keyed by plugin name. Used by docgen.
    pub fn plugin_options(&self) -> Result<PluginOptionSpecs, PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::CollectPluginOptions { reply: reply_tx })
            .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)
    }

    pub fn send_run_init_lua(
        &self,
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::RunInitLua {
            source,
            source_name,
            plugin_dir,
            reply: reply_tx,
        })
        .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
    }

    pub fn unload(&self, plugin: &str) -> Result<(), PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::ClearPlugin {
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
            source.to_owned(),
            None,
            PluginPermissions::trusted(),
            Arc::new(opts),
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
            source.to_owned(),
            None,
            permissions,
            PluginOpts::default(),
        )
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        let permissions = load_plugin_permissions(plugin_dir.as_deref());
        let name: Arc<str> = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map_or_else(|| Arc::from("user"), Arc::from);
        // Test-only path today. Once user plugin dirs exist: derive a real
        // plugin name, since the hardcoded "user" would collide across files,
        // pass the `plugins.<name>` opts through, and teach the
        // unknown-plugin guards about user plugin names.
        self.send_load(name, source, plugin_dir, permissions, PluginOpts::default())
    }

    pub fn event_handle(&self) -> Option<EventHandle> {
        self.inner
            .as_ref()
            .map(|t| EventHandle { tx: t.tx.clone() })
    }

    pub fn command_reader(&self) -> LuaCommandReader {
        self.inner
            .as_ref()
            .map(|t| t.command_reader.clone())
            .unwrap_or_else(LuaCommandReader::empty)
    }

    pub fn keymap_reader(&self) -> KeymapReader {
        self.inner
            .as_ref()
            .map(|t| t.keymap_reader.clone())
            .unwrap_or_else(KeymapReader::empty)
    }

    pub fn hint_reader(&self) -> HintReader {
        self.inner
            .as_ref()
            .map(|t| t.hint_reader.clone())
            .unwrap_or_else(HintReader::empty)
    }

    pub fn ui_action_rx(&self) -> Option<flume::Receiver<UiAction>> {
        self.inner.as_ref().map(|t| t.ui_action_rx.clone())
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

    pub fn set_sandbox_config(&self, config: craft_config::SandboxConfig) {
        let _ = self.tx.send(Request::SetSandboxConfig { config });
    }

    pub fn run_command(&self, plugin: Arc<str>, command: Arc<str>, args: String) {
        let _ = self.tx.try_send(Request::RunCommand {
            plugin,
            command,
            args,
        });
    }

    pub fn collect_prompt_slots(&self) -> craft_agent::prompt::ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots { reply: tx });
        rx.recv().unwrap_or_default()
    }

    pub async fn collect_prompt_slots_async(&self) -> craft_agent::prompt::ResolvedSlots {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::CollectPromptSlots { reply: tx });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{LuaCommandInfo, LuaCommandWriter};
    use craft_agent::prompt::{PromptId, Slot};
    use craft_agent::tools::ToolRegistry;
    use std::time::Instant;

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

    #[test]
    fn load_builtins_on_disabled_host_is_noop() {
        let mut host = PluginHost::disabled();
        host.load_builtins(&PluginsConfig::from_plugins(HashMap::new()))
            .unwrap();
    }

    /// The second call sends `Shutdown` on a sender that is already
    /// disconnected; it must swallow that error and keep rejecting work.
    #[test]
    fn begin_shutdown_rejects_later_loads_and_is_idempotent() {
        let mut host = PluginHost::new(Arc::new(ToolRegistry::new()), None).unwrap();
        host.begin_shutdown();
        assert!(host.load_source("late", "return {}").is_err());
        host.begin_shutdown();
        assert!(host.load_source("later", "return {}").is_err());
    }

    #[test]
    fn begin_shutdown_on_disabled_host_is_noop() {
        PluginHost::disabled().begin_shutdown();
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
        let handle = host.event_handle().unwrap();
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

    #[test]
    fn sessions_builtin_registers_commands() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::with_all_builtins(Arc::clone(&reg)).unwrap();
        let snap = host.command_reader().load();
        let names: Vec<&str> = snap.commands.iter().map(|c| c.name.as_ref()).collect();
        assert!(
            names.contains(&"/sessions"),
            "expected /sessions command, got: {names:?}"
        );
        assert!(
            names.contains(&"/rename"),
            "expected /rename command, got: {names:?}"
        );
    }

    #[test]
    fn run_command_sends_correct_request() {
        let (tx, rx) = flume::bounded(8);
        let handle = EventHandle { tx };
        handle.run_command(Arc::from("myplugin"), Arc::from("/greet"), "world".into());
        let req = rx.try_recv().unwrap();
        match req {
            Request::RunCommand {
                plugin,
                command,
                args,
            } => {
                assert_eq!(plugin.as_ref(), "myplugin");
                assert_eq!(command.as_ref(), "/greet");
                assert_eq!(args, "world");
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

    #[test]
    fn disabled_host_returns_defaults() {
        let host = PluginHost::disabled();
        let snap = host.command_reader().load();
        assert_eq!(snap.commands.len(), 0);
        assert_eq!(snap.generation, 0);
        assert!(host.ui_action_rx().is_none());
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

        let handle = host.event_handle().expect("host is live");
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

    #[test_case::test_case(true ; "with_init_lua_present")]
    #[test_case::test_case(false ; "without_init_lua")]
    fn disabled_host_skips_init_files(with_init: bool) {
        let dir = tempfile::tempdir().unwrap();
        if with_init {
            fs::create_dir_all(dir.path().join(".craft")).unwrap();
            fs::write(
                dir.path().join(".craft/init.lua"),
                "error('should not run')",
            )
            .unwrap();
        }
        let host = PluginHost::disabled();
        let config = host
            .load_init_files(dir.path())
            .expect("disabled host skips init");
        assert!(config.is_none(), "disabled host returns no config");
    }

    #[test]
    fn disabled_host_skips_load_builtins() {
        let mut host = PluginHost::disabled();
        let config = PluginsConfig::from_plugins(HashMap::new());
        assert!(
            !config.names.is_empty(),
            "default config enables builtin plugins"
        );
        host.load_builtins(&config)
            .expect("disabled host skips builtin plugin load");
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
        let handle = host.event_handle().unwrap();
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
        let handle = host.event_handle().unwrap();
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
        let handle = host.event_handle().unwrap();
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
        let handle = host.event_handle().unwrap();
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
        let handle = host.event_handle().unwrap();
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
        let handle = host.event_handle().unwrap();
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
        let handle = host.event_handle().unwrap();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
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
        let slots = host.event_handle().unwrap().collect_prompt_slots();
        assert_eq!(
            hint_contents(&slots, PromptId::System, Slot::Identity),
            ["Dyn identity".to_string()]
        );
    }
}
