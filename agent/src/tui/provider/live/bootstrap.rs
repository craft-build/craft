//! Startup and discovery: build the [`CraftProvider`] from config — resolve
//! instructions and permissions, discover per-provider model catalogs, and
//! pick the initial model selection.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::error::{InvalidSnafu, Result};
use crate::providers::{CatalogModel, Provider as ClientProvider, ProviderKind};

use super::cards;
use super::{AgentEvent, CraftProvider, ModelChoice, Selection, Tone, report};

/// Resolve a persisted `provider/model` spec (from `--model`, a tier
/// default, or a prior session's header) against the discovered catalogs.
/// `None` when the provider is gone or the model is no longer listed.
pub(super) fn selection_for_spec(
    catalogs: &BTreeMap<String, Vec<CatalogModel>>,
    spec: &str,
) -> Option<Selection> {
    let (provider, resolved) = crate::model_selection::resolve_spec(catalogs, spec)?;
    Some(Selection {
        provider,
        model: resolved.model,
        context_length: resolved.context_length,
    })
}

/// Flat model menu rows across all usable providers, with the current
/// selection's index.
pub(super) fn catalog_choices(
    catalogs: &BTreeMap<String, Vec<CatalogModel>>,
    selection: &Selection,
) -> (Vec<ModelChoice>, usize) {
    let mut choices = Vec::new();
    let mut current = 0;
    for (provider, models) in catalogs {
        for model in models {
            if provider == &selection.provider && model.id == selection.model {
                current = choices.len();
            }
            choices.push(ModelChoice {
                provider: provider.clone(),
                model: model.id.clone(),
                label: model.label().to_owned(),
                provider_label: provider.clone(),
            });
        }
    }
    (choices, current)
}

impl CraftProvider {
    /// Validate the whole session up front, before the terminal UI starts:
    /// config, workspace, and at least one usable model catalog.
    pub async fn new(config: Config, cwd: impl AsRef<Path>) -> Result<Self> {
        if config.providers.is_empty() {
            return InvalidSnafu {
                reason: crate::setup::setup_hint(),
            }
            .fail();
        }
        let cwd = cwd.as_ref();
        let mut catalogs: BTreeMap<String, Vec<CatalogModel>> = BTreeMap::new();
        let mut notes = Vec::new();
        // B.11: start the MCP client up front but never await `ready` here —
        // the first frame must not block on a slow server initialize. The
        // turn path awaits the gate before registering tools. The event
        // channel is created here (before `start` consumes the provider) so
        // MCP log notifications can reach the TUI as AgentEvents.
        let (mcp_evt_tx, mcp_evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let mcp_events = crate::mcp::McpEvents {
            on_log: Some(Arc::new(move |_server, level, message| {
                // Warning-or-worse only: the session layer routes info-level
                // logs to tracing.
                let _ = mcp_evt_tx.send(AgentEvent::Notice {
                    tone: Tone::Warning,
                    text: format!("[{level}] {message}"),
                });
            })),
            ..Default::default()
        };
        let sandbox = crate::sandbox::SandboxPolicy::resolve(&config, false);
        let env = crate::runtime::workspace_env(
            cwd,
            crate::runtime::McpStartup::Background(mcp_events),
            true,
            sandbox,
        )
        .await?;
        let (instructions, permissions, workspace) =
            (env.instructions, env.permissions, env.workspace);
        if let Some(note) = env.sandbox_note {
            notes.push(note);
        }
        if !env.mcp_errors.is_empty() {
            notes.push(format!("mcp: {}", env.mcp_errors));
        }
        let mcp = workspace.mcp();
        for (name, provider_config) in &config.providers {
            if provider_config.kind == ProviderKind::Voyageai {
                notes.push(format!("{name}: no completion models (non-chat provider)"));
                continue;
            }
            match ClientProvider::from_config(provider_config) {
                Ok(provider) => match provider.models(provider_config).await {
                    Ok(models) => {
                        if models.is_empty() {
                            notes.push(format!("{name}: no models discovered"));
                        } else {
                            catalogs.insert(name.clone(), models);
                        }
                    }
                    Err(error) => notes.push(format!("{name}: {}", report(error))),
                },
                Err(error) => notes.push(format!("{name}: {}", report(error))),
            }
        }
        if catalogs.is_empty() {
            return InvalidSnafu {
                reason: format!(
                    "no usable provider/model catalog ({}) - {}",
                    notes.join("; "),
                    crate::setup::setup_hint()
                ),
            }
            .fail();
        }

        // H.2 models.dev catalog: warm the 24h disk cache (best-effort, with
        // a fetch budget) and fill context/output metadata the Rig listing
        // lacks. Failures degrade to whatever discovery already provided.
        let _ =
            tokio::time::timeout(crate::models_dev::FETCH_BUDGET, crate::models_dev::warm()).await;
        for (name, provider_config) in &config.providers {
            if let Some(models) = catalogs.get_mut(name) {
                crate::models_dev::enrich_catalog(provider_config.kind.as_str(), models);
            }
        }

        // H.3 model-tier registry: load persisted tier overrides and feed in
        // the discovered catalogs so tier defaults can be resolved.
        let state_dir = crate::storage::StateDir::resolve().ok();
        if let Some(state_dir) = &state_dir {
            crate::model_registry::load_from_storage(state_dir);
        }
        for (name, provider_config) in &config.providers {
            if let Some(models) = catalogs.get(name) {
                crate::model_registry::set_known_models(
                    name,
                    provider_config.kind.as_str(),
                    models
                        .iter()
                        .map(|m| crate::model_registry::ModelInfo {
                            context_window: m.context_length,
                            ..crate::model_registry::ModelInfo::new(m.id.clone())
                        })
                        .collect(),
                );
            }
        }

        let (provider, models) = catalogs.iter().next().expect("catalogs is non-empty");
        let first = models.first().expect("each catalog is non-empty");
        // Reuse the model from the most recent session in this cwd, so the
        // user does not reselect it every launch. Falls back to the
        // Medium-tier default, then the first catalog entry; an explicit
        // `--model` still overrides later via `with_model_spec`.
        let last_used = state_dir
            .as_ref()
            .and_then(|dir| {
                crate::storage::sessions::latest_model(&cwd.display().to_string(), dir)
                    .ok()
                    .flatten()
            })
            .and_then(|spec| selection_for_spec(&catalogs, &spec));
        let tier_default =
            crate::model_registry::spec_for_tier_any(crate::model_registry::ModelTier::Medium)
                .and_then(|spec| selection_for_spec(&catalogs, &spec));
        let selection = last_used.or(tier_default).unwrap_or(Selection {
            provider: provider.clone(),
            model: first.id.clone(),
            context_length: first.context_length,
        });
        Ok(Self {
            config: Arc::new(config),
            workspace,
            instructions,
            permissions,
            catalogs,
            notes,
            selection,
            cwd_label: cards::display_path(cwd),
            branch: cards::git_branch(cwd).await,
            resume_latest: false,
            resume_session: None,
            mcp,
            mcp_evt_rx,
        })
    }

    /// Override the startup model selection with a `provider/model` spec
    /// (`craft -m`, G.1). Unknown specs fail fast instead of silently
    /// falling back to the tier default.
    pub fn with_model_spec(mut self, spec: &str) -> crate::error::Result<Self> {
        use snafu::ensure;
        let (provider, model) = spec.split_once('/').ok_or_else(|| {
            crate::error::InvalidSnafu {
                reason: format!("--model expects provider/model-id, got {spec:?}"),
            }
            .build()
        })?;
        ensure!(
            self.catalogs.contains_key(provider),
            crate::error::InvalidSnafu {
                reason: format!(
                    "unknown provider {provider:?} in --model {spec:?} \
                     (configured: {})",
                    self.catalogs.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            }
        );
        let entry = self.catalogs[provider]
            .iter()
            .find(|m| m.id == model)
            .ok_or_else(|| {
                crate::error::InvalidSnafu {
                    reason: format!("provider {provider:?} has no model {model:?}"),
                }
                .build()
            })?;
        self.selection = Selection {
            provider: provider.to_string(),
            model: entry.id.clone(),
            context_length: entry.context_length,
        };
        Ok(self)
    }
}
