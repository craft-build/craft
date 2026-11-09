//! Startup and discovery: build the [`CraftProvider`] from config — resolve
//! instructions and permissions, discover per-provider model catalogs, and
//! pick the initial model selection.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::config::Config;
use crate::error::{InvalidSnafu, Result};
use crate::provider::ProviderHandle;
use animist::provider::ModelInfo;

use super::approval::ApprovalGate;
use super::cards;
use super::question::QuestionAsker;
use super::turn::{RunSignals, resolve_model, start_forwarder};
use super::usage_recorder::UsageLedger;
use super::{
    AgentEvent, CraftProvider, LoopCtx, ModelChoice, Selection, SessionKit, SessionState, Tone,
    report,
};

/// Resolve a persisted `provider/model` spec (from `--model`, a tier
/// default, or a prior session's header) against the discovered catalogs.
/// `None` when the provider is gone or the model is no longer listed.
pub(super) fn selection_for_spec(
    catalogs: &BTreeMap<String, Vec<ModelInfo>>,
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
    config: &crate::config::Config,
    catalog: &animist::catalog::Catalog,
    catalogs: &BTreeMap<String, Vec<ModelInfo>>,
    selection: &Selection,
) -> (Vec<ModelChoice>, usize) {
    let mut choices = Vec::new();
    let mut current = 0;
    for (provider, models) in catalogs {
        let key = config
            .providers
            .get(provider)
            .map(crate::provider::catalog_key)
            .unwrap_or("openai-compatible");
        for info in models {
            let Some(id) = info.ids.first() else { continue };
            if provider == &selection.provider && id == &selection.model {
                current = choices.len();
            }
            let label = config
                .providers
                .get(provider)
                .and_then(|p| p.models().get(id))
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| id.clone());
            // Vision claims come from the catalog row: a model the catalog
            // does not know allows the attach (metadata miss) rather than
            // the provider row's default-false.
            let vision = catalog
                .model(key, id)
                .is_none_or(|entry| entry.info.features.vision);
            choices.push(ModelChoice {
                provider: provider.clone(),
                model: id.clone(),
                label,
                provider_label: provider.clone(),
                thinking_choices: crate::thinking::choices_for(
                    crate::thinking::ThinkingConfig::Off,
                    Some(info),
                ),
                vision,
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
        let mut catalogs: BTreeMap<String, Vec<ModelInfo>> = BTreeMap::new();
        let mut notes = Vec::new();
        // Start the MCP client up front but never await `ready` here —
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
        // models.dev: the session catalog loads once (24h disk cache,
        // bounded refresh) BEFORE discovery, matching headless and ACP —
        // providers merge their catalog slice into the listing by exact ID,
        // so a cold catalog would leave context windows and thinking
        // support permanently empty this session. Failures degrade to
        // whatever discovery provides.
        let catalog = crate::provider::load_catalog().await;
        for (name, provider_config) in &config.providers {
            match ProviderHandle::from_config(provider_config, Some(&catalog)).await {
                Ok(provider) => match provider.models(provider_config, Some(&catalog)).await {
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

        // Model-tier registry: load persisted tier overrides and feed in
        // the discovered catalogs so tier defaults can be resolved.
        let state_dir = crate::storage::StateDir::resolve().ok();
        if let Some(state_dir) = &state_dir {
            crate::model_registry::load_from_storage(state_dir);
        }
        for (name, provider_config) in &config.providers {
            if let Some(models) = catalogs.get(name) {
                crate::model_registry::set_known_models(
                    name,
                    crate::provider::config_kind_str(provider_config),
                    models
                        .iter()
                        .map(|m| crate::model_registry::ModelInfo {
                            context_window: m.config.context_window,
                            ..crate::model_registry::ModelInfo::new(
                                m.ids.first().cloned().unwrap_or_default(),
                            )
                        })
                        .collect(),
                );
            }
        }

        let (provider, models) = catalogs.iter().next().expect("catalogs is non-empty");
        let first = models.first().expect("each catalog is non-empty");
        let first_id = first
            .ids
            .first()
            .cloned()
            .expect("listing rows carry an id");
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
            model: first_id,
            context_length: first.config.context_window,
        });
        Ok(Self {
            config: Arc::new(config),
            workspace,
            instructions,
            permissions,
            catalogs,
            catalog,
            notes,
            selection,
            cwd_label: cards::display_path(cwd),
            branch: cards::git_branch(cwd).await,
            resume_latest: false,
            resume_session: None,
            mcp,
            mcp_evt_rx,
            custom_commands: true,
            starts_in_plan_mode: false,
        })
    }

    /// Override the startup model selection with a `provider/model` spec
    /// (`craft -m`). Unknown specs fail fast instead of silently
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
            .find(|info| info.ids.first().is_some_and(|id| id == model))
            .ok_or_else(|| {
                crate::error::InvalidSnafu {
                    reason: format!("provider {provider:?} has no model {model:?}"),
                }
                .build()
            })?;
        self.selection = Selection {
            provider: provider.to_string(),
            model: entry.ids.first().expect("row carries an id").clone(),
            context_length: entry.config.context_window,
        };
        Ok(self)
    }
}

/// A persisted session being restored into a fresh conversation: the
/// persister owns the loaded history and the session's on-disk identity.
pub(super) struct RestoredSession {
    pub(super) persister: Arc<tokio::sync::Mutex<crate::harness::Persister>>,
}

/// Assemble one session's [`SessionState`]: resolve the model, open (or
/// reuse) the persister, wire the approval/question seams and the subagent
/// launcher into the conversation, and pair the event stream the caller
/// forwards. The state is not installed yet; [`install_session`] does that.
pub(super) async fn build_session_state(
    kit: &SessionKit,
    selection: &Selection,
    restore: Option<RestoredSession>,
    thinking: crate::thinking::ThinkingConfig,
) -> Result<(SessionState, crate::harness::EventStream), String> {
    let model = resolve_model(&kit.config, &kit.listings, selection).await?;
    let (session_ref, persister) = match restore {
        Some(restored) => {
            let id = restored.persister.lock().await.store().session().id.clone();
            (id, Some(restored.persister))
        }
        None => {
            let session_ref = crate::id::SessionRef::generate();
            let persister = match &kit.state_dir {
                Some(dir) => Some(Arc::new(tokio::sync::Mutex::new(
                    crate::harness::Persister::open_in(
                        dir.clone(),
                        session_ref.clone(),
                        &kit.cwd,
                        &LoopCtx::model_spec(selection),
                    )
                    .await
                    .map_err(|error| error.to_string())?,
                ))),
                None => None,
            };
            (session_ref, persister)
        }
    };
    // The persister's history is the conversation's history (the same
    // change feed); a non-persisting session starts a fresh one.
    let history = match &persister {
        Some(persister) => persister.lock().await.history(),
        None => animist::history::History::new(),
    };

    // The approval seam is shared by the conversation and every subagent,
    // so children cannot bypass approvals: one permission engine, one
    // interaction, and (for the TUI) the auto-review reviewer.
    let approval = Arc::new(ApprovalGate::new(
        kit.park.clone(),
        kit.evt_tx.clone(),
        kit.cancel_cell.clone(),
    ));
    let policy =
        crate::harness::PolicyLayerInputs::new(kit.permissions.clone(), approval.clone()).reviewer(
            crate::auto_review::reviewer_for(model.clone(), kit.config.agent.prompt_format),
        );
    // The question seam parks on the same slots the loop answers.
    let asker = Arc::new(QuestionAsker::new(
        kit.park.clone(),
        kit.evt_tx.clone(),
        kit.cancel_cell.clone(),
    ));
    let (sink, stream) = crate::harness::EventSink::new();
    let workspace = kit.workspace.clone();
    // Register MCP tool annotations once the tool set is settled
    // so the gate consults the same hints as the turn path.
    if let Some(mcp) = workspace.mcp() {
        kit.permissions.sync_mcp_annotations(&mcp);
    }
    // Reauth hook over the session provider's credential.
    let reauth = crate::provider::reauth_hook(
        kit.config
            .providers
            .get(&selection.provider)
            .ok_or_else(|| format!("provider {} is not configured", selection.provider))?,
        &model.model.id,
        &model.model.info,
    )
    .await
    .map_err(|error| error.to_string())?;
    let conversation = crate::harness::ConversationBuilder::new(
        &kit.config,
        &kit.cwd,
        &kit.instructions_text,
        model,
        workspace.clone().registry(&history).map_err(|error| {
            report(crate::error::Error::BuildConversation {
                source: Box::new(error),
            })
        })?,
        sink,
    )
    .history(history)
    .thinking(thinking)
    .policy(policy.clone())
    // A 401 parks the run on the env-credential reauth hook instead
    // of failing the turn (the TUI's env is refreshed by plugins and
    // config reloads while it runs).
    .reauth(reauth)
    .workspace(workspace.clone())
    .persist(persister.clone())
    .build()
    .map_err(report)?;
    // Two-phase arming: the launcher needs the built conversation, the
    // conversation's tool set needs the launcher — one registry rebuild.
    let launcher = Arc::new(crate::subagent::SubagentLauncher {
        parent: conversation.clone(),
        provider: selection.provider.clone(),
        providers: kit.config.providers.clone(),
        agent: crate::config::AgentConfig::default(),
        compression: kit.config.compression.clone(),
        base_prompt: format!("{}{}", kit.config.agent.preamble, kit.instructions_text),
        workspace: workspace.clone(),
        cancel: kit.cancel_cell.clone(),
        cancels: kit.subagent_cancels.clone(),
        sink: conversation.sink(),
        policy,
        cwd: kit.cwd.clone(),
    });
    let armed = workspace
        .clone()
        .with_questions(asker)
        .with_subagents(launcher)
        .registry(&conversation.history())
        .map_err(|error| error.to_string())?;
    conversation
        .set_tools(armed)
        .map_err(|error| error.to_string())?;
    if let Some(persister) = &persister {
        persister.lock().await.store_mut().set_thinking(thinking);
    }
    let usage = UsageLedger::open().with_session_id(session_ref.id().to_string());
    Ok((
        SessionState {
            thinking,
            conversation,
            model_spec: format!("{}/{}", selection.provider, selection.model),
            signals: RunSignals::new(),
            usage,
        },
        stream,
    ))
}

/// Open the loop's first session: build the state, wrap it in its shared
/// cell, and start the conversation's forwarder. Later rebuilds go through
/// [`install_session`] instead.
pub(super) async fn open_session(
    kit: &SessionKit,
    thinking: crate::thinking::ThinkingConfig,
    restore: Option<RestoredSession>,
) -> Result<Arc<tokio::sync::Mutex<SessionState>>, String> {
    let selection = Arc::unwrap_or_clone(kit.selection.load_full());
    let (session, stream) = build_session_state(kit, &selection, restore, thinking).await?;
    let state = Arc::new(tokio::sync::Mutex::new(session));
    let signals = start_forwarder(
        stream,
        state.clone(),
        kit.evt_tx.clone(),
        kit.files.clone(),
        kit.selection.clone(),
    );
    state.lock().await.signals = signals;
    Ok(state)
}

/// Build and install one session: assemble the state, start the new
/// conversation's forwarder, and swap the state in — dropping the previous
/// conversation ends its forwarder. Errors leave the current session
/// untouched.
pub(super) async fn install_session(
    kit: &SessionKit,
    state: &Arc<tokio::sync::Mutex<SessionState>>,
    thinking: crate::thinking::ThinkingConfig,
    restore: Option<RestoredSession>,
) -> Result<(), String> {
    let selection = Arc::unwrap_or_clone(kit.selection.load_full());
    let (session, stream) = build_session_state(kit, &selection, restore, thinking).await?;
    let signals = start_forwarder(
        stream,
        state.clone(),
        kit.evt_tx.clone(),
        kit.files.clone(),
        kit.selection.clone(),
    );
    *state.lock().await = SessionState { signals, ..session };
    Ok(())
}
