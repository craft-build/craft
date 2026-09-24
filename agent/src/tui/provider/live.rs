//! CraftProvider: the TUI's real backend, driving the shared run loop the
//! ACP server uses.
//!
//! Turn semantics mirror `acp::run_turn`: a per-turn provider rebuild, the
//! configured compaction stages ahead of the model call, cancellation through
//! the run loop's `CancelToken`, and history committed only on a successful
//! run. Edit-family tools are gated behind the UI's approve/reject seam via
//! the dispatch `BeforeExecute` hook: no workspace mutation runs without an
//! explicit user decision.

mod approval;
mod turn;
mod usage_recorder;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc};
use tokio::task::AbortHandle;

use crate::compaction::CompactionState;
use crate::config::Config;
use crate::error::{InvalidSnafu, Result, client_error};
use crate::permissions::{PermissionAnswer, PermissionManager, PermissionsConfig};
use crate::providers::{CatalogModel, Provider as ClientProvider, ProviderKind};
use crate::run;
use crate::tools::Workspace;

use approval::decide;
use turn::{TurnCtx, run_turn};
use usage_recorder::UsageLedger;

use super::cards::{self, Files};
use super::{AgentEvent, Command, ModelChoice, Provider, Status};

/// Render an error and its sources as one client-facing message.
pub(super) fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

/// Session shared between the command loop and the (single) running turn.
#[derive(Default)]
struct SessionState {
    history: Vec<crate::history::Message>,
    /// Shared with the run loop for in-run overflow recovery.
    compaction: crate::run::SharedCompactionState,
    /// Session-wide tool dedup cache, shared by the dispatcher and cleared
    /// by the compaction engine.
    dedup: crate::run::SharedDedupCache,
    /// Session-wide guardrail counters, shared by the dispatcher and reset
    /// by the compaction engine.
    guardrails: crate::run::SharedGuardrails,
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pending_approval: Option<(
        String,
        tokio::sync::oneshot::Sender<crate::permissions::PermissionAnswer>,
    )>,
    /// Per-model usage totals and the cost ledger they feed.
    usage: UsageLedger,
}

impl SessionState {
    /// Link the compaction state to this session's dedup cache so a
    /// compaction run clears it.
    fn linked() -> Self {
        let dedup = crate::run::shared_cache();
        let guardrails = crate::run::shared_guardrails();
        Self {
            compaction: std::sync::Arc::new(std::sync::Mutex::new(
                CompactionState::default()
                    .with_dedup(dedup.clone())
                    .with_guardrails(guardrails.clone()),
            )),
            dedup,
            guardrails,
            usage: UsageLedger::open(),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
struct Selection {
    provider: String,
    model: String,
    context_length: Option<u32>,
}

/// The live TUI backend: configured providers, discovered model catalogs, and
/// a workspace-scoped agent at `cwd`.
pub struct CraftProvider {
    config: Arc<Config>,
    workspace: Workspace,
    /// Instruction files (AGENTS.md and friends) discovered at startup;
    /// appended to the system prompt and shared with tool injection.
    instructions: crate::instructions::Instructions,
    /// Permission rule engine: persistent `permissions.toml` rules, session
    /// grants, and per-tool defaults; consulted by the approval gate.
    permissions: Arc<PermissionManager>,
    /// Per-provider discovered models, config key order.
    catalogs: BTreeMap<String, Vec<CatalogModel>>,
    /// Discovery problems surfaced to the user as notes at startup.
    notes: Vec<String>,
    selection: Selection,
    cwd_label: String,
    branch: String,
}

impl CraftProvider {
    /// Discover instruction files and the permission rule engine for the
    /// workspace at `cwd`, off the async worker.
    async fn resolve_instructions(
        cwd: &Path,
    ) -> (crate::instructions::Instructions, PermissionManager) {
        let instructions = tokio::task::spawn_blocking({
            let cwd = cwd.display().to_string();
            move || crate::instructions::load_instructions(&cwd)
        })
        .await
        .unwrap_or_default();
        let permissions = tokio::task::spawn_blocking({
            let cwd = cwd.to_path_buf();
            move || PermissionManager::new(crate::permissions::load_permissions(&cwd), cwd)
        })
        .await
        .unwrap_or_else(|_| {
            PermissionManager::new(PermissionsConfig::default(), cwd.to_path_buf())
        });
        (instructions, permissions)
    }

    /// Validate the whole session up front, before the terminal UI starts:
    /// config, workspace, and at least one usable model catalog.
    pub async fn new(config: Config, cwd: impl AsRef<Path>) -> Result<Self> {
        if config.providers.is_empty() {
            return InvalidSnafu {
                reason: "no providers are configured in ~/.config/craft/agent.toml",
            }
            .fail();
        }
        let cwd = cwd.as_ref();
        let (instructions, permissions) = Self::resolve_instructions(cwd).await;
        let workspace = Workspace::new(cwd)
            .map_err(client_error)?
            .with_loaded_instructions(instructions.loaded.clone());

        let mut catalogs: BTreeMap<String, Vec<CatalogModel>> = BTreeMap::new();
        let mut notes = Vec::new();
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
                reason: format!("no usable provider/model catalog ({})", notes.join("; ")),
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
        if let Ok(state_dir) = crate::storage::StateDir::resolve() {
            crate::model_registry::load_from_storage(&state_dir);
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
        // Prefer the Medium-tier default when the registry can resolve one;
        // otherwise keep the first catalog entry.
        let selection =
            crate::model_registry::spec_for_tier_any(crate::model_registry::ModelTier::Medium)
                .and_then(|spec| {
                    let (provider, model) = spec.split_once('/')?;
                    let models = catalogs.get(provider)?;
                    let entry = models.iter().find(|m| m.id == model)?;
                    Some(Selection {
                        provider: provider.to_string(),
                        model: entry.id.clone(),
                        context_length: entry.context_length,
                    })
                })
                .unwrap_or(Selection {
                    provider: provider.clone(),
                    model: first.id.clone(),
                    context_length: first.context_length,
                });
        Ok(Self {
            config: Arc::new(config),
            workspace,
            instructions,
            permissions: Arc::new(permissions),
            catalogs,
            notes,
            selection,
            cwd_label: cards::display_path(cwd),
            branch: cards::git_branch(cwd).await,
        })
    }

    /// The session's command loop: receives [`Command`]s, drives turns and
    /// approvals, and streams [`AgentEvent`]s back. Spawned by `start`.
    async fn spawn_command_loop(
        self,
        mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
        evt_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    ) {
        let state = Arc::new(Mutex::new(SessionState::linked()));
        let files: Files = Files::default();
        let (cancel_flag, _) = run::cancel_channel();
        let mut current_turn: Option<AbortHandle> = None;
        let mut selection = self.selection;
        let workspace = self.workspace;
        let permissions = self.permissions;
        let snapshots = workspace.snapshots().clone();
        let instructions_text = self.instructions.text;
        let config = self.config;
        let catalogs = self.catalogs;

        // Flat model menu rows across all usable providers, with the
        // current selection's index.
        let catalog_choices = |selection: &Selection| -> (Vec<ModelChoice>, usize) {
            let mut choices = Vec::new();
            let mut current = 0;
            for (provider, models) in &catalogs {
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
        };

        let _ = evt_tx.send(AgentEvent::SessionInfo {
            cwd: self.cwd_label,
            branch: self.branch,
        });
        for note in self.notes {
            let _ = evt_tx.send(AgentEvent::AssistantText(note));
        }
        let (models, current) = catalog_choices(&selection);
        let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
        let _ = evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));

        // Signal cancellation and abort any in-flight turn. Callers then
        // differ only in how much session state they rebuild.
        let interrupt = |current_turn: &mut Option<AbortHandle>| {
            cancel_flag.set(true);
            if let Some(h) = current_turn.take() {
                h.abort();
            }
        };
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                Command::SendMessage(text) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if let Some(h) = current_turn.take() {
                        h.abort();
                    }
                    let cancel_token = cancel_flag.token();
                    let handle = tokio::spawn(run_turn(
                        TurnCtx {
                            config: config.clone(),
                            workspace: workspace.clone(),
                            instructions_text: instructions_text.clone(),
                            selection: selection.clone(),
                            state: state.clone(),
                            files: files.clone(),
                            cancel: cancel_token.clone(),
                            tx: evt_tx.clone(),
                            permissions: permissions.clone(),
                        },
                        text,
                    ));
                    current_turn = Some(handle.abort_handle());
                }
                Command::Approve { id, always } => {
                    let answer = if always {
                        PermissionAnswer::AllowAlwaysLocal
                    } else {
                        PermissionAnswer::AllowSession
                    };
                    decide(&state, id, answer).await
                }
                Command::Reject { id, always } => {
                    let answer = if always {
                        PermissionAnswer::DenyAlwaysLocal
                    } else {
                        PermissionAnswer::Deny
                    };
                    decide(&state, id, answer).await
                }
                Command::ToggleAutoReview => {
                    let on = permissions.toggle_auto_review();
                    let _ = evt_tx.send(AgentEvent::AssistantText(format!(
                        "auto-review {}.",
                        if on { "on" } else { "off" }
                    )));
                }
                Command::GetUsage => {
                    let rows = state.lock().await.usage.rows();
                    let _ = evt_tx.send(AgentEvent::UsageSnapshot(rows));
                }
                Command::Interrupt => {
                    interrupt(&mut current_turn);
                    let _ = evt_tx.send(AgentEvent::AssistantEnd);
                    let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                }
                Command::Clear => {
                    interrupt(&mut current_turn);
                    // Reset through `linked` so the fresh session's
                    // compaction state keeps working dedup/guardrails
                    // handles; a bare default would strand the caches the
                    // dispatcher still points at.
                    *state.lock().await = SessionState::linked();
                    let _ = evt_tx.send(AgentEvent::AssistantEnd);
                    let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                }
                Command::Reset => {
                    interrupt(&mut current_turn);
                    *state.lock().await = SessionState::linked();
                    files.lock().expect("files lock").clear();
                    let _ = evt_tx.send(AgentEvent::AssistantEnd);
                    let _ = evt_tx.send(AgentEvent::FilesSet(Vec::new()));
                    let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    let _ = evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));
                }
                Command::Undo => {
                    if current_turn.is_some() {
                        // Restoring mid-run would race the turn's writes
                        // and drain its live capture session.
                        let _ = evt_tx.send(AgentEvent::AssistantText(
                            "A turn is still running; wait for it to finish before undoing.".into(),
                        ));
                        continue;
                    }
                    let message = snapshots
                        .rollback()
                        .await
                        .unwrap_or_else(|| "Nothing to undo.".into());
                    let _ = evt_tx.send(AgentEvent::AssistantText(message));
                }
                Command::SelectModel { provider, model } => {
                    let found = catalogs
                        .get(&provider)
                        .and_then(|models| models.iter().find(|m| m.id == model))
                        .map(|m| m.context_length);
                    match found {
                        Some(context_length) => {
                            selection = Selection {
                                provider,
                                model,
                                context_length,
                            };
                            let (models, current) = catalog_choices(&selection);
                            let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
                        }
                        None => {
                            let _ = evt_tx.send(AgentEvent::AssistantText(format!(
                                "Unknown model {model:?} on provider {provider:?}."
                            )));
                        }
                    }
                }
            }
        }
    }
}

impl Provider for CraftProvider {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();

        tokio::spawn(self.spawn_command_loop(cmd_rx, evt_tx));

        (cmd_tx, evt_rx)
    }
}
