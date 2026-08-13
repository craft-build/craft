//! User configuration loading, validation, and permissions management.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use craft_config_macro::ConfigSection;
use craft_storage::paths;
use craft_storage::sessions::{StoredThinking, ThinkingParseError};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;
use tracing::warn;

pub mod model_roles;
pub mod providers;

const PROJECT_DIR: &str = ".craft";
const PERMISSIONS_FILE: &str = "permissions.toml";

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 50 * 1024;
pub const DEFAULT_MAX_OUTPUT_LINES: usize = 2000;
pub const DEFAULT_MAX_LINE_BYTES: usize = 3000;
pub const DEFAULT_FLASH_DURATION_MS: u64 = 1500;
pub const DEFAULT_TYPEWRITER_MS_PER_CHAR: u64 = 4;
pub const DEFAULT_MOUSE_SCROLL_LINES: u32 = 3;
pub const DEFAULT_MAX_INPUT_LINES: u32 = 20;

pub const DEFAULT_CODE_EXECUTION_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_MAX_CONTINUATION_TURNS: u32 = 3;
pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);
pub const DEFAULT_INTERPRETER_MAX_MEMORY_MB: usize = 50;

pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_LOW_SPEED_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_STREAM_TIMEOUT_SECS: u64 = 300;

pub const DEFAULT_MAX_LOG_BYTES_MB: u64 = 200;
pub const DEFAULT_MAX_LOG_FILES: u32 = 10;
pub const DEFAULT_INPUT_HISTORY_SIZE: usize = 100;

pub const MIN_OUTPUT_BYTES: usize = 1024;
pub const MIN_OUTPUT_LINES: usize = 10;
pub const MIN_LINE_BYTES: usize = 80;
pub const MIN_CODE_EXECUTION_TIMEOUT_SECS: u64 = 5;
pub const MIN_MAX_CONTINUATION_TURNS: u32 = 1;
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
const MAX_COMPACTION_PERCENT: u8 = 99;
const COMPACTION_BUFFER_EXPECTED: &str =
    r#"a token count (e.g. 12000) or a percent of the context window (e.g. "20%")"#;
pub const MIN_INTERPRETER_MAX_MEMORY_MB: usize = 10;
pub const MIN_MOUSE_SCROLL_LINES: u32 = 1;
pub const MIN_MAX_INPUT_LINES: u32 = 1;
pub const MIN_TOOL_OUTPUT_LINES: usize = 1;
pub const MIN_MAX_LOG_BYTES_MB: u64 = 1;
pub const MIN_MAX_LOG_FILES: u32 = 1;
pub const MIN_INPUT_HISTORY_SIZE: usize = 10;
pub const MIN_CONNECT_TIMEOUT_SECS: u64 = 1;
pub const MIN_LOW_SPEED_TIMEOUT_SECS: u64 = 1;
pub const MIN_STREAM_TIMEOUT_SECS: u64 = 10;

pub const DEFAULT_COMPRESSION_ENABLED: bool = true;
pub const DEFAULT_COMPRESSION_MAX_LOG_LINES: usize = 50;
pub const DEFAULT_COMPRESSION_MAX_SEARCH_FILES: usize = 20;
pub const DEFAULT_COMPRESSION_MAX_MATCHES_PER_FILE: usize = 5;
pub const DEFAULT_COMPRESSION_MAX_DIFF_LINES: usize = 100;
pub const DEFAULT_COMPRESSION_MAX_JSON_ITEMS: usize = 15;
pub const DEFAULT_COMPRESSION_JSON_FIRST_KEEP: usize = 5;
pub const DEFAULT_COMPRESSION_JSON_LAST_KEEP: usize = 3;
pub const DEFAULT_COMPRESSION_PROTECT_RECENT: usize = 2;
pub const DEFAULT_COMPRESSION_CODE_RATE: f32 = 0.3;

pub const DEFAULT_BUILTINS: &[&str] = &[
    "bash",
    "glob",
    "grep",
    "memory",
    "question",
    "sessions",
    "skill",
    "todo_write",
    "view_image",
    "webfetch",
    "websearch",
];

pub const OPT_IN_TOOLS: &[&str] = &["edit_lines", "insert_lines"];

pub const FILE_WRITE_TOOLS: &[&str] = &["write", "edit", "multiedit", "edit_lines", "insert_lines"];

#[derive(Debug, Clone, Copy)]
pub enum ConfigValue {
    Bool(bool),
    U64(u64),
    Str(&'static str),
}

impl ConfigValue {
    pub fn format_default(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::U64(v) => v.to_string(),
            Self::Str(s) => (*s).to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigField {
    pub name: &'static str,
    pub ty: &'static str,
    pub default: ConfigValue,
    pub min: Option<u64>,
    pub description: &'static str,
}

pub const TOP_LEVEL_FIELDS: &[ConfigField] = &[
    ConfigField {
        name: "always_yolo",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with YOLO mode (skip permission prompts, deny rules still apply)",
    },
    ConfigField {
        name: "always_auto_review",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with auto-review (an LLM auto-decides allow/deny on permission prompts instead of asking)",
    },
    ConfigField {
        name: "always_fast",
        ty: "bool",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with Anthropic fast mode (Opus only; ignored otherwise)",
    },
    ConfigField {
        name: "always_thinking",
        ty: "bool | string",
        default: ConfigValue::Bool(false),
        min: None,
        description: "Start every session with extended thinking (true/\"adaptive\", \"off\", an effort level (\"minimal\" to \"max\"), or a token budget)",
    },
];

/// Error type for permission file operations.
#[derive(Debug, Error)]
pub enum PermissionError {
    #[error("cannot determine home directory")]
    NoHomeDir,
    #[error("failed to parse permissions: {0}")]
    Parse(#[source] toml_edit::TomlError),
    #[error("[{tool}] is not a table")]
    NotATable { tool: String },
    #[error("[{tool}].{key} is not an array")]
    NotAnArray { tool: String, key: String },
    #[error("cannot create config dir: {0}")]
    CreateDir(#[source] std::io::Error),
    #[error("cannot write permissions: {0}")]
    Write(#[source] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid config: {section}.{field} = {value} is below minimum ({min})")]
    BelowMinimum {
        section: &'static str,
        field: &'static str,
        value: u64,
        min: u64,
    },
    #[error("invalid config: always_thinking: {0}")]
    Thinking(#[from] ThinkingParseError),
    #[error(
        "invalid config: the `tools` table in craft.setup was renamed to `plugins` \
         (plugins can provide more than tools).\n\n\
         Fix your config with:\n\n    \
         sed -i.bak 's/^\\( *\\)tools *=/\\1plugins =/' ~/.config/craft/init.lua\n\n\
         Run it on .craft/init.lua too if you keep a project config. \
         A .bak backup is left next to the file."
    )]
    RenamedToolsTable,
    #[error(
        "invalid config: plugins.{plugin}: no bundled plugin is named \"{plugin}\" \
         (bundled plugins: {valid})"
    )]
    UnknownPlugin { plugin: String, valid: String },
    #[error("invalid config: provider.{field} contains invalid glob pattern `{pattern}`: {source}")]
    InvalidModelPattern {
        field: &'static str,
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

fn check(
    section: &'static str,
    field: &'static str,
    value: u64,
    min: u64,
) -> Result<(), ConfigError> {
    if value < min {
        return Err(ConfigError::BelowMinimum {
            section,
            field,
            value,
            min,
        });
    }
    Ok(())
}

macro_rules! merge_option {
    ($self:ident, $overlay:ident, $($field:ident),+) => {
        $(if $overlay.$field.is_some() { $self.$field = $overlay.$field; })+
    };
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum AlwaysThinking {
    Toggle(bool),
    Budget(u32),
    Mode(String),
}

impl AlwaysThinking {
    fn resolve(self) -> Result<StoredThinking, ThinkingParseError> {
        match self {
            Self::Toggle(true) => Ok(StoredThinking::Adaptive),
            Self::Toggle(false) => Ok(StoredThinking::Off),
            Self::Budget(n) => StoredThinking::parse_setting(&n.to_string()),
            Self::Mode(s) => StoredThinking::parse_setting(&s),
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct RawConfig {
    pub always_yolo: Option<bool>,
    pub always_auto_review: Option<bool>,
    pub always_fast: Option<bool>,
    pub always_thinking: Option<AlwaysThinking>,
    #[serde(default)]
    pub ui: UiFileConfig,
    pub agent: AgentFileConfig,
    pub provider: ProviderFileConfig,
    pub storage: StorageFileConfig,
    #[serde(default)]
    pub compression: CompressionFileConfig,
    #[serde(default)]
    pub sandbox: SandboxFileConfig,
    #[serde(default)]
    pub repomap: RepoMapFileConfig,
    #[serde(default)]
    pub watch: WatchFileConfig,
    pub plugins: HashMap<String, PluginFileConfig>,
    /// Renamed to `plugins`; kept so old configs fail with a pointer to the
    /// new name instead of a generic unknown-field error.
    tools: HashMap<String, PluginFileConfig>,
}

impl RawConfig {
    pub fn merge(&mut self, overlay: RawConfig) {
        merge_option!(
            self,
            overlay,
            always_yolo,
            always_auto_review,
            always_fast,
            always_thinking
        );
        self.ui.merge(overlay.ui);
        self.agent.merge(overlay.agent);
        self.provider.merge(overlay.provider);
        self.storage.merge(overlay.storage);
        self.compression.merge(overlay.compression);
        self.sandbox.merge(overlay.sandbox);
        self.repomap.merge(overlay.repomap);
        self.watch.merge(overlay.watch);
        for (name, plugin) in overlay.plugins {
            let entry = self.plugins.entry(name).or_default();
            if plugin.enabled.is_some() {
                entry.enabled = plugin.enabled;
            }
            entry.opts.extend(plugin.opts);
        }
        self.tools.extend(overlay.tools);
    }

    pub fn into_config(self, no_rtk: bool) -> Result<Config, ConfigError> {
        self.validate_plugin_tables()?;
        let mut disabled_tools: Vec<String> = self
            .plugins
            .iter()
            .filter(|(_, cfg)| cfg.enabled == Some(false))
            .map(|(name, _)| name.clone())
            .collect();
        for &name in OPT_IN_TOOLS {
            if self.plugins.get(name).and_then(|t| t.enabled) != Some(true) {
                disabled_tools.push(name.to_string());
            }
        }
        let mut agent = AgentConfig::from_file(self.agent, no_rtk, disabled_tools);
        let repomap = RepoMapConfig::from_file(self.repomap);
        agent.repomap = repomap.clone();
        Ok(Config {
            always_yolo: self.always_yolo.unwrap_or(false),
            always_auto_review: self.always_auto_review.unwrap_or(false),
            always_fast: self.always_fast.unwrap_or(false),
            always_thinking: self
                .always_thinking
                .map(AlwaysThinking::resolve)
                .transpose()?,
            ui: UiConfig::from_file(self.ui),
            agent,
            provider: ProviderConfig::from_file(self.provider)?,
            storage: StorageConfig::from_file(self.storage),
            compression: CompressionConfig::from_file(self.compression),
            sandbox: SandboxConfig::from_file(self.sandbox),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::from_plugins(self.plugins),
            repomap,
            watch: WatchConfig::from_file(self.watch),
        })
    }

    /// A `plugins.<name>` key that matches no bundled plugin is a typo or an
    /// old config, so fail loudly instead of letting it silently drift.
    /// The legacy `tools` table is rejected with a pointer to `plugins`.
    /// `OPT_IN_TOOLS` names (native edit sub-tools toggled through the
    /// plugins table) are accepted even though they are not Lua plugins.
    fn validate_plugin_tables(&self) -> Result<(), ConfigError> {
        if !self.tools.is_empty() {
            return Err(ConfigError::RenamedToolsTable);
        }
        let mut unknown: Vec<&String> = self
            .plugins
            .keys()
            .filter(|name| {
                !DEFAULT_BUILTINS.contains(&name.as_str()) && !OPT_IN_TOOLS.contains(&name.as_str())
            })
            .collect();
        unknown.sort();
        if let Some(&plugin) = unknown.first() {
            return Err(ConfigError::UnknownPlugin {
                plugin: plugin.clone(),
                valid: DEFAULT_BUILTINS.join(", "),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default)]
pub struct PluginFileConfig {
    pub enabled: Option<bool>,
    /// Plugin-specific options passed through opaquely; each plugin declares
    /// and validates its own via `craft.api.register_options`.
    #[serde(flatten)]
    pub opts: JsonMap<String, JsonValue>,
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct UiFileConfig {
    pub splash_animation: Option<bool>,
    pub scrollbar: Option<bool>,
    pub flash_duration_ms: Option<u64>,
    pub typewriter_ms_per_char: Option<u64>,
    pub mouse_scroll_lines: Option<u32>,
    pub max_input_lines: Option<u32>,
    pub show_thinking: Option<bool>,
    pub theme: Option<String>,
    pub clock_format: Option<ClockFormat>,
    pub tool_output_lines: Option<ToolOutputLinesFile>,
    pub keybindings: Option<HashMap<String, KeybindingOverride>>,
}

/// A keybinding override: a single chord, a list of chords, or null/empty to disable.
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum KeybindingOverride {
    Chord(String),
    Chords(Vec<String>),
}

impl KeybindingOverride {
    pub fn into_chords(self) -> Vec<String> {
        match self {
            Self::Chord(s) => vec![s],
            Self::Chords(v) => v,
        }
    }
}

impl UiFileConfig {
    fn merge(&mut self, overlay: UiFileConfig) {
        merge_option!(
            self,
            overlay,
            splash_animation,
            scrollbar,
            flash_duration_ms,
            typewriter_ms_per_char,
            mouse_scroll_lines,
            max_input_lines,
            show_thinking,
            theme,
            clock_format
        );
        match (self.tool_output_lines.as_mut(), overlay.tool_output_lines) {
            (Some(base), Some(over)) => base.merge(over),
            (None, Some(over)) => self.tool_output_lines = Some(over),
            _ => {}
        }
        if let Some(over) = overlay.keybindings {
            match self.keybindings.as_mut() {
                Some(base) => base.extend(over),
                None => self.keybindings = Some(over),
            }
        }
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ToolOutputLinesFile {
    pub bash: Option<usize>,
    pub code_execution: Option<usize>,
    pub task: Option<usize>,
    pub grep: Option<usize>,
    pub read: Option<usize>,
    pub write: Option<usize>,
    pub web: Option<usize>,
    pub other: Option<usize>,
}

impl ToolOutputLinesFile {
    fn merge(&mut self, overlay: ToolOutputLinesFile) {
        merge_option!(
            self,
            overlay,
            bash,
            code_execution,
            task,
            grep,
            read,
            write,
            web,
            other
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBuffer {
    Tokens(u32),
    Percent(u8),
}

impl CompactionBuffer {
    pub fn resolve(self, context_window: u32) -> u32 {
        match self {
            Self::Tokens(n) => n,
            Self::Percent(p) => (u64::from(context_window) * u64::from(p) / 100) as u32,
        }
    }
}

impl Serialize for CompactionBuffer {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Tokens(n) => s.serialize_u32(*n),
            Self::Percent(p) => s.collect_str(&format_args!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for CompactionBuffer {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BufferVisitor;

        impl serde::de::Visitor<'_> for BufferVisitor {
            type Value = CompactionBuffer;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(COMPACTION_BUFFER_EXPECTED)
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u32::try_from(v)
                    .ok()
                    .filter(|n| *n >= MIN_COMPACTION_BUFFER)
                    .map(CompactionBuffer::Tokens)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "compaction_buffer must be at least {MIN_COMPACTION_BUFFER} tokens"
                        ))
                    })
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                self.visit_u64(u64::try_from(v).unwrap_or(0))
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
                s.strip_suffix('%')
                    .and_then(|n| n.trim().parse::<u8>().ok())
                    .filter(|p| (1..=MAX_COMPACTION_PERCENT).contains(p))
                    .map(CompactionBuffer::Percent)
                    .ok_or_else(|| {
                        E::custom(format!(
                            "invalid compaction_buffer {s:?}: expected {COMPACTION_BUFFER_EXPECTED}"
                        ))
                    })
            }
        }

        d.deserialize_any(BufferVisitor)
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct AgentFileConfig {
    pub max_output_bytes: Option<usize>,
    pub max_output_lines: Option<usize>,
    pub max_line_bytes: Option<usize>,
    pub code_execution_timeout_secs: Option<u64>,
    pub max_continuation_turns: Option<u32>,
    pub compaction_buffer: Option<CompactionBuffer>,
    pub compaction_instructions: Option<String>,
    pub post_compaction_instructions: Option<String>,
    pub stale_read_check: Option<bool>,
    pub interpreter_max_memory_mb: Option<usize>,
    #[serde(default)]
    pub trust_decay: TrustDecayConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
    #[serde(default)]
    pub format: FormatConfig,
    #[serde(default)]
    pub small_model: SmallModelConfig,
    #[serde(default)]
    pub dynamic_tools: DynamicToolsConfig,
    #[serde(default)]
    pub advisor: AdvisorConfig,
    pub flow: Option<FlowConfig>,
    #[serde(default)]
    pub ttsr: TtsrConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    pub hooks_enabled: Option<bool>,
    pub judge_model: Option<String>,
}

impl AgentFileConfig {
    fn merge(&mut self, overlay: AgentFileConfig) {
        merge_option!(
            self,
            overlay,
            max_output_bytes,
            max_output_lines,
            max_line_bytes,
            code_execution_timeout_secs,
            max_continuation_turns,
            compaction_buffer,
            stale_read_check,
            interpreter_max_memory_mb,
            hooks_enabled,
            judge_model
        );
        if overlay.advisor.enabled || overlay.advisor.model.is_some() {
            self.advisor = overlay.advisor;
        }
        if let Some(flow) = overlay.flow {
            self.flow = Some(flow);
        }
        if overlay.ttsr.enabled {
            self.ttsr = overlay.ttsr;
        }
        self.compaction.merge(overlay.compaction);
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderFileConfig {
    pub default_model: Option<String>,
    pub allowed_models: Option<Vec<String>>,
    pub excluded_models: Option<Vec<String>>,
    pub connect_timeout_secs: Option<u64>,
    pub low_speed_timeout_secs: Option<u64>,
    pub stream_timeout_secs: Option<u64>,
}

impl ProviderFileConfig {
    fn merge(&mut self, overlay: ProviderFileConfig) {
        merge_option!(
            self,
            overlay,
            default_model,
            allowed_models,
            excluded_models,
            connect_timeout_secs,
            low_speed_timeout_secs,
            stream_timeout_secs
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct StorageFileConfig {
    pub max_log_bytes_mb: Option<u64>,
    pub max_log_files: Option<u32>,
    pub input_history_size: Option<usize>,
}

impl StorageFileConfig {
    fn merge(&mut self, overlay: StorageFileConfig) {
        merge_option!(
            self,
            overlay,
            max_log_bytes_mb,
            max_log_files,
            input_history_size
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionFileConfig {
    pub enabled: Option<bool>,
    pub code_compression_rate: Option<f32>,
    pub max_log_lines: Option<usize>,
    pub max_search_files: Option<usize>,
    pub max_matches_per_file: Option<usize>,
    pub max_diff_lines: Option<usize>,
    pub max_json_items: Option<usize>,
    pub json_first_keep: Option<usize>,
    pub json_last_keep: Option<usize>,
    pub protect_recent_tool_outputs: Option<usize>,
}

impl CompressionFileConfig {
    fn merge(&mut self, overlay: CompressionFileConfig) {
        merge_option!(
            self,
            overlay,
            enabled,
            code_compression_rate,
            max_log_lines,
            max_search_files,
            max_matches_per_file,
            max_diff_lines,
            max_json_items,
            json_first_keep,
            json_last_keep,
            protect_recent_tool_outputs
        );
    }
}

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxFileConfig {
    pub enabled: Option<bool>,
    pub mode: Option<SandboxMode>,
    pub network: Option<bool>,
}

impl SandboxFileConfig {
    fn merge(&mut self, overlay: SandboxFileConfig) {
        merge_option!(self, overlay, enabled, mode, network);
    }
}

#[derive(Default)]
struct PermissionsFileConfig {
    default: Option<DefaultEffect>,
    tools: HashMap<String, ToolPermissions>,
    mcp_rules: Vec<PermissionRule>,
    mcp_defaults: HashMap<ToolKey, DefaultEffect>,
}

impl PermissionsFileConfig {
    fn merge(&mut self, overlay: PermissionsFileConfig) {
        if overlay.default.is_some() {
            self.default = overlay.default;
        }
        self.tools.extend(overlay.tools);
        self.mcp_rules.extend(overlay.mcp_rules);
        self.mcp_defaults.extend(overlay.mcp_defaults);
    }
}

impl<'de> Deserialize<'de> for PermissionsFileConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let table = toml::Table::deserialize(deserializer)?;
        let default = table
            .get("default")
            .and_then(|v| DefaultEffect::deserialize(v.clone()).ok())
            .or_else(|| {
                table
                    .get("allow_all")?
                    .as_bool()?
                    .then_some(DefaultEffect::Allow)
            });

        let mut tools = HashMap::new();
        let mut mcp_rules = Vec::new();
        let mut mcp_defaults = HashMap::new();

        for (k, v) in table.iter() {
            if k.is_empty() || k == "allow_all" || k == "default" {
                continue;
            }
            if k == "mcp" {
                if let Some(mcp_table) = v.as_table() {
                    for (server_name, server_value) in mcp_table {
                        if let Some(server_table) = server_value.as_table() {
                            parse_mcp_server_table(
                                server_name,
                                server_table,
                                &mut mcp_rules,
                                &mut mcp_defaults,
                            );
                        } else {
                            tracing::warn!(
                                server = server_name.as_str(),
                                "[mcp.{}] is not a table — skipping",
                                server_name
                            );
                        }
                    }
                } else {
                    tracing::warn!("[mcp] is not a table (got {}) — skipping", v.type_str());
                }
            } else if let Ok(tp) = v.clone().try_into::<ToolPermissions>() {
                if k.contains('.') {
                    tracing::warn!(
                        key = k.as_str(),
                        "tool section [{}] contains a dot — did you mean [mcp.{}]? Skipping.",
                        k,
                        k
                    );
                } else {
                    tools.insert(k.clone(), tp);
                }
            }
        }

        Ok(Self {
            default,
            tools,
            mcp_rules,
            mcp_defaults,
        })
    }
}

#[derive(Deserialize)]
struct ToolPermissions {
    allow: Option<ScopeSet>,
    deny: Option<ScopeSet>,
    default: Option<DefaultEffect>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeSet {
    All(bool),
    Scopes(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    Allow,
    Deny,
    #[default]
    Prompt,
}

impl From<Effect> for DefaultEffect {
    fn from(e: Effect) -> Self {
        match e {
            Effect::Allow => DefaultEffect::Allow,
            Effect::Deny => DefaultEffect::Deny,
        }
    }
}

#[derive(Debug, Clone)]
pub enum PermissionTarget {
    Global,
    Project(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolKey {
    Wildcard,
    Native(Arc<str>),
    McpServer { server: Arc<str> },
    McpTool { server: Arc<str>, tool: Arc<str> },
}

impl serde::Serialize for ToolKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// Check if a name matches the LLM wire format: `^[a-zA-Z0-9_-]{1,64}$`.
pub fn is_valid_wire_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl ToolKey {
    pub fn parse(name: &str) -> Result<Self, ToolKeyParseError> {
        if name.is_empty() {
            return Err(ToolKeyParseError::EmptyName);
        }
        if name == "*" {
            return Ok(Self::Wildcard);
        }
        match name.split_once('.') {
            Some(("", _)) | Some((_, "")) => {
                Err(ToolKeyParseError::MalformedParts(name.to_string()))
            }
            Some((server, "*")) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                Ok(Self::McpServer {
                    server: server.into(),
                })
            }
            Some((server, tool)) => {
                if !is_valid_server_name(server) {
                    return Err(ToolKeyParseError::InvalidServerName(server.to_string()));
                }
                if !is_valid_wire_name(tool) {
                    return Err(ToolKeyParseError::InvalidToolName(tool.to_string()));
                }
                let wire_len = server.len() + 2 + tool.len();
                if wire_len > 64 {
                    return Err(ToolKeyParseError::WireNameTooLong {
                        server: server.to_string(),
                        tool: tool.to_string(),
                        len: wire_len,
                    });
                }
                Ok(Self::McpTool {
                    server: server.into(),
                    tool: tool.into(),
                })
            }
            None => {
                if !is_valid_wire_name(name) {
                    return Err(ToolKeyParseError::InvalidToolName(name.to_string()));
                }
                Ok(Self::Native(name.into()))
            }
        }
    }

    pub fn native(name: &str) -> Self {
        match name {
            "*" => Self::Wildcard,
            _ => {
                assert!(!name.is_empty(), "native tool name must not be empty");
                assert!(
                    !name.contains('.'),
                    "native tool name must not contain dots: {name:?} - use ToolKey::parse for MCP tools"
                );
                Self::Native(name.into())
            }
        }
    }

    pub fn is_mcp(&self) -> bool {
        matches!(self, Self::McpServer { .. } | Self::McpTool { .. })
    }
}

impl std::fmt::Display for ToolKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wildcard => write!(f, "*"),
            Self::Native(name) => write!(f, "{name}"),
            Self::McpServer { server } => write!(f, "{server}.*"),
            Self::McpTool { server, tool } => write!(f, "{server}.{tool}"),
        }
    }
}

/// Error returned when a tool key string fails validation.
#[derive(Debug, thiserror::Error)]
pub enum ToolKeyParseError {
    #[error("tool name is empty")]
    EmptyName,
    #[error("malformed tool key: empty server or tool part in {0:?}")]
    MalformedParts(String),
    #[error("invalid server name {0:?}: must match [a-zA-Z0-9-]{{1,64}}")]
    InvalidServerName(String),
    #[error("invalid tool name {0:?}: must match [a-zA-Z0-9_-]{{1,64}}")]
    InvalidToolName(String),
    #[error("wire name {server}__{tool} is {len} chars, max 64")]
    WireNameTooLong {
        server: String,
        tool: String,
        len: usize,
    },
}

#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub tool: ToolKey,
    pub scope: Option<String>,
    pub effect: Effect,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionsConfig {
    pub default: DefaultEffect,
    pub tool_defaults: HashMap<ToolKey, DefaultEffect>,
    pub rules: Vec<PermissionRule>,
    pub yolo: bool,
    pub auto_review: bool,
}

#[derive(Clone)]
pub struct Config {
    pub always_yolo: bool,
    pub always_auto_review: bool,
    pub always_fast: bool,
    pub always_thinking: Option<StoredThinking>,
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub provider: ProviderConfig,
    pub storage: StorageConfig,
    pub compression: CompressionConfig,
    pub sandbox: SandboxConfig,
    pub permissions: PermissionsConfig,
    pub plugins: PluginsConfig,
    pub repomap: RepoMapConfig,
    pub watch: WatchConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub enum ClockFormat {
    #[serde(rename = "12h")]
    Hour12,
    #[serde(rename = "24h")]
    Hour24,
    #[default]
    #[serde(rename = "system")]
    System,
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "ui")]
pub struct UiConfig {
    #[config(default = true, desc = "Show splash animation on startup")]
    pub splash_animation: bool,

    #[config(default = true, desc = "Show vertical scrollbar in scrollable areas")]
    pub scrollbar: bool,

    #[config(default = DEFAULT_FLASH_DURATION_MS, desc = "Duration of flash messages (ms)")]
    pub flash_duration_ms: u64,

    #[config(default = DEFAULT_TYPEWRITER_MS_PER_CHAR, desc = "Typewriter effect speed (ms/char)")]
    pub typewriter_ms_per_char: u64,

    #[config(default = DEFAULT_MOUSE_SCROLL_LINES, min = MIN_MOUSE_SCROLL_LINES, desc = "Lines per mouse wheel scroll")]
    pub mouse_scroll_lines: u32,

    #[config(default = DEFAULT_MAX_INPUT_LINES, min = MIN_MAX_INPUT_LINES, desc = "Maximum visible input lines")]
    pub max_input_lines: u32,

    #[config(
        default = true,
        desc = "When true (default), show full model reasoning live and persisted. When false, hide reasoning behind an indicator (thinking> ...) with a click-to-expand hint, both while thinking and after it completes"
    )]
    pub show_thinking: bool,

    #[config(default = ClockFormat::System, ty = "String", default_doc = "system", desc = "Clock format for timestamps: \"12h\", \"24h\", or \"system\" (follow the OS preference, 24h when unknown)")]
    pub clock_format: ClockFormat,

    #[config(skip, default = "None")]
    pub theme: Option<String>,

    #[config(skip, default = "ToolOutputLines::default()")]
    pub tool_output_lines: ToolOutputLines,

    #[config(skip, default = "KeybindingsConfig::default()")]
    pub keybindings: KeybindingsConfig,
}

/// Resolved user keybinding overrides as `(snake_case action id, chords)`.
/// An empty chords list disables the action.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingsConfig {
    pub entries: Vec<(String, Vec<String>)>,
}

impl KeybindingsConfig {
    pub fn from_file(f: Option<HashMap<String, KeybindingOverride>>) -> Self {
        let Some(map) = f else {
            return Self::default();
        };
        let mut entries: Vec<(String, Vec<String>)> = map
            .into_iter()
            .map(|(id, v)| (id, v.into_chords()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl UiConfig {
    pub fn flash_duration(&self) -> Duration {
        Duration::from_millis(self.flash_duration_ms)
    }

    fn from_file(f: UiFileConfig) -> Self {
        Self {
            splash_animation: f.splash_animation.unwrap_or(true),
            scrollbar: f.scrollbar.unwrap_or(true),
            flash_duration_ms: f.flash_duration_ms.unwrap_or(DEFAULT_FLASH_DURATION_MS),
            typewriter_ms_per_char: f
                .typewriter_ms_per_char
                .unwrap_or(DEFAULT_TYPEWRITER_MS_PER_CHAR),
            mouse_scroll_lines: f.mouse_scroll_lines.unwrap_or(DEFAULT_MOUSE_SCROLL_LINES),
            max_input_lines: f.max_input_lines.unwrap_or(DEFAULT_MAX_INPUT_LINES),
            show_thinking: f.show_thinking.unwrap_or(true),
            clock_format: f.clock_format.unwrap_or_default(),
            theme: f.theme,
            tool_output_lines: ToolOutputLines::from_file(f.tool_output_lines),
            keybindings: KeybindingsConfig::from_file(f.keybindings),
        }
    }

    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        self.tool_output_lines.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputLines {
    pub bash: usize,
    pub code_execution: usize,
    pub task: usize,
    pub grep: usize,
    pub read: usize,
    pub write: usize,
    pub web: usize,
    pub other: usize,
}

impl ToolOutputLines {
    pub const DEFAULT: Self = Self {
        bash: 5,
        code_execution: 5,
        task: 5,
        grep: 3,
        read: 3,
        write: 7,
        web: 3,
        other: 3,
    };

    pub const FIELD_DEFAULTS: &[(&'static str, usize)] = &[
        ("bash", Self::DEFAULT.bash),
        ("code_execution", Self::DEFAULT.code_execution),
        ("task", Self::DEFAULT.task),
        ("grep", Self::DEFAULT.grep),
        ("read", Self::DEFAULT.read),
        ("write", Self::DEFAULT.write),
        ("web", Self::DEFAULT.web),
        ("other", Self::DEFAULT.other),
    ];

    fn from_file(f: Option<ToolOutputLinesFile>) -> Self {
        let d = Self::DEFAULT;
        let f = f.unwrap_or_default();
        Self {
            bash: f.bash.unwrap_or(d.bash),
            code_execution: f.code_execution.unwrap_or(d.code_execution),
            task: f.task.unwrap_or(d.task),
            grep: f.grep.unwrap_or(d.grep),
            read: f.read.unwrap_or(d.read),
            write: f.write.unwrap_or(d.write),
            web: f.web.unwrap_or(d.web),
            other: f.other.unwrap_or(d.other),
        }
    }

    fn fields(&self) -> [(&'static str, usize); 8] {
        [
            ("bash", self.bash),
            ("code_execution", self.code_execution),
            ("task", self.task),
            ("grep", self.grep),
            ("read", self.read),
            ("write", self.write),
            ("web", self.web),
            ("other", self.other),
        ]
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        for (name, value) in self.fields() {
            check(
                "ui.tool_output_lines",
                name,
                value as u64,
                MIN_TOOL_OUTPUT_LINES as u64,
            )?;
        }
        Ok(())
    }

    pub fn get(&self, name: &str) -> usize {
        match name {
            "bash" => self.bash,
            "code_execution" => self.code_execution,
            "task" => self.task,
            "grep" | "glob" => self.grep,
            "read" => self.read,
            "memory" => self.write,
            name if FILE_WRITE_TOOLS.contains(&name) => self.write,
            "webfetch" | "websearch" => self.web,
            _ => self.other,
        }
    }
}

impl Default for ToolOutputLines {
    fn default() -> Self {
        Self::DEFAULT
    }
}

const DEFAULT_WARN_AFTER: u32 = 3;
const DEFAULT_DROP_AFTER: u32 = 5;
const DEFAULT_MIN_TOOLS: usize = 5;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct TrustDecayConfig {
    #[serde(default = "default_warn_after")]
    pub warn_after: u32,
    #[serde(default = "default_drop_after")]
    pub drop_after: u32,
    #[serde(default = "default_min_tools")]
    pub min_tools: usize,
    #[serde(default = "default_true")]
    pub reset_on_success: bool,
}

fn default_warn_after() -> u32 {
    DEFAULT_WARN_AFTER
}
fn default_drop_after() -> u32 {
    DEFAULT_DROP_AFTER
}
fn default_min_tools() -> usize {
    DEFAULT_MIN_TOOLS
}
fn default_true() -> bool {
    true
}

impl Default for TrustDecayConfig {
    fn default() -> Self {
        Self {
            warn_after: DEFAULT_WARN_AFTER,
            drop_after: DEFAULT_DROP_AFTER,
            min_tools: DEFAULT_MIN_TOOLS,
            reset_on_success: true,
        }
    }
}

const DEFAULT_MAX_VALIDATION_ITERATIONS: u8 = 3;
const DEFAULT_VALIDATION_TIMEOUT_SECS: u64 = 30;
const DEFAULT_FORMAT_TIMEOUT_SECS: u64 = 15;
const DEFAULT_COMPACTION_THRESHOLD: f64 = 0.50;
const DEFAULT_CONTEXT_WINDOW_THRESHOLD: u32 = 32_000;
const DEFAULT_ADVISOR_DEDUP_SIZE: usize = 16;
const DEFAULT_ADVISOR_MAX_ACT_TURNS: u32 = 2;
const DEFAULT_FLOW_MAX_REVIEW_ITERATIONS: u32 = 3;
const DEFAULT_FLOW_MAX_QA_ITERATIONS: u32 = 2;
const DEFAULT_FLOW_PARALLEL_CHUNKS: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValidationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_validation_iterations")]
    pub max_iterations: u8,
    pub command: Option<String>,
    #[serde(default = "default_validation_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_max_validation_iterations() -> u8 {
    DEFAULT_MAX_VALIDATION_ITERATIONS
}

fn default_validation_timeout_secs() -> u64 {
    DEFAULT_VALIDATION_TIMEOUT_SECS
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_iterations: DEFAULT_MAX_VALIDATION_ITERATIONS,
            command: None,
            timeout_secs: default_validation_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FormatConfig {
    #[serde(default)]
    pub enabled: bool,
    pub command: Option<String>,
    #[serde(default = "default_format_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_format_timeout_secs() -> u64 {
    DEFAULT_FORMAT_TIMEOUT_SECS
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: None,
            timeout_secs: default_format_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct SmallModelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub reduced_tools: bool,
    #[serde(default)]
    pub compact_prompt: bool,
    #[serde(default)]
    pub aggressive_truncation: bool,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,
    #[serde(default = "default_true")]
    pub forgiving_parsing: bool,
    #[serde(default = "default_context_window_threshold")]
    pub auto_detect_context_window: u32,
}

fn default_compaction_threshold() -> f64 {
    DEFAULT_COMPACTION_THRESHOLD
}
fn default_context_window_threshold() -> u32 {
    DEFAULT_CONTEXT_WINDOW_THRESHOLD
}

impl Default for SmallModelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reduced_tools: false,
            compact_prompt: false,
            aggressive_truncation: false,
            compaction_threshold: DEFAULT_COMPACTION_THRESHOLD,
            forgiving_parsing: true,
            auto_detect_context_window: DEFAULT_CONTEXT_WINDOW_THRESHOLD,
        }
    }
}

impl SmallModelConfig {
    pub fn should_activate(&self, context_window: u32) -> bool {
        if self.enabled {
            return true;
        }
        context_window > 0 && context_window < self.auto_detect_context_window
    }
}

const MIN_COMPACT_PERCENT: u8 = 1;
const MAX_COMPACT_PERCENT: u8 = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct ModelThreshold {
    #[serde(default)]
    pub reserve_tokens: Option<u32>,
    #[serde(default)]
    pub compact_percent: Option<u8>,
    #[serde(default)]
    pub keep_recent_tokens: Option<u32>,
}

impl ModelThreshold {
    fn merge_from(&mut self, overlay: ModelThreshold) {
        if overlay.reserve_tokens.is_some() {
            self.reserve_tokens = overlay.reserve_tokens;
        }
        if overlay.compact_percent.is_some() {
            self.compact_percent = overlay.compact_percent;
        }
        if overlay.keep_recent_tokens.is_some() {
            self.keep_recent_tokens = overlay.keep_recent_tokens;
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CompactionConfig {
    #[serde(default)]
    pub model_thresholds: HashMap<String, ModelThreshold>,
    #[serde(default)]
    pub global_threshold: Option<ModelThreshold>,
}

impl CompactionConfig {
    fn merge(&mut self, overlay: CompactionConfig) {
        for (key, over) in &overlay.model_thresholds {
            self.model_thresholds
                .entry(key.clone())
                .or_default()
                .merge_from(*over);
        }
        if overlay.global_threshold.is_some() {
            self.global_threshold = overlay.global_threshold;
        }
    }
}

/// Resolve the effective [`ModelThreshold`] for a given model.
///
/// Lookup order: exact `provider/modelId` key, then `modelId`, then
/// `global_threshold`. Returns `None` when no override applies.
pub fn resolve_threshold<'a>(
    config: &'a CompactionConfig,
    provider_model_id: Option<&str>,
    model_id: &str,
) -> Option<&'a ModelThreshold> {
    if let Some(key) = provider_model_id
        && let Some(t) = config.model_thresholds.get(key)
    {
        return Some(t);
    }
    if let Some(t) = config.model_thresholds.get(model_id) {
        return Some(t);
    }
    config.global_threshold.as_ref()
}

/// Resolve the effective reserve-tokens for a threshold, handling both
/// absolute (`reserve_tokens`) and percentage (`compact_percent`) modes.
pub fn resolve_reserve_tokens(threshold: &ModelThreshold, context_window: u32) -> Option<u32> {
    if let Some(reserve) = threshold.reserve_tokens {
        return Some(reserve);
    }
    let pct = threshold.compact_percent?;
    if !(MIN_COMPACT_PERCENT..=MAX_COMPACT_PERCENT).contains(&pct) {
        return None;
    }
    if context_window == 0 {
        return None;
    }
    let reserve = (context_window as f64 * (1.0 - pct as f64 / 100.0)).round() as u32;
    Some(reserve)
}

/// Clamp a context window so compaction thresholds never go negative or
/// absurdly small. Returns `max(declared, reserve_tokens + compaction_buffer)`,
/// guarding against overflow.
pub fn effective_context_window(declared: u32, reserve_tokens: u32, compaction_buffer: u32) -> u32 {
    declared.max(reserve_tokens.saturating_add(compaction_buffer))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DynamicToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Deprecated: all builtin tools are now always advertised. Only MCP server
    /// tools are gated behind promotion. This field is kept for config compatibility
    /// and ignored.
    #[serde(default)]
    pub core: Vec<String>,
}

/// Always-on lightweight reviewer that reads the transcript delta each turn and
/// emits at most one deduped note. Distinct from `judge` (goal-completion) and
/// `review` (on-demand subagent). Off by default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdvisorConfig {
    #[serde(default)]
    pub enabled: bool,
    /// A `provider/model_id` spec. When unset, the advisor role from
    /// `model_roles.toml` is used; falling back to the active model.
    #[serde(default)]
    pub model: Option<String>,
    /// Maximum advisor notes kept in the dedup FIFO.
    #[serde(default = "default_advisor_dedup_size")]
    pub dedup_size: usize,
    /// Minimum severity that triggers an automatic follow-up turn instead of
    /// stopping for the user. Notes at or above this severity are pushed into
    /// the agent's own context and the run continues. `off` keeps the advisor
    /// display-only.
    #[serde(default = "default_advisor_auto_act")]
    pub auto_act: AdvisorAutoAct,
    /// Maximum advisor-driven follow-up turns a single run may take. Once
    /// exhausted the run stops and displays the note as usual.
    #[serde(default = "default_advisor_max_act_turns")]
    pub max_act_turns: u32,
}

fn default_advisor_dedup_size() -> usize {
    DEFAULT_ADVISOR_DEDUP_SIZE
}

fn default_advisor_auto_act() -> AdvisorAutoAct {
    AdvisorAutoAct::Concern
}

fn default_advisor_max_act_turns() -> u32 {
    DEFAULT_ADVISOR_MAX_ACT_TURNS
}

/// Advisor auto-act severity threshold. Declaration order doubles as the
/// severity ranking: `Off < Nit < Concern < Blocker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AdvisorAutoAct {
    Off,
    Nit,
    Concern,
    Blocker,
}

/// Flow mode configuration. Off by default; when enabled, the agent can run
/// the multi-stage Flow pipeline (scout, tpm, plan, execute, qa, integrator,
/// verifier) with persisted per-workstream documents.
#[derive(Debug, Clone, Deserialize, Serialize, ConfigSection)]
#[config(section = "agent.flow")]
#[serde(default, deny_unknown_fields)]
pub struct FlowConfig {
    #[serde(default)]
    #[config(default = false, desc = "Enable the Flow multi-stage pipeline")]
    pub enabled: bool,
    #[serde(default = "default_flow_max_review_iterations")]
    #[config(
        default = 3,
        desc = "How many times Review can send a chunk back to Execute"
    )]
    pub max_review_iterations: u32,
    #[serde(default = "default_flow_max_qa_iterations")]
    #[config(
        default = 2,
        desc = "How many times QA can send a chunk back to Execute"
    )]
    pub max_qa_iterations: u32,
    #[serde(default = "default_flow_parallel_chunks")]
    #[config(default = 1, desc = "Chunks to run at once")]
    pub parallel_chunks: u32,
}

fn default_flow_max_review_iterations() -> u32 {
    DEFAULT_FLOW_MAX_REVIEW_ITERATIONS
}

fn default_flow_max_qa_iterations() -> u32 {
    DEFAULT_FLOW_MAX_QA_ITERATIONS
}

fn default_flow_parallel_chunks() -> u32 {
    DEFAULT_FLOW_PARALLEL_CHUNKS
}

/// Time-traveling stream rules. Off by default; when enabled, rules loaded from
/// `.craft/rules/*.md` (lines prefixed with `rule:`) are matched against the
/// in-flight stream text each turn, and a firing rule injects a system reminder.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TtsrConfig {
    #[serde(default)]
    pub enabled: bool,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            dedup_size: default_advisor_dedup_size(),
            auto_act: default_advisor_auto_act(),
            max_act_turns: default_advisor_max_act_turns(),
        }
    }
}

impl Default for DynamicToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            core: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, ConfigSection, Serialize)]
#[config(section = "agent")]
pub struct AgentConfig {
    #[config(default = DEFAULT_MAX_OUTPUT_BYTES, min = MIN_OUTPUT_BYTES, desc = "Max tool output size (bytes)")]
    pub max_output_bytes: usize,

    #[config(default = DEFAULT_MAX_OUTPUT_LINES, min = MIN_OUTPUT_LINES, desc = "Max tool output lines")]
    pub max_output_lines: usize,

    #[config(default = DEFAULT_MAX_LINE_BYTES, min = MIN_LINE_BYTES, desc = "Max bytes per line before truncation (read tool)")]
    pub max_line_bytes: usize,

    #[config(default = DEFAULT_CODE_EXECUTION_TIMEOUT_SECS, min = MIN_CODE_EXECUTION_TIMEOUT_SECS, desc = "Code execution timeout (seconds)")]
    pub code_execution_timeout_secs: u64,

    #[config(default = DEFAULT_MAX_CONTINUATION_TURNS, min = MIN_MAX_CONTINUATION_TURNS, desc = "Max automatic continuation turns")]
    pub max_continuation_turns: u32,

    #[config(default = DEFAULT_COMPACTION_BUFFER, ty = "u32 | string", default_doc = "20%", desc = "Context reserved for compaction: token count or percent of the context window (e.g. \"20%\")")]
    pub compaction_buffer: CompactionBuffer,

    #[config(
        ty = "String",
        default = "None",
        desc = "Extra instructions appended to the compaction summary prompt"
    )]
    pub compaction_instructions: Option<String>,

    #[config(
        ty = "String",
        default = "None",
        desc = "Extra instructions the agent receives after any compaction (e.g. re-read plan.md)"
    )]
    pub post_compaction_instructions: Option<String>,

    #[config(default = DEFAULT_INTERPRETER_MAX_MEMORY_MB, min = MIN_INTERPRETER_MAX_MEMORY_MB, desc = "Memory limit for code interpreter (MB)")]
    pub interpreter_max_memory_mb: usize,

    #[config(
        default = true,
        desc = "Require re-reading a file that changed on disk before editing it"
    )]
    pub stale_read_check: bool,

    #[config(skip, default = false)]
    pub no_rtk: bool,

    #[config(skip, default = "Vec::new()")]
    pub allowed_tools: Vec<String>,

    #[config(skip, default = "Vec::new()")]
    pub disabled_tools: Vec<String>,

    #[config(skip, default = "TrustDecayConfig::default()")]
    pub trust_decay: TrustDecayConfig,

    #[config(skip, default = "ValidationConfig::default()")]
    pub validation: ValidationConfig,

    #[config(skip, default = "FormatConfig::default()")]
    pub format: FormatConfig,

    #[config(skip, default = "SmallModelConfig::default()")]
    pub small_model: SmallModelConfig,

    #[config(skip, default = "DynamicToolsConfig::default()")]
    pub dynamic_tools: DynamicToolsConfig,

    #[config(skip, default = "AdvisorConfig::default()")]
    pub advisor: AdvisorConfig,

    #[config(skip, default = "FlowConfig::default()")]
    pub flow: FlowConfig,

    #[config(skip, default = "TtsrConfig::default()")]
    pub ttsr: TtsrConfig,

    #[config(skip, default = "CompactionConfig::default()")]
    pub compaction: CompactionConfig,

    #[config(skip, default = "true")]
    pub hooks_enabled: bool,

    #[config(skip, default = "None")]
    pub max_turns: Option<u32>,

    #[config(skip, default = "None")]
    pub judge_model: Option<String>,

    #[config(skip, default = "RepoMapConfig::default()")]
    pub repomap: RepoMapConfig,

    #[config(skip, default = "true")]
    pub memory_extraction: bool,
}

impl AgentConfig {
    pub fn resolve_compaction_buffer(&self, context_window: u32) -> u32 {
        self.compaction_buffer.resolve(context_window)
    }

    pub fn effective_output_limits(&self) -> (usize, usize) {
        let small_active = self.small_model.enabled;
        let factor = if small_active && self.small_model.aggressive_truncation {
            0.5
        } else {
            1.0
        };
        (
            (self.max_output_bytes as f64 * factor) as usize,
            (self.max_output_lines as f64 * factor) as usize,
        )
    }
    fn from_file(file: AgentFileConfig, no_rtk: bool, disabled_tools: Vec<String>) -> Self {
        Self {
            no_rtk,
            max_output_bytes: file.max_output_bytes.unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            max_output_lines: file.max_output_lines.unwrap_or(DEFAULT_MAX_OUTPUT_LINES),
            max_line_bytes: file.max_line_bytes.unwrap_or(DEFAULT_MAX_LINE_BYTES),
            code_execution_timeout_secs: file
                .code_execution_timeout_secs
                .unwrap_or(DEFAULT_CODE_EXECUTION_TIMEOUT_SECS),
            max_continuation_turns: file
                .max_continuation_turns
                .unwrap_or(DEFAULT_MAX_CONTINUATION_TURNS),
            compaction_buffer: file.compaction_buffer.unwrap_or(DEFAULT_COMPACTION_BUFFER),
            compaction_instructions: file.compaction_instructions,
            post_compaction_instructions: file.post_compaction_instructions,
            interpreter_max_memory_mb: file
                .interpreter_max_memory_mb
                .unwrap_or(DEFAULT_INTERPRETER_MAX_MEMORY_MB),
            stale_read_check: file.stale_read_check.unwrap_or(true),
            allowed_tools: Vec::new(),
            disabled_tools,
            trust_decay: file.trust_decay,
            validation: file.validation,
            format: file.format,
            small_model: file.small_model,
            dynamic_tools: file.dynamic_tools,
            advisor: file.advisor,
            flow: file.flow.unwrap_or_default(),
            ttsr: file.ttsr,
            compaction: file.compaction,
            hooks_enabled: file.hooks_enabled.unwrap_or(true),
            max_turns: None,
            judge_model: file.judge_model,
            repomap: RepoMapConfig::default(),
            memory_extraction: true,
        }
    }

    pub fn validate_all(&self) -> Result<(), ConfigError> {
        self.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, ConfigSection)]
#[config(section = "provider", fields_only)]
pub struct ProviderConfig {
    #[config(
        ty = "String",
        desc = "Default model identifier (e.g. `anthropic/claude-sonnet-4-6`)"
    )]
    pub default_model: Option<String>,

    #[config(
        ty = "string[]",
        default_doc = "[]",
        desc = "Glob patterns for permitted qualified model specs; empty permits all models"
    )]
    pub allowed_models: Vec<String>,

    #[config(
        ty = "string[]",
        default_doc = "[]",
        desc = "Glob patterns for excluded qualified model specs; exclusions take precedence"
    )]
    pub excluded_models: Vec<String>,

    #[config(skip)]
    pub model_policy: ModelPolicy,

    #[config(key = "connect_timeout_secs", ty = "u64", default = DEFAULT_CONNECT_TIMEOUT_SECS,
             min = MIN_CONNECT_TIMEOUT_SECS, val = "self.connect_timeout.as_secs()",
             desc = "HTTP connect timeout (seconds)")]
    pub connect_timeout: Duration,

    #[config(key = "low_speed_timeout_secs", ty = "u64", default = DEFAULT_LOW_SPEED_TIMEOUT_SECS,
             min = MIN_LOW_SPEED_TIMEOUT_SECS, val = "self.low_speed_timeout.as_secs()",
             desc = "Low speed timeout (seconds with less than 1 byte received)")]
    pub low_speed_timeout: Duration,

    #[config(key = "stream_timeout_secs", ty = "u64", default = DEFAULT_STREAM_TIMEOUT_SECS,
             min = MIN_STREAM_TIMEOUT_SECS, val = "self.stream_timeout.as_secs()",
             desc = "Streaming response timeout (seconds)")]
    pub stream_timeout: Duration,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            default_model: None,
            allowed_models: Vec::new(),
            excluded_models: Vec::new(),
            model_policy: ModelPolicy::allow_all(),
            connect_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            low_speed_timeout: Duration::from_secs(DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            stream_timeout: Duration::from_secs(DEFAULT_STREAM_TIMEOUT_SECS),
        }
    }
}

impl ProviderConfig {
    fn from_file(f: ProviderFileConfig) -> Result<Self, ConfigError> {
        let allowed_models = f.allowed_models.unwrap_or_default();
        let excluded_models = f.excluded_models.unwrap_or_default();
        let model_policy = ModelPolicy::new(&allowed_models, &excluded_models)?;
        Ok(Self {
            default_model: f.default_model,
            allowed_models,
            excluded_models,
            model_policy,
            connect_timeout: Duration::from_secs(
                f.connect_timeout_secs
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
            ),
            low_speed_timeout: Duration::from_secs(
                f.low_speed_timeout_secs
                    .unwrap_or(DEFAULT_LOW_SPEED_TIMEOUT_SECS),
            ),
            stream_timeout: Duration::from_secs(
                f.stream_timeout_secs.unwrap_or(DEFAULT_STREAM_TIMEOUT_SECS),
            ),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ModelPolicy {
    allowed: GlobSet,
    excluded: GlobSet,
    has_allowed_models: bool,
    has_excluded_models: bool,
}

impl Default for ModelPolicy {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl ModelPolicy {
    fn allow_all() -> Self {
        Self::new(&[], &[]).expect("empty model policy is valid")
    }

    pub fn new(allowed_models: &[String], excluded_models: &[String]) -> Result<Self, ConfigError> {
        Ok(Self {
            allowed: Self::compile("allowed_models", allowed_models)?,
            excluded: Self::compile("excluded_models", excluded_models)?,
            has_allowed_models: !allowed_models.is_empty(),
            has_excluded_models: !excluded_models.is_empty(),
        })
    }

    fn compile(field: &'static str, patterns: &[String]) -> Result<GlobSet, ConfigError> {
        let mut globset = GlobSetBuilder::new();
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .map_err(|source| ConfigError::InvalidModelPattern {
                    field,
                    pattern: pattern.clone(),
                    source,
                })?;
            globset.add(glob);
        }
        globset
            .build()
            .map_err(|source| ConfigError::InvalidModelPattern {
                field,
                pattern: String::new(),
                source,
            })
    }

    pub fn allows(&self, spec: &str) -> bool {
        (!self.has_allowed_models || self.allowed.is_match(spec)) && !self.excluded.is_match(spec)
    }

    pub fn is_restrictive(&self) -> bool {
        self.has_allowed_models || self.has_excluded_models
    }
}

#[derive(Debug, Clone, Copy, ConfigSection)]
#[config(section = "storage", fields_only)]
pub struct StorageConfig {
    #[config(key = "max_log_bytes_mb", ty = "u64", default = DEFAULT_MAX_LOG_BYTES_MB,
             min = MIN_MAX_LOG_BYTES_MB, val = "self.max_log_bytes / (1024 * 1024)",
             desc = "Max total log size (MB)")]
    pub max_log_bytes: u64,

    #[config(default = DEFAULT_MAX_LOG_FILES, min = MIN_MAX_LOG_FILES,
             desc = "Max number of log files to keep")]
    pub max_log_files: u32,

    #[config(default = DEFAULT_INPUT_HISTORY_SIZE, min = MIN_INPUT_HISTORY_SIZE,
             desc = "Number of input history entries to retain")]
    pub input_history_size: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_log_bytes: DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024,
            max_log_files: DEFAULT_MAX_LOG_FILES,
            input_history_size: DEFAULT_INPUT_HISTORY_SIZE,
        }
    }
}

impl StorageConfig {
    fn from_file(f: StorageFileConfig) -> Self {
        Self {
            max_log_bytes: f.max_log_bytes_mb.unwrap_or(DEFAULT_MAX_LOG_BYTES_MB) * 1024 * 1024,
            max_log_files: f.max_log_files.unwrap_or(DEFAULT_MAX_LOG_FILES),
            input_history_size: f.input_history_size.unwrap_or(DEFAULT_INPUT_HISTORY_SIZE),
        }
    }
}

#[derive(Debug, Clone, ConfigSection, Serialize)]
#[config(section = "compression")]
pub struct CompressionConfig {
    #[config(default = DEFAULT_COMPRESSION_ENABLED, desc = "Enable tool output compression")]
    pub enabled: bool,

    #[config(skip, default = "DEFAULT_COMPRESSION_CODE_RATE")]
    pub code_compression_rate: f32,

    #[config(default = DEFAULT_COMPRESSION_MAX_LOG_LINES, min = 10, desc = "Max lines in compressed log output")]
    pub max_log_lines: usize,

    #[config(default = DEFAULT_COMPRESSION_MAX_SEARCH_FILES, min = 5, desc = "Max files in compressed search output")]
    pub max_search_files: usize,

    #[config(default = DEFAULT_COMPRESSION_MAX_MATCHES_PER_FILE, min = 1, desc = "Max matches per file in search output")]
    pub max_matches_per_file: usize,

    #[config(default = DEFAULT_COMPRESSION_MAX_DIFF_LINES, min = 10, desc = "Max lines in compressed diff output")]
    pub max_diff_lines: usize,

    #[config(default = DEFAULT_COMPRESSION_MAX_JSON_ITEMS, min = 5, desc = "Max items in compressed JSON array output")]
    pub max_json_items: usize,

    #[config(skip, default = "DEFAULT_COMPRESSION_JSON_FIRST_KEEP")]
    pub json_first_keep: usize,

    #[config(skip, default = "DEFAULT_COMPRESSION_JSON_LAST_KEEP")]
    pub json_last_keep: usize,

    #[config(default = DEFAULT_COMPRESSION_PROTECT_RECENT, min = 1, desc = "Never compress the last N tool outputs")]
    pub protect_recent_tool_outputs: usize,
}

impl CompressionConfig {
    fn from_file(f: CompressionFileConfig) -> Self {
        Self {
            enabled: f.enabled.unwrap_or(DEFAULT_COMPRESSION_ENABLED),
            code_compression_rate: f
                .code_compression_rate
                .unwrap_or(DEFAULT_COMPRESSION_CODE_RATE),
            max_log_lines: f.max_log_lines.unwrap_or(DEFAULT_COMPRESSION_MAX_LOG_LINES),
            max_search_files: f
                .max_search_files
                .unwrap_or(DEFAULT_COMPRESSION_MAX_SEARCH_FILES),
            max_matches_per_file: f
                .max_matches_per_file
                .unwrap_or(DEFAULT_COMPRESSION_MAX_MATCHES_PER_FILE),
            max_diff_lines: f
                .max_diff_lines
                .unwrap_or(DEFAULT_COMPRESSION_MAX_DIFF_LINES),
            max_json_items: f
                .max_json_items
                .unwrap_or(DEFAULT_COMPRESSION_MAX_JSON_ITEMS),
            json_first_keep: f
                .json_first_keep
                .unwrap_or(DEFAULT_COMPRESSION_JSON_FIRST_KEEP),
            json_last_keep: f
                .json_last_keep
                .unwrap_or(DEFAULT_COMPRESSION_JSON_LAST_KEEP),
            protect_recent_tool_outputs: f
                .protect_recent_tool_outputs
                .unwrap_or(DEFAULT_COMPRESSION_PROTECT_RECENT),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    WorkspaceWrite,
    ReadOnly,
    DangerFullAccess,
    Off,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: SandboxMode,
    #[serde(default = "default_true")]
    pub network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: SandboxMode::WorkspaceWrite,
            network: true,
        }
    }
}

impl SandboxConfig {
    fn from_file(f: SandboxFileConfig) -> Self {
        Self {
            enabled: f.enabled.unwrap_or(true),
            mode: f.mode.unwrap_or_default(),
            network: f.network.unwrap_or(true),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PluginsConfig {
    pub names: Vec<String>,
    /// Per-plugin option tables, without `enabled`. Each plugin validates
    /// its own via `craft.api.register_options` at load time.
    pub opts: HashMap<String, JsonMap<String, JsonValue>>,
}

impl PluginsConfig {
    pub fn from_plugins(plugins: HashMap<String, PluginFileConfig>) -> Self {
        let mut all: Vec<String> = DEFAULT_BUILTINS
            .iter()
            .filter(|name| plugins.get(**name).and_then(|t| t.enabled).unwrap_or(true))
            .map(|s| s.to_string())
            .collect();

        let mut extra: Vec<&String> = plugins
            .iter()
            .filter(|(name, cfg)| {
                !DEFAULT_BUILTINS.contains(&name.as_str()) && cfg.enabled.unwrap_or(false)
            })
            .map(|(name, _)| name)
            .collect();
        extra.sort();
        all.extend(extra.into_iter().cloned());

        let opts = plugins
            .into_iter()
            .filter(|(_, cfg)| !cfg.opts.is_empty())
            .map(|(name, cfg)| (name, cfg.opts))
            .collect();

        Self { names: all, opts }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RepoMapConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_repomap_max_tokens")]
    pub max_tokens: u32,
}

const DEFAULT_REPOMAP_MAX_TOKENS: u32 = 1024;

fn default_repomap_max_tokens() -> u32 {
    DEFAULT_REPOMAP_MAX_TOKENS
}

impl Default for RepoMapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: DEFAULT_REPOMAP_MAX_TOKENS,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RepoMapFileConfig {
    pub enabled: Option<bool>,
    pub max_tokens: Option<u32>,
}

impl RepoMapFileConfig {
    pub fn merge(&mut self, overlay: RepoMapFileConfig) {
        merge_option!(self, overlay, enabled, max_tokens);
    }
}

impl RepoMapConfig {
    pub fn from_file(f: RepoMapFileConfig) -> Self {
        Self {
            enabled: f.enabled.unwrap_or(true),
            max_tokens: f.max_tokens.unwrap_or(DEFAULT_REPOMAP_MAX_TOKENS),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WatchConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WatchFileConfig {
    pub enabled: Option<bool>,
}

impl WatchFileConfig {
    pub fn merge(&mut self, overlay: WatchFileConfig) {
        merge_option!(self, overlay, enabled);
    }
}

impl WatchConfig {
    pub fn from_file(f: WatchFileConfig) -> Self {
        Self {
            enabled: f.enabled.unwrap_or(false),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.ui.validate_all()?;
        self.agent.validate_all()?;
        self.provider.validate()?;
        self.storage.validate()?;
        self.compression.validate()?;
        Ok(())
    }
}

fn push_rules(
    rules: &mut Vec<PermissionRule>,
    tools: &HashMap<String, ToolPermissions>,
    effect: Effect,
) {
    for (tool, perms) in tools {
        let scope_set = match effect {
            Effect::Deny => &perms.deny,
            Effect::Allow => &perms.allow,
        };
        let Some(scope_set) = scope_set else {
            continue;
        };
        match scope_set {
            ScopeSet::All(true) => rules.push(PermissionRule {
                tool: ToolKey::native(tool),
                scope: None,
                effect,
            }),
            ScopeSet::Scopes(scopes) => {
                for s in scopes {
                    rules.push(PermissionRule {
                        tool: ToolKey::native(tool),
                        scope: Some(s.clone()),
                        effect,
                    });
                }
            }
            ScopeSet::All(false) => {}
        }
    }
}

pub fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn is_valid_tool_name(name: &str) -> bool {
    is_valid_wire_name(name)
}

fn push_mcp_tool_rule(
    rules: &mut Vec<PermissionRule>,
    server_name: &str,
    tool_name: &str,
    effect: Effect,
) {
    let qualified = format!("{server_name}.{tool_name}");
    match ToolKey::parse(&qualified) {
        Ok(key) => {
            rules.push(PermissionRule {
                tool: key,
                scope: None,
                effect,
            });
        }
        Err(e) => {
            tracing::warn!(
                server = server_name,
                tool = tool_name,
                error = %e,
                "skipping invalid MCP tool name"
            );
        }
    }
}

fn child_table<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, PermissionError> {
    table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()))
        .as_table_mut()
        .ok_or(PermissionError::NotATable {
            tool: key.to_string(),
        })
}

fn push_unique(
    table: &mut toml_edit::Table,
    key: &str,
    value: &str,
) -> Result<(), PermissionError> {
    let arr = table
        .entry(key)
        .or_insert_with(|| toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())))
        .as_array_mut()
        .ok_or(PermissionError::NotAnArray {
            tool: String::new(),
            key: key.to_string(),
        })?;
    if !arr.iter().any(|v| v.as_str() == Some(value)) {
        arr.push(value);
        arr.set_trailing("\n");
        arr.set_trailing_comma(true);
        for item in arr.iter_mut() {
            item.decor_mut().set_prefix("\n    ");
        }
    }
    Ok(())
}

fn parse_mcp_server_table(
    server_name: &str,
    table: &toml::Table,
    rules: &mut Vec<PermissionRule>,
    mcp_defaults: &mut HashMap<ToolKey, DefaultEffect>,
) {
    if !is_valid_server_name(server_name) {
        tracing::warn!(
            server = server_name,
            "skipping [mcp.{}] — invalid server name; must contain only alphanumeric characters and hyphens",
            server_name
        );
        return;
    }

    for (key, value) in table {
        match key.as_str() {
            "allow" | "deny" => {
                let effect = if key == "allow" {
                    Effect::Allow
                } else {
                    Effect::Deny
                };
                match value {
                    toml::Value::Array(arr) => {
                        for item in arr {
                            if let Some(tool_name) = item.as_str() {
                                if tool_name == "*" {
                                    rules.push(PermissionRule {
                                        tool: ToolKey::McpServer {
                                            server: server_name.into(),
                                        },
                                        scope: None,
                                        effect,
                                    });
                                    continue;
                                }
                                push_mcp_tool_rule(rules, server_name, tool_name, effect);
                            }
                        }
                    }
                    toml::Value::Boolean(true) => {
                        tracing::warn!(
                            server = server_name,
                            key = key.as_str(),
                            "{} = true is deprecated — use default = \"{}\" instead; ignoring",
                            key,
                            key
                        );
                    }
                    toml::Value::Boolean(false) => {}
                    toml::Value::String(s) => {
                        let tool_name = s.as_str();
                        if tool_name == "*" {
                            rules.push(PermissionRule {
                                tool: ToolKey::McpServer {
                                    server: server_name.into(),
                                },
                                scope: None,
                                effect,
                            });
                        } else {
                            tracing::info!(
                                server = server_name,
                                tool = tool_name,
                                "{} = \"{}\" coerced to {} = [\"{}\"] — consider using array syntax",
                                key,
                                tool_name,
                                key,
                                tool_name
                            );
                            push_mcp_tool_rule(rules, server_name, tool_name, effect);
                        }
                    }
                    other => {
                        tracing::warn!(
                            server = server_name,
                            key = key.as_str(),
                            value = ?other,
                            "unexpected value for [mcp.{}].{} — expected array of tool names or default = \"allow\"/\"deny\"",
                            server_name,
                            key
                        );
                    }
                }
            }
            "default" => {
                if let Ok(d) = value.clone().try_into::<DefaultEffect>() {
                    mcp_defaults.insert(
                        ToolKey::McpServer {
                            server: server_name.into(),
                        },
                        d,
                    );
                } else {
                    tracing::warn!(
                        server = server_name,
                        value = ?value,
                        "invalid [mcp.{}].default value — expected \"allow\", \"deny\", or \"prompt\"",
                        server_name
                    );
                }
            }
            other => {
                if value.is_table() {
                    tracing::warn!(
                        server = server_name,
                        key = other,
                        "unknown key [mcp.{}.{}] — server names cannot contain dots; use [mcp.{}] instead if this is a server name",
                        server_name,
                        other,
                        other
                    );
                } else {
                    tracing::warn!(
                        server = server_name,
                        key = other,
                        "unknown key in [mcp.{}] — ignored",
                        server_name
                    );
                }
            }
        }
    }
}

fn build_permissions(
    global: PermissionsFileConfig,
    project: PermissionsFileConfig,
) -> PermissionsConfig {
    let global_default = global.default.unwrap_or(DefaultEffect::Prompt);
    let default = match project.default {
        Some(DefaultEffect::Allow) => global_default,
        Some(d) => d,
        None => global_default,
    };

    let mut tool_defaults = HashMap::new();
    for (tool, perms) in &global.tools {
        if let Some(d) = perms.default {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring [\"*\"].default — use the top-level `default` field instead for global fallback behavior"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &global.mcp_defaults {
        tool_defaults.insert(key.clone(), *d);
    }
    for (tool, perms) in &project.tools {
        if let Some(d) = perms.default
            && d != DefaultEffect::Allow
        {
            let key = ToolKey::native(tool);
            if matches!(key, ToolKey::Wildcard) {
                tracing::warn!(
                    tool = tool,
                    "ignoring project [\"*\"].default — use the top-level `default` field instead"
                );
            } else {
                tool_defaults.insert(key, d);
            }
        }
    }
    for (key, d) in &project.mcp_defaults {
        if *d != DefaultEffect::Allow {
            tool_defaults.insert(key.clone(), *d);
        }
    }

    let mut rules = Vec::new();
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &global.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    for tools in [&global.tools, &project.tools] {
        push_rules(&mut rules, tools, Effect::Deny);
        push_rules(&mut rules, tools, Effect::Allow);
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Deny {
            rules.push(rule.clone());
        }
    }
    for rule in &project.mcp_rules {
        if rule.effect == Effect::Allow {
            rules.push(rule.clone());
        }
    }
    PermissionsConfig {
        default,
        tool_defaults,
        rules,
        yolo: false,
        auto_review: false,
    }
}

pub fn global_config_dir() -> Option<PathBuf> {
    paths::config_dir().ok()
}

pub fn global_config_dirs() -> Vec<PathBuf> {
    config_search_dirs(global_config_dir().as_deref())
}

fn config_search_dirs(global: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = global {
        dirs.push(d.to_path_buf());
    }
    if let Ok(xdg) = paths::xdg_config_dir()
        && dirs.first() != Some(&xdg)
    {
        dirs.push(xdg);
    }
    dirs
}

fn load_env_files_with_global(cwd: &Path, global: Option<&Path>) {
    let mut vars = HashMap::new();
    if let Some(path) = global {
        collect_env_vars(&path.join(".env"), &mut vars);
    }
    collect_env_vars(&cwd.join(PROJECT_DIR).join(".env"), &mut vars);

    for (key, value) in vars {
        if std::env::var_os(&key).is_none() {
            // SAFETY: caller guarantees single-threaded execution (before async runtime)
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

fn collect_env_vars(path: &Path, vars: &mut HashMap<String, String>) {
    let Ok(iter) = dotenvy::from_path_iter(path) else {
        return;
    };
    for item in iter.flatten() {
        vars.insert(item.0, item.1);
    }
}

/// Loads `.env` files from global and project config dirs, setting environment variables
/// for any keys not already set in the process.
///
/// # Safety (caller requirement)
///
/// Must be called before spawning any threads or the async runtime.
/// `std::env::set_var` is unsafe when concurrent access to the environment exists.
pub fn load_env_files(cwd: &Path) {
    load_env_files_with_global(cwd, global_config_dir().as_deref());
}

pub fn load_permissions(cwd: &Path) -> PermissionsConfig {
    let global_dirs = config_search_dirs(global_config_dir().as_deref());
    load_permissions_inner(cwd, &global_dirs)
}

fn load_permissions_inner(cwd: &Path, global_dirs: &[PathBuf]) -> PermissionsConfig {
    let mut global_perms = PermissionsFileConfig::default();
    for dir in global_dirs {
        if let Some(p) = read_permissions_file(&dir.join(PERMISSIONS_FILE)) {
            global_perms.merge(p);
        }
    }

    let project_perms =
        read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap_or_default();

    build_permissions(global_perms, project_perms)
}

fn migrate_mcp_entry(
    doc: &mut toml_edit::DocumentMut,
    server_name: &str,
    tool_name: &str,
    item: &toml_edit::Item,
) {
    // Old format: ["mcp:server__tool"] with booleans or scope-string arrays.
    // New format: [mcp.server] allow = ["tool_name"]. Old scope strings were
    // dead code (MCP scopes are always wildcarded), so only the effect survives.
    let mut push = |effect_key: &str| {
        let res = child_table(doc.as_table_mut(), "mcp")
            .and_then(|mcp| child_table(mcp, server_name))
            .and_then(|server| push_unique(server, effect_key, tool_name));
        if let Err(e) = res {
            warn!(
                server = server_name,
                tool = tool_name,
                error = %e,
                "skipping MCP entry migration"
            );
        }
    };

    // Bare boolean: old format like [mcp]\ndeepwiki__search = true
    // means "allow this tool".
    if let Some(b) = item.as_bool() {
        if b {
            push("allow");
        }
        return;
    }

    if let Some(old_table) = item.as_table() {
        for (key, value) in old_table.iter() {
            match key {
                "allow" | "deny" => {
                    if value.as_bool() == Some(true) || value.as_array().is_some() {
                        push(key);
                    }
                }
                _ => {
                    warn!(
                        key,
                        server = server_name,
                        tool = tool_name,
                        "dropping unknown key in old MCP entry during migration"
                    );
                }
            }
        }
    }
}

/// Migrates old permission formats and returns the (possibly rewritten)
/// file content. The rewrite to disk is best-effort: loading uses the
/// migrated content even when the write fails.
fn migrate_permissions_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let Ok(mut doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Some(content);
    };
    let mut migrated = false;

    if let Some(item) = doc.remove("allow_all") {
        migrated = true;
        if item.as_bool() == Some(true) {
            doc.insert("default", toml_edit::value("allow"));
        }
    }

    let flat_old_keys: Vec<String> = doc
        .iter()
        .filter_map(|(k, _)| {
            k.strip_prefix("mcp:")
                .and_then(|rest| rest.contains("__").then(|| k.to_string()))
        })
        .collect();

    for old_key in flat_old_keys {
        if let Some(item) = doc.remove(&old_key) {
            let rest = &old_key[4..];
            if let Some((server, tool)) = rest.split_once("__") {
                if !is_valid_server_name(server) || !is_valid_tool_name(tool) {
                    tracing::error!(
                        key = old_key.as_str(),
                        server = server,
                        tool = tool,
                        "SECURITY: skipping migration of malformed MCP key — rules for this tool will not be restored"
                    );
                    continue;
                }
                migrate_mcp_entry(&mut doc, server, tool, &item);
                migrated = true;
            }
        }
    }

    let nested_old_entries: Vec<(String, String, toml_edit::Item)> = {
        let mut entries = Vec::new();
        if let Some(toml_edit::Item::Table(mcp_table)) = doc.get("mcp") {
            for (key, _) in mcp_table.iter() {
                if key.contains("__")
                    && let Some((server, tool)) = key.split_once("__")
                {
                    let item = mcp_table.get(key).cloned();
                    if let Some(item) = item {
                        entries.push((server.to_string(), tool.to_string(), item));
                    }
                }
            }
        }
        entries
    };

    for (server_name, tool_name, item) in nested_old_entries {
        if !is_valid_server_name(&server_name) || !is_valid_tool_name(&tool_name) {
            tracing::error!(
                server = server_name.as_str(),
                tool = tool_name.as_str(),
                "SECURITY: skipping migration of malformed nested MCP key — rules for this tool will not be restored"
            );
            continue;
        }
        if let Some(toml_edit::Item::Table(mcp_table)) = doc.get_mut("mcp") {
            mcp_table.remove(&format!("{server_name}__{tool_name}"));
        }
        migrate_mcp_entry(&mut doc, &server_name, &tool_name, &item);
        migrated = true;
    }

    if let Some(toml_edit::Item::Table(mcp_table)) = doc.get("mcp")
        && mcp_table.is_empty()
    {
        doc.remove("mcp");
    }

    if !migrated {
        return Some(content);
    }
    let new_content = doc.to_string();
    if let Err(e) = craft_storage::atomic_write(path, new_content.as_bytes()) {
        warn!(path = %path.display(), error = %e, "failed to persist migrated permissions file");
    }
    Some(new_content)
}

fn read_permissions_file(path: &Path) -> Option<PermissionsFileConfig> {
    let content = migrate_permissions_file(path)?;
    match toml::from_str(&content) {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to parse permissions");
            None
        }
    }
}

fn storage_io_error(e: craft_storage::StorageError) -> PermissionError {
    PermissionError::Write(match e {
        craft_storage::StorageError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    })
}

pub fn append_permission_rule(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
) -> Result<(), PermissionError> {
    let dir = config_search_dirs(global_config_dir().as_deref())
        .into_iter()
        .last();
    append_permission_rule_with_global(tool, scope, effect, target, dir)
}

fn append_permission_rule_with_global(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    target: &PermissionTarget,
    global: Option<PathBuf>,
) -> Result<(), PermissionError> {
    match target {
        PermissionTarget::Global => append_global_permission(tool, scope, effect, global),
        PermissionTarget::Project(cwd) => append_project_permission(tool, scope, effect, cwd),
    }
}

fn append_global_permission(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    global: Option<PathBuf>,
) -> Result<(), PermissionError> {
    let path = global
        .ok_or(PermissionError::NoHomeDir)?
        .join(PERMISSIONS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(PermissionError::Parse)?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(PermissionError::CreateDir)?;
    }
    craft_storage::atomic_write(&path, doc.to_string().as_bytes()).map_err(storage_io_error)?;
    Ok(())
}

fn append_project_permission(
    tool: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
    cwd: &Path,
) -> Result<(), PermissionError> {
    let path = cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE);
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(PermissionError::Parse)?;

    insert_permission_entry(&mut doc, tool, scope, effect)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(PermissionError::CreateDir)?;
    }
    craft_storage::atomic_write(&path, doc.to_string().as_bytes()).map_err(storage_io_error)?;
    Ok(())
}

fn insert_permission_entry(
    doc: &mut toml_edit::DocumentMut,
    tool_key: &ToolKey,
    scope: Option<&str>,
    effect: Effect,
) -> Result<(), PermissionError> {
    let key = match effect {
        Effect::Allow => "allow",
        Effect::Deny => "deny",
    };

    match tool_key {
        // MCP scopes are always wildcarded, so `scope` is ignored for MCP keys.
        ToolKey::McpTool { server, tool } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            push_unique(server_table, key, tool)?;
        }
        ToolKey::McpServer { server } => {
            let server_table = child_table(child_table(doc.as_table_mut(), "mcp")?, server)?;
            server_table.insert("default", toml_edit::value(key));
        }
        ToolKey::Wildcard => {
            // Wildcard rules are config-only; runtime never writes them.
            return Err(PermissionError::NotATable {
                tool: "*".to_string(),
            });
        }
        ToolKey::Native(name) => {
            let tool_table = child_table(doc.as_table_mut(), name)?;
            match scope {
                Some(s) => push_unique(tool_table, key, s)?,
                None => {
                    tool_table.insert(key, toml_edit::value(true));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_storage::sessions::Effort;
    use std::fs;
    use tempfile::TempDir;
    use test_case::test_case;

    fn write_global_permissions(dir: &Path, content: &str) {
        let perms_dir = dir.join(".config/craft");
        fs::create_dir_all(&perms_dir).unwrap();
        fs::write(perms_dir.join("permissions.toml"), content).unwrap();
    }

    fn global_config_dir(dir: &Path) -> PathBuf {
        dir.join(".config/craft")
    }

    #[test_case("12000", CompactionBuffer::Tokens(12_000) ; "tokens_number")]
    #[test_case("\"20%\"", CompactionBuffer::Percent(20) ; "percent_string")]
    #[test_case("\" 5 %\"", CompactionBuffer::Percent(5) ; "percent_with_spaces")]
    fn compaction_buffer_deserializes(json: &str, expected: CompactionBuffer) {
        let parsed: CompactionBuffer = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test_case("500" ; "tokens_below_min")]
    #[test_case("-1" ; "negative_tokens")]
    #[test_case("\"0%\"" ; "zero_percent")]
    #[test_case("\"100%\"" ; "percent_too_high")]
    #[test_case("\"abc%\"" ; "non_numeric_percent")]
    fn compaction_buffer_rejects(json: &str) {
        assert!(serde_json::from_str::<CompactionBuffer>(json).is_err());
    }

    #[test_case(CompactionBuffer::Tokens(10_000), 64_000, 10_000 ; "tokens_ignore_window")]
    #[test_case(CompactionBuffer::Percent(20), 64_000, 12_800 ; "percent_of_window")]
    fn compaction_buffer_resolves(buffer: CompactionBuffer, window: u32, expected: u32) {
        assert_eq!(buffer.resolve(window), expected);
    }

    #[test]
    fn compaction_buffer_serializes_percent_as_string() {
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Percent(20)).unwrap(),
            serde_json::json!("20%")
        );
        assert_eq!(
            serde_json::to_value(CompactionBuffer::Tokens(9_000)).unwrap(),
            serde_json::json!(9_000)
        );
    }

    #[test]
    fn empty_config_returns_defaults() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(config.ui.splash_animation);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(
            config.provider.connect_timeout,
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS)
        );
        assert_eq!(
            config.storage.max_log_bytes,
            DEFAULT_MAX_LOG_BYTES_MB * 1024 * 1024
        );
        assert!(config.ui.keybindings.is_empty());
    }

    #[test]
    fn keybindings_config_toml_rejects_bad() {
        let raw = r#"
[ui]
keybindings = "not a table"
"#;
        let result: std::result::Result<RawConfig, toml::de::Error> = toml::from_str(raw);
        assert!(result.is_err());
    }

    #[test]
    fn keybindings_config_parses_chord_and_list_and_disable() {
        let raw = r#"
[ui.keybindings]
search = "Ctrl+F"
plan_toggle = ["Ctrl+T", "Alt+T"]
tasks = []
"#;
        let parsed: RawConfig = toml::from_str(raw).unwrap();
        let config = parsed.into_config(false).unwrap();
        let entries = &config.ui.keybindings.entries;
        let by_id: std::collections::HashMap<&str, &Vec<String>> =
            entries.iter().map(|(k, v)| (k.as_str(), v)).collect();
        assert_eq!(by_id["search"], &vec!["Ctrl+F".to_string()]);
        assert_eq!(
            by_id["plan_toggle"],
            &vec!["Ctrl+T".to_string(), "Alt+T".to_string()]
        );
        assert_eq!(by_id["tasks"], &Vec::<String>::new());
    }

    #[test]
    fn partial_agent_config_preserves_unset_fields() {
        let raw = RawConfig {
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                code_execution_timeout_secs: Some(60),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.agent.max_output_lines, 5000);
        assert_eq!(config.agent.code_execution_timeout_secs, 60);
        assert_eq!(config.agent.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    }

    #[test]
    fn merge_overlay_wins_field_by_field() {
        let mut base = RawConfig {
            always_yolo: Some(false),
            ui: UiFileConfig {
                splash_animation: Some(false),
                flash_duration_ms: Some(2000),
                ..Default::default()
            },
            agent: AgentFileConfig {
                max_output_lines: Some(3000),
                max_line_bytes: Some(800),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            always_yolo: Some(true),
            agent: AgentFileConfig {
                max_output_lines: Some(5000),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_yolo, Some(true), "overlay wins");
        assert_eq!(base.agent.max_output_lines, Some(5000), "overlay wins");
        assert_eq!(base.agent.max_line_bytes, Some(800), "base preserved");
        assert_eq!(base.ui.splash_animation, Some(false), "base preserved");
        assert_eq!(base.ui.flash_duration_ms, Some(2000), "base preserved");
    }

    #[test_case("max_output_bytes",  0 ; "zero_output_bytes")]
    #[test_case("max_output_lines",  0 ; "zero_output_lines")]
    #[test_case("max_line_bytes",    0 ; "zero_line_bytes")]
    #[test_case("max_output_bytes",  500 ; "below_min_output_bytes")]
    #[test_case("max_line_bytes",    10 ; "below_min_line_bytes")]
    fn validate_rejects_invalid_agent(field: &str, value: usize) {
        let mut config = AgentConfig::default();
        match field {
            "max_output_bytes" => config.max_output_bytes = value,
            "max_output_lines" => config.max_output_lines = value,
            "max_line_bytes" => config.max_line_bytes = value,
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(err, ConfigError::BelowMinimum { field: f, .. } if f == field));
    }

    #[test]
    fn tool_output_lines_per_tool_override() {
        let raw = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(20),
                    read: Some(20),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.tool_output_lines.bash, 20);
        assert_eq!(config.ui.tool_output_lines.read, 20);
        assert_eq!(
            config.ui.tool_output_lines.grep,
            ToolOutputLines::DEFAULT.grep
        );
    }

    #[test_case("provider", "connect_timeout_secs", 0 ; "provider_zero_connect_timeout")]
    #[test_case("storage",  "max_log_files",        0 ; "storage_zero_log_files")]
    #[test_case("ui",       "mouse_scroll_lines",   0 ; "ui_zero_scroll_lines")]
    #[test_case("ui",       "max_input_lines",      0 ; "ui_zero_max_input_lines")]
    #[test_case("agent",    "code_execution_timeout_secs", 1 ; "agent_code_execution_timeout_too_low")]
    fn validate_rejects_invalid_sections(section: &str, field: &str, value: u64) {
        let mut config = Config {
            always_yolo: false,
            always_auto_review: false,
            always_fast: false,
            always_thinking: None,
            ui: UiConfig::default(),
            agent: AgentConfig::default(),
            provider: ProviderConfig::default(),
            storage: StorageConfig::default(),
            compression: CompressionConfig::default(),
            sandbox: SandboxConfig::default(),
            permissions: PermissionsConfig::default(),
            plugins: PluginsConfig::default(),
            repomap: RepoMapConfig::default(),
            watch: WatchConfig::default(),
        };
        match (section, field) {
            ("provider", "connect_timeout_secs") => {
                config.provider.connect_timeout = Duration::from_secs(value)
            }
            ("storage", "max_log_files") => config.storage.max_log_files = value as u32,
            ("ui", "mouse_scroll_lines") => config.ui.mouse_scroll_lines = value as u32,
            ("ui", "max_input_lines") => config.ui.max_input_lines = value as u32,
            ("agent", "code_execution_timeout_secs") => {
                config.agent.code_execution_timeout_secs = value
            }
            _ => unreachable!(),
        }
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::BelowMinimum { section: s, field: f, .. } if s == section && f == field
        ));
    }

    #[test]
    fn show_thinking_deserializes_true() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = true\n").unwrap();
        assert!(raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_deserializes_false() {
        let raw: RawConfig = toml::from_str("[ui]\nshow_thinking = false\n").unwrap();
        assert!(!raw.ui.show_thinking.unwrap());
    }

    #[test]
    fn show_thinking_missing_defaults_true() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.ui.show_thinking);
    }

    #[test]
    fn permissions_loaded_from_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"allow\"\n\n\
             [bash]\nallow = [\n    \"cargo *\",\n]\ndeny = [\n    \"rm -rf *\",\n]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Allow);
        assert_eq!(perms.rules.len(), 2);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[0].tool, ToolKey::native("bash"));
        assert_eq!(perms.rules[0].scope.as_deref(), Some("rm -rf *"));
        assert_eq!(perms.rules[1].effect, Effect::Allow);
        assert_eq!(perms.rules[1].tool, ToolKey::native("bash"));
        assert_eq!(perms.rules[1].scope.as_deref(), Some("cargo *"));
    }

    #[test]
    fn permissions_merge_global_and_project() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm -rf *\"]\n",
        );
        let craft_dir = dir.path().join(".craft");
        fs::create_dir_all(&craft_dir).unwrap();
        fs::write(
            craft_dir.join("permissions.toml"),
            "[read]\nallow = true\n\
             [write]\ndeny = [\"/etc/*\"]\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert_eq!(perms.rules.len(), 4);

        let deny_rules: Vec<_> = perms
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Deny)
            .collect();
        let allow_rules: Vec<_> = perms
            .rules
            .iter()
            .filter(|r| r.effect == Effect::Allow)
            .collect();

        assert_eq!(deny_rules.len(), 2);
        assert_eq!(deny_rules[0].tool, ToolKey::native("bash"));
        assert_eq!(deny_rules[1].tool, ToolKey::native("write"));

        assert_eq!(allow_rules.len(), 2);
        assert_eq!(allow_rules[0].tool, ToolKey::native("bash"));
        assert_eq!(allow_rules[1].tool, ToolKey::native("read"));
    }

    #[test]
    fn project_default_allow_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let craft_dir = dir.path().join(".craft");
        fs::create_dir_all(&craft_dir).unwrap();
        fs::write(craft_dir.join("permissions.toml"), "default = \"allow\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);
    }

    #[test]
    fn append_permission_rule_writes_to_permissions_file() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("rm -rf *"),
            Effect::Deny,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[bash]"));
        assert!(content.contains("cargo *"));
        assert!(content.contains("rm -rf *"));
        assert!(!content.contains("[permissions]"));
    }

    #[test]
    fn no_permissions_file_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);
        assert!(perms.rules.is_empty());
    }

    #[test]
    fn permissions_default_deny_global() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "default = \"deny\"\n");

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn permissions_default_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[bash]\ndefault = \"allow\"\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Deny);
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("bash")).copied(),
            Some(DefaultEffect::Allow)
        );
    }

    #[test]
    fn permissions_default_merge_project_overrides_global_per_tool() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[bash]\ndefault = \"allow\"\n");
        let craft_dir = dir.path().join(".craft");
        fs::create_dir_all(&craft_dir).unwrap();
        fs::write(
            craft_dir.join("permissions.toml"),
            "[bash]\ndefault = \"deny\"\n",
        )
        .unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::native("bash")).copied(),
            Some(DefaultEffect::Deny)
        );
    }

    #[test]
    fn permissions_allow_all_migrated() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "allow_all = true\n\n[bash]\nallow = [\"cargo *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Allow);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(!content.contains("allow_all"));
        assert!(content.contains("default = \"allow\""));
    }

    #[test]
    fn permissions_allow_all_false_migrated_removed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "allow_all = false\n");

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Prompt);

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(!content.contains("allow_all"));
        assert!(!content.contains("default"));
    }

    #[test]
    fn project_default_deny_allowed() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        let craft_dir = dir.path().join(".craft");
        fs::create_dir_all(&craft_dir).unwrap();
        fs::write(craft_dir.join("permissions.toml"), "default = \"deny\"\n").unwrap();

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.default, DefaultEffect::Deny);
    }

    #[test]
    fn deny_rules_before_allow_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[bash]\nallow = [\"git *\"]\ndeny = [\"rm *\"]\n",
        );

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(perms.rules[1].effect, Effect::Allow);
    }

    #[test]
    fn append_permission_rule_deduplicates() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();
        append_permission_rule_with_global(
            &ToolKey::native("bash"),
            Some("cargo *"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert_eq!(content.matches("cargo *").count(), 1);
    }

    #[test]
    fn append_permission_rule_writes_mcp_nested_form() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();

        append_permission_rule_with_global(
            &ToolKey::parse("deepwiki.search").unwrap(),
            Some("*"),
            Effect::Allow,
            &PermissionTarget::Global,
            Some(global.clone()),
        )
        .unwrap();

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "nested table present");
        assert!(content.contains("\"search\""), "tool name in array");
        assert!(!content.contains("deepwiki.search"), "no flat key");
        assert!(!content.contains("__"), "no __ separator");
    }

    #[test]
    fn permissions_mcp_per_tool_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.deepwiki]\nallow = [\"search\", \"fetch\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 2);
        assert!(perms.rules.iter().any(|r| r.tool
            == ToolKey::McpTool {
                server: "deepwiki".into(),
                tool: "search".into()
            }
            && r.effect == Effect::Allow));
        assert!(perms.rules.iter().any(|r| r.tool
            == ToolKey::McpTool {
                server: "deepwiki".into(),
                tool: "fetch".into()
            }
            && r.effect == Effect::Allow));
    }

    #[test]
    fn permissions_mcp_server_wide_allow_true_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.deepwiki]\nallow = true\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 0, "no rules generated");
        assert!(
            !perms.tool_defaults.contains_key(&ToolKey::McpServer {
                server: "deepwiki".into()
            }),
            "allow = true is deprecated and ignored — no default injected"
        );
    }

    #[test]
    fn permissions_mcp_deny_true_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.server]\ndeny = true\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(
            !perms.tool_defaults.contains_key(&ToolKey::McpServer {
                server: "server".into()
            }),
            "deny = true is deprecated and ignored — no default injected"
        );
    }

    #[test]
    fn explicit_default_preserved_with_deprecated_deny_true() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.server]\ndefault = \"allow\"\ndeny = true\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "server".into()
            }),
            Some(&DefaultEffect::Allow),
            "explicit default still works; deprecated deny = true is ignored"
        );
    }

    #[test]
    fn permissions_mcp_deny_rules() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.github]\ndeny = [\"admin_delete\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "github".into(),
                tool: "admin_delete".into()
            }
        );
        assert_eq!(perms.rules[0].effect, Effect::Deny);
    }

    #[test]
    fn permissions_mcp_dotted_tool_name_rejected() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[mcp.myserver]\nallow = [\"web.search\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(perms.rules.len(), 0, "dotted tool name should be rejected");
    }

    #[test]
    fn permissions_mcp_default_allow() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "default = \"deny\"\n\n[mcp.exa]\ndefault = \"allow\"\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Allow),
            "MCP server default should be extracted"
        );
    }

    #[test]
    fn permissions_mcp_default_prompt() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(
            dir.path(),
            "[mcp.exa]\ndefault = \"prompt\"\nallow = [\"search\"]\n",
        );
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert_eq!(
            perms.tool_defaults.get(&ToolKey::McpServer {
                server: "exa".into()
            }),
            Some(&DefaultEffect::Prompt),
            "MCP server default = prompt should be extracted"
        );
        assert_eq!(perms.rules.len(), 1);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::McpTool {
                server: "exa".into(),
                tool: "search".into()
            }
        );
    }

    #[test]
    fn migrate_mcp_old_flat_keys() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("permissions.toml"),
            "[\"mcp:deepwiki__search\"]\nallow = true\n\n[\"mcp:github__issue\"]\nallow = [\"read\"]\n",
        )
        .unwrap();

        let _perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "server table present");
        assert!(content.contains("[mcp.github]"), "server table present");
        assert!(content.contains("\"search\""), "tool name migrated");
        assert!(content.contains("\"issue\""), "tool name migrated");
        assert!(
            !content.contains("mcp:deepwiki__search"),
            "old flat key gone"
        );
        assert!(!content.contains("mcp:github__issue"), "old flat key gone");
        assert!(!content.contains("__"), "no old __ separator remains");
    }

    #[test]
    fn migrate_mcp_nested_bare_keys() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("permissions.toml"),
            "[mcp]\n\ndeepwiki__search = true\n\ngithub__issue = true\n",
        )
        .unwrap();

        let _perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));

        let content = fs::read_to_string(global.join("permissions.toml")).unwrap();
        assert!(content.contains("[mcp.deepwiki]"), "server table present");
        assert!(content.contains("[mcp.github]"), "server table present");
        assert!(content.contains("\"search\""), "tool name migrated");
        assert!(content.contains("\"issue\""), "tool name migrated");
        assert!(!content.contains("__"), "no old __ separator remains");
    }

    #[test]
    fn empty_tool_key_sections_ignored() {
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        write_global_permissions(dir.path(), "[\"\"]\ndefault = \"allow\"\nallow = [\"x\"]\n");
        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        assert!(perms.rules.is_empty());
        assert!(perms.tool_defaults.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn migration_applies_in_memory_when_write_fails() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("permissions.toml"),
            "[\"mcp:github__delete\"]\ndeny = true\n",
        )
        .unwrap();
        fs::set_permissions(&global, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(global.join("probe"), b"x").is_ok() {
            return; // running as root, cannot simulate a read-only dir
        }

        let perms = load_permissions_inner(dir.path(), std::slice::from_ref(&global));
        fs::set_permissions(&global, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(perms.rules.len(), 1);
        assert_eq!(perms.rules[0].effect, Effect::Deny);
        assert_eq!(
            perms.rules[0].tool,
            ToolKey::parse("github.delete").unwrap()
        );
    }

    #[test]
    fn env_file_precedence() {
        const GLOBAL_ONLY: &str = "TEST_CRAFT_GLOBAL_ONLY";
        const PROJECT_SHADOWS: &str = "TEST_CRAFT_PROJECT_SHADOWS";
        const PROCESS_WINS: &str = "TEST_CRAFT_PROCESS_WINS";

        let dir = TempDir::new().unwrap();
        let global = global_config_dir(dir.path());
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join(".env"),
            format!("{GLOBAL_ONLY}=global\n{PROJECT_SHADOWS}=global\n{PROCESS_WINS}=global"),
        )
        .unwrap();

        let craft_dir = dir.path().join(".craft");
        fs::create_dir_all(&craft_dir).unwrap();
        fs::write(
            craft_dir.join(".env"),
            format!("{PROJECT_SHADOWS}=project\n{PROCESS_WINS}=project"),
        )
        .unwrap();

        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::set_var(PROCESS_WINS, "process");
        }

        load_env_files_with_global(dir.path(), Some(&global));

        assert_eq!(std::env::var(GLOBAL_ONLY).unwrap(), "global");
        assert_eq!(std::env::var(PROJECT_SHADOWS).unwrap(), "project");
        assert_eq!(std::env::var(PROCESS_WINS).unwrap(), "process");

        unsafe {
            std::env::remove_var(GLOBAL_ONLY);
            std::env::remove_var(PROJECT_SHADOWS);
            std::env::remove_var(PROCESS_WINS);
        }
    }

    #[test]
    fn plugins_default_builtins_populated_when_enabled() {
        let config = RawConfig::default().into_config(false).unwrap();
        assert!(
            !config.plugins.names.is_empty(),
            "enabled plugins should have default builtins"
        );
    }

    fn plugin_enabled(enabled: bool) -> PluginFileConfig {
        PluginFileConfig {
            enabled: Some(enabled),
            opts: JsonMap::new(),
        }
    }

    #[test]
    fn merge_plugins_overlay_wins_per_key() {
        let mut base: RawConfig = toml::from_str(
            "[plugins.glob]\nenabled = true\n\
             [plugins.websearch]\nenabled = true\n\
             [plugins.grep]\nenabled = true\nsearch_result_limit = 200\nmax_line_bytes = 900\n",
        )
        .unwrap();
        let overlay: RawConfig = toml::from_str(
            "[plugins.websearch]\nenabled = false\n\
             [plugins.alpha_tool]\nenabled = true\n\
             [plugins.grep]\nsearch_result_limit = 50\n",
        )
        .unwrap();

        base.merge(overlay);
        assert_eq!(
            base.plugins["glob"].enabled,
            Some(true),
            "base-only key preserved"
        );
        assert_eq!(
            base.plugins["websearch"].enabled,
            Some(false),
            "overlay replaces"
        );
        assert_eq!(
            base.plugins["alpha_tool"].enabled,
            Some(true),
            "overlay-only key added"
        );
        let grep = &base.plugins["grep"];
        assert_eq!(
            grep.enabled,
            Some(true),
            "enabled preserved when overlay omits it"
        );
        assert_eq!(
            grep.opts["search_result_limit"],
            serde_json::json!(50),
            "overlay opt wins"
        );
        assert_eq!(
            grep.opts["max_line_bytes"],
            serde_json::json!(900),
            "base opt preserved"
        );
    }

    #[test]
    fn merge_always_fast_and_thinking_overlay_wins() {
        let mut base = RawConfig {
            always_fast: Some(false),
            always_thinking: Some(AlwaysThinking::Mode("off".into())),
            ..Default::default()
        };
        let overlay = RawConfig {
            always_fast: Some(true),
            always_thinking: Some(AlwaysThinking::Toggle(true)),
            ..Default::default()
        };
        base.merge(overlay);

        assert_eq!(base.always_fast, Some(true), "overlay wins");
        assert_eq!(
            base.always_thinking,
            Some(AlwaysThinking::Toggle(true)),
            "overlay wins"
        );
    }

    #[test_case(AlwaysThinking::Toggle(true), StoredThinking::Adaptive ; "toggle_true")]
    #[test_case(AlwaysThinking::Toggle(false), StoredThinking::Off ; "toggle_false")]
    #[test_case(AlwaysThinking::Budget(8192), StoredThinking::Budget { tokens: 8192 } ; "budget_number")]
    #[test_case(AlwaysThinking::Mode("xhigh".into()), StoredThinking::Effort { level: Effort::XHigh } ; "effort_xhigh")]
    #[test_case(AlwaysThinking::Mode("minimal".into()), StoredThinking::Effort { level: Effort::Minimal } ; "effort_minimal")]
    fn always_thinking_toggle_resolve(input: AlwaysThinking, expected: StoredThinking) {
        assert_eq!(input.resolve(), Ok(expected));
    }

    #[test]
    fn into_config_resolves_always_thinking() {
        let defaults = RawConfig::default().into_config(false).unwrap();
        assert!(defaults.always_thinking.is_none());

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("8192".into())),
            ..Default::default()
        };
        let config = raw.into_config(false).unwrap();
        assert_eq!(
            config.always_thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );

        let raw = RawConfig {
            always_thinking: Some(AlwaysThinking::Mode("fast".into())),
            ..Default::default()
        };
        let err = raw.into_config(false).err().expect("expected config error");
        assert!(matches!(err, ConfigError::Thinking(_)));
    }

    #[test]
    fn max_input_lines_defaults_and_deserializes() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = raw.into_config(false).unwrap();
        assert_eq!(config.ui.max_input_lines, DEFAULT_MAX_INPUT_LINES);

        let raw: RawConfig = toml::from_str("[ui]\nmax_input_lines = 5\n").unwrap();
        assert_eq!(raw.ui.max_input_lines.unwrap(), 5);
    }

    #[test_case("[ui]\nsplash_animaton = true\n" ; "top_level_typo")]
    #[test_case("agent = { bsh_timeout_secs = 60 }\n" ; "nested_section_typo")]
    fn deny_unknown_fields_rejects(toml_str: &str) {
        let result: Result<RawConfig, _> = toml::from_str(toml_str);
        assert!(
            result.is_err(),
            "unknown field should be rejected: {toml_str}"
        );
    }

    #[test]
    fn deny_unknown_fields_accepts_valid_plugins() {
        const VALID: &str =
            "[plugins.bash]\nenabled = true\n[plugins.websearch]\nenabled = false\n";
        let result: Result<RawConfig, _> = toml::from_str(VALID);
        assert!(
            result.is_ok(),
            "valid plugins section should parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn plugin_extra_keys_parse_into_opts() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = true\ntimeout_secs = 180\n").unwrap();
        let bash = &raw.plugins["bash"];
        assert_eq!(bash.enabled, Some(true));
        assert_eq!(bash.opts["timeout_secs"], serde_json::json!(180));
    }

    #[test]
    fn from_plugins_default() {
        let plugins = PluginsConfig::from_plugins(HashMap::new());
        let expected: Vec<String> = DEFAULT_BUILTINS.iter().map(|s| s.to_string()).collect();
        assert_eq!(plugins.names, expected);
    }

    #[test]
    fn from_plugins_enable_disable_and_sort() {
        let mut entries = HashMap::new();
        entries.insert("websearch".to_string(), plugin_enabled(false));
        entries.insert("zeta".to_string(), plugin_enabled(true));
        entries.insert("alpha".to_string(), plugin_enabled(true));
        entries.insert("custom_tool".to_string(), PluginFileConfig::default());

        let plugins = PluginsConfig::from_plugins(entries);
        assert!(
            !plugins.names.contains(&"websearch".to_string()),
            "disabled builtin removed"
        );
        assert!(
            plugins.names.contains(&"glob".to_string()),
            "untouched builtin stays"
        );
        assert!(
            plugins.names.contains(&"bash".to_string()),
            "bash is a default builtin"
        );
        assert!(
            !plugins.names.contains(&"custom_tool".to_string()),
            "enabled=None non-default ignored"
        );

        let extras: Vec<_> = plugins
            .names
            .iter()
            .filter(|t| !DEFAULT_BUILTINS.contains(&t.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            extras,
            vec!["alpha", "zeta"],
            "extras sorted alphabetically"
        );
    }

    #[test]
    fn into_config_wires_plugin_names_and_opts() {
        let raw: RawConfig = toml::from_str(
            "[plugins.bash]\ntimeout_secs = 180\n[plugins.websearch]\nenabled = false\n",
        )
        .unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.plugins.names.contains(&"bash".to_string()));
        assert!(!config.plugins.names.contains(&"websearch".to_string()));
        assert!(
            config.plugins.names.contains(&"glob".to_string()),
            "untouched builtin stays"
        );
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180)
        );
        assert!(
            !config.plugins.opts.contains_key("websearch"),
            "enabled-only tables produce no opts"
        );
    }

    #[test]
    fn disabled_plugin_keeps_opts_but_not_load_entry() {
        let raw: RawConfig =
            toml::from_str("[plugins.bash]\nenabled = false\ntimeout_secs = 180\n").unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(!config.plugins.names.contains(&"bash".to_string()));
        assert_eq!(
            config.plugins.opts["bash"]["timeout_secs"],
            serde_json::json!(180),
            "opts survive for when the plugin is re-enabled"
        );
    }

    #[test_case("enabled = false" ; "enabled_false")]
    #[test_case("search_result_limit = 50" ; "opts_only")]
    fn unknown_plugin_name_errors(body: &str) {
        let raw: RawConfig = toml::from_str(&format!("[plugins.gerp]\n{body}\n")).unwrap();
        let Err(err) = raw.into_config(false) else {
            panic!("plugins.gerp should be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no bundled plugin is named \"gerp\"") && msg.contains("grep"),
            "error should name the typo and list bundled plugins, got: {msg}"
        );
    }

    #[test]
    fn renamed_tools_table_errors() {
        let raw: RawConfig = toml::from_str("[tools.bash]\nenabled = true\n").unwrap();
        let Err(err) = raw.into_config(false) else {
            panic!("old tools table should be rejected");
        };
        assert!(
            err.to_string().contains("renamed to `plugins`"),
            "got: {err}"
        );
    }

    #[test]
    fn merge_tool_output_lines_field_level_overlay() {
        let mut base = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(50),
                    read: Some(30),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let overlay = RawConfig {
            ui: UiFileConfig {
                tool_output_lines: Some(ToolOutputLinesFile {
                    bash: Some(100),
                    grep: Some(15),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        base.merge(overlay);
        let tol = base.ui.tool_output_lines.as_ref().unwrap();
        assert_eq!(tol.bash, Some(100), "overlay wins");
        assert_eq!(tol.read, Some(30), "base preserved");
        assert_eq!(tol.grep, Some(15), "overlay added");
    }

    #[test]
    fn into_config_plugins_flow_to_names() {
        let raw: RawConfig = toml::from_str(
            "[plugins.bash]\nenabled = true\n[plugins.websearch]\nenabled = false\n",
        )
        .unwrap();
        let config = raw.into_config(false).unwrap();
        assert!(config.plugins.names.contains(&"bash".to_string()));
        assert!(!config.plugins.names.contains(&"websearch".to_string()));
        assert!(config.plugins.names.contains(&"glob".to_string()));
    }

    #[test]
    fn default_builtins_sorted() {
        for pair in DEFAULT_BUILTINS.windows(2) {
            assert!(
                pair[0] < pair[1],
                "DEFAULT_BUILTINS not sorted: {:?} >= {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn opt_in_tools_require_explicit_enable() {
        let default_config = RawConfig::default().into_config(false).unwrap();
        for &name in OPT_IN_TOOLS {
            assert!(
                default_config
                    .agent
                    .disabled_tools
                    .contains(&name.to_string()),
                "{name} should be disabled by default"
            );
        }

        let mut plugins = HashMap::new();
        for &name in OPT_IN_TOOLS {
            plugins.insert(name.to_string(), plugin_enabled(true));
        }
        let enabled_config = RawConfig {
            plugins,
            ..Default::default()
        }
        .into_config(false)
        .unwrap();
        for &name in OPT_IN_TOOLS {
            assert!(
                !enabled_config
                    .agent
                    .disabled_tools
                    .contains(&name.to_string()),
                "{name} should be enabled when configured"
            );
        }
    }

    #[test_case(Some(32_768), 200_000, Some(32_768) ; "reserve_tokens_absolute")]
    #[test_case(None, 200_000, None ; "no_threshold_returns_none")]
    fn resolve_reserve_tokens_uses_absolute_or_none(
        reserve: Option<u32>,
        context_window: u32,
        expected: Option<u32>,
    ) {
        let threshold = ModelThreshold {
            reserve_tokens: reserve,
            ..Default::default()
        };
        assert_eq!(resolve_reserve_tokens(&threshold, context_window), expected);
    }

    #[test]
    fn resolve_reserve_tokens_compact_percent_rounds() {
        let threshold = ModelThreshold {
            compact_percent: Some(65),
            ..Default::default()
        };
        assert_eq!(resolve_reserve_tokens(&threshold, 200_000), Some(70_000));
    }

    #[test_case(0 ; "percent_zero")]
    #[test_case(100 ; "percent_hundred")]
    fn resolve_reserve_tokens_rejects_out_of_range(pct: u8) {
        let threshold = ModelThreshold {
            compact_percent: Some(pct),
            ..Default::default()
        };
        assert_eq!(resolve_reserve_tokens(&threshold, 200_000), None);
    }

    #[test]
    fn resolve_reserve_tokens_rejects_zero_context_window() {
        let threshold = ModelThreshold {
            compact_percent: Some(65),
            ..Default::default()
        };
        assert_eq!(resolve_reserve_tokens(&threshold, 0), None);
    }

    #[test_case(200_000, 32_768, 4_096, 200_000 ; "declared_above_floor")]
    #[test_case(30_000, 32_768, 4_096, 36_864 ; "declared_below_floor_clamped")]
    #[test_case(200_000, 0, 0, 200_000 ; "zero_floor_keeps_declared")]
    fn effective_context_window_enforces_floor(
        declared: u32,
        reserve_tokens: u32,
        compaction_buffer: u32,
        expected: u32,
    ) {
        assert_eq!(
            effective_context_window(declared, reserve_tokens, compaction_buffer),
            expected
        );
    }

    #[test]
    fn effective_context_window_saturates_on_overflow() {
        assert_eq!(
            effective_context_window(10_000, u32::MAX, 10),
            u32::MAX.saturating_add(10)
        );
    }

    #[test]
    fn resolve_threshold_prefers_provider_model_key() {
        let mut config = CompactionConfig::default();
        config.model_thresholds.insert(
            "anthropic/claude".into(),
            ModelThreshold {
                reserve_tokens: Some(10_000),
                ..Default::default()
            },
        );
        config.model_thresholds.insert(
            "claude".into(),
            ModelThreshold {
                reserve_tokens: Some(20_000),
                ..Default::default()
            },
        );
        let t = resolve_threshold(&config, Some("anthropic/claude"), "claude").unwrap();
        assert_eq!(t.reserve_tokens, Some(10_000));
    }

    #[test]
    fn resolve_threshold_falls_back_to_model_id_then_global() {
        let config = CompactionConfig {
            model_thresholds: HashMap::from([(
                "claude".into(),
                ModelThreshold {
                    reserve_tokens: Some(20_000),
                    ..Default::default()
                },
            )]),
            global_threshold: Some(ModelThreshold {
                reserve_tokens: Some(30_000),
                ..Default::default()
            }),
        };
        let t = resolve_threshold(&config, Some("anthropic/other"), "claude").unwrap();
        assert_eq!(t.reserve_tokens, Some(20_000));

        let t = resolve_threshold(&config, Some("anthropic/other"), "unknown").unwrap();
        assert_eq!(t.reserve_tokens, Some(30_000));

        assert_eq!(
            resolve_threshold(&config, None, "unknown")
                .unwrap()
                .reserve_tokens,
            Some(30_000)
        );
    }

    #[test]
    fn resolve_threshold_returns_none_when_unset() {
        let config = CompactionConfig::default();
        assert!(resolve_threshold(&config, Some("anthropic/x"), "x").is_none());
    }

    #[test]
    fn provider_model_lists_inherit_replace_and_clear() {
        let mut global = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["anthropic/*".into()]),
                excluded_models: Some(vec!["*/*-preview".into()]),
                ..Default::default()
            },
            ..Default::default()
        };
        global.merge(RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(Vec::new()),
                excluded_models: None,
                ..Default::default()
            },
            ..Default::default()
        });

        let provider = global.into_config(false).unwrap().provider;
        assert!(provider.allowed_models.is_empty());
        assert_eq!(provider.excluded_models, ["*/*-preview"]);
        assert!(provider.model_policy.allows("openai/gpt-5"));
        assert!(!provider.model_policy.allows("openai/gpt-5-preview"));
    }

    #[test]
    fn model_policy_matches_qualified_specs() {
        let config = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["openai/gpt-5".into(), "opencode/*".into()]),
                excluded_models: Some(vec!["*/*-preview".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false)
        .unwrap();
        let policy = &config.provider.model_policy;

        assert!(policy.allows("openai/gpt-5"));
        assert!(policy.allows("opencode/nvidia/openai/gpt-oss-120b"));
        assert!(!policy.allows("anthropic/claude-sonnet-4-6"));
        assert!(!policy.allows("opencode/gpt-5-preview"));

        let exclude_only = RawConfig {
            provider: ProviderFileConfig {
                excluded_models: Some(vec!["anthropic/*".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false)
        .unwrap();
        assert!(exclude_only.provider.model_policy.allows("openai/gpt-5"));
        assert!(
            !exclude_only
                .provider
                .model_policy
                .allows("anthropic/claude-sonnet-4-6")
        );
    }

    #[test]
    fn invalid_model_pattern_is_a_config_error() {
        let result = RawConfig {
            provider: ProviderFileConfig {
                allowed_models: Some(vec!["[".into()]),
                ..Default::default()
            },
            ..Default::default()
        }
        .into_config(false);

        assert!(matches!(
            result,
            Err(ConfigError::InvalidModelPattern { field: "allowed_models", pattern, .. }) if pattern == "["
        ));
    }
}
