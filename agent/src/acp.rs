//! Craft's ACP agent boundary: the shared run loop exposed over
//! agent-client-protocol v1.
//!
//! Every capability of the run loop (`run::run` with read/grep/edit/delete
//! tools, streaming output, per-turn usage, caller-owned history) is surfaced
//! here: provider and model selection travel as session config options, model
//! output streams as `session/update` notifications, and `session/cancel`
//! stops the in-flight turn. The loop itself is unchanged; this module only
//! translates.
//!
//! Deliberately not advertised: `session/load` (the loop keeps history in
//! memory only), embedded context, image, and audio prompts, and MCP servers.

use agent_client_protocol::{
    Agent as AcpRole, Client as AcpClient, ConnectionTo, Error, JsonRpcResponse, Responder, Stdio,
    on_receive_notification, on_receive_request,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
            ConfigOptionUpdate, Content, ContentBlock, ContentChunk, Implementation,
            InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
            PromptCapabilities, PromptRequest, PromptResponse as AcpPromptResponse,
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
            SessionConfigValueId, SessionId, SessionNotification, SessionUpdate,
            SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
            ToolCall as AcpToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate,
            ToolCallUpdateFields, ToolKind, UsageUpdate,
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
    providers::{CatalogModel, Provider, ProviderKind},
    run::{self, RunOutcome},
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
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config,
            sessions: Mutex::new(BTreeMap::new()),
            next_session: AtomicU64::new(1),
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
        let instructions = tokio::task::spawn_blocking({
            let cwd = cwd.display().to_string();
            move || crate::instructions::load_instructions(&cwd)
        })
        .await
        .map_err(|e| e.to_string())?;
        let workspace = Workspace::new(cwd)
            .map_err(|e| e.to_string())?
            .with_loaded_instructions(instructions.loaded.clone());
        let provider_name = self
            .config
            .providers
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| {
                "no providers are configured in ~/.config/craft/agent.toml".to_string()
            })?;
        let (_provider, models) = self.provider_catalog(&provider_name).await?;
        let model = models
            .first()
            .map(|model| model.id.clone())
            .unwrap_or_default();
        let context_length = models.first().and_then(|model| model.context_length);
        let (cancel, _) = run::cancel_channel();
        let dedup = run::shared_cache();
        Ok(Session {
            workspace,
            instructions,
            history: Vec::new(),
            provider_name,
            models,
            model,
            context_length,
            compaction: std::sync::Arc::new(std::sync::Mutex::new(
                crate::compaction::CompactionState::default().with_dedup(dedup.clone()),
            )),
            dedup,
            cancel,
        })
    }
}

/// Serve the Craft agent loop over ACP on stdin/stdout until the client disconnects.
pub async fn serve(config: Config) -> std::result::Result<(), Error> {
    let state = Arc::new(AppState::new(config));

    let init_state = state.clone();
    let new_state = state.clone();
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
                let version = if request.protocol_version == ProtocolVersion::V1 {
                    ProtocolVersion::V1
                } else {
                    ProtocolVersion::LATEST
                };
                let response = InitializeResponse::new(version)
                    .agent_capabilities(
                        AgentCapabilities::new()
                            .load_session(false)
                            .prompt_capabilities(PromptCapabilities::new()),
                    )
                    .agent_info(Implementation::new("craft-acp", env!("CARGO_PKG_VERSION")));
                let _ = init_state;
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
                let (workspace, history, provider_name, model, cancel_rx) = {
                    let mut sessions = prompt_state.sessions.lock().await;
                    let Some(session) = sessions.get_mut(request.session_id.0.as_ref()) else {
                        drop(sessions);
                        return respond_setup_error(responder, "unknown session".into());
                    };
                    // Re-arm the session-wide cancellation flag for this turn.
                    let _ = session.cancel.set(false);
                    (
                        session.workspace.clone(),
                        session.history.clone(),
                        session.provider_name.clone(),
                        session.model.clone(),
                        session.cancel.token(),
                    )
                };
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
                    let _ = session.cancel.set(true);
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

    let tools = workspace.register().with_dedup(dedup);
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
        preamble: Some(crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", cwd)
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &format!("{}{}", state.config.agent.preamble, instructions_text),
            &crate::prompt::ResolvedSlots::default(),
        )),
        temperature: state.config.agent.temperature,
        max_tokens: state.config.agent.max_tokens,
        max_turns: run::RunParams::UNBOUNDED,
        recency: None,
        compression: state.config.compression.clone(),
        max_continuation_turns: run::RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
        compaction: Some(compaction_ctx),
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
                | run::Event::Error(_)
                | run::Event::Retry { .. }
                | run::Event::AutoCompacting { .. }
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
            ("edit".into(), "edited file.rs".into()),
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
}
