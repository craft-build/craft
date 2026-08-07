//! Model role routing, per-role fallback chains, and credential stacking.
//!
//! Roles route work by intent (`default`, `advisor`, plus the Flow pipeline
//! roles `scout`/`tpm`/`plan`/`req`/`execute`/`review`/`qa`/`integrator`/
//! `verifier`) instead of a single global model. Each role maps to an ordered
//! fallback chain of model specs: on a retryable error (429/quota/timeout) the
//! next entry in the chain takes the rest of the turn. The interactive model
//! picker (`Ctrl+P`) keeps cycling all models globally; roles resolve a
//! starting model for the advisor and the Flow pipeline stages.
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
    Advisor,
    FlowScout,
    FlowTpm,
    FlowPlan,
    FlowReq,
    FlowExecute,
    FlowReview,
    FlowQa,
    FlowIntegrator,
    FlowVerifier,
    MemoryExtractor,
}

impl ModelRole {
    pub const ALL: &[ModelRole] = &[
        ModelRole::Default,
        ModelRole::Advisor,
        ModelRole::FlowScout,
        ModelRole::FlowTpm,
        ModelRole::FlowPlan,
        ModelRole::FlowReq,
        ModelRole::FlowExecute,
        ModelRole::FlowReview,
        ModelRole::FlowQa,
        ModelRole::FlowIntegrator,
        ModelRole::FlowVerifier,
        ModelRole::MemoryExtractor,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ModelRole::Default => "default",
            ModelRole::Advisor => "advisor",
            ModelRole::FlowScout => "scout",
            ModelRole::FlowTpm => "tpm",
            ModelRole::FlowPlan => "plan",
            ModelRole::FlowReq => "req",
            ModelRole::FlowExecute => "execute",
            ModelRole::FlowReview => "review",
            ModelRole::FlowQa => "qa",
            ModelRole::FlowIntegrator => "integrator",
            ModelRole::FlowVerifier => "verifier",
            ModelRole::MemoryExtractor => "memory_extractor",
        }
    }
}

impl std::str::FromStr for ModelRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(ModelRole::Default),
            "advisor" => Ok(ModelRole::Advisor),
            "scout" => Ok(ModelRole::FlowScout),
            "tpm" => Ok(ModelRole::FlowTpm),
            "plan" => Ok(ModelRole::FlowPlan),
            "req" => Ok(ModelRole::FlowReq),
            "execute" => Ok(ModelRole::FlowExecute),
            "review" => Ok(ModelRole::FlowReview),
            "qa" => Ok(ModelRole::FlowQa),
            "integrator" => Ok(ModelRole::FlowIntegrator),
            "verifier" => Ok(ModelRole::FlowVerifier),
            "memory_extractor" => Ok(ModelRole::MemoryExtractor),
            other => Err(format!(
                "unknown model role '{other}'; expected one of default, advisor, scout, tpm, plan, req, execute, review, qa, integrator, verifier, memory_extractor"
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
    #[test_case("ADVISOR", ModelRole::Advisor ; "advisor_case_insensitive")]
    #[test_case("scout", ModelRole::FlowScout ; "scout")]
    #[test_case("plan", ModelRole::FlowPlan ; "plan")]
    fn role_parses(s: &str, expected: ModelRole) {
        assert_eq!(ModelRole::from_str(s).unwrap(), expected);
    }

    #[test]
    fn role_rejects_unknown() {
        assert!(ModelRole::from_str("turbo").is_err());
    }

    #[test_case(ModelRole::Default, "default")]
    #[test_case(ModelRole::Advisor, "advisor")]
    #[test_case(ModelRole::FlowScout, "scout")]
    #[test_case(ModelRole::FlowVerifier, "verifier")]
    #[test_case(ModelRole::MemoryExtractor, "memory_extractor")]
    fn role_roundtrips_str(role: ModelRole, s: &str) {
        assert_eq!(role.as_str(), s);
    }

    #[test]
    fn parse_chain_from_toml() {
        let toml = r#"
[default]
chain = [{ model = "anthropic/claude-sonnet-4-20250514" }, { model = "openai/gpt-4o" }]

[advisor]
chain = [{ model = "anthropic/claude-haiku" }]
"#;
        let cfg: ModelRolesConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.chain_for(ModelRole::Default),
            vec!["anthropic/claude-sonnet-4-20250514", "openai/gpt-4o"]
        );
        assert_eq!(
            cfg.chain_for(ModelRole::Advisor),
            vec!["anthropic/claude-haiku"]
        );
        assert!(cfg.chain_for(ModelRole::FlowPlan).is_empty());
        assert!(!cfg.is_empty());
    }

    #[test]
    fn empty_config_is_empty() {
        let cfg = ModelRolesConfig::default();
        assert!(cfg.is_empty());
        assert!(cfg.chain_for(ModelRole::Default).is_empty());
    }
}
