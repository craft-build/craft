//! Configuration for `~/.config/craft.bml` or the split files under
//! `~/.config/craft/*.bml` (legacy `~/.craft/` is searched first when it
//! exists). Format is BarkML.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;
use snafu::ResultExt;

use crate::error::{
    InvalidBaseUrlSnafu, InvalidBmlSnafu, InvalidProviderSnafu, InvalidSnafu, LoadConfigSnafu,
    ReadConfigSnafu, Result,
};
use crate::providers::ProviderKind;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agent: AgentConfig,
    /// Seed new sessions; saved session preferences win on resume.
    pub always_thinking: Option<crate::thinking::ThinkingConfig>,
    /// Compaction stages, ascending by context fill ratio.
    #[serde(default = "default_compaction")]
    pub compaction: Vec<CompactionConfig>,
    /// Context reserved so compaction fires before the window truly fills.
    #[serde(default = "default_compaction_buffer")]
    pub compaction_buffer: CompactionBuffer,
    /// Tool-output pre-compression applied to the model's request view.
    pub compression: crate::compression::CompressionConfig,
    /// User keybinding overlay: snake_case action id → chord list (F.1).
    /// An empty list disables the action; unknown ids are warned and dropped.
    #[serde(default)]
    pub keybindings: std::collections::BTreeMap<String, Vec<String>>,
    /// OS command sandbox for `bash` (fail-closed when the backend is
    /// required but missing). Defaults preserve the shipped behavior:
    /// enabled, `workspace_write`, network allowed.
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

/// `[sandbox]` section. Mirrors the predecessor `craft-config::SandboxConfig`;
/// resolved into a [`crate::sandbox::SandboxPolicy`] at session start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub enabled: bool,
    /// `workspace_write` | `read_only` | `danger_full_access` | `off`.
    pub mode: crate::sandbox::SandboxMode,
    pub network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: crate::sandbox::SandboxMode::WorkspaceWrite,
            network: true,
        }
    }
}

/// Which compaction strategy runs when a stage's threshold is crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionKind {
    /// LLM-powered summary of the conversation head.
    Llm,
    /// Deterministic no-LLM summary (ported from Craft's VCC compaction).
    Vcc,
}

/// One `[[compaction]]` stage: run `kind` once history reaches `context`
/// (a fill ratio of the model's context window, 0-1).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    pub kind: CompactionKind,
    /// Accepts a number or a quoted string ("0.8").
    #[serde(deserialize_with = "de_ratio")]
    pub context: f64,
}

fn de_ratio<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Ratio {
        Number(f64),
        String(String),
    }
    match Ratio::deserialize(deserializer)? {
        Ratio::Number(value) => Ok(value),
        Ratio::String(text) => text
            .trim()
            .parse()
            .map_err(|_| serde::de::Error::custom("context must be a ratio between 0 and 1")),
    }
}

/// Context reserved for compaction: an absolute token count or a percent of
/// the context window (BML: `20000` or `"20%"`). Subtracted from the window
/// when the engine decides whether the context is full, so compaction fires
/// before the provider would start rejecting requests (ported from Craft's
/// `craft_config::CompactionBuffer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionBuffer {
    Tokens(u32),
    Percent(u8),
}

/// Tokens below this the buffer is pointless: overflow would arrive before
/// compaction could react.
pub const MIN_COMPACTION_BUFFER: u32 = 1_000;
pub const DEFAULT_COMPACTION_BUFFER: CompactionBuffer = CompactionBuffer::Percent(20);

fn default_compaction_buffer() -> CompactionBuffer {
    DEFAULT_COMPACTION_BUFFER
}

impl Default for CompactionBuffer {
    fn default() -> Self {
        DEFAULT_COMPACTION_BUFFER
    }
}

impl CompactionBuffer {
    /// Resolve against a context window: percent scales with the window,
    /// token counts are absolute.
    pub fn resolve(self, context_window: u32) -> u32 {
        match self {
            Self::Tokens(n) => n,
            Self::Percent(p) => (u64::from(context_window) * u64::from(p) / 100) as u32,
        }
    }
}

impl<'de> Deserialize<'de> for CompactionBuffer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = CompactionBuffer;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a token count (>= 1000) or a percent of the window like \"20%\"")
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v).map_err(|_| E::custom("compaction_buffer must be nonnegative"))?;
                self.visit_u64(v as u64)
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
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                let Some(digits) = v.strip_suffix('%') else {
                    return Err(E::custom("expected a trailing '%' for a percent buffer"));
                };
                let percent: u8 = digits.trim().parse().map_err(|_| {
                    E::custom(format!(
                        "invalid compaction_buffer {v:?}: expected a percent 0-100"
                    ))
                })?;
                Ok(CompactionBuffer::Percent(percent))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

fn default_compaction() -> Vec<CompactionConfig> {
    vec![
        CompactionConfig {
            kind: CompactionKind::Vcc,
            context: 0.6,
        },
        CompactionConfig {
            kind: CompactionKind::Llm,
            context: 0.8,
        },
    ]
}

/// Advisor auto-act severity threshold (C.12). Declaration order doubles as
/// the severity ranking: `Off < Nit < Concern < Blocker`.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Deserialize,
    serde::Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AdvisorAutoAct {
    #[default]
    Off,
    Nit,
    Concern,
    Blocker,
}

/// Always-on lightweight reviewer that reads the transcript delta after a
/// terminal reply and emits at most one deduped note (C.12). Off by default.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Maximum advisor notes kept in the dedup FIFO.
    #[serde(default = "default_advisor_dedup_size")]
    pub dedup_size: usize,
    /// Minimum severity that triggers an automatic follow-up turn instead of
    /// stopping for the user. Notes at or above this severity are pushed into
    /// the agent's own context and the run continues.
    #[serde(default = "default_advisor_auto_act")]
    pub auto_act: AdvisorAutoAct,
    /// Maximum advisor-driven follow-up turns a single run may take.
    #[serde(default = "default_advisor_max_act_turns")]
    pub max_act_turns: u32,
}

fn default_advisor_dedup_size() -> usize {
    8
}

fn default_advisor_auto_act() -> AdvisorAutoAct {
    AdvisorAutoAct::Concern
}

fn default_advisor_max_act_turns() -> u32 {
    2
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dedup_size: default_advisor_dedup_size(),
            auto_act: default_advisor_auto_act(),
            max_act_turns: default_advisor_max_act_turns(),
        }
    }
}

/// Defaults for each agent run, independent of provider/model selection.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// System instructions. An empty string keeps the built-in defaults; any
    /// text is appended to the assembled system prompt's instructions slot.
    pub preamble: String,
    /// Omit to preserve the provider/model's default sampling behavior.
    pub temperature: Option<f64>,
    /// Request-level output cap, not a model catalog metadata override.
    pub max_tokens: Option<u64>,
    /// Run default and inherited subagent preference.
    pub thinking: Option<crate::thinking::ThinkingConfig>,
    /// Bound on model calls per run. `None` keeps each surface's default
    /// (unbounded in the TUI, [`crate::run::RunParams`] defaults headless);
    /// set by `--max-turns` (G.1).
    pub max_turns: Option<u32>,
    /// Post-turn advisor (C.12).
    pub advisor: AdvisorConfig,
    /// Post-turn argosy memory extraction (Phase 4 of the argosy
    /// integration): durable facts from the turn are written into the
    /// project's local argosy after a successful run. Default on.
    #[serde(default = "default_memory_extraction")]
    pub memory_extraction: bool,
}

fn default_memory_extraction() -> bool {
    true
}

impl AgentConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_tokens == Some(0) {
            return InvalidSnafu {
                reason: "agent.max_tokens must be positive",
            }
            .fail();
        }
        if self.max_turns == Some(0) {
            return InvalidSnafu {
                reason: "agent.max_turns must be positive",
            }
            .fail();
        }
        // Providers have different upper bounds (and some disallow temperature).
        // Validate the portable constraint here; leave model-specific rules to Rig.
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            return InvalidSnafu {
                reason: "agent.temperature must be finite and nonnegative",
            }
            .fail();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// Environment variable containing the credential; never store keys in config.
    pub api_key_env: Option<String>,
    /// Full API base, including any protocol prefix such as `/v1`.
    pub base_url: Option<String>,
    /// Azure OpenAI API version; other providers reject this field.
    pub api_version: Option<String>,
    /// ChatGPT account ID when using an explicit access token.
    pub account_id: Option<String>,
    /// Disable discovery for compatible servers without a models endpoint.
    #[serde(default = "default_discover_models")]
    pub discover_models: bool,
    #[serde(default)]
    pub models: BTreeMap<String, ModelConfig>,
}

fn default_discover_models() -> bool {
    true
}

/// Partial metadata overrides, keyed by the exact provider model/deployment ID.
/// An empty table is sufficient to register an otherwise unlisted model.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub name: Option<String>,
    pub description: Option<String>,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub supports_thinking: Option<bool>,
    pub requires_thinking: Option<bool>,
    pub reasoning_options: Option<Vec<crate::thinking::ReasoningOption>>,
    pub thinking_fields: Option<crate::thinking::ThinkingFields>,
}

impl Config {
    pub async fn load() -> Result<Self> {
        let loaded = crate::bml::load_global();
        if let Some((path, error)) = loaded.errors.first() {
            return Err(error.clone())
                .context(InvalidBmlSnafu)
                .context(LoadConfigSnafu { path: path.clone() });
        }
        match loaded.doc {
            Some(doc) => Self::from_statement(&doc),
            None => {
                crate::bml::warn_legacy_toml(false);
                Ok(Self::default())
            }
        }
    }

    /// A missing file means no configured providers. Other I/O errors are fatal.
    pub async fn load_from(path: &Path) -> Result<Self> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error).context(ReadConfigSnafu {
                    path: path.to_path_buf(),
                });
            }
        };
        Self::parse(&text).with_context(|_| LoadConfigSnafu {
            path: path.to_path_buf(),
        })
    }

    pub fn parse(text: &str) -> Result<Self> {
        // An empty document keeps every default; barkml itself rejects it.
        if text.trim().is_empty() {
            let doc = barkml::Statement::new_module(
                "main",
                indexmap::IndexMap::new(),
                barkml::Metadata::default(),
            );
            return Self::from_statement(&doc);
        }
        let doc = crate::bml::parse(text).context(InvalidBmlSnafu)?;
        let config = Self::from_statement(&doc)?;
        Self::validate(config)
    }

    /// Extract a [`Config`] from a merged BarkML document, taking only the
    /// sections that belong to the main config (`agent`, `provider`,
    /// `compaction`, `compression`, `keybind`/`keybindings`); unrelated
    /// blocks (`mcp`, `permissions`) are left for their own loaders.
    pub fn from_statement(doc: &barkml::Statement) -> Result<Self> {
        use serde_json::Value as Json;
        let mut map = serde_json::Map::new();

        let root = crate::bml::container_json(doc);
        if let Some(value) = root.get("always_thinking") {
            map.insert("always_thinking".into(), value.clone());
        }
        for section in ["agent", "compression"] {
            if let Some(child) = doc.get_child(section, &[]) {
                map.insert(section.to_string(), crate::bml::container_json(child));
            }
        }

        // `provider "name" { ... }` labeled blocks → providers map.
        let mut providers = serde_json::Map::new();
        for (id, labels, block) in doc.blocks() {
            if id != "provider" {
                continue;
            }
            let Some(name) = labels.first().and_then(|l| l.as_string().cloned()) else {
                continue;
            };
            let mut json = crate::bml::container_json(block);
            // Labeled `model "id" { ... }` children collect under `model`;
            // the struct field is `models`.
            if let Some(models) = json.as_object_mut().unwrap().remove("model") {
                json.as_object_mut()
                    .unwrap()
                    .insert("models".to_string(), models);
            }
            providers.insert(name, json);
        }
        if !providers.is_empty() {
            map.insert("providers".to_string(), Json::Object(providers));
        }

        // `compaction "kind" { ... }` labeled blocks → ordered stages.
        let mut stages = Vec::new();
        for (id, labels, block) in doc.blocks() {
            if id != "compaction" {
                continue;
            }
            let mut json = crate::bml::container_json(block);
            if let Some(label) = labels.first().and_then(|l| l.as_string().cloned())
                && !json.as_object().is_some_and(|o| o.contains_key("kind"))
            {
                json.as_object_mut()
                    .unwrap()
                    .insert("kind".to_string(), Json::String(label));
            }
            stages.push(json);
        }
        if !stages.is_empty() {
            map.insert("compaction".to_string(), Json::Array(stages));
        }

        if let Some(value) = doc
            .get_child("compaction_buffer", &[])
            .and_then(|child| child.get_value())
        {
            map.insert(
                "compaction_buffer".to_string(),
                crate::bml::value_json(value),
            );
        }

        // `keybind "action" { keys = [...] }` or `keybindings = { ... }`.
        let mut keybindings = serde_json::Map::new();
        for (id, labels, block) in doc.blocks() {
            if id != "keybind" {
                continue;
            }
            let Some(action) = labels.first().and_then(|l| l.as_string().cloned()) else {
                continue;
            };
            let keys = block
                .get_child("keys", &[])
                .and_then(|k| k.get_value())
                .map(crate::bml::value_json)
                .unwrap_or(Json::Array(Vec::new()));
            keybindings.insert(action, keys);
        }
        if !keybindings.is_empty() {
            map.insert("keybindings".to_string(), Json::Object(keybindings));
        } else if let Some(value) = doc
            .get_child("keybindings", &[])
            .and_then(|child| child.get_value())
        {
            map.insert("keybindings".to_string(), crate::bml::value_json(value));
        }

        let config: Self = serde_json::from_value(Json::Object(map)).map_err(|e| {
            InvalidSnafu {
                reason: e.to_string(),
            }
            .build()
        })?;
        Self::validate(config)
    }

    fn validate(config: Self) -> Result<Self> {
        config.agent.validate()?;
        if let Err(reason) = config.compression.validate() {
            return InvalidSnafu { reason }.fail();
        }
        let mut seen_kinds = std::collections::BTreeSet::new();
        for stage in &config.compaction {
            if !stage.context.is_finite() || stage.context <= 0.0 || stage.context >= 1.0 {
                return InvalidSnafu {
                    reason: format!(
                        "compaction stage {:?}: context must be between 0 and 1",
                        stage.kind
                    ),
                }
                .fail();
            }
            if !seen_kinds.insert(stage.kind) {
                return InvalidSnafu {
                    reason: format!(
                        "compaction stage {:?}: each kind may appear at most once",
                        stage.kind
                    ),
                }
                .fail();
            }
        }
        for (name, provider) in &config.providers {
            if name.trim().is_empty() {
                return InvalidSnafu {
                    reason: "provider names must not be empty",
                }
                .fail();
            }
            provider
                .validate()
                .with_context(|_| InvalidProviderSnafu { name: name.clone() })?;
        }
        Ok(config)
    }
}

impl ProviderConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.api_key_env {
            if name.trim().is_empty() || name.contains(['=', '\0']) {
                return InvalidSnafu {
                    reason: "api_key_env must be a nonempty environment variable name",
                }
                .fail();
            }
            if self.kind == ProviderKind::Llamafile {
                return InvalidSnafu {
                    reason: "llamafile does not accept credentials",
                }
                .fail();
            }
            if self.kind == ProviderKind::Bedrock {
                return InvalidSnafu {
                    reason: "bedrock authenticates through the AWS default credential chain \
                             (AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, ~/.aws profiles, or SSO), \
                             not api_key_env",
                }
                .fail();
            }
        }
        if self.base_url.is_some() && self.kind == ProviderKind::Bedrock {
            return InvalidSnafu {
                reason: "bedrock does not accept base_url; override the AWS endpoint with \
                         AWS_ENDPOINT_URL or a profile endpoint_url in ~/.aws/config",
            }
            .fail();
        }
        if let Some(base) = &self.base_url {
            let url = url::Url::parse(base).context(InvalidBaseUrlSnafu)?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return InvalidSnafu {
                    reason: "base_url must be HTTP(S), without credentials, query, or fragment",
                }
                .fail();
            }
        }
        if self.api_version.is_some() && self.kind != ProviderKind::Azure {
            return InvalidSnafu {
                reason: "api_version is only supported by azure",
            }
            .fail();
        }
        if self
            .api_version
            .as_ref()
            .is_some_and(|v| v.trim().is_empty())
        {
            return InvalidSnafu {
                reason: "api_version must not be empty",
            }
            .fail();
        }
        if self.account_id.is_some()
            && (self.kind != ProviderKind::Chatgpt || self.api_key_env.is_none())
        {
            return InvalidSnafu {
                reason: "account_id requires chatgpt with api_key_env pointing to an access token",
            }
            .fail();
        }
        for (id, model) in &self.models {
            if id.trim().is_empty() {
                return InvalidSnafu {
                    reason: "model IDs must not be empty",
                }
                .fail();
            }
            if model.context_length == Some(0) || model.max_output_tokens == Some(0) {
                return InvalidSnafu {
                    reason: format!("model {id:?}: token limits must be positive"),
                }
                .fail();
            }
            if let Some(fields) = &model.thinking_fields
                && fields
                    .off
                    .iter()
                    .chain(fields.adaptive.iter())
                    .chain(fields.levels.values())
                    .any(|value| !value.is_object())
            {
                return InvalidSnafu {
                    reason: format!("model {id:?}: thinking_fields modes must be request objects"),
                }
                .fail();
            }
            if let Some(options) = &model.reasoning_options
                && options
                    .iter()
                    .any(|o| o.min.zip(o.max).is_some_and(|(min, max)| min > max))
            {
                return InvalidSnafu {
                    reason: format!("model {id:?}: reasoning budget min exceeds max"),
                }
                .fail();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_defaults_and_model_overrides_parse() {
        use crate::thinking::{Effort, ThinkingConfig};
        let config = Config::parse(
            r#"
            always_thinking = "low"
            agent { thinking = 4096 }
            provider "local" {
              kind = "openai-compatible"
              model "custom" {
                supports_thinking = true
                requires_thinking = true
                reasoning_options = [{ type = "effort", values = ["low", "high"] }]
                thinking_fields {
                  off { enable_thinking = false }
                  high { enable_thinking = true }
                }
              }
            }
        "#,
        )
        .unwrap();
        assert_eq!(
            config.always_thinking,
            Some(ThinkingConfig::Effort(Effort::Low))
        );
        assert_eq!(config.agent.thinking, Some(ThinkingConfig::Budget(4096)));
        let model = &config.providers["local"].models["custom"];
        assert_eq!(
            model.reasoning_options.as_ref().unwrap()[0].values,
            ["low", "high"]
        );
        assert_eq!(
            model
                .thinking_fields
                .as_ref()
                .unwrap()
                .off
                .as_ref()
                .unwrap()["enable_thinking"],
            false
        );
        for value in ["0", "\"nonsense\""] {
            assert!(Config::parse(&format!("always_thinking = {value}")).is_err());
        }
    }

    #[test]
    fn parses_example() {
        let config = Config::parse(include_str!("../agent.example.bml")).unwrap();
        assert_eq!(config.providers.len(), 4);
        assert_eq!(config.agent.preamble, "");
        assert_eq!(
            config.compression,
            crate::compression::CompressionConfig::default()
        );
        assert_eq!(
            config.compaction,
            vec![
                CompactionConfig {
                    kind: CompactionKind::Vcc,
                    context: 0.6,
                },
                CompactionConfig {
                    kind: CompactionKind::Llm,
                    context: 0.8,
                },
            ]
        );
    }

    #[test]
    fn compaction_defaults_to_vcc_then_llm() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.compaction.len(), 2);
        assert_eq!(config.compaction[0].kind, CompactionKind::Vcc);
        assert!((config.compaction[0].context - 0.6).abs() < 1e-9);
        assert_eq!(config.compaction[1].kind, CompactionKind::Llm);
        assert!((config.compaction[1].context - 0.8).abs() < 1e-9);
        assert_eq!(config.compaction_buffer, DEFAULT_COMPACTION_BUFFER);
    }

    #[test]
    fn compaction_buffer_parses_tokens_and_percent() {
        let tokens = Config::parse("compaction_buffer = 20000").unwrap();
        assert_eq!(tokens.compaction_buffer, CompactionBuffer::Tokens(20000));
        let percent = Config::parse("compaction_buffer = \"20%\"").unwrap();
        assert_eq!(percent.compaction_buffer, CompactionBuffer::Percent(20));
        assert!(Config::parse("compaction_buffer = 100").is_err());
        assert!(Config::parse("compaction_buffer = \"20\"").is_err());
    }

    #[test]
    fn compaction_buffer_resolves_against_window() {
        assert_eq!(CompactionBuffer::Tokens(20000).resolve(100_000), 20_000);
        assert_eq!(CompactionBuffer::Percent(20).resolve(100_000), 20_000);
        assert_eq!(CompactionBuffer::Percent(20).resolve(0), 0);
        assert_eq!(CompactionBuffer::Percent(33).resolve(1000), 330);
    }

    #[test]
    fn compression_defaults_when_absent() {
        let config = Config::parse("").unwrap();
        assert_eq!(
            config.compression,
            crate::compression::CompressionConfig::default()
        );
    }

    #[test]
    fn compression_accepts_overrides() {
        let config = Config::parse(
            "compression {\n  enabled = false\n  code_compression_rate = 0.5\n  max_log_lines = 25\n}",
        )
        .unwrap();
        assert!(!config.compression.enabled);
        assert!((config.compression.code_compression_rate - 0.5).abs() < 1e-6);
        assert_eq!(config.compression.max_log_lines, 25);
        assert_eq!(config.compression.max_diff_lines, 100);
    }

    #[test]
    fn rejects_invalid_compression_rate() {
        assert!(Config::parse("compression { code_compression_rate = 0.0 }").is_err());
        assert!(Config::parse("compression { code_compression_rate = 1.5 }").is_err());
        assert!(Config::parse("compression { no_such_knob = 1 }").is_err());
    }

    #[test]
    fn compaction_accepts_string_or_number_ratios() {
        let config = Config::parse(
            "compaction \"llm\" { context = \"0.75\" }\ncompaction \"vcc\" { context = 0.5 }",
        )
        .unwrap();
        assert_eq!(config.compaction.len(), 2);
        assert!((config.compaction[0].context - 0.75).abs() < 1e-9);
        assert!((config.compaction[1].context - 0.5).abs() < 1e-9);
    }

    #[test]
    fn rejects_invalid_compaction_settings() {
        for text in [
            "compaction \"unknown\" { context = 0.5 }",
            "compaction \"llm\" { context = 0 }",
            "compaction \"llm\" { context = 1.0 }",
            "compaction \"llm\" { context = \"not a number\" }",
            "compaction \"llm\" { }",
            "compaction \"llm\" { context = 0.5\n  extra = true }",
            "compaction \"vcc\" { context = 0.5 }\ncompaction \"vcc\" { context = 0.7 }",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn agent_config_is_optional_and_supports_partial_overrides() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.agent.temperature, None);
        assert_eq!(config.agent.max_tokens, None);
        let config = Config::parse("agent { max_tokens = 1024 }").unwrap();
        assert_eq!(config.agent.max_tokens, Some(1024));
        let config = Config::parse("agent { max_turns = 4 }").unwrap();
        assert_eq!(config.agent.max_turns, Some(4));
        assert_eq!(config.agent.preamble, AgentConfig::default().preamble);
    }

    #[test]
    fn rejects_invalid_agent_settings() {
        for field in [
            "max_turns = 0",
            "max_tokens = 0",
            "temperature = -0.1",
            "temperature = nan",
            "temperature = inf",
            "unknown_setting = true",
        ] {
            assert!(
                Config::parse(&format!("agent {{ {field} }}")).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn rejects_invalid_configuration() {
        for text in [
            "provider \"x\" { kind = \"unknown\" }",
            "provider \"x\" { kind = \"openai\"\n  api_key = \"do-not-store-keys\" }",
            "provider \"x\" { kind = \"openai\"\n  base_url = \"file:///tmp/api\" }",
            "provider \"x\" { kind = \"openai\"\n  base_url = \"https://user:secret@example.com\" }",
            "provider \"x\" { kind = \"openai\"\n  api_version = \"v1\" }",
            "provider \"x\" { kind = \"openai\"\n  api_key_env = \"\" }",
            "provider \"x\" { kind = \"openai\"\n  model \"test\" { context_length = 0 } }",
            "provider \"x\" { kind = \"amazon-bedrock\"\n  api_key_env = \"AWS_ACCESS_KEY_ID\" }",
            "provider \"x\" { kind = \"amazon-bedrock\"\n  base_url = \"https://bedrock-runtime.us-east-1.amazonaws.com\" }",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn bedrock_config_is_credential_free() {
        let config = Config::parse(
            "provider \"bedrock\" {\n  kind = \"amazon-bedrock\"\n  model \"us.anthropic.claude-sonnet-4-5-v2\" { context_length = 200000 }\n}",
        )
        .unwrap();
        let provider = &config.providers["bedrock"];
        assert_eq!(provider.kind, ProviderKind::Bedrock);
        assert!(provider.api_key_env.is_none());
        assert!(provider.base_url.is_none());
    }

    #[test]
    fn split_layout_merges_deep_and_labeled() {
        let dir = tempfile::tempdir().unwrap();
        let xdg = dir.path().join("craft");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::write(
            dir.path().join("craft.bml"),
            "agent { temperature = 0.2 }\nprovider \"x\" { kind = \"openai\" }\ncompaction \"vcc\" { context = 0.5 }",
        )
        .unwrap();
        std::fs::write(
            xdg.join("extra.bml"),
            "agent { max_tokens = 512 }\nprovider \"x\" { base_url = \"https://llm.example.com\" }\ncompaction \"llm\" { context = 0.9 }",
        )
        .unwrap();

        let loaded = crate::bml::load_global_from(std::slice::from_ref(&xdg), Some(xdg.as_path()));
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let doc = loaded.doc.expect("bml sources exist");
        let config = Config::from_statement(&doc).unwrap();
        // Deep merge: both agent fields apply.
        assert_eq!(config.agent.temperature, Some(0.2));
        assert_eq!(config.agent.max_tokens, Some(512));
        // Repeated blocks accumulate.
        assert_eq!(config.compaction.len(), 2);
        // Labeled block override merges per-field.
        let provider = &config.providers["x"];
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://llm.example.com")
        );
    }

    #[test]
    fn legacy_toml_without_bml_is_detected_and_bml_wins() {
        let dir = tempfile::tempdir().unwrap();
        // No bml anywhere: a legacy agent.toml must be flagged.
        std::fs::write(dir.path().join("agent.toml"), "[agent]\n").unwrap();
        let loaded = crate::bml::load_global_from(&[dir.path().to_path_buf()], None);
        assert!(loaded.doc.is_none());
        assert!(!crate::bml::legacy_toml_files_in(&[dir.path().to_path_buf()]).is_empty());

        // With bml present, the legacy file is not flagged as blocking.
        std::fs::write(dir.path().join("craft.bml"), "agent { max_tokens = 1 }").unwrap();
        let loaded = crate::bml::load_global_from(&[dir.path().to_path_buf()], None);
        let doc = loaded.doc.expect("bml source found");
        let config = Config::from_statement(&doc).unwrap();
        assert_eq!(config.agent.max_tokens, Some(1));
        assert_eq!(config.providers.len(), 0);
    }

    #[tokio::test]
    async fn missing_config_is_empty() {
        let path = std::env::temp_dir().join(format!(
            "craft-agent-missing-{}-{}.bml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        assert!(Config::load_from(&path).await.unwrap().providers.is_empty());
    }
}
