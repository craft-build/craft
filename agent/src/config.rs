//! Configuration for `~/.config/craft/agent.toml` (legacy `~/.craft/` is
//! searched first when it exists).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use snafu::{OptionExt, ResultExt};

use crate::error::{
    ConfigDirSnafu, HomeDirectorySnafu, InvalidBaseUrlSnafu, InvalidProviderSnafu, InvalidSnafu,
    InvalidTomlSnafu, LoadConfigSnafu, ReadConfigSnafu, Result,
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
pub struct AgentConfig {
    /// System instructions. An empty string keeps the built-in defaults; any
    /// text is appended to the assembled system prompt's instructions slot.
    pub preamble: String,
    /// Omit to preserve the provider/model's default sampling behavior.
    pub temperature: Option<f64>,
    /// Request-level output cap, not a model catalog metadata override.
    pub max_tokens: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            // Extra instructions appended to the assembled system prompt's
            // instructions slot; empty keeps the built-in defaults.
            preamble: String::new(),
            temperature: None,
            max_tokens: None,
        }
    }
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

#[derive(Debug, Deserialize)]
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
#[derive(Debug, Default, Deserialize)]
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
