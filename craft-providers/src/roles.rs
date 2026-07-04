//! Model role resolution and fallback chains.
//!
//! A role (`default`/`advisor`, plus the Flow pipeline roles `scout`/`tpm`/
//! `plan`/`req`/`execute`/`review`/`qa`/`integrator`/`verifier`) maps to an ordered
//! fallback chain of model specs read from `model_roles.toml` (see
//! `craft_config::model_roles`). The primary entry is the first resolvable spec;
//! the remainder form a fallback chain consumed by `stream_with_retry` when the
//! active provider hits a retryable error (429/quota/timeout) and key rotation
//! is exhausted.
//!
//! When a role is unconfigured, resolution falls back to the caller-supplied
//! model/provider so missing config reproduces single-model behavior.

use std::sync::Arc;

use craft_config::model_roles::{ModelRole, ModelRolesConfig};

use crate::Model;
use crate::Timeouts;
use crate::model::ModelError;
use crate::provider::{self, Provider};

/// One resolvable hop in a fallback chain.
#[derive(Clone)]
pub struct ChainHop {
    pub model: Model,
    pub provider: Arc<dyn Provider>,
}

/// A fully resolved role: a primary hop plus zero or more fallback hops.
#[derive(Clone)]
pub struct ResolvedRole {
    pub primary: ChainHop,
    pub fallbacks: Vec<ChainHop>,
}

impl ResolvedRole {
    pub fn chain(self) -> Vec<ChainHop> {
        let mut out = vec![self.primary];
        out.extend(self.fallbacks);
        out
    }
}

/// Resolve a role from `model_roles.toml`. Returns `None` when the role is
/// unconfigured or no chain entry resolves successfully, in which case the
/// caller should use its existing model/provider.
pub async fn resolve_role(
    role: ModelRole,
    fallback_model: Model,
    fallback_provider: Arc<dyn Provider>,
    timeouts: Timeouts,
) -> ResolvedRole {
    let cfg = ModelRolesConfig::load();
    let specs = cfg.chain_for(role);
    if specs.is_empty() {
        return ResolvedRole {
            primary: ChainHop {
                model: fallback_model,
                provider: fallback_provider,
            },
            fallbacks: Vec::new(),
        };
    }
    let mut hops = Vec::new();
    for spec in specs {
        match resolve_hop(spec, timeouts).await {
            Ok(hop) => hops.push(hop),
            Err(e) => {
                tracing::warn!(role = role.as_str(), spec, error = %e, "skipping unresolvable chain entry");
            }
        }
    }
    let mut hops_iter = hops.into_iter();
    match hops_iter.next() {
        Some(primary) => ResolvedRole {
            primary,
            fallbacks: hops_iter.collect(),
        },
        None => ResolvedRole {
            primary: ChainHop {
                model: fallback_model,
                provider: fallback_provider,
            },
            fallbacks: Vec::new(),
        },
    }
}

async fn resolve_hop(spec: &str, timeouts: Timeouts) -> Result<ChainHop, String> {
    let mut model = Model::from_spec(spec).map_err(|e: ModelError| e.to_string())?;
    let provider = provider::from_model(&mut model, timeouts)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ChainHop {
        model,
        provider: Arc::from(provider),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::UnconfiguredProvider;
    use craft_config::model_roles::{ChainEntry, ModelRolesConfig, RoleDef};
    use std::collections::HashMap;

    #[test]
    fn empty_config_falls_back_to_caller_model() {
        let cfg = ModelRolesConfig::default();
        assert!(cfg.chain_for(ModelRole::Default).is_empty());
    }

    #[test]
    fn chain_for_returns_specs_in_order() {
        let mut roles = HashMap::new();
        roles.insert(
            "default".to_string(),
            RoleDef {
                chain: vec![
                    ChainEntry {
                        model: "anthropic/a".into(),
                    },
                    ChainEntry {
                        model: "openai/b".into(),
                    },
                ],
            },
        );
        let cfg = ModelRolesConfig { roles };
        assert_eq!(
            cfg.chain_for(ModelRole::Default),
            vec!["anthropic/a", "openai/b"]
        );
    }

    #[tokio::test]
    async fn resolve_role_unconfigured_uses_fallback() {
        let null = Arc::new(UnconfiguredProvider) as Arc<dyn Provider>;
        let model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        let resolved = resolve_role(
            ModelRole::Advisor,
            model.clone(),
            Arc::clone(&null),
            Timeouts::default(),
        )
        .await;
        assert!(resolved.fallbacks.is_empty());
        assert_eq!(resolved.primary.model.id, model.id);
    }
}
