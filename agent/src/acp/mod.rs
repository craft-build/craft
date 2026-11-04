//! Craft's ACP agent boundary: the shared conversation exposed over
//! agent-client-protocol v1.
//!
//! Every capability of the conversation (`harness::run_turn` with
//! read/grep/edit/delete tools, streaming output, per-turn usage) is
//! surfaced here: model selection travels as one grouped session config
//! option (one group per provider, `{provider}/{model}` values) and the
//! reasoning policy as a `thought_level` option, model output streams as
//! `session/update` notifications, and `session/cancel`
//! stops the in-flight turn. Gated tool calls ask the client over
//! `session/request_permission`, the `question` tool is mapped onto
//! `elicitation/create` forms, the `task` tool spawns subagents under the
//! turn's shared permission gate (like the TUI/headless surfaces), and
//! `session/load` resumes a persisted session.
//!
//! Deliberately not advertised: embedded context, image, and audio
//! prompts. `mcp_servers` on new/load is ignored.

mod elicitation;
mod permissions;
#[cfg(test)]
mod tests;
mod turn;

use turn::run_turn;

use agent_client_protocol::{
    Agent as AcpRole, Client as AcpClient, Error, JsonRpcResponse, Stdio, on_receive_notification,
    on_receive_request,
    schema::ProtocolVersion,
    v1::{
        ConnectionTo, Responder,
        schema::{
            AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest,
            CloseSessionResponse, ConfigOptionUpdate, ContentBlock, Implementation,
            InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
            NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
            PromptResponse as AcpPromptResponse, SessionCapabilities, SessionCloseCapabilities,
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectGroup,
            SessionConfigSelectOption, SessionId, SessionNotification, SessionUpdate,
            SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
        },
    },
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex;

use crate::{
    config::Config, model_selection::CatalogModel, permissions::PermissionManager,
    provider::ProviderHandle, tools::Workspace,
};

pub const MODEL_OPTION_ID: &str = "model";
pub const THINKING_OPTION_ID: &str = "thinking";

struct Session {
    /// The session's conversation: history, model, layers, and the event
    /// sink, all outliving every turn.
    conversation: crate::harness::Conversation,
    /// The conversation's single event stream, lent to the in-flight turn.
    events: crate::harness::EventStream,
    /// The per-turn cancellation cell: each prompt stores its cancellation
    /// here, and the gate, asker, MCP tools, and subagent launcher race it.
    cancel_cell: Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    /// Loaded sessions persist through the change feed; fresh ones do not.
    persister: Option<Arc<tokio::sync::Mutex<crate::harness::Persister>>>,
    provider_name: String,
    models: Vec<animist::provider::ModelInfo>,
    model: String,
    context_length: Option<u32>,
    thinking: crate::thinking::ThinkingConfig,
    /// In-flight turn id; `Some` while a `session/prompt` turn is running.
    /// Claimed and released under the `sessions` lock, so no atomic is
    /// needed; a second prompt is rejected rather than queued.
    turn: Option<u64>,
}

impl Session {
    /// Claim the in-flight turn slot for `turn_id`; fails closed while
    /// another turn is running so two turns can never interleave their
    /// history commits or re-arm each other's cancellation flag.
    fn begin_turn(&mut self, turn_id: u64) -> std::result::Result<(), String> {
        if self.turn.is_some() {
            return Err("a prompt is already in flight for this session".into());
        }
        self.turn = Some(turn_id);
        Ok(())
    }

    fn config_options(
        &self,
        config: &Config,
        listings: &BTreeMap<String, Vec<animist::provider::ModelInfo>>,
    ) -> Vec<SessionConfigOption> {
        vec![
            model_option(config, listings, &self.provider_name, &self.model),
            thinking_option(self.thinking, self.model_info(&self.model)),
        ]
    }
}

fn thinking_option(
    current: crate::thinking::ThinkingConfig,
    info: Option<&animist::provider::ModelInfo>,
) -> SessionConfigOption {
    let choices = crate::thinking::choices_for(current, info);
    let current = crate::thinking::selected_choice(current, &choices);
    SessionConfigOption::select(
        THINKING_OPTION_ID,
        "Thinking",
        current.to_string(),
        choices
            .into_iter()
            .map(|thinking| {
                let value = thinking.to_string();
                SessionConfigSelectOption::new(value.clone(), value)
            })
            .collect::<Vec<_>>(),
    )
    .category(SessionConfigOptionCategory::ThoughtLevel)
    .description("Reasoning policy for this session and its subagents")
}

/// The spec-intended multi-provider shape: one `model` select whose
/// options are grouped per provider with `"{provider}/{model}"` values,
/// so a single pick switches both. Providers whose listing could not be
/// resolved are omitted — the option degrades instead of failing the
/// session.
fn model_option(
    config: &Config,
    listings: &BTreeMap<String, Vec<animist::provider::ModelInfo>>,
    provider_name: &str,
    model: &str,
) -> SessionConfigOption {
    let mut current_value = String::new();
    let groups = listings
        .iter()
        .map(|(name, models)| {
            let options = CatalogModel::rows(models, config.providers.get(name))
                .into_iter()
                .map(|row| {
                    let value = format!("{name}/{}", row.id);
                    if name == provider_name && row.id == model {
                        current_value = value.clone();
                    }
                    let mut option = SessionConfigSelectOption::new(value, row.label());
                    if let Some(description) = &row.description {
                        option = option.description(description.clone());
                    }
                    option
                })
                .collect::<Vec<_>>();
            SessionConfigSelectGroup::new(name.clone(), name.clone(), options)
        })
        .filter(|group| !group.options.is_empty())
        .collect::<Vec<_>>();
    if current_value.is_empty() {
        // The current model's row is missing (a listing that dropped the
        // id); keep the `{provider}/{model}` invariant so clients can
        // still display the selection.
        current_value = format!("{provider_name}/{model}");
    }
    SessionConfigOption::select(MODEL_OPTION_ID, "Model", current_value, groups)
        .category(SessionConfigOptionCategory::Model)
        .description("Model across the configured providers; one pick switches provider and model")
}

impl Session {
    /// The animist info for a model id, when the listing carries it. Any
    /// of the entry's ids matches, so aliases resolve too.
    fn model_info(&self, id: &str) -> Option<&animist::provider::ModelInfo> {
        self.models
            .iter()
            .find(|info| info.ids.iter().any(|listed| listed == id))
    }
}

struct AppState {
    config: Config,
    sessions: Mutex<BTreeMap<String, Session>>,
    /// Provider handles and listings by provider name, cached for the
    /// process lifetime so option building and per-turn resolution stop
    /// re-fetching. Never held across an await.
    catalogs: Mutex<BTreeMap<String, (ProviderHandle, Vec<animist::provider::ModelInfo>)>>,
    next_session: AtomicU64,
    /// Monotonic source of per-session in-flight turn ids.
    next_turn: AtomicU64,
    /// Client capabilities from `initialize`, consulted before elicitation.
    client_caps: Mutex<Option<ClientCapabilities>>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config,
            sessions: Mutex::new(BTreeMap::new()),
            catalogs: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
            next_turn: AtomicU64::new(1),
            client_caps: Mutex::new(None),
        }
    }

    /// Resolve a configured provider and its merged model catalog,
    /// serving and populating the process-wide cache. A failed fetch is
    /// not cached, so the next call retries.
    async fn provider_catalog(
        &self,
        name: &str,
    ) -> std::result::Result<(ProviderHandle, Vec<animist::provider::ModelInfo>), String> {
        let config = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| format!("unknown provider {name:?}"))?;
        if let Some(cached) = self.catalogs.lock().await.get(name) {
            return Ok(cached.clone());
        }
        let catalog = crate::provider::load_catalog().await;
        let provider = ProviderHandle::from_config(config, Some(&catalog))
            .await
            .map_err(report)?;
        let models = provider
            .models(config, Some(&catalog))
            .await
            .map_err(report)?;
        let entry = {
            let mut catalogs = self.catalogs.lock().await;
            catalogs
                .entry(name.to_owned())
                .or_insert((provider, models))
                .clone()
        };
        Ok(entry)
    }

    /// Every configured provider's listing for the grouped model option,
    /// fetched in parallel on first use and cached thereafter. A provider
    /// that cannot be resolved is omitted, never fatal.
    async fn listings(&self) -> BTreeMap<String, Vec<animist::provider::ModelInfo>> {
        let names: Vec<String> = self.config.providers.keys().cloned().collect();
        futures::future::join_all(names.iter().map(|name| async move {
            self.provider_catalog(name)
                .await
                .ok()
                .map(|(_, models)| (name.clone(), models))
        }))
        .await
        .into_iter()
        .flatten()
        .collect()
    }

    /// The cached listings only; never fetches, so turn-time option
    /// rebuilds stay off the network.
    async fn cached_listings(&self) -> BTreeMap<String, Vec<animist::provider::ModelInfo>> {
        self.catalogs
            .lock()
            .await
            .iter()
            .map(|(name, (_, models))| (name.clone(), models.clone()))
            .collect()
    }

    async fn open_session(
        &self,
        cwd: &Path,
        connection: ConnectionTo<AcpClient>,
        session_id: SessionId,
        client_caps: ClientCapabilities,
    ) -> std::result::Result<Session, String> {
        let (instructions, workspace, permissions) = self.open_workspace(cwd).await?;
        let provider_name = self
            .config
            .providers
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| "no providers are configured in ~/.config/craft.bml".to_string())?;
        let (_provider, models) = self.provider_catalog(&provider_name).await?;
        let resolved = crate::model_selection::resolve_or_default(&models, None);
        let (model, context_length) = match resolved {
            Some(r) => (r.model, r.context_length),
            None => (String::new(), None),
        };
        self.build_session(BuildInputs {
            workspace,
            instructions,
            permissions,
            persister: None,
            provider_name,
            models,
            model,
            context_length,
            cwd: cwd.display().to_string(),
            connection,
            session_id,
            client_caps,
        })
        .await
    }

    /// Resume a persisted session (`session/load`): history, provider,
    /// and model come from the stored record; the workspace is rooted at the
    /// client-requested `cwd`, not the recorded one — the client decides
    /// where the resumed session runs.
    async fn load_session(
        &self,
        cwd: &Path,
        stored_id: &str,
        connection: ConnectionTo<AcpClient>,
        session_id: SessionId,
        client_caps: ClientCapabilities,
    ) -> std::result::Result<Session, String> {
        use crate::storage::StateDir;
        let session_ref = stored_id
            .parse::<crate::id::SessionRef>()
            .map_err(|_| format!("invalid session id {stored_id:?}"))?;
        let dir = StateDir::resolve().map_err(|e| e.to_string())?;
        let persister = crate::harness::Persister::open_in(
            dir,
            session_ref,
            &cwd.display().to_string(),
            "unknown",
        )
        .await
        .map_err(|e| format!("session {stored_id:?} could not be loaded: {e}"))?;
        let stored_model = persister.store().session().model.clone();
        let stored_thinking = persister.store().session().meta.thinking;
        let stored_entries = persister.store().session().messages().to_vec();
        let (instructions, workspace, permissions) = self.open_workspace(cwd).await?;
        // The stored model spec is `{provider}/{model}` (see `run_turn`).
        let (provider_name, model): (String, Option<String>) = match stored_model.split_once('/') {
            Some((provider, model))
                if !self.config.providers.contains_key(provider) || model.is_empty() =>
            {
                (self.default_provider()?, None)
            }
            Some((provider, model)) => (provider.to_owned(), Some(model.to_owned())),
            None => (self.default_provider()?, None),
        };
        let (_provider, models) = self.provider_catalog(&provider_name).await?;
        let resolved = crate::model_selection::resolve_or_default(&models, model.as_deref());
        let (model, context_length) = match resolved {
            Some(r) => (r.model, r.context_length),
            None => (String::new(), None),
        };
        // `build_session` consumes the connection and session id; the
        // replay below still needs both.
        let (replay_connection, replay_session_id) = (connection.clone(), session_id.clone());
        let mut session = self
            .build_session(BuildInputs {
                workspace,
                instructions,
                permissions,
                persister: Some(Arc::new(tokio::sync::Mutex::new(persister))),
                provider_name,
                models,
                model,
                context_length,
                cwd: cwd.display().to_string(),
                connection,
                session_id,
                client_caps,
            })
            .await?;
        if let Some(thinking) = stored_thinking {
            session.thinking = thinking;
        }
        session.thinking =
            crate::thinking::reconcile_for(session.thinking, session.model_info(&session.model));
        session
            .conversation
            .update_settings(|settings| settings.thinking = session.thinking);
        // The resume contract: the replayed transcript reaches the client as
        // session updates before the `session/load` response, built by the
        // same shared replay the TUI resume uses.
        for update in turn::replay_updates(&crate::harness::replay::transcript(&stored_entries)) {
            let _ = replay_connection
                .send_notification(SessionNotification::new(replay_session_id.clone(), update));
        }
        Ok(session)
    }

    fn default_provider(&self) -> std::result::Result<String, String> {
        self.config
            .providers
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| "no providers are configured in ~/.config/craft.bml".to_string())
    }

    /// Instructions discovery, workspace, and the permission engine —
    /// shared by `session/new` and `session/load`, through the runtime
    /// setup contract (headless `Connected` MCP, no reviewer install).
    async fn open_workspace(
        &self,
        cwd: &Path,
    ) -> std::result::Result<
        (
            crate::instructions::Instructions,
            Workspace,
            Arc<PermissionManager>,
        ),
        String,
    > {
        let env = crate::runtime::workspace_env(
            cwd,
            crate::runtime::McpStartup::Connected,
            crate::sandbox::SandboxPolicy::resolve(&self.config, false),
        )
        .await
        .map_err(report)?;
        Ok((env.instructions, env.workspace, env.permissions))
    }

    async fn build_session(&self, inputs: BuildInputs) -> std::result::Result<Session, String> {
        let BuildInputs {
            workspace,
            instructions,
            permissions,
            persister,
            provider_name,
            models,
            model,
            context_length,
            cwd,
            connection,
            session_id,
            client_caps,
        } = inputs;
        // The conversation needs a resolved model handle; the catalog's
        // first entry stands in when nothing is selected yet.
        let model = if model.is_empty() {
            models
                .first()
                .and_then(|info| info.ids.first().cloned())
                .ok_or_else(|| "provider has no models".to_string())?
        } else {
            model
        };
        let (provider, _models) = self.provider_catalog(&provider_name).await?;
        let provider_config = &self.config.providers[&provider_name];
        let handle = provider
            .model(provider_config, None, &model)
            .await
            .map_err(report)?;

        let preference = self
            .config
            .always_thinking
            .or(self.config.agent.thinking)
            .unwrap_or_default();
        let info = models
            .iter()
            .find(|info| info.ids.first().is_some_and(|id| id.as_str() == model));
        let thinking = crate::thinking::reconcile_for(preference, info);

        let cancel_cell = Arc::new(arc_swap::ArcSwap::from_pointee(
            animist::agent::Cancellation::default(),
        ));
        let gate = Arc::new(crate::acp::permissions::AcpPermissionGate::new(
            connection.clone(),
            session_id.clone(),
            cancel_cell.clone(),
        ));
        // No reviewer: ACP asks go to the client and the surface never
        // toggles auto-review on, so the policy layer falls through to the
        // responder.
        let policy = crate::harness::PolicyLayerInputs::new(Arc::clone(&permissions), gate.clone());
        let asker = Arc::new(crate::acp::elicitation::ElicitationAsker {
            connection: connection.clone(),
            session_id: session_id.clone(),
            cancel: cancel_cell.clone(),
            caps: client_caps,
        });
        let (sink, events) = crate::harness::EventSink::new();
        let history = match &persister {
            Some(persister) => persister.lock().await.history(),
            None => animist::history::History::new(),
        };
        // Register MCP tool annotations once the tool set is
        // settled so the gate consults the same hints as the TUI.
        if let Some(mcp) = workspace.mcp() {
            permissions.sync_mcp_annotations(&mcp);
        }
        // Reauth hook over the session provider's credential; a 401
        // parks the run for the refresh instead of failing the turn.
        let reauth = crate::provider::reauth_hook(provider_config, &model)
            .await
            .map_err(report)?;
        let conversation = crate::harness::ConversationBuilder::new(
            &self.config,
            &cwd,
            &instructions.text,
            handle.clone(),
            workspace.clone().registry(&history).map_err(|error| {
                report(crate::error::Error::Invalid {
                    reason: format!("building tool registry: {error}"),
                })
            })?,
            sink,
        )
        .history(history)
        .max_turns(crate::harness::MaxTurns::Unbounded)
        .thinking(thinking)
        .policy(policy.clone())
        .reauth(reauth)
        .workspace(workspace.clone())
        .persist(persister.clone())
        .build()
        .map_err(report)?;

        let launcher = Arc::new(crate::subagent::SubagentLauncher {
            parent: conversation.clone(),
            provider: provider_name.clone(),
            providers: self.config.providers.clone(),
            agent: crate::config::AgentConfig {
                thinking: Some(thinking),
                ..Default::default()
            },
            compression: self.config.compression.clone(),
            base_prompt: format!("{}{}", self.config.agent.preamble, instructions.text),
            workspace: workspace.clone(),
            cancel: cancel_cell.clone(),
            cancels: Arc::new(crate::harness::cancel::CancelMap::new()),
            sink: conversation.sink(),
            policy,
            cwd: cwd.clone(),
        });
        let armed = workspace
            .clone()
            .with_questions(asker)
            .with_subagents(launcher)
            .registry(&conversation.history())
            .map_err(|e| e.to_string())?;
        conversation.set_tools(armed).map_err(|e| e.to_string())?;

        Ok(Session {
            conversation,
            events,
            cancel_cell,
            persister,
            provider_name,
            models,
            model,
            context_length,
            thinking,
            turn: None,
        })
    }
}

/// The inputs [`AppState::build_session`] assembles.
struct BuildInputs {
    workspace: Workspace,
    instructions: crate::instructions::Instructions,
    permissions: Arc<PermissionManager>,
    persister: Option<Arc<tokio::sync::Mutex<crate::harness::Persister>>>,
    provider_name: String,
    models: Vec<animist::provider::ModelInfo>,
    model: String,
    context_length: Option<u32>,
    cwd: String,
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    client_caps: ClientCapabilities,
}

/// Serve the Craft agent loop over ACP on stdin/stdout until the client disconnects.
pub async fn serve(config: Config) -> std::result::Result<(), Error> {
    // Session-scoped catalog: best-effort, bounded; metadata stays unknown
    // without it. Held by the state it feeds.
    crate::provider::load_catalog().await;

    let state = Arc::new(AppState::new(config));

    let init_state = state.clone();
    let new_state = state.clone();
    let load_state = state.clone();
    let set_state = state.clone();
    let prompt_state = state.clone();
    let close_state = state.clone();
    let cancel_state = state.clone();

    AcpRole
        .v1()
        .name("craft-acp")
        .on_receive_request(
            async move |request: InitializeRequest,
                        responder: Responder<InitializeResponse>,
                        _connection: ConnectionTo<AcpClient>| {
                // Echo the requested version when supported, else our latest.
                *init_state.client_caps.lock().await = Some(request.client_capabilities.clone());
                let version = if request.protocol_version == ProtocolVersion::V1 {
                    ProtocolVersion::V1
                } else {
                    ProtocolVersion::LATEST
                };
                let response = InitializeResponse::new(version)
                    .agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(true)
                            .prompt_capabilities(PromptCapabilities::new())
                            // The close handler exists; advertising it lets
                            // capable clients actually release sessions.
                            .session_capabilities(
                                SessionCapabilities::new()
                                    .close(SessionCloseCapabilities::new()),
                            ),
                    )
                    .agent_info(Implementation::new("craft-acp", env!("CARGO_PKG_VERSION")));
                responder.respond(response)
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        connection: ConnectionTo<AcpClient>| {
                // The id is minted first: the session's conversation is
                // built with the gate and asker bound to it.
                let id = format!(
                    "{}-{}",
                    std::process::id(),
                    new_state.next_session.fetch_add(1, Ordering::Relaxed),
                );
                let client_caps = new_state
                    .client_caps
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_default();
                let session = match new_state
                    .open_session(
                        &request.cwd,
                        connection.clone(),
                        SessionId::new(id.clone()),
                        client_caps,
                    )
                    .await
                {
                    Ok(session) => session,
                    Err(message) => return respond_setup_error(responder, message),
                };
                // `mcp_servers` is ignored.
                let listings = new_state.listings().await;
                let options = session.config_options(&new_state.config, &listings);
                let response =
                    NewSessionResponse::new(SessionId::new(id.clone())).config_options(options);
                new_state.sessions.lock().await.insert(id, session);
                responder.respond(response)
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest,
                        responder: Responder<LoadSessionResponse>,
                        connection: ConnectionTo<AcpClient>| {
                // The loaded session keeps the stored id as its ACP id.
                let stored_id = request.session_id.0.to_string();
                let client_caps = load_state
                    .client_caps
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_default();
                let session = match load_state
                    .load_session(
                        &request.cwd,
                        &stored_id,
                        connection.clone(),
                        request.session_id.clone(),
                        client_caps,
                    )
                    .await
                {
                    Ok(session) => session,
                    Err(message) => return respond_setup_error(responder, message),
                };
                let listings = load_state.listings().await;
                let options = session.config_options(&load_state.config, &listings);
                load_state.sessions.lock().await.insert(stored_id, session);
                responder.respond(LoadSessionResponse::new().config_options(options))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest,
                        responder: Responder<SetSessionConfigOptionResponse>,
                        connection: ConnectionTo<AcpClient>| {
                let Some(value) = request.value.as_value_id().map(|id| id.0.to_string()) else {
                    return respond_setup_error(
                        responder,
                        "expected a configuration value id".into(),
                    );
                };
                // What the resolved change should apply to the session.
                enum ConfigChange {
                    Model {
                        provider: String,
                        resolved: crate::model_selection::ResolvedModel,
                        models: Vec<animist::provider::ModelInfo>,
                    },
                    Thinking(crate::thinking::ThinkingConfig),
                }
                // Resolution happens outside the sessions lock: the model
                // arm may fetch a provider listing (network I/O), and
                // prompt/cancel must not stall behind a provider switch.
                let resolve = match request.config_id.0.as_ref() {
                    MODEL_OPTION_ID => match value.split_once('/') {
                        None => Err(format!("model values are \"provider/model\"; got {value:?}")),
                        Some((provider, model)) => {
                            if !set_state.config.providers.contains_key(provider) {
                                Err(format!("unknown provider {provider:?}"))
                            } else {
                                set_state
                                    .provider_catalog(provider)
                                    .await
                                    .and_then(|(_, models)| {
                                        resolve_model_in_listing(&models, model)
                                            .map(|resolved| ConfigChange::Model {
                                                provider: provider.to_owned(),
                                                resolved,
                                                models,
                                            })
                                            .ok_or_else(|| {
                                                format!(
                                                    "unknown model {model:?} on provider {provider:?}"
                                                )
                                            })
                                    })
                            }
                        }
                    },
                    THINKING_OPTION_ID => {
                        // Spec: clients must send one of the offered
                        // values; anything else is a client bug.
                        let sessions = set_state.sessions.lock().await;
                        let Some(session) = sessions.get(request.session_id.0.as_ref()) else {
                            drop(sessions);
                            return respond_setup_error(responder, "unknown session".into());
                        };
                        offered_thinking(
                            &value,
                            session.thinking,
                            session.model_info(&session.model),
                        )
                        .map(ConfigChange::Thinking)
                    }
                    other => Err(format!("unknown configuration option {other:?}")),
                };
                let change = match resolve {
                    Ok(change) => change,
                    Err(message) => return respond_setup_error(responder, message),
                };
                // Session open populated the cache (the model arm may have
                // just refreshed one provider); never fetch under a config
                // change.
                let listings = set_state.cached_listings().await;
                let options = {
                    let mut sessions = set_state.sessions.lock().await;
                    let Some(session) = sessions.get_mut(request.session_id.0.as_ref()) else {
                        drop(sessions);
                        return respond_setup_error(responder, "unknown session".into());
                    };
                    match change {
                        ConfigChange::Model {
                            provider,
                            resolved,
                            models,
                        } => {
                            session.provider_name = provider;
                            session.model = resolved.model;
                            session.context_length = resolved.context_length;
                            session.models = models;
                        }
                        ConfigChange::Thinking(thinking) => session.thinking = thinking,
                    }
                    session.thinking = crate::thinking::reconcile_for(
                        session.thinking,
                        session.model_info(&session.model),
                    );
                    session
                        .conversation
                        .update_settings(|settings| settings.thinking = session.thinking);
                    if let Some(persister) = &session.persister {
                        let mut persister = persister.lock().await;
                        persister.store_mut().set_thinking(session.thinking);
                        persister
                            .store_mut()
                            .set_model(format!("{}/{}", session.provider_name, session.model));
                    }
                    session.config_options(&set_state.config, &listings)
                };
                let notification = SessionNotification::new(
                    request.session_id.clone(),
                    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options.clone())),
                );
                connection.send_notification(notification)?;
                responder.respond(SetSessionConfigOptionResponse::new(options))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest,
                        responder: Responder<AcpPromptResponse>,
                        connection: ConnectionTo<AcpClient>| {
                let text = match prompt_text(&request.prompt) {
                    Ok(text) => text,
                    Err(message) => return respond_setup_error(responder, message),
                };
                let (conversation, mut events, provider_name, model, thinking, turn_cancel, turn_id) = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    let Some(session) = sessions.get_mut(request.session_id.0.as_ref()) else {
                        drop(sessions);
                        return respond_setup_error(responder, "unknown session".into());
                    };
                    // Fail closed: a second prompt while one is in flight is
                    // rejected rather than queued, so the turns can never
                    // interleave on the conversation or re-arm each other's
                    // cancellation.
                    let turn_id = prompt_state.next_turn.fetch_add(1, Ordering::Relaxed);
                    if let Err(message) = session.begin_turn(turn_id) {
                        drop(sessions);
                        return responder.respond_with_error(invalid_request(message));
                    }
                    // A fresh cancellation for this turn, visible to the
                    // gate, asker, MCP tools, and subagents through the cell.
                    let turn_cancel = animist::agent::Cancellation::default();
                    session.cancel_cell.store(Arc::new(turn_cancel.clone()));
                    (
                        session.conversation.clone(),
                        std::mem::replace(&mut session.events, crate::harness::EventSink::new().1),
                        session.provider_name.clone(),
                        session.model.clone(),
                        session.thinking,
                        turn_cancel,
                        turn_id,
                    )
                };
                let run_state = prompt_state.clone();
                let prompt_session_id = request.session_id.clone();
                let return_state = prompt_state.clone();
                let return_session_id = prompt_session_id.clone();
                tokio::spawn(async move {
                    run_turn(
                        run_state,
                        connection,
                        prompt_session_id,
                        text,
                        conversation,
                        &mut events,
                        provider_name,
                        model,
                        thinking,
                        turn_cancel,
                        turn_id,
                        responder,
                    )
                    .await;
                    // Return the stream to the session for the next turn.
                    let mut sessions = return_state.sessions.lock().await;
                    if let Some(session) = sessions.get_mut(return_session_id.0.as_ref())
                        && session.turn == Some(turn_id)
                    {
                        session.events = events;
                        session.turn = None;
                    }
                });
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        _connection: ConnectionTo<AcpClient>| {
                let session = close_state
                    .sessions
                    .lock()
                    .await
                    .remove(request.session_id.0.as_ref());
                // Release the conversation's resources before the session
                // goes away (async cleanup cannot run in `Drop`).
                if let Some(session) = session {
                    let _ = session.conversation.shutdown().await;
                }
                responder.respond(CloseSessionResponse::new())
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                let sessions = cancel_state.sessions.lock().await;
                if let Some(session) = sessions.get(notification.session_id.0.as_ref()) {
                    session.cancel_cell.load().cancel();
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}

fn respond_setup_error<T: JsonRpcResponse>(
    responder: Responder<T>,
    message: String,
) -> std::result::Result<(), Error> {
    responder.respond_with_error(invalid_params(message))
}

/// Spec rule for `session/set_config_option`: the client must send one of
/// the option's offered values. Parses and enforces that for the thinking
/// option.
fn offered_thinking(
    value: &str,
    current: crate::thinking::ThinkingConfig,
    info: Option<&animist::provider::ModelInfo>,
) -> std::result::Result<crate::thinking::ThinkingConfig, String> {
    let choices = crate::thinking::choices_for(current, info);
    crate::thinking::ThinkingConfig::parse(value, current).and_then(|thinking| {
        choices
            .contains(&thinking)
            .then_some(thinking)
            .ok_or_else(|| format!("{value:?} is not one of the offered thinking levels"))
    })
}

/// Resolve the model half of a `"{provider}/{model}"` option value
/// against a fetched listing. Strict like [`crate::model_selection::resolve_spec`]:
/// the listing must still carry the id (any of its aliases), else the
/// pick is stale and the client should re-pick. The canonical first id is
/// what gets stored.
fn resolve_model_in_listing(
    models: &[animist::provider::ModelInfo],
    model: &str,
) -> Option<crate::model_selection::ResolvedModel> {
    models
        .iter()
        .find(|info| info.ids.iter().any(|listed| listed == model))
        .map(|info| crate::model_selection::ResolvedModel {
            // A listed id matched, so `ids` is non-empty.
            model: info
                .ids
                .first()
                .cloned()
                .unwrap_or_else(|| model.to_owned()),
            context_length: info.config.context_window,
        })
}

/// Render an error and its sources as one client-facing message.
fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

fn invalid_params(message: impl Into<String>) -> Error {
    let mut error = Error::invalid_params();
    error.message = message.into();
    error
}

fn invalid_request(message: impl Into<String>) -> Error {
    let mut error = Error::invalid_request();
    error.message = message.into();
    error
}

async fn clear_turn(
    sessions: &Mutex<BTreeMap<String, Session>>,
    session_id: &SessionId,
    turn_id: u64,
) {
    let mut sessions = sessions.lock().await;
    if let Some(session) = sessions.get_mut(session_id.0.as_ref())
        && session.turn == Some(turn_id)
    {
        session.turn = None;
    }
}

fn internal_error(message: impl Into<String>) -> Error {
    let mut error = Error::internal_error();
    error.message = message.into();
    error
}

/// Flatten prompt content blocks into one user message. Text blocks are kept
/// verbatim; resource links become an explicit context-file section the model
/// can open with its read tool.
fn prompt_text(prompt: &[ContentBlock]) -> std::result::Result<String, String> {
    let mut text = String::new();
    let mut context_files = Vec::new();
    for block in prompt {
        match block {
            ContentBlock::Text(content) => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&content.text);
            }
            ContentBlock::ResourceLink(link) => context_files.push(link.name.clone()),
            _ => return Err("only text and resource-link content is supported".into()),
        }
    }
    if !context_files.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("Context files:\n");
        for file in context_files {
            text.push_str(&format!("- {file}\n"));
        }
    }
    Ok(text)
}
