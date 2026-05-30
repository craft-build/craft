use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use include_dir::{Dir, include_dir};
use craft_agent::tools::ToolRegistry;
use craft_config::{PluginsConfig, RawConfig};

use crate::api::command::{LuaCommandReader, UiAction};
use crate::error::PluginError;
use crate::runtime::{self, ClickReply, LuaThread, Request, RestoreReply};
use serde_json::Value;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

struct BundledPlugin {
    name: &'static str,
    dir: Dir<'static>,
}

/// `lib` is not a default builtin; it only exists so plugins can
/// `require()` shared modules across plugin boundaries.
static BUNDLED_PLUGINS: &[BundledPlugin] = &[
    BundledPlugin {
        name: "index",
        dir: include_dir!("$CARGO_MANIFEST_DIR/../plugins/index"),
    },
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
    pub fn new(registry: Arc<ToolRegistry>) -> Result<Self, PluginError> {
        let lua = runtime::spawn(registry, *BUNDLED_DIRS)?;
        Ok(Self { inner: Some(lua) })
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn load_init_files(&self, cwd: &Path) -> Result<Option<RawConfig>, PluginError> {
        let mut merged: Option<RawConfig> = None;

        for global_dir in craft_config::global_config_dirs() {
            self.run_init_file(&global_dir.join("init.lua"), "global/init.lua", &mut merged)?;
        }
        self.run_init_file(&cwd.join(".craft/init.lua"), "project/init.lua", &mut merged)?;

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
        for builtin in &config.tools {
            let dir = match BUNDLED_PLUGINS.iter().find(|p| p.name == builtin.as_str()) {
                Some(p) => &p.dir,
                None => {
                    tracing::warn!(
                        builtin = builtin.as_str(),
                        "unknown builtin plugin, skipping"
                    );
                    continue;
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
            self.load_source_named(name, init.to_owned(), None)?;
        }
        Ok(())
    }

    fn tx(&self) -> Result<&flume::Sender<Request>, PluginError> {
        self.inner
            .as_ref()
            .map(|r| &r.tx)
            .ok_or(PluginError::HostDead)
    }

    fn send_load(
        &self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<(), PluginError> {
        let tx = self.tx()?;
        let (reply_tx, reply_rx) = flume::bounded(1);
        tx.send(Request::LoadSource {
            name,
            source,
            plugin_dir,
            reply: reply_tx,
        })
        .map_err(|_| PluginError::HostDead)?;
        reply_rx.recv().map_err(|_| PluginError::HostDead)?
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

    fn load_source_named(
        &mut self,
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
    ) -> Result<(), PluginError> {
        self.send_load(name, source, plugin_dir)
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
        self.send_load(Arc::from(name), source.to_owned(), None)
    }

    pub fn load_plugin_file(&self, path: &Path) -> Result<(), PluginError> {
        let source = fs::read_to_string(path).map_err(|e| PluginError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
        let plugin_dir = path.parent().map(Path::to_path_buf);
        let name: Arc<str> = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map_or_else(|| Arc::from("user"), Arc::from);
        self.send_load(name, source, plugin_dir)
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

    pub fn ui_action_rx(&self) -> Option<flume::Receiver<UiAction>> {
        self.inner.as_ref().map(|t| t.ui_action_rx.clone())
    }
}

#[derive(Clone)]
pub struct EventHandle {
    tx: flume::Sender<Request>,
}

impl EventHandle {
    pub fn fire_click(&self, tool_id: &str, row: u32) -> Option<ClickReply> {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.try_send(Request::FireBufClick {
            tool_id: tool_id.to_owned(),
            row,
            reply: tx,
        });
        rx.recv().ok().flatten()
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

    pub fn restore_tool(
        &self,
        tool: &str,
        tool_use_id: &str,
        output: &str,
        input: &Value,
        is_error: bool,
        tool_output_lines: &craft_config::ToolOutputLines,
    ) -> Option<RestoreReply> {
        let (tx, rx) = flume::bounded(1);
        let _ = self.tx.send(Request::RestoreTool {
            tool: Arc::from(tool),
            tool_use_id: tool_use_id.to_owned(),
            output: output.to_owned(),
            input: input.clone(),
            is_error,
            tool_output_lines: *tool_output_lines,
            reply: tx,
        });
        rx.recv().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::command::{LuaCommandInfo, LuaCommandWriter};
    use craft_agent::tools::ToolRegistry;

    #[test]
    fn command_writer_reader_pair_works() {
        let (writer, reader) = LuaCommandWriter::new();
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 0);

        writer.publish(vec![LuaCommandInfo {
            name: Arc::from("/test"),
            description: Arc::from("desc"),
            plugin: Arc::from("p"),
        }]);
        let snap = reader.load();
        assert_eq!(snap.commands.len(), 1);
        assert!(snap.generation > 0);
    }

    #[test]
    fn memory_builtin_registers_command() {
        let reg = Arc::new(ToolRegistry::new());
        let mut host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_builtins(&PluginsConfig::from_tools(std::collections::HashMap::new()))
            .unwrap();
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
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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

    #[test]
    fn callback_string_lands_in_targeted_prompt_only() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        assert!(handle.collect_prompt_slots()
            .get(
                craft_agent::prompt::PromptId::System,
                craft_agent::prompt::Slot::ToolUsage,
            )
            .is_empty());
    }

    #[test]
    fn static_no_prompt_lands_on_all_prompts_with_slot() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        assert!(slots.get(
            craft_agent::prompt::PromptId::Research,
            craft_agent::prompt::Slot::Conventions,
        ).is_empty());
        assert_eq!(slots.get(
            craft_agent::prompt::PromptId::System,
            craft_agent::prompt::Slot::Conventions,
        ).len(), 1);
    }

    #[test]
    fn explicit_prompt_without_slot_is_dropped() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        host.load_source(
            "bad_target",
            r#"
            craft.api.register_prompt_hint({
                slot = "after_instructions",
                prompt = "research",
                content = "DROPPED",
            })
            "#,
        )
        .unwrap();
        let handle = host.event_handle().unwrap();
        let slots = handle.collect_prompt_slots();
        assert!(slots.get(
            craft_agent::prompt::PromptId::Research,
            craft_agent::prompt::Slot::AfterInstructions,
        ).is_empty());
    }

    #[test]
    fn prompt_list_targets_each_listed_prompt() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        assert_eq!(slots.get(
            craft_agent::prompt::PromptId::System,
            craft_agent::prompt::Slot::ToolUsage,
        ).len(), 1);
        assert_eq!(slots.get(
            craft_agent::prompt::PromptId::Research,
            craft_agent::prompt::Slot::ToolUsage,
        ).len(), 1);
        assert!(slots.get(
            craft_agent::prompt::PromptId::General,
            craft_agent::prompt::Slot::ToolUsage,
        ).is_empty());
    }

    #[test]
    fn multiple_plugins_sorted_by_plugin_name() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        assert_eq!(entries[0].content, "from_aaa", "BTreeMap should sort by plugin name");
        assert_eq!(entries[1].content, "from_zzz");
    }

    #[test]
    fn unload_clears_all_hints_from_plugin() {
        let reg = Arc::new(ToolRegistry::new());
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
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
        assert!(handle.collect_prompt_slots()
            .get(
                craft_agent::prompt::PromptId::System,
                craft_agent::prompt::Slot::ToolUsage,
            )
            .is_empty());
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
        let host = PluginHost::new(Arc::clone(&reg)).unwrap();
        assert!(host.load_source("bad_hint", lua_code).is_err());
    }
}
