//! Configuration for `~/.config/craft/agent.toml`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rig::model::{Model, ModelList};
use serde::Deserialize;

use crate::providers::ProviderKind;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agent: AgentConfig,
}

/// Defaults for each agent run, independent of provider/model selection.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// System instructions. An empty string disables the preamble.
    pub preamble: String,
    /// Total model calls per run, including retries and continuations.
    pub max_turns: usize,
    /// Omit to preserve the provider/model's default sampling behavior.
    pub temperature: Option<f64>,
    /// Request-level output cap, not a model catalog metadata override.
    pub max_tokens: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            preamble: "You are Craft, an AI coding assistant.".into(),
            max_turns: 16,
            temperature: None,
            max_tokens: None,
        }
    }
}

impl AgentConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_turns == 0 {
            bail!("agent.max_turns must be positive");
        }
        if self.max_tokens == Some(0) {
            bail!("agent.max_tokens must be positive");
        }
        // Providers have different upper bounds (and some disallow temperature).
        // Validate the portable constraint here; leave model-specific rules to Rig.
        if self
            .temperature
            .is_some_and(|value| !value.is_finite() || value < 0.0)
        {
            bail!("agent.temperature must be finite and nonnegative");
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
        // Deliberately use the specified path, even on macOS and Windows.
        Ok(dirs::home_dir()
            .context("cannot determine home directory for agent configuration")?
            .join(".config/craft/agent.toml"))
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
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("loading {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("invalid agent TOML")?;
        config.agent.validate()?;
        for (name, provider) in &config.providers {
            if name.trim().is_empty() {
                bail!("provider names must not be empty");
            }
            provider
                .validate()
                .with_context(|| format!("provider {name:?}"))?;
        }
        Ok(config)
    }
}

impl ProviderConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.api_key_env {
            if name.trim().is_empty() || name.contains(['=', '\0']) {
                bail!("api_key_env must be a nonempty environment variable name");
            }
            if self.kind == ProviderKind::Llamafile {
                bail!("llamafile does not accept credentials");
            }
        }
        if let Some(base) = &self.base_url {
            let url = url::Url::parse(base).context("base_url must be an absolute HTTP(S) URL")?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                bail!("base_url must be HTTP(S), without credentials, query, or fragment");
            }
        }
        if self.api_version.is_some() && self.kind != ProviderKind::Azure {
            bail!("api_version is only supported by azure");
        }
        if self
            .api_version
            .as_ref()
            .is_some_and(|v| v.trim().is_empty())
        {
            bail!("api_version must not be empty");
        }
        if self.account_id.is_some()
            && (self.kind != ProviderKind::Chatgpt || self.api_key_env.is_none())
        {
            bail!("account_id requires chatgpt with api_key_env pointing to an access token");
        }
        for (id, model) in &self.models {
            if id.trim().is_empty() {
                bail!("model IDs must not be empty");
            }
            if model.context_length == Some(0) || model.max_output_tokens == Some(0) {
                bail!("model {id:?}: token limits must be positive");
            }
        }
        Ok(())
    }

    /// Configured fields win; omitted fields preserve discovery metadata.
    /// New IDs are added, and the final catalog is sorted by ID.
    pub fn merge_models(&self, discovered: ModelList) -> ModelList {
        let mut models: BTreeMap<_, _> = discovered
            .into_iter()
            .map(|model| (model.id.clone(), model))
            .collect();
        for (id, settings) in &self.models {
            let model = models
                .entry(id.clone())
                .or_insert_with(|| Model::from_id(id));
            if let Some(name) = &settings.name {
                model.name = Some(name.clone());
            }
            if let Some(description) = &settings.description {
                model.description = Some(description.clone());
            }
            if let Some(context_length) = settings.context_length {
                model.context_length = Some(context_length);
            }
            if let Some(max_output_tokens) = settings.max_output_tokens {
                model.max_output_tokens = Some(max_output_tokens);
            }
        }
        ModelList::new(models.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let config = Config::parse(include_str!("../agent.example.toml")).unwrap();
        assert_eq!(config.providers.len(), 4);
        assert_eq!(config.agent.max_turns, 16);
    }

    #[test]
    fn agent_config_is_optional_and_supports_partial_overrides() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.agent.max_turns, AgentConfig::default().max_turns);
        assert_eq!(config.agent.temperature, None);
        assert_eq!(config.agent.max_tokens, None);
        let config = Config::parse("[agent]\nmax_turns = 4\nmax_tokens = 1024").unwrap();
        assert_eq!(config.agent.max_turns, 4);
        assert_eq!(config.agent.max_tokens, Some(1024));
        assert_eq!(config.agent.preamble, AgentConfig::default().preamble);
    }

    #[test]
    fn rejects_invalid_agent_settings() {
        for field in [
            "max_turns = 0",
            "max_turns = -1",
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

    #[test]
    fn merges_partial_overrides_and_manual_models() {
        let config = Config::parse(
            "[providers.x]\nkind = 'openai'\n\
             [providers.x.models.existing]\nmax_output_tokens = 2048\n\
             [providers.x.models.new]\nname = 'Manual model'",
        )
        .unwrap();
        let mut existing = Model::new("existing", "Discovered name");
        existing.context_length = Some(8192);
        let merged = config.providers["x"]
            .merge_models(ModelList::new(vec![existing, Model::from_id("untouched")]));
        let models: Vec<_> = merged.into_iter().collect();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].name.as_deref(), Some("Discovered name"));
        assert_eq!(models[0].context_length, Some(8192));
        assert_eq!(models[0].max_output_tokens, Some(2048));
        assert_eq!(models[1].id, "new");
        assert_eq!(models[2].id, "untouched");
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
