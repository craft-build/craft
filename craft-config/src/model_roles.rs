//! Model role routing, per-role fallback chains, and credential stacking.
//!
//! Roles route work by intent (`default`, `smol`, `slow`, `plan`, `commit`,
//! `advisor`) instead of a single global model. Each role maps to an ordered
//! fallback chain of model specs: on a retryable error (429/quota/timeout) the
//! next entry in the chain takes the rest of the turn. The interactive model
//! picker (`Ctrl+P`) keeps cycling all models globally; roles resolve a starting
//! model for subagents (`smol`), the advisor (`advisor`), and CLI flags
//! (`--smol`/`--slow`/`--plan`).
//!
//! Config lives in `model_roles.toml` next to `providers.toml`. Missing config
//! falls back to a single-model (current behavior).

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use craft_storage::paths;

const MODEL_ROLES_FILE: &str = "model_roles.toml";

/// A named role a turn can be routed to. Order is significant only for
/// `all()` iteration; resolution looks up by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    Default,
    Smol,
    Slow,
    Plan,
    Commit,
    Advisor,
}

impl ModelRole {
    pub const ALL: &[ModelRole] = &[
        ModelRole::Default,
        ModelRole::Smol,
        ModelRole::Slow,
        ModelRole::Plan,
        ModelRole::Commit,
        ModelRole::Advisor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ModelRole::Default => "default",
            ModelRole::Smol => "smol",
            ModelRole::Slow => "slow",
            ModelRole::Plan => "plan",
            ModelRole::Commit => "commit",
            ModelRole::Advisor => "advisor",
        }
    }
}

impl std::str::FromStr for ModelRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(ModelRole::Default),
            "smol" => Ok(ModelRole::Smol),
            "slow" => Ok(ModelRole::Slow),
            "plan" => Ok(ModelRole::Plan),
            "commit" => Ok(ModelRole::Commit),
            "advisor" => Ok(ModelRole::Advisor),
            other => Err(format!(
                "unknown model role '{other}'; expected one of default, smol, slow, plan, commit, advisor"
            )),
        }
    }
}

/// One entry in a role's fallback chain. `model` is a `provider/model_id` spec
/// string consumable by `Model::from_spec`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChainEntry {
    pub model: String,
}

/// A single role definition: an ordered fallback chain (first = primary).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoleDef {
    #[serde(default)]
    pub chain: Vec<ChainEntry>,
}

impl RoleDef {
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    pub fn specs(&self) -> Vec<&str> {
        self.chain.iter().map(|e| e.model.as_str()).collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelRolesConfig {
    #[serde(flatten)]
    pub roles: HashMap<String, RoleDef>,
}

impl ModelRolesConfig {
    /// Read and parse `model_roles.toml`. Missing file = empty config (single-model behavior).
    pub fn load() -> Self {
        let path = model_roles_file_path();
        let Some(content) = read_file(&path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&content) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%e, path = %path.display(), "failed to parse model_roles.toml; ignoring");
                Self::default()
            }
        }
    }

    /// The fallback chain for a role, or empty when the role is unconfigured.
    pub fn chain_for(&self, role: ModelRole) -> Vec<&str> {
        self.roles
            .get(role.as_str())
            .map(|d| d.specs())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.roles.values().all(RoleDef::is_empty)
    }
}

fn read_file(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path).ok()
}

pub fn model_roles_file_path() -> PathBuf {
    paths::config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(MODEL_ROLES_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use test_case::test_case;

    #[test_case("default", ModelRole::Default ; "default")]
    #[test_case("SMOL", ModelRole::Smol ; "smol_case_insensitive")]
    #[test_case("plan", ModelRole::Plan ; "plan")]
    #[test_case("advisor", ModelRole::Advisor ; "advisor")]
    fn role_parses(s: &str, expected: ModelRole) {
        assert_eq!(ModelRole::from_str(s).unwrap(), expected);
    }

    #[test]
    fn role_rejects_unknown() {
        assert!(ModelRole::from_str("turbo").is_err());
    }

    #[test_case(ModelRole::Default, "default")]
    #[test_case(ModelRole::Advisor, "advisor")]
    fn role_roundtrips_str(role: ModelRole, s: &str) {
        assert_eq!(role.as_str(), s);
    }

    #[test]
    fn parse_chain_from_toml() {
        let toml = r#"
[default]
chain = [{ model = "anthropic/claude-sonnet-4-20250514" }, { model = "openai/gpt-4o" }]

[smol]
chain = [{ model = "anthropic/claude-haiku" }]
"#;
        let cfg: ModelRolesConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.chain_for(ModelRole::Default),
            vec!["anthropic/claude-sonnet-4-20250514", "openai/gpt-4o"]
        );
        assert_eq!(
            cfg.chain_for(ModelRole::Smol),
            vec!["anthropic/claude-haiku"]
        );
        assert!(cfg.chain_for(ModelRole::Plan).is_empty());
        assert!(!cfg.is_empty());
    }

    #[test]
    fn empty_config_is_empty() {
        let cfg = ModelRolesConfig::default();
        assert!(cfg.is_empty());
        assert!(cfg.chain_for(ModelRole::Default).is_empty());
    }
}
