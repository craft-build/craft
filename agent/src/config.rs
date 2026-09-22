//! Configuration for `~/.config/craft/agent.toml` (legacy `~/.craft/` is
//! searched first when it exists).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use snafu::ResultExt;

use crate::error::{
    ConfigDirSnafu, InvalidBaseUrlSnafu, InvalidProviderSnafu, InvalidSnafu, InvalidTomlSnafu,
    LoadConfigSnafu, ReadConfigSnafu, Result,
};
use crate::providers::ProviderKind;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agent: AgentConfig,
    /// Compaction stages, ascending by context fill ratio.
    #[serde(default = "default_compaction")]
    pub compaction: Vec<CompactionConfig>,
    /// Context reserved so compaction fires before the window truly fills.
    #[serde(default = "default_compaction_buffer")]
    pub compaction_buffer: CompactionBuffer,
    /// Tool-output pre-compression applied to the model's request view.
    pub compression: crate::compression::CompressionConfig,
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
/// the context window (TOML: `20000` or `"20%"`). Subtracted from the window
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

/// Defaults for each agent run, independent of provider/model selection.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct AgentConfig {
    /// System instructions. An empty string keeps the built-in defaults; any
    /// text is appended to the assembled system prompt's instructions slot.
    pub preamble: String,
    /// Omit to preserve the provider/model's default sampling behavior.
    pub temperature: Option<f64>,
    /// Request-level output cap, not a model catalog metadata override.
    pub max_tokens: Option<u64>,
}

impl AgentConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_tokens == Some(0) {
            return InvalidSnafu {
                reason: "agent.max_tokens must be positive",
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
    /// Environment variable containing the credential; never store keys in TOML.
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
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        // Search legacy `~/.craft/` first when it exists, then the XDG config
        // dir; a fresh install with no file anywhere defaults to the XDG path.
        if let Some(found) = crate::paths::find_config_path("agent.toml") {
            return Ok(found);
        }
        let dir = crate::paths::xdg_config_dir().context(ConfigDirSnafu)?;
        Ok(dir.join("agent.toml"))
    }

    pub async fn load() -> Result<Self> {
        Self::load_from(&Self::path()?).await
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
        let config: Self = toml::from_str(text).context(InvalidTomlSnafu)?;
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
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let config = Config::parse(include_str!("../agent.example.toml")).unwrap();
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
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default = "default_compaction_buffer")]
            compaction_buffer: CompactionBuffer,
        }
        let tokens: Wrap = toml::from_str("compaction_buffer = 20000").unwrap();
        assert_eq!(tokens.compaction_buffer, CompactionBuffer::Tokens(20000));
        let percent: Wrap = toml::from_str(r#"compaction_buffer = "20%""#).unwrap();
        assert_eq!(percent.compaction_buffer, CompactionBuffer::Percent(20));
        assert!(toml::from_str::<Wrap>("compaction_buffer = 100").is_err());
        assert!(toml::from_str::<Wrap>(r#"compaction_buffer = "20""#).is_err());
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
            "[compression]\nenabled = false\ncode_compression_rate = 0.5\nmax_log_lines = 25",
        )
        .unwrap();
        assert!(!config.compression.enabled);
        assert!((config.compression.code_compression_rate - 0.5).abs() < 1e-6);
        assert_eq!(config.compression.max_log_lines, 25);
        assert_eq!(config.compression.max_diff_lines, 100);
    }

    #[test]
    fn rejects_invalid_compression_rate() {
        assert!(Config::parse("[compression]\ncode_compression_rate = 0.0").is_err());
        assert!(Config::parse("[compression]\ncode_compression_rate = 1.5").is_err());
        assert!(Config::parse("[compression]\nno_such_knob = 1").is_err());
    }

    #[test]
    fn compaction_accepts_string_or_number_ratios() {
        let config = Config::parse(
            "[[compaction]]\nkind = \"llm\"\ncontext = \"0.75\"\n\
             [[compaction]]\nkind = \"vcc\"\ncontext = 0.5",
        )
        .unwrap();
        assert_eq!(config.compaction.len(), 2);
        assert!((config.compaction[0].context - 0.75).abs() < 1e-9);
        assert!((config.compaction[1].context - 0.5).abs() < 1e-9);
    }

    #[test]
    fn rejects_invalid_compaction_settings() {
        for text in [
            "[[compaction]]\nkind = \"unknown\"\ncontext = 0.5",
            "[[compaction]]\nkind = \"llm\"\ncontext = 0",
            "[[compaction]]\nkind = \"llm\"\ncontext = 1.0",
            "[[compaction]]\nkind = \"llm\"\ncontext = \"not a number\"",
            "[[compaction]]\nkind = \"llm\"",
            "[[compaction]]\nkind = \"llm\"\ncontext = 0.5\nextra = true",
            "[[compaction]]\nkind = \"vcc\"\ncontext = 0.5\n\
             [[compaction]]\nkind = \"vcc\"\ncontext = 0.7",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn agent_config_is_optional_and_supports_partial_overrides() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.agent.temperature, None);
        assert_eq!(config.agent.max_tokens, None);
        let config = Config::parse("[agent]\nmax_tokens = 1024").unwrap();
        assert_eq!(config.agent.max_tokens, Some(1024));
        assert_eq!(config.agent.preamble, AgentConfig::default().preamble);
    }

    #[test]
    fn rejects_invalid_agent_settings() {
        for field in [
            "max_turns = 4",
            "max_tokens = 0",
            "temperature = -0.1",
            "temperature = nan",
            "temperature = inf",
            "unknown_setting = true",
        ] {
            assert!(
                Config::parse(&format!("[agent]\n{field}")).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn rejects_invalid_configuration() {
        for text in [
            "[providers.x]\nkind = 'unknown'",
            "[providers.x]\nkind = 'openai'\napi_key = 'do-not-store-keys'",
            "[providers.x]\nkind = 'openai'\nbase_url = 'file:///tmp/api'",
            "[providers.x]\nkind = 'openai'\nbase_url = 'https://user:secret@example.com'",
            "[providers.x]\nkind = 'openai'\napi_version = 'v1'",
            "[providers.x]\nkind = 'openai'\napi_key_env = ''",
            "[providers.x]\nkind = 'openai'\n[providers.x.models.test]\ncontext_length = 0",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[tokio::test]
    async fn missing_config_is_empty() {
        let path = std::env::temp_dir().join(format!(
            "craft-agent-missing-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        assert!(Config::load_from(&path).await.unwrap().providers.is_empty());
    }
}
