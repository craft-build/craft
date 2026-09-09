//! Craft's ACP agent boundary: the CLI loop exposed over agent-client-protocol v1.
//!
//! Every capability of the base loop (`agent::build` with read/grep/edit/delete
//! tools, streaming output, per-turn usage, caller-owned history) is surfaced
//! here: provider and model selection travel as session config options, model
//! output streams as `session/update` notifications, and `session/cancel`
//! stops the in-flight turn. The loop itself is unchanged; this module only
//! translates.
//!
//! Deliberately not advertised: `session/load` (the loop keeps history in
//! memory only), embedded context, image, and audio prompts, and MCP servers.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

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
use futures::StreamExt;
use rig::agent::{
    AgentHook, HookContext, MultiTurnStreamItem, StreamingError,
    hook::{
        CompletionCall as CompletionCallEvent, CompletionCallAction, CompletionResponse,
        ObservationAction, ToolCall as ToolCallEvent, ToolCallAction,
    },
};
use rig::completion::{Message, PromptError};
use rig::model::Model;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent};
use tokio::sync::{Mutex, watch};

use crate::{
    config::Config,
    providers::{Provider, ProviderKind},
    tools::Workspace,
};

pub const PROVIDER_OPTION_ID: &str = "provider";
pub const MODEL_OPTION_ID: &str = "model";

struct Session {
    workspace: Workspace,
    history: Vec<Message>,
    provider_name: String,
    models: Vec<Model>,
    model: String,
    context_length: Option<u32>,
    cancel: watch::Sender<bool>,
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

fn model_option(models: &[Model], current: &str) -> SessionConfigOption {
    SessionConfigOption::select(
        MODEL_OPTION_ID,
        "Model",
        SessionConfigValueId::new(current),
        models
            .iter()
            .map(|model| {
                let name = model.name.clone().unwrap_or_else(|| model.id.clone());
                let mut option = SessionConfigSelectOption::new(model.id.clone(), name);
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
    ) -> std::result::Result<(Provider, Vec<Model>), String> {
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
        let provider = Provider::from_config(config).map_err(|e| format!("{e:#}"))?;
        let models: Vec<Model> = provider
            .models(config)
            .await
            .map_err(|e| format!("{e:#}"))?
            .into_iter()
            .collect();
        Ok((provider, models))
    }

    async fn open_session(&self, cwd: &Path) -> std::result::Result<Session, String> {
        let workspace = Workspace::new(cwd).map_err(|e| format!("{e:#}"))?;
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
        let (cancel, _) = watch::channel(false);
        Ok(Session {
            workspace,
            history: Vec::new(),
            provider_name,
            models,
            model,
            context_length,
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
                        if session.models.iter().any(|model| model.id == value) {
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
                    let _ = session.cancel.send(false);
                    (
                        session.workspace.clone(),
                        session.history.clone(),
                        session.provider_name.clone(),
                        session.model.clone(),
                        session.cancel.subscribe(),
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
                    let _ = session.cancel.send(true);
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

/// Stops the run at the next hook boundary once the client cancels.
struct CancelHook(watch::Receiver<bool>);

impl CancelHook {
    fn cancelled(&self) -> bool {
        *self.0.borrow()
    }
}

impl AgentHook for CancelHook {
    async fn on_completion_call(
        &self,
        _: &HookContext,
        _: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if self.cancelled() {
            CompletionCallAction::stop("cancelled by client")
        } else {
            CompletionCallAction::Continue
        }
    }

    async fn on_completion_response(
        &self,
        _: &HookContext,
        _: CompletionResponse<'_>,
    ) -> ObservationAction {
        if self.cancelled() {
            ObservationAction::stop("cancelled by client")
        } else {
            ObservationAction::Continue
        }
    }

    async fn on_tool_call(&self, _: &HookContext, _: ToolCallEvent<'_>) -> ToolCallAction {
        if self.cancelled() {
            ToolCallAction::Stop("cancelled by client".into())
        } else {
            ToolCallAction::Run
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn(
    state: Arc<AppState>,
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    text: String,
    history: Vec<Message>,
    workspace: Workspace,
    provider_name: String,
    model: String,
    mut cancel_rx: watch::Receiver<bool>,
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
    let agent = match crate::agent::build(&provider, &model, &state.config.agent, &workspace) {
        Ok(agent) => agent,
        Err(error) => fail!(format!("{error:#}")),
    };

    let send = |update: SessionUpdate| -> std::result::Result<(), Error> {
        connection.send_notification(SessionNotification::new(session_id.clone(), update))
    };

    let mut stream = agent
        .runner(text)
        .history(history.clone())
        .add_hook(CancelHook(cancel_rx.clone()))
        .stream()
        .await;

    let mut emitted_text = false;
    let context_length = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(session_id.0.as_ref())
            .and_then(|session| session.context_length)
    };
    let mut cancelled = false;
    let final_response = loop {
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                if matches!(changed, Ok(())) && *cancel_rx.borrow_and_update() {
                    cancelled = true;
                    break None;
                }
            }
            item = stream.next() => match item {
                None => break None,
                Some(Ok(item)) => match item {
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::Text(delta),
                    ) => {
                        emitted_text = true;
                        if send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new(delta.text)),
                        )))
                        .is_err()
                        {
                            return;
                        }
                    }
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                    ) => {
                        if send(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                            ContentBlock::Text(TextContent::new(reasoning)),
                        )))
                        .is_err()
                        {
                            return;
                        }
                    }
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id,
                        },
                    ) => {
                        if send(SessionUpdate::ToolCall(tool_call_start(
                            &tool_call,
                            &internal_call_id,
                        )))
                        .is_err()
                        {
                            return;
                        }
                    }
                    MultiTurnStreamItem::StreamAssistantItem(_) => {}
                    MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                        tool_result,
                        internal_call_id,
                    }) => {
                        if send(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                            ToolCallId::new(internal_call_id),
                            ToolCallUpdateFields::new()
                                .status(ToolCallStatus::Completed)
                                .content(vec![tool_result_content(&tool_result)]),
                        )))
                        .is_err()
                        {
                            return;
                        }
                    }
                    MultiTurnStreamItem::CompletionCall(call) => {
                        if let Some(size) = context_length {
                            let _ = send(SessionUpdate::UsageUpdate(UsageUpdate::new(
                                call.usage.input_tokens,
                                u64::from(size),
                            )));
                        }
                    }
                    MultiTurnStreamItem::ToolExecutionCommitted { .. } => {}
                    MultiTurnStreamItem::ModelTurnRetried { .. } => {}
                    MultiTurnStreamItem::FinalResponse(response) => break Some(response),
                },
                Some(Err(error)) => {
                    match &error {
                        StreamingError::Prompt(prompt_error)
                            if matches!(prompt_error.as_ref(), PromptError::MaxTurnsError { .. }) =>
                        {
                            let _ = responder
                                .respond(AcpPromptResponse::new(StopReason::MaxTurnRequests));
                        }
                        StreamingError::Prompt(prompt_error)
                            if matches!(
                                prompt_error.as_ref(),
                                PromptError::PromptCancelled { .. }
                            ) =>
                        {
                            let _ =
                                responder.respond(AcpPromptResponse::new(StopReason::Cancelled));
                        }
                        _ => fail!(error.to_string()),
                    }
                    return;
                }
            },
        }
    };

    // Dropping the stream aborts the in-flight provider request; any already
    // running filesystem tool call finishes and its result is discarded. The
    // cancelled turn's history is not committed, matching the loop's semantics.
    drop(stream);
    if cancelled {
        let _ = responder.respond(AcpPromptResponse::new(StopReason::Cancelled));
        return;
    }

    let Some(response) = final_response else {
        fail!("the agent stream ended without a final response");
    };
    if !emitted_text && !response.output.is_empty() {
        let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(response.output.clone())),
        )));
    }
    // Commit this turn only on success: failed and cancelled runs leave the
    // session history untouched, exactly like the base loop.
    if let Some(messages) = response.messages() {
        let mut sessions = state.sessions.lock().await;
        if let Some(session) = sessions.get_mut(session_id.0.as_ref()) {
            session.history = merge_history(history, messages.to_vec());
        }
    }
    let _ = responder.respond(AcpPromptResponse::new(StopReason::EndTurn));
}

/// The run transcript covers only this turn; prepend the caller-owned history.
fn merge_history(input: Vec<Message>, run: Vec<Message>) -> Vec<Message> {
    let mut merged = input;
    merged.extend(run);
    merged
}

fn tool_call_start(tool_call: &rig::core::completion::message::ToolCall, id: &str) -> AcpToolCall {
    let mut call = AcpToolCall::new(ToolCallId::new(id), tool_title(tool_call));
    call.kind = tool_kind(&tool_call.function.name);
    call.status = ToolCallStatus::InProgress;
    call.raw_input = Some(tool_call.function.arguments.clone());
    call
}

fn tool_title(tool_call: &rig::core::completion::message::ToolCall) -> String {
    let name = &tool_call.function.name;
    if let Some(detail) = first_string_argument(&tool_call.function.arguments) {
        format!("{name} {detail}")
    } else {
        name.clone()
    }
}

fn first_string_argument(arguments: &serde_json::Value) -> Option<String> {
    let (_, value) = arguments
        .as_object()?
        .iter()
        .find(|(_, value)| value.is_string())?;
    value.as_str().map(str::to_owned)
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

fn tool_result_content(result: &rig::core::completion::message::ToolResult) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
        tool_result_text(&result.content),
    ))))
}

/// Tool results arrive as Rig content blocks: text passes through verbatim,
/// while JSON payloads (serialized tool outputs such as `ReadOutput`) are
/// pretty-printed so clients show the data itself rather than the enum's
/// Debug rendering (`Json { value: Object {...} }`).
fn tool_result_text(items: &[rig::core::completion::message::ToolResultContent]) -> String {
    items
        .iter()
        .map(|item| match item {
            rig::core::completion::message::ToolResultContent::Text(text) => text.text.clone(),
            rig::core::completion::message::ToolResultContent::Json { value, .. } => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
            other => format!("{other:?}"),
        })
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
            workspace: Workspace::new(std::env::temp_dir()).unwrap(),
            history: Vec::new(),
            provider_name: "openai".into(),
            models: vec![Model::new("gpt-x", "GPT X")],
            model: "gpt-x".into(),
            context_length: Some(128_000),
            cancel: watch::channel(false).0,
        };
        let options = session.config_options(&["openai".into(), "llamafile".into()]);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].id.0.as_ref(), "provider");
        assert_eq!(options[1].id.0.as_ref(), "model");
    }

    #[test]
    fn merge_history_prepends_session_history() {
        let input = vec![Message::user("earlier")];
        let run = vec![Message::user("now"), Message::assistant("reply")];
        let merged = merge_history(input, run);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0], Message::user("earlier"));
    }

    #[test]
    fn tool_kinds_follow_the_registered_tools() {
        assert_eq!(tool_kind("read"), ToolKind::Read);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("edit"), ToolKind::Edit);
        assert_eq!(tool_kind("delete"), ToolKind::Delete);
        assert_eq!(tool_kind("other"), ToolKind::Other);
    }

    #[test]
    fn tool_result_text_pretty_prints_json_results() {
        let items = vec![rig::core::completion::message::ToolResultContent::Json {
            value: serde_json::json!({ "path": "Cargo.lock", "total_lines": 2 }),
        }];
        let text = tool_result_text(&items);
        assert!(text.contains("\"path\": \"Cargo.lock\""), "unexpected: {text}");
        assert!(text.contains("\"total_lines\": 2"), "unexpected: {text}");
        assert!(!text.contains("Json {"), "Debug rendering leaked: {text}");
    }
}
