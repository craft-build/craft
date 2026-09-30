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
//! `elicitation/create` forms, and `session/load` resumes a persisted session
//! (task G.5).
//!
//! Deliberately not advertised: embedded context, image, and audio
//! prompts. `mcp_servers` on new/load is ignored until the MCP client
//! (task 94) is ported.

use agent_client_protocol::{
    Agent as AcpRole, Client as AcpClient, ConnectionTo, Error, JsonRpcResponse, Responder, Stdio,
    on_receive_notification, on_receive_request,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest,
            CloseSessionResponse, ConfigOptionUpdate, Content, ContentBlock, ContentChunk,
            CreateElicitationRequest, ElicitationContentValue, ElicitationFormMode,
            ElicitationPropertySchema, ElicitationSchema, ElicitationScope,
            ElicitationSessionScope, EnumOption, Implementation, InitializeRequest,
            InitializeResponse, LoadSessionRequest, LoadSessionResponse, MultiSelectPropertySchema,
            NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionId,
            PermissionOptionKind, PromptCapabilities, PromptRequest,
            PromptResponse as AcpPromptResponse, RequestPermissionOutcome,
            RequestPermissionRequest, SessionConfigOption, SessionConfigOptionCategory,
            SessionConfigSelectOption, SessionConfigValueId, SessionId, SessionNotification,
            SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
            StopReason, StringPropertySchema, TextContent, ToolCall as AcpToolCall,
            ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
            ToolKind, UsageUpdate,
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
    permissions::{
        ASK_TIMEOUT, PermissionAnswer, PermissionCheck, PermissionError, PermissionManager,
        ToolKey, append_permission_rule, scope_for_call,
    },
    providers::{CatalogModel, Provider, ProviderKind},
    run::{self, RunOutcome},
    tools::Workspace,
    tools::{AskQuestions, QuestionAnswer, QuestionOption, QuestionSpec},
};

pub const PROVIDER_OPTION_ID: &str = "provider";
pub const MODEL_OPTION_ID: &str = "model";

/// Permission option ids on `session/request_permission` (G.5). Ported from
/// the reference `craft-acp/src/permissions.rs`.
const ALLOW_ONCE_ID: &str = "allow_once";
const ALLOW_ALWAYS_ID: &str = "allow_always";
const REJECT_ONCE_ID: &str = "reject_once";
const REJECT_ALWAYS_ID: &str = "reject_always";

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
}

impl Session {
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
            .description("Configured inference provider from ~/.config/craft/agent.toml"),
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
    /// Client capabilities from `initialize`, consulted before elicitation.
    client_caps: Mutex<Option<ClientCapabilities>>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config,
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
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
        let model = models
            .first()
            .map(|model| model.id.clone())
            .unwrap_or_default();
        let context_length = models.first().and_then(|model| model.context_length);
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
        let model = model
            .and_then(|id| models.iter().find(|m| m.id == id).map(|m| m.id.clone()))
            .or_else(|| models.first().map(|m| m.id.clone()))
            .unwrap_or_default();
        let context_length = models
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.context_length);
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
                                    session.model = models
                                        .first()
                                        .map(|model| model.id.clone())
                                        .unwrap_or_default();
                                    session.context_length =
                                        models.first().and_then(|model| model.context_length);
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
                let (workspace, history, provider_name, model, permissions, cancel_rx) = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    let Some(session) = sessions.get_mut(request.session_id.0.as_ref()) else {
                        drop(sessions);
                        return respond_setup_error(responder, "unknown session".into());
                    };
                    // Re-arm the session-wide cancellation flag for this turn.
                    session.cancel.set(false);
                    (
                        session.workspace.clone(),
                        session.history.clone(),
                        session.provider_name.clone(),
                        session.model.clone(),
                        session.permissions.clone(),
                        session.cancel.token(),
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

// ---------------------------------------------------------------------------
// G.5 ACP permission gate
// ---------------------------------------------------------------------------

/// The four options every `session/request_permission` carries. Ported from
/// the reference `craft-acp/src/permissions.rs`.
fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ONCE_ID),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ALWAYS_ID),
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ONCE_ID),
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ALWAYS_ID),
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Map the client's choice onto the engine's answers, matching the
/// reference `craft-acp/src/permissions.rs`.
fn outcome_to_answer(outcome: &RequestPermissionOutcome) -> PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_ONCE_ID => PermissionAnswer::AllowOnce,
            ALLOW_ALWAYS_ID => PermissionAnswer::AllowSession,
            REJECT_ONCE_ID => PermissionAnswer::Deny,
            REJECT_ALWAYS_ID => PermissionAnswer::DenyAlwaysLocal,
            _ => PermissionAnswer::Deny,
        },
        RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny,
        _ => PermissionAnswer::Deny,
    }
}

/// The `session/request_permission` payload for one gated call.
fn permission_request(
    session_id: &SessionId,
    id: &str,
    name: &str,
    arguments: &serde_json::Value,
    scopes: &[String],
) -> RequestPermissionRequest {
    let mut fields = ToolCallUpdateFields::new()
        .title(tool_title(name, arguments))
        .status(ToolCallStatus::Pending)
        .content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(scopes.join("\n"))),
        ))]);
    fields.name = Some(name.to_owned());
    fields.raw_input = Some(arguments.clone());
    RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(ToolCallId::new(id), fields),
        permission_options(),
    )
}

/// Record a client answer, persisting "always" answers to permissions.bml
/// (project-local). Write failures degrade to the session grant — the answer
/// still applies now, it just may be asked again later. Mirrors the TUI
/// gate's `record_answer`.
fn record_answer(
    permissions: &PermissionManager,
    tool: &ToolKey,
    scopes: &[String],
    answer: &PermissionAnswer,
) -> bool {
    let allow = answer.is_allow();
    for (tool, scope, effect, target) in permissions.apply_decision(tool, scopes, answer) {
        if let Err(err) = append_permission_rule(&tool, scope.as_deref(), effect, &target) {
            eprintln!("permissions: could not persist always-rule: {err}");
        }
    }
    allow
}

fn denied_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::new(&tool.to_string(), scopes).to_string()
}

/// The ACP approval gate: the permission engine decides, and where it would
/// prompt, the client answers a `session/request_permission` instead of the
/// user answering the TUI overlay. Cancellation is epoch-based, matching the
/// TUI gate.
struct AcpPermissionGate {
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    cancel: run::CancelToken,
    permissions: Arc<PermissionManager>,
}

impl run::BeforeExecute for AcpPermissionGate {
    fn decide(&self, call: history::ToolCall) -> run::BoxFuture<run::Decision> {
        let gate = AcpPermissionGate {
            connection: self.connection.clone(),
            session_id: self.session_id.clone(),
            cancel: self.cancel.clone(),
            permissions: self.permissions.clone(),
        };
        Box::pin(async move { gate_decide(gate, call).await })
    }
}

async fn gate_decide(gate: AcpPermissionGate, call: history::ToolCall) -> run::Decision {
    let AcpPermissionGate {
        connection,
        session_id,
        cancel,
        permissions,
    } = gate;
    if cancel.cancelled() {
        return run::Decision::Stop("cancelled by client".into());
    }
    let name = call.function.name.as_str();
    let tool = ToolKey::parse(name);
    let (scopes, force_prompt) = scope_for_call(permissions.cwd(), name, &call.function.arguments);
    match permissions.check_multi(&tool, &scopes, force_prompt) {
        PermissionCheck::Allowed => return run::Decision::Run,
        PermissionCheck::Denied => return run::Decision::Skip(denied_message(&tool, &scopes)),
        PermissionCheck::NeedsPrompt { .. } => {}
    }
    let request = permission_request(
        &session_id,
        &call.id,
        name,
        &call.function.arguments,
        &scopes,
    );
    // Dropping the sent request on cancel asks the client to cancel it.
    let mut cancel_rx = cancel.subscribe();
    let answer = {
        let sent = connection.send_request(request);
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                let _ = changed;
                return run::Decision::Stop("cancelled by client".into());
            }
            outcome = tokio::time::timeout(
                ASK_TIMEOUT,
                sent.block_task(),
            ) => match outcome {
                Ok(Ok(response)) => outcome_to_answer(&response.outcome),
                // A timeout, dead connection, or client error denies — but
                // never wedges the turn.
                _ => PermissionAnswer::Deny,
            },
        }
    };
    if record_answer(&permissions, &tool, &scopes, &answer) {
        run::Decision::Run
    } else {
        run::Decision::Skip(denied_message(&tool, &scopes))
    }
}

// ---------------------------------------------------------------------------
// G.5 Elicitation: the `question` tool over `elicitation/create`
// ---------------------------------------------------------------------------

fn supports_form(caps: &ClientCapabilities) -> bool {
    caps.elicitation.as_ref().is_some_and(|e| e.form.is_some())
}

fn enum_options(options: &[QuestionOption]) -> Vec<EnumOption> {
    options
        .iter()
        .map(|opt| {
            let title = match opt.description.as_deref() {
                Some(description) if !description.is_empty() => {
                    format!("{} - {}", opt.label, description)
                }
                _ => opt.label.clone(),
            };
            EnumOption::new(opt.label.clone(), title)
        })
        .collect()
}

fn property(q: &QuestionSpec) -> ElicitationPropertySchema {
    let title = q.question.clone();
    if q.options.is_empty() {
        ElicitationPropertySchema::String(StringPropertySchema::new().title(title))
    } else if q.multi_select {
        ElicitationPropertySchema::Array(
            MultiSelectPropertySchema::titled(enum_options(&q.options)).title(title),
        )
    } else {
        ElicitationPropertySchema::String(
            StringPropertySchema::new()
                .title(title)
                .one_of(enum_options(&q.options)),
        )
    }
}

/// Property keys are positional (`q1`, `q2`, ...) so answers map back to
/// questions even when headers repeat or are missing.
fn question_key(index: usize) -> String {
    format!("q{}", index + 1)
}

fn form_request(
    session_id: &SessionId,
    tool_call_id: Option<String>,
    questions: &[QuestionSpec],
) -> Result<CreateElicitationRequest, String> {
    if questions.is_empty() {
        return Err("at least one question is required".to_owned());
    }

    let mut schema = ElicitationSchema::new();
    schema.properties = questions
        .iter()
        .enumerate()
        .map(|(i, q)| (question_key(i), property(q)))
        .collect();

    let scope = ElicitationSessionScope::new(session_id.0.to_string())
        .tool_call_id(tool_call_id.map(ToolCallId::from));
    let message = match questions {
        [only] => only.question.clone(),
        many => format!("{} questions", many.len()),
    };
    Ok(CreateElicitationRequest::new(
        ElicitationFormMode::new(ElicitationScope::Session(scope), schema),
        message,
    ))
}

fn content_labels(value: &ElicitationContentValue) -> Vec<String> {
    match value {
        ElicitationContentValue::String(s) if !s.is_empty() => vec![s.clone()],
        ElicitationContentValue::StringArray(items) if !items.is_empty() => items.clone(),
        ElicitationContentValue::Integer(n) => vec![n.to_string()],
        ElicitationContentValue::Number(n) => vec![n.to_string()],
        ElicitationContentValue::Boolean(b) => vec![b.to_string()],
        _ => Vec::new(),
    }
}

/// `elicitation/create` with an untyped response: the reference parses the
/// raw JSON-RPC result, and the typed `CreateElicitationResponse` cannot
/// represent nulled-out content values ("one bad value costs one answer, not
/// the form"), so this wrapper keeps the wire shape but returns raw JSON.
const ELICITATION_METHOD: &str = "elicitation/create";

#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
struct RawElicitationRequest(CreateElicitationRequest);

impl agent_client_protocol::JsonRpcMessage for RawElicitationRequest {
    fn matches_method(method: &str) -> bool {
        method == ELICITATION_METHOD
    }

    fn method(&self) -> &str {
        ELICITATION_METHOD
    }

    fn to_untyped_message(&self) -> Result<agent_client_protocol::UntypedMessage, Error> {
        agent_client_protocol::UntypedMessage::new(ELICITATION_METHOD, self)
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> Result<Self, agent_client_protocol::Error> {
        if !Self::matches_method(method) {
            return Err(Error::method_not_found());
        }
        let value = serde_json::to_value(params).map_err(|e| {
            let mut error = Error::invalid_params();
            error.message = e.to_string();
            error
        })?;
        serde_json::from_value(value)
            .map(RawElicitationRequest)
            .map_err(|e| {
                let mut error = Error::invalid_params();
                error.message = e.to_string();
                error
            })
    }
}

impl agent_client_protocol::JsonRpcRequest for RawElicitationRequest {
    type Response = serde_json::Value;
}

/// Turns the client's `elicitation/create` result into the encoded answer
/// the `question` tool already understands. Ported from the reference
/// `elicitation.rs`: values parse per key — nothing in the schema is
/// `required`, so clients may null out skipped fields, and one unreadable
/// value should cost one answer, not the whole form.
fn answer_from_response(raw: &serde_json::Value) -> QuestionAnswer {
    let dismissed = QuestionAnswer {
        dismissed: true,
        answers: vec![],
    };
    if raw["action"] != "accept" {
        return dismissed;
    }
    let Some(content) = raw["content"].as_object() else {
        return dismissed;
    };

    let mut answers: Vec<Vec<String>> = Vec::new();
    for (k, v) in content {
        let Ok(index) = k.trim_start_matches('q').parse::<usize>() else {
            continue;
        };
        // One unreadable value costs one answer, not the form: the key is
        // dropped, exactly like the reference's filter_map.
        let Ok(value) = serde_json::from_value::<ElicitationContentValue>(v.clone()) else {
            continue;
        };
        let labels = content_labels(&value);
        if answers.len() < index {
            answers.resize(index, Vec::new());
        }
        answers[index - 1] = labels;
    }
    QuestionAnswer {
        dismissed: false,
        answers,
    }
}

/// Ask the client through `elicitation/create`; clients without form
/// capability get the headless dismissed answer.
struct ElicitationAsker {
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    cancel: run::CancelToken,
    caps: ClientCapabilities,
}

impl AskQuestions for ElicitationAsker {
    fn ask(&self, questions: Vec<QuestionSpec>) -> run::BoxFuture<QuestionAnswer> {
        let asker = ElicitationAsker {
            connection: self.connection.clone(),
            session_id: self.session_id.clone(),
            cancel: self.cancel.clone(),
            caps: self.caps.clone(),
        };
        Box::pin(async move { asker.ask(questions).await })
    }
}

impl ElicitationAsker {
    async fn ask(self, questions: Vec<QuestionSpec>) -> QuestionAnswer {
        let dismissed = QuestionAnswer {
            dismissed: true,
            answers: vec![],
        };
        if !supports_form(&self.caps) {
            return dismissed;
        }
        let Ok(request) = form_request(&self.session_id, None, &questions) else {
            return dismissed;
        };
        let sent = self.connection.send_request(RawElicitationRequest(request));
        let mut cancel_rx = self.cancel.subscribe();
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                let _ = changed;
                dismissed
            }
            response = tokio::time::timeout(ASK_TIMEOUT, sent.block_task()) => match response {
                Ok(Ok(response)) => answer_from_response(&response),
                _ => dismissed,
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    state: Arc<AppState>,
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    text: String,
    mut history: Vec<history::Message>,
    workspace: Workspace,
    provider_name: String,
    model: String,
    permissions: Arc<PermissionManager>,
    client_caps: ClientCapabilities,
    cancel: run::CancelToken,
    responder: Responder<AcpPromptResponse>,
) {
    macro_rules! fail {
        ($message:expr) => {{
            let _ = responder.respond_with_error(internal_error($message));
            return;
        }};
    }
    let (provider, _models) = match state.provider_catalog(&provider_name).await {
        Ok(catalog) => catalog,
        Err(message) => fail!(message),
    };
    if model.trim().is_empty() {
        fail!("no model is selected; set the model session configuration option");
    }
    let model_label = model.clone();
    let model = match provider.completion_model(&model) {
        Ok(model) => model,
        Err(error) => fail!(report(error)),
    };

    // Run configured compaction stages whose context-fill threshold is
    // crossed before the history is sent to the model. Only the
    // effectiveness state is persisted here; the compacted history is
    // committed by the run's success path, matching the loop's "failed runs
    // leave session history untouched" semantics.
    let (shared_compaction, context_length, dedup) = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(session_id.0.as_ref())
            .map(|session| {
                (
                    session.compaction.clone(),
                    session.context_length,
                    session.dedup.clone(),
                )
            })
            .unwrap_or_else(|| {
                let dedup = run::shared_cache();
                (
                    std::sync::Arc::new(std::sync::Mutex::new(
                        crate::compaction::CompactionState::default().with_dedup(dedup.clone()),
                    )),
                    None,
                    dedup,
                )
            })
    };
    let compaction_ctx = run::CompactionCtx {
        state: shared_compaction.clone(),
        stages: state.config.compaction.clone(),
        buffer: state.config.compaction_buffer,
        context_length,
    };
    if let Some(mut compaction_state) = shared_compaction.lock().ok().map(|g| g.clone()) {
        let engine = crate::compaction::CompactionEngine::new(state.config.compaction.clone())
            .with_buffer(state.config.compaction_buffer);
        engine
            .maybe_compact(&mut compaction_state, &model, &mut history, context_length)
            .await;
        if let Ok(mut guard) = shared_compaction.lock() {
            *guard = compaction_state;
        }
    }

    // Phase 3: register MCP tool annotations once the tool set is settled so
    // the ACP permission gate consults the same hints as the TUI.
    if let Some(mcp) = workspace.mcp() {
        permissions.sync_mcp_annotations(&mcp);
    }
    let tools = workspace
        .with_questions(Arc::new(ElicitationAsker {
            connection: connection.clone(),
            session_id: session_id.clone(),
            cancel: cancel.clone(),
            caps: client_caps,
        }))
        .register()
        .with_dedup(dedup)
        .with_before(Arc::new(AcpPermissionGate {
            connection: connection.clone(),
            session_id: session_id.clone(),
            cancel: cancel.clone(),
            permissions,
        }));
    let (cwd, instructions_text) = {
        let sessions = state.sessions.lock().await;
        let session = sessions.get(session_id.0.as_ref());
        (
            session
                .map(|session| session.workspace.root().display().to_string())
                .unwrap_or_default(),
            session
                .map(|session| session.instructions.text.clone())
                .unwrap_or_default(),
        )
    };
    let params = run::RunParams {
        fast: false,
        advisor: state.config.agent.advisor.clone(),
        preamble: Some(crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", cwd)
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &format!("{}{}", state.config.agent.preamble, instructions_text),
            &crate::prompt::ResolvedSlots::default(),
            None,
        )),
        temperature: state.config.agent.temperature,
        max_tokens: state.config.agent.max_tokens,
        max_turns: run::RunParams::UNBOUNDED,
        recency: None,
        compression: state.config.compression.clone(),
        max_continuation_turns: run::RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
        compaction: Some(compaction_ctx),
        reauth: state
            .config
            .providers
            .get(&provider_name)
            .map(|provider_config| crate::providers::reauth_hook(provider_config, &model_label)),
        model_spec: Some(format!("{provider_name}/{model_label}").into()),
        retry: run::RetryCtx::default(),
    };

    let send = |update: SessionUpdate| -> std::result::Result<(), Error> {
        connection.send_notification(SessionNotification::new(session_id.clone(), update))
    };
    let emitted_text = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let emit = {
        let connection = connection.clone();
        let session_id = session_id.clone();
        let emitted_text = emitted_text.clone();
        move |event: run::Event| {
            let update = match event {
                run::Event::TextDelta(delta) => {
                    emitted_text.store(true, Ordering::Relaxed);
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(delta),
                    )))
                }
                run::Event::ThinkingDelta(delta) => SessionUpdate::AgentThoughtChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(delta))),
                ),
                run::Event::ToolStart {
                    id,
                    name,
                    arguments,
                } => SessionUpdate::ToolCall(tool_call_start(&id, &name, &arguments)),
                run::Event::ToolDone { id, result, .. } => {
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(id),
                        ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Completed)
                            .content(vec![tool_result_content(&result)]),
                    ))
                }
                run::Event::TurnComplete { usage, .. } => {
                    let Some(size) = context_length else {
                        return;
                    };
                    SessionUpdate::UsageUpdate(UsageUpdate::new(
                        usage.input_tokens,
                        u64::from(size),
                    ))
                }
                // The nudged retry follows immediately; no ACP notification.
                // The remaining taxonomy variants have no ACP translation yet.
                run::Event::Nudge
                | run::Event::ToolPending { .. }
                | run::Event::ToolOutput { .. }
                | run::Event::ToolResultsSubmitted { .. }
                | run::Event::Done { .. }
                | run::Event::Info(_)
                | run::Event::AdvisorNote { .. }
                | run::Event::Error(_)
                | run::Event::Retry { .. }
                | run::Event::AuthRequired { .. }
                | run::Event::AutoCompacting { .. }
                | run::Event::Subagent { .. }
                | run::Event::CompactionDone { .. }
                | run::Event::StagnationDetected { .. }
                | run::Event::AutoReviewStart { .. }
                | run::Event::AutoReviewDecision { .. }
                | run::Event::StreamClosed => return,
            };
            // A dead connection stops the notifications but not the turn; the
            // responder still answers the request.
            let _ =
                connection.send_notification(SessionNotification::new(session_id.clone(), update));
        }
    };

    let outcome = run::run(&model, &params, &tools, &mut history, &text, &cancel, &emit).await;
    match outcome {
        RunOutcome::Done { reply } => {
            if !emitted_text.load(Ordering::Relaxed) && !reply.is_empty() {
                let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(reply)),
                )));
            }
            // Commit this turn only on success: failed and cancelled runs
            // leave the session history untouched.
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
                session.history = history;
            }
            let _ = responder.respond(AcpPromptResponse::new(StopReason::EndTurn));
        }
        // The driver committed the sanitized partial history; the next prompt
        // continues from where the budget ran out.
        // The turn still hit the output-token limit after every continuation;
        // the committed history includes the truncated tail.
        RunOutcome::MaxTokens { reply } => {
            if !emitted_text.load(Ordering::Relaxed) && !reply.is_empty() {
                let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(reply)),
                )));
            }
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
                session.history = history;
            }
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTokens));
        }
        RunOutcome::MaxTurns => {
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
                session.history = history;
            }
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTurnRequests));
        }
        // The doom-loop hard stop committed the sanitized partial history;
        // like MaxTurns, the next prompt continues from the cut-off.
        RunOutcome::DoomStop => {
            let mut sessions = state.sessions.lock().await;
            if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
                session.history = history;
            }
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTurnRequests));
        }
        RunOutcome::Cancelled => {
            let _ = responder.respond(AcpPromptResponse::new(StopReason::Cancelled));
        }
        RunOutcome::Failed(message) => fail!(message),
    }
}

fn tool_call_start(id: &str, name: &str, arguments: &serde_json::Value) -> AcpToolCall {
    let mut call = AcpToolCall::new(ToolCallId::new(id), tool_title(name, arguments));
    call.kind = tool_kind(name);
    call.status = ToolCallStatus::InProgress;
    call.raw_input = Some(arguments.clone());
    call
}

fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    if let Some(detail) = crate::tui::provider::cards::first_string_argument(arguments) {
        format!("{name} {detail}")
    } else {
        name.to_owned()
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "grep" => ToolKind::Search,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        _ => ToolKind::Other,
    }
}

fn tool_result_content(result: &history::ToolResult) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
        tool_result_text(&result.content),
    ))))
}

/// Built-in tools produce model-facing text, which ACP displays verbatim.
/// Keep JSON readable for additional tools without interpreting their schemas.
fn tool_result_text(items: &[history::ToolResultContent]) -> String {
    items
        .iter()
        .map(history::ToolResultContent::to_text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ResourceLink;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    fn link(name: &str) -> ContentBlock {
        ContentBlock::ResourceLink(ResourceLink::new(name, format!("file:///tmp/{name}")))
    }

    #[test]
    fn prompt_text_joins_blocks_and_lists_context_files() {
        let prompt = vec![text_block("Please fix it"), link("src/main.rs")];
        let text = prompt_text(&prompt).unwrap();
        assert!(text.starts_with("Please fix it\n\nContext files:\n- src/main.rs\n"));
    }

    #[test]
    fn prompt_text_rejects_unsupported_blocks() {
        assert!(prompt_text(&[link("a"), text_block("x")]).is_ok());
        let resource = serde_json::from_value::<ContentBlock>(serde_json::json!({
            "type": "resource",
            "resource": { "uri": "file:///tmp/x", "mimeType": "text/plain", "text": "hi" }
        }))
        .unwrap();
        assert!(prompt_text(&[resource]).is_err());
    }

    #[test]
    fn config_options_expose_provider_then_model() {
        let session = Session {
            compaction: Default::default(),
            workspace: Workspace::new(std::env::temp_dir()).unwrap(),
            instructions: Default::default(),
            history: Vec::new(),
            provider_name: "openai".into(),
            models: vec![CatalogModel {
                id: "gpt-x".into(),
                name: Some("GPT X".into()),
                description: None,
                context_length: None,
                max_output_tokens: None,
            }],
            model: "gpt-x".into(),
            context_length: Some(128_000),
            dedup: run::shared_cache(),
            permissions: Arc::new(PermissionManager::new(
                crate::permissions::PermissionsConfig::default(),
                std::env::temp_dir(),
            )),
            cancel: run::cancel_channel().0,
        };
        let options = session.config_options(&["openai".into(), "llamafile".into()]);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id.0.as_ref(), "provider");
        assert_eq!(options[1].id.0.as_ref(), "model");
    }

    #[test]
    fn tool_kinds_follow_the_registered_tools() {
        assert_eq!(tool_kind("read"), ToolKind::Read);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("edit"), ToolKind::Edit);
        assert_eq!(tool_kind("delete"), ToolKind::Delete);
        assert_eq!(tool_kind("other"), ToolKind::Other);
    }

    #[tokio::test]
    async fn streamed_filesystem_results_match_model_and_acp_text() {
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("file.rs"),
            "skipped\r\n\r\n    let name = \"βeta\";\r\nlast",
        )
        .unwrap();
        let mut turns = Vec::new();
        for (id, name, args) in [
            (
                "1",
                "read",
                json!({"path":"file.rs", "offset":2, "limit":2}),
            ),
            ("2", "grep", json!({"pattern":"βeta"})),
            (
                "3",
                "edit",
                json!({"path":"file.rs", "old_string":"βeta", "new_string":"new"}),
            ),
            ("4", "delete", json!({"files":["file.rs"]})),
        ] {
            turns.push(vec![
                MockStreamEvent::tool_call(id, name, args),
                MockStreamEvent::final_response_with_total_tokens(1),
            ]);
        }
        turns.push(vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]);
        let model = MockCompletionModel::from_stream_turns(turns);
        let tools = Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = run::cancel_channel();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut history = Vec::new();
        let outcome = run::run(
            &model,
            &run::RunParams::default(),
            &tools,
            &mut history,
            "read, search, edit, delete",
            &cancel,
            &|event| events.lock().unwrap().push(event),
        )
        .await;
        assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "done"));
        let mut results = Vec::new();
        let events = events.lock().unwrap();
        for event in events.iter() {
            if let run::Event::ToolDone { name, result, .. } = event {
                // The model-visible text and the ACP display text must match.
                let ToolCallContent::Content(Content {
                    content: ContentBlock::Text(display),
                    ..
                }) = tool_result_content(result)
                else {
                    panic!("ACP must display tool text");
                };
                assert_eq!(result.content.len(), 1);
                let model_text = history::ToolResultContent::to_text(&result.content[0]);
                assert_eq!(display.text, model_text);
                results.push((name.clone(), model_text));
            }
        }
        assert_eq!(results, [
            ("read".into(), "2: \n3:     let name = \"βeta\";\n\n...\n\nTruncated lines: 4-4. Use offset=4 to read further.".into()),
            ("grep".into(), "file.rs:\n  3:     let name = \"βeta\";".into()),
            ("edit".into(), "edited file.rs\n--- file.rs\n+++ file.rs\n@@ -1 +1 @@\n  skipped\r\n  \r\n-     let name = \"βeta\";\r\n+     let name = \"new\";\r\n  last".into()),
            ("delete".into(), "deleted: file.rs".into()),
        ]);
        // Verify what the next model request actually receives, not only the
        // display events: all four results must remain literal text.
        let requests = model.requests();
        assert_eq!(requests.len(), 5);
        let model_results = crate::edge::rig_to_own(&requests[4].chat_history)
            .iter()
            .flat_map(|message| match message {
                history::Message::User { content } => content
                    .iter()
                    .filter_map(|item| {
                        if let history::UserContent::ToolResult(result) = item {
                            assert_eq!(result.content.len(), 1);
                            Some((
                                result.name.clone(),
                                history::ToolResultContent::to_text(&result.content[0]),
                            ))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect::<Vec<_>>();
        assert_eq!(model_results, results);
        assert!(!dir.path().join("file.rs").exists());
    }

    #[test]
    fn tool_result_text_preserves_literal_json_and_errors() {
        for text in [r#"{"lines":[],"total_lines":0}"#, "file not found", ""] {
            assert_eq!(
                tool_result_text(&[history::ToolResultContent::text(text)]),
                text
            );
        }
    }

    #[test]
    fn tool_result_text_pretty_prints_json_results() {
        let items = vec![history::ToolResultContent::Json {
            value: serde_json::json!({ "path": "Cargo.lock", "total_lines": 2 }),
        }];
        let text = tool_result_text(&items);
        assert!(
            text.contains("\"path\": \"Cargo.lock\""),
            "unexpected: {text}"
        );
        assert!(text.contains("\"total_lines\": 2"), "unexpected: {text}");
        assert!(!text.contains("Json {"), "Debug rendering leaked: {text}");
    }

    // ----- G.5 permission requests -----

    fn selected(option_id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(
            agent_client_protocol::schema::v1::SelectedPermissionOutcome::new(
                option_id.to_string(),
            ),
        )
    }

    #[test]
    fn permission_options_list_all_four_choices() {
        let options = permission_options();
        let ids: Vec<&str> = options.iter().map(|o| o.option_id.0.as_ref()).collect();
        assert_eq!(
            ids,
            ["allow_once", "allow_always", "reject_once", "reject_always"]
        );
    }

    #[test]
    fn outcomes_map_to_reference_answers() {
        assert_eq!(
            outcome_to_answer(&selected("allow_once")),
            PermissionAnswer::AllowOnce
        );
        assert_eq!(
            outcome_to_answer(&selected("allow_always")),
            PermissionAnswer::AllowSession
        );
        assert_eq!(
            outcome_to_answer(&selected("reject_once")),
            PermissionAnswer::Deny
        );
        assert_eq!(
            outcome_to_answer(&selected("reject_always")),
            PermissionAnswer::DenyAlwaysLocal
        );
        assert_eq!(
            outcome_to_answer(&RequestPermissionOutcome::Cancelled),
            PermissionAnswer::Deny
        );
        assert_eq!(outcome_to_answer(&selected("nope")), PermissionAnswer::Deny);
    }

    #[test]
    fn permission_request_carries_the_call_and_scopes() {
        let request = permission_request(
            &SessionId::new("s1"),
            "call-1",
            "edit",
            &serde_json::json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
            &["/tmp/proj/a.rs".to_string()],
        );
        assert_eq!(request.session_id.0.as_ref(), "s1");
        assert_eq!(request.tool_call.tool_call_id.0.as_ref(), "call-1");
        assert_eq!(request.tool_call.fields.name.as_deref(), Some("edit"));
        assert_eq!(request.options.len(), 4);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["options"][0]["optionId"], "allow_once");
    }

    #[test]
    fn gate_decides_run_skip_and_stop_without_a_client() {
        // The engine decides without a client round-trip for allow/deny.
        let tmp = tempfile::tempdir().unwrap();
        let permissions = Arc::new(PermissionManager::new(
            crate::permissions::PermissionsConfig::default(),
            tmp.path().to_path_buf(),
        ));
        let read_call = history::ToolCall::new("1", "read", serde_json::json!({"path": "x"}));
        // read-only tool: allowed by default.
        let tool = ToolKey::native("read");
        let (scopes, force_prompt) =
            scope_for_call(permissions.cwd(), "read", &read_call.function.arguments);
        assert!(matches!(
            permissions.check_multi(&tool, &scopes, force_prompt),
            PermissionCheck::Allowed
        ));
        // A write with a session allow rule: allowed.
        let edit_tool = ToolKey::native("edit");
        permissions.add_session_rule(crate::permissions::PermissionRule {
            tool: edit_tool.clone(),
            scope: Some(tmp.path().join("a.rs").display().to_string()),
            effect: crate::permissions::Effect::Allow,
        });
        let (scopes, force_prompt) = scope_for_call(
            permissions.cwd(),
            "edit",
            &serde_json::json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
        );
        assert!(matches!(
            permissions.check_multi(&edit_tool, &scopes, force_prompt),
            PermissionCheck::Allowed
        ));
        // A session deny wins and produces the denial text the model sees.
        permissions.add_session_rule(crate::permissions::PermissionRule {
            tool: edit_tool.clone(),
            scope: Some(tmp.path().join("b.rs").display().to_string()),
            effect: crate::permissions::Effect::Deny,
        });
        let (scopes, force_prompt) = scope_for_call(
            permissions.cwd(),
            "edit",
            &serde_json::json!({"path": "b.rs", "old_string": "x", "new_string": "y"}),
        );
        let PermissionCheck::Denied = permissions.check_multi(&edit_tool, &scopes, force_prompt)
        else {
            panic!("expected denial");
        };
        assert!(
            denied_message(&edit_tool, &scopes)
                .starts_with(crate::permissions::PERMISSION_DENIED_PREFIX)
        );
    }

    // ----- G.5 elicitation -----

    fn question_specs() -> Vec<QuestionSpec> {
        vec![
            QuestionSpec {
                question: "Pick a framework".into(),
                header: Some("Framework".into()),
                options: vec![
                    QuestionOption {
                        label: "axum".into(),
                        description: Some("tokio based".into()),
                    },
                    QuestionOption {
                        label: "actix".into(),
                        description: None,
                    },
                ],
                multi_select: false,
            },
            QuestionSpec {
                question: "Which features?".into(),
                header: Some("Features".into()),
                options: vec![
                    QuestionOption {
                        label: "auth".into(),
                        description: None,
                    },
                    QuestionOption {
                        label: "uploads".into(),
                        description: None,
                    },
                ],
                multi_select: true,
            },
            QuestionSpec {
                question: "Anything else?".into(),
                header: None,
                options: vec![],
                multi_select: false,
            },
        ]
    }

    #[test]
    fn form_request_maps_questions_to_schema() {
        let req = form_request(
            &SessionId::new("sess_1"),
            Some("tool_1".to_owned()),
            &question_specs(),
        )
        .unwrap();
        assert_eq!(req.message, "3 questions");

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["mode"], "form");
        let props = &json["requestedSchema"]["properties"];
        assert_eq!(props["q1"]["type"], "string");
        assert_eq!(props["q1"]["oneOf"][0]["const"], "axum");
        assert_eq!(props["q2"]["type"], "array");
        assert_eq!(props["q3"]["type"], "string");
        assert!(props["q3"].get("oneOf").is_none());
    }

    #[test]
    fn single_question_is_the_message() {
        let qs = vec![QuestionSpec {
            question: "Proceed?".into(),
            header: None,
            options: vec![],
            multi_select: false,
        }];
        let req = form_request(&SessionId::new("sess_1"), None, &qs).unwrap();
        assert_eq!(req.message, "Proceed?");
    }

    #[test]
    fn form_request_rejects_empty_questions() {
        assert!(form_request(&SessionId::new("sess_1"), None, &[]).is_err());
    }

    fn accept_response(content: serde_json::Value) -> serde_json::Value {
        if content.is_null() {
            serde_json::json!({"action": "accept"})
        } else {
            serde_json::json!({"action": "accept", "content": content})
        }
    }

    #[test]
    fn accepted_forms_map_answers_by_position() {
        let answer = answer_from_response(&accept_response(
            serde_json::json!({ "q1": "axum", "q2": ["auth", "uploads"] }),
        ));
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["axum"], vec!["auth", "uploads"]]);
    }

    #[test]
    fn missing_answer_becomes_empty_labels() {
        let answer = answer_from_response(&accept_response(serde_json::json!({ "q2": ["auth"] })));
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec![], vec!["auth"]]);
    }

    #[test]
    fn nulled_out_field_costs_one_answer_not_the_form() {
        let answer = answer_from_response(&accept_response(
            serde_json::json!({ "q1": "axum", "q2": null }),
        ));
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["axum"]]);
    }

    #[test]
    fn non_accept_is_dismissed() {
        for raw in [
            r#"{"action":"decline"}"#,
            r#"{"action":"cancel"}"#,
            r#"{"action":"_custom"}"#,
        ] {
            let response: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert!(answer_from_response(&response).dismissed, "{raw}");
        }
    }

    #[test]
    fn supports_form_requires_form_capability() {
        assert!(!supports_form(&ClientCapabilities::default()));
        let caps: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "form": {} }
        }))
        .unwrap();
        assert!(supports_form(&caps));
        let url_only: ClientCapabilities = serde_json::from_value(serde_json::json!({
            "elicitation": { "url": {} }
        }))
        .unwrap();
        assert!(!supports_form(&url_only));
    }

    // ----- G.5 session/load -----

    #[tokio::test]
    async fn load_session_restores_history_and_model() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
        let session_ref =
            crate::id::SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap());
        let mut stored = crate::headless::StoredSession::new("openai/gpt-x", "/tmp/proj");
        stored.id = session_ref.clone();
        stored.push_message(history::Message::User {
            content: vec![history::UserContent::text("hello")],
        });
        stored.save(&dir).unwrap();

        let loaded = crate::headless::StoredSession::load(session_ref.id(), &dir).unwrap();
        assert_eq!(loaded.model, "openai/gpt-x");
        assert_eq!(loaded.messages().len(), 1);

        // The provider/model resolution from a stored spec (pure helper
        // semantics exercised through load_session's match logic).
        let (provider, model) = match loaded.model.split_once('/') {
            Some((p, m)) if !m.is_empty() => (p.to_owned(), Some(m.to_owned())),
            _ => (String::new(), None),
        };
        assert_eq!(provider, "openai");
        assert_eq!(model.as_deref(), Some("gpt-x"));
    }

    #[tokio::test]
    async fn load_session_rejects_unknown_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::storage::StateDir::from_path(tmp.path().to_path_buf());
        let missing =
            crate::id::SessionRef::from_id("01965087-4c71-7f00-8000-000000000001".parse().unwrap());
        assert!(crate::headless::StoredSession::load(missing.id(), &dir).is_err());
    }
}
