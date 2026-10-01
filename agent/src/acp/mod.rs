//! Craft's ACP agent boundary: the shared run loop exposed over
//! agent-client-protocol v1.
//!
//! Every capability of the run loop (`run::run` with read/grep/edit/delete
//! tools, streaming output, per-turn usage, caller-owned history) is surfaced
//! here: provider and model selection travel as session config options, model
//! output streams as `session/update` notifications, and `session/cancel`
//! stops the in-flight turn. The loop itself is unchanged; this module only
//! translates. Gated tool calls ask the client over
//! `session/request_permission`, the `question` tool is mapped onto
//! `elicitation/create` forms, the `task` tool spawns subagents under the
//! turn's shared permission gate (like the TUI/headless surfaces), and
//! `session/load` resumes a persisted session (task G.5).
//!
//! Deliberately not advertised: embedded context, image, and audio
//! prompts. `mcp_servers` on new/load is ignored until the MCP client
//! (task 94) is ported.

mod elicitation;
mod permissions;
#[cfg(test)]
mod tests;
mod turn;

use turn::run_turn;

use agent_client_protocol::{
    Agent as AcpRole, Client as AcpClient, ConnectionTo, Error, JsonRpcResponse, Responder, Stdio,
    on_receive_notification, on_receive_request,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest,
            CloseSessionResponse, ConfigOptionUpdate, ContentBlock, Implementation,
            InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
            NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
            PromptResponse as AcpPromptResponse, SessionConfigOption, SessionConfigOptionCategory,
            SessionConfigSelectOption, SessionConfigValueId, SessionId, SessionNotification,
            SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
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
    config::Config,
    history,
    permissions::PermissionManager,
    providers::{CatalogModel, Provider, ProviderKind},
    run,
    tools::Workspace,
};

pub const PROVIDER_OPTION_ID: &str = "provider";
pub const MODEL_OPTION_ID: &str = "model";

struct Session {
    workspace: Workspace,
    /// Instruction files (AGENTS.md and friends) discovered at session open.
    instructions: crate::instructions::Instructions,
    history: Vec<history::Message>,
    provider_name: String,
    models: Vec<CatalogModel>,
    model: String,
    context_length: Option<u32>,
    /// Effectiveness state for the configured compaction stages, shared
    /// with the run loop for in-run overflow recovery.
    compaction: run::SharedCompactionState,
    /// Session-wide tool dedup cache, shared by the dispatcher and cleared
    /// by the compaction engine.
    dedup: run::SharedDedupCache,
    /// Session-lifetime permission engine; the ACP gate prompts the client
    /// where the TUI gate prompts the user (G.5).
    permissions: Arc<PermissionManager>,
    cancel: run::CancelFlag,
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

    fn config_options(&self, provider_names: &[String]) -> Vec<SessionConfigOption> {
        vec![
            SessionConfigOption::select(
                PROVIDER_OPTION_ID,
                "Provider",
                self.provider_name.clone(),
                provider_names
                    .iter()
                    .map(|name| SessionConfigSelectOption::new(name.clone(), name.clone()))
                    .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::ModelConfig)
            .description("Configured inference provider from ~/.config/craft.bml"),
            model_option(&self.models, &self.model),
        ]
    }
}

fn model_option(models: &[CatalogModel], current: &str) -> SessionConfigOption {
    SessionConfigOption::select(
        MODEL_OPTION_ID,
        "Model",
        SessionConfigValueId::new(current),
        models
            .iter()
            .map(|model| {
                let mut option =
                    SessionConfigSelectOption::new(model.id.clone(), model.label().to_owned());
                if let Some(description) = &model.description {
                    option = option.description(description.clone());
                }
                option
            })
            .collect::<Vec<_>>(),
    )
    .category(SessionConfigOptionCategory::Model)
    .description("Model on the selected provider; the provider validates the ID on first use")
}

struct AppState {
    config: Config,
    sessions: Mutex<BTreeMap<String, Session>>,
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
            next_session: AtomicU64::new(1),
            next_turn: AtomicU64::new(1),
            client_caps: Mutex::new(None),
        }
    }

    fn provider_names(&self) -> Vec<String> {
        self.config.providers.keys().cloned().collect()
    }

    /// Resolve a configured provider and its merged model catalog.
    async fn provider_catalog(
        &self,
        name: &str,
    ) -> std::result::Result<(Provider, Vec<CatalogModel>), String> {
        let config = self
            .config
            .providers
            .get(name)
            .ok_or_else(|| format!("unknown provider {name:?}"))?;
        if config.kind == ProviderKind::Voyageai {
            return Err(format!(
                "provider {name:?} does not support completion models"
            ));
        }
        let provider = Provider::from_config(config).map_err(report)?;
        let models: Vec<CatalogModel> = provider.models(config).await.map_err(report)?;
        Ok((provider, models))
    }

    async fn open_session(&self, cwd: &Path) -> std::result::Result<Session, String> {
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
        Ok(self.build_session(
            workspace,
            instructions,
            permissions,
            Vec::new(),
            provider_name,
            models,
            model,
            context_length,
        ))
    }

    /// Resume a persisted session (G.5 `session/load`): history, provider,
    /// and model come from the stored record; the workspace is rooted at the
    /// client-requested `cwd`, not the recorded one — the client decides
    /// where the resumed session runs.
    async fn load_session(
        &self,
        cwd: &Path,
        stored_id: &str,
    ) -> std::result::Result<Session, String> {
        use crate::storage::StateDir;
        let session_ref = stored_id
            .parse::<crate::id::SessionRef>()
            .map_err(|_| format!("invalid session id {stored_id:?}"))?;
        let dir = StateDir::resolve().map_err(|e| e.to_string())?;
        let stored = crate::headless::StoredSession::load(session_ref.id(), &dir)
            .map_err(|e| format!("session {stored_id:?} could not be loaded: {e}"))?;
        let (instructions, workspace, permissions) = self.open_workspace(cwd).await?;
        // The stored model spec is `{provider}/{model}` (see `run_turn`).
        let stored_model = stored.model.clone();
        let (provider_name, model) = match stored_model.split_once('/') {
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
        Ok(self.build_session(
            workspace,
            instructions,
            permissions,
            stored.messages().to_vec(),
            provider_name,
            models,
            model,
            context_length,
        ))
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
    /// shared by `session/new` and `session/load`.
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
        let (instructions, permissions) = tokio::task::spawn_blocking({
            let cwd = cwd.display().to_string();
            move || {
                let instructions = crate::instructions::load_instructions(&cwd);
                let permissions = PermissionManager::new(
                    crate::permissions::load_permissions(Path::new(&cwd)),
                    cwd.into(),
                );
                (instructions, permissions)
            }
        })
        .await
        .map_err(|e| e.to_string())?;
        let workspace = Workspace::new(cwd)
            .map_err(|e| e.to_string())?
            .with_loaded_instructions(instructions.loaded.clone());
        // MCP (B.11): headless paths have no frame to protect, so connect up
        // front — `start_connected` waits for every server to settle.
        let (mcp_handle, _mcp_errors) = crate::mcp::start_connected(workspace.root()).await;
        workspace.set_mcp(mcp_handle);
        Ok((instructions, workspace, Arc::new(permissions)))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_session(
        &self,
        workspace: Workspace,
        instructions: crate::instructions::Instructions,
        permissions: Arc<PermissionManager>,
        history: Vec<history::Message>,
        provider_name: String,
        models: Vec<CatalogModel>,
        model: String,
        context_length: Option<u32>,
    ) -> Session {
        let (cancel, _) = run::cancel_channel();
        let dedup = run::shared_cache();
        Session {
            workspace,
            instructions,
            history,
            provider_name,
            models,
            model,
            context_length,
            compaction: Arc::new(std::sync::Mutex::new(
                crate::compaction::CompactionState::default().with_dedup(dedup.clone()),
            )),
            dedup,
            permissions,
            cancel,
            turn: None,
        }
    }
}

/// Serve the Craft agent loop over ACP on stdin/stdout until the client disconnects.
pub async fn serve(config: Config) -> std::result::Result<(), Error> {
    let state = Arc::new(AppState::new(config));

    let init_state = state.clone();
    let new_state = state.clone();
    let load_state = state.clone();
    let set_state = state.clone();
    let prompt_state = state.clone();
    let close_state = state.clone();
    let cancel_state = state.clone();

    AcpRole
        .builder()
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
                            .prompt_capabilities(PromptCapabilities::new()),
                    )
                    .agent_info(Implementation::new("craft-acp", env!("CARGO_PKG_VERSION")));
                responder.respond(response)
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest,
                        responder: Responder<NewSessionResponse>,
                        _connection: ConnectionTo<AcpClient>| {
                let session = match new_state.open_session(&request.cwd).await {
                    Ok(session) => session,
                    Err(message) => return respond_setup_error(responder, message),
                };
                // `mcp_servers` is ignored until the MCP client (task 94)
                // is ported.
                let options = session.config_options(&new_state.provider_names());
                let id = format!(
                    "{}-{}",
                    std::process::id(),
                    new_state.next_session.fetch_add(1, Ordering::Relaxed),
                );
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
                        _connection: ConnectionTo<AcpClient>| {
                // The loaded session keeps the stored id as its ACP id.
                let stored_id = request.session_id.0.to_string();
                let session = match load_state.load_session(&request.cwd, &stored_id).await {
                    Ok(session) => session,
                    Err(message) => return respond_setup_error(responder, message),
                };
                let options = session.config_options(&load_state.provider_names());
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
                    return respond_setup_error(responder, "expected a provider/model id".into());
                };
                let mut sessions = set_state.sessions.lock().await;
                let Some(session) = sessions.get_mut(request.session_id.0.as_ref()) else {
                    drop(sessions);
                    return respond_setup_error(responder, "unknown session".into());
                };
                let outcome = match request.config_id.0.as_ref() {
                    PROVIDER_OPTION_ID => {
                        if !set_state.config.providers.contains_key(&value) {
                            Err(format!("unknown provider {value:?}"))
                        } else {
                            match set_state.provider_catalog(&value).await {
                                Ok((_provider, models)) => {
                                    let resolved =
                                        crate::model_selection::resolve_or_default(&models, None);
                                    match resolved {
                                        Some(r) => {
                                            session.model = r.model;
                                            session.context_length = r.context_length;
                                        }
                                        None => {
                                            session.model = String::new();
                                            session.context_length = None;
                                        }
                                    }
                                    session.models = models;
                                    session.provider_name = value;
                                    Ok(())
                                }
                                Err(message) => Err(message),
                            }
                        }
                    }
                    MODEL_OPTION_ID => {
                        if let Some(model) = session.models.iter().find(|model| model.id == value) {
                            session.context_length = model.context_length;
                            session.model = value;
                            Ok(())
                        } else {
                            Err(format!("unknown model {value:?}"))
                        }
                    }
                    other => Err(format!("unknown configuration option {other:?}")),
                };
                if let Err(message) = outcome {
                    drop(sessions);
                    return respond_setup_error(responder, message);
                }
                let options = session.config_options(&set_state.provider_names());
                let notification = SessionNotification::new(
                    request.session_id.clone(),
                    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(options.clone())),
                );
                drop(sessions);
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
                let (workspace, history, provider_name, model, permissions, cancel_rx, turn_id) = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    let Some(session) = sessions.get_mut(request.session_id.0.as_ref()) else {
                        drop(sessions);
                        return respond_setup_error(responder, "unknown session".into());
                    };
                    // Fail closed: a second prompt while one is in flight is
                    // rejected rather than queued, so the turns can never
                    // clobber the shared history buffer or re-arm each
                    // other's cancellation flag.
                    let turn_id = prompt_state.next_turn.fetch_add(1, Ordering::Relaxed);
                    if let Err(message) = session.begin_turn(turn_id) {
                        drop(sessions);
                        return responder.respond_with_error(invalid_request(message));
                    }
                    // Re-arm the session-wide cancellation flag for this turn.
                    session.cancel.set(false);
                    (
                        session.workspace.clone(),
                        session.history.clone(),
                        session.provider_name.clone(),
                        session.model.clone(),
                        session.permissions.clone(),
                        session.cancel.token(),
                        turn_id,
                    )
                };
                let client_caps = prompt_state
                    .client_caps
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_default();
                let run_state = prompt_state.clone();
                let session_id = request.session_id.clone();
                tokio::spawn(async move {
                    run_turn(
                        run_state,
                        connection,
                        session_id,
                        text,
                        history,
                        workspace,
                        provider_name,
                        model,
                        permissions,
                        client_caps,
                        cancel_rx,
                        turn_id,
                        responder,
                    )
                    .await;
                });
                Ok(())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CloseSessionRequest,
                        responder: Responder<CloseSessionResponse>,
                        _connection: ConnectionTo<AcpClient>| {
                close_state
                    .sessions
                    .lock()
                    .await
                    .remove(request.session_id.0.as_ref());
                responder.respond(CloseSessionResponse::new())
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                let sessions = cancel_state.sessions.lock().await;
                if let Some(session) = sessions.get(notification.session_id.0.as_ref()) {
                    session.cancel.set(true);
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

/// Commit a finished turn's history only while `turn_id` is still the
/// session's in-flight turn, then release the slot. A stale (superseded or
/// canceled) turn commits nothing, so it cannot overwrite history committed
/// by a newer accepted turn.
async fn commit_turn(
    sessions: &Mutex<BTreeMap<String, Session>>,
    session_id: &SessionId,
    turn_id: u64,
    history: Vec<history::Message>,
) {
    let mut sessions = sessions.lock().await;
    if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
        if session.turn == Some(turn_id) {
            session.history = history;
            session.turn = None;
        }
    }
}

/// Release the in-flight slot for `turn_id` without touching history
/// (cancelled/failed turns leave session history untouched).
async fn clear_turn(
    sessions: &Mutex<BTreeMap<String, Session>>,
    session_id: &SessionId,
    turn_id: u64,
) {
    let mut sessions = sessions.lock().await;
    if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
        if session.turn == Some(turn_id) {
            session.turn = None;
        }
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
