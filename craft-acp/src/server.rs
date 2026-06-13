use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    AgentNotification, AgentRequest, AgentResponse, ConfigOptionUpdate, ContentBlock,
    CurrentModeUpdate, EmbeddedResourceResource, Error as AcpError, ImageContent, JsonRpcMessage,
    LoadSessionRequest, NewSessionRequest, Notification, PromptRequest, PromptResponse, Request,
    RequestId, RequestPermissionRequest, RequestPermissionResponse, Response, SessionId,
    SessionModeId, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, TextContent,
    ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use color_eyre::eyre::Context;
use craft_agent::headless::{self, InteractiveHandle, InteractiveParams};
use craft_agent::types::{AgentEvent, BatchToolStatus, ToolOutput};
use craft_agent::{AgentInput, AgentMode, Envelope, ImageMediaType, ImageSource};
use craft_providers::Message;
use craft_providers::model::Model;
use flume::{Receiver, Sender};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tracing::{debug, warn};

use crate::{AcpParams, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;

type PendingPrompt = Arc<Mutex<Option<RequestId>>>;
type ModelSpecs = Arc<Mutex<Vec<String>>>;

struct SessionState {
    handle: InteractiveHandle,
    current_mode: AgentMode,
    current_model: String,
    pending_prompt: PendingPrompt,
}

struct SessionInfo {
    session_id: String,
    current_model: String,
}

type SharedSession = Arc<Mutex<Option<SessionInfo>>>;

struct Server {
    out_tx: Sender<Value>,
    model_specs: ModelSpecs,
    shared_session: SharedSession,
    session: Option<SessionState>,
}

impl Server {
    fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::new(id, result));
    }
}

pub async fn serve(params: AcpParams) -> color_eyre::Result<()> {
    let (out_tx, out_rx) = flume::unbounded::<Value>();

    let writer_task = tokio::spawn(async move {
        let stdout = std::io::stdout();
        while let Ok(msg) = out_rx.recv_async().await {
            let mut handle = stdout.lock();
            if serde_json::to_writer(&mut handle, &msg).is_ok() {
                let _ = handle.write_all(b"\n");
                let _ = handle.flush();
            }
        }
    });

    let shared_session: SharedSession = Arc::new(Mutex::new(None));
    let model_specs: ModelSpecs = Arc::new(Mutex::new(Vec::new()));

    let bg_specs = Arc::clone(&model_specs);
    let bg_session = Arc::clone(&shared_session);
    let bg_out = out_tx.clone();

    let _bg_fetch = tokio::spawn(async move {
        craft_providers::provider::fetch_all_models(|batch| {
            if batch.models.is_empty() {
                return;
            }
            let mut specs = bg_specs.lock().unwrap_or_else(|e| e.into_inner());
            specs.extend(batch.models);
            let guard = specs.clone();
            drop(specs);

            let sess = bg_session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(info) = &*sess {
                let sid = SessionId::from(info.session_id.clone());
                session_update(
                    &bg_out,
                    &sid,
                    SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(vec![
                        methods::mode_config_option(methods::MODE_BUILD),
                        methods::model_config_option(&info.current_model, &guard),
                    ])),
                );
            }
        })
        .await;
    });

    let mut server = Server {
        out_tx,
        model_specs,
        shared_session,
        session: None,
    };

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        let line = match lines.next_line().await.context("read stdin")? {
            Some(l) => l,
            None => break,
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let raw: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "invalid JSON on stdin");
                server.respond(RequestId::Null, Err(AcpError::parse_error()));
                continue;
            }
        };

        let id = raw.get("id").map(request_id);

        if raw.get("result").is_some() || raw.get("error").is_some() {
            handle_incoming_response(&server, &raw);
        } else if let Some(method) = raw.get("method").and_then(Value::as_str) {
            match id {
                Some(id) => handle_request(&mut server, method, id, &raw, &params),
                None => handle_notification(&server, method),
            }
        } else if let Some(id) = id {
            server.respond(id, Err(AcpError::invalid_request()));
        }
    }

    drop(server);
    let _ = writer_task.await;

    Ok(())
}

fn request_id(v: &Value) -> RequestId {
    serde_json::from_value(v.clone()).unwrap_or(RequestId::Null)
}

fn handle_request(srv: &mut Server, method: &str, id: RequestId, raw: &Value, params: &AcpParams) {
    let result = match method {
        "initialize" => Ok(AgentResponse::InitializeResponse(
            methods::initialize_response(),
        )),
        "session/new" => parse_params::<NewSessionRequest>(raw).map(|req| {
            let handle = spawn_session(params, req.cwd, None, Vec::new());
            let spec = params.model.spec();
            let resp = {
                let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
                methods::new_session_response(&handle.session_id)
                    .config_options(vec![
                        methods::mode_config_option(methods::MODE_BUILD),
                        methods::model_config_option(&spec, &specs),
                    ])
            };
            install_session(srv, handle, spec);
            AgentResponse::NewSessionResponse(resp)
        }),
        "session/load" => parse_params::<LoadSessionRequest>(raw).and_then(|req| {
            let session_id = req.session_id.0.to_string();
            let history = load_history(&session_id)?;
            let sid = SessionId::from(session_id.clone());
            for update in translate::replay_history(&history) {
                session_update(&srv.out_tx, &sid, update);
            }
            let handle = spawn_session(params, req.cwd, Some(session_id), history);
            let spec = params.model.spec();
            let resp = {
                let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
                methods::load_session_response()
                    .config_options(vec![
                        methods::mode_config_option(methods::MODE_BUILD),
                        methods::model_config_option(&spec, &specs),
                    ])
            };
            install_session(srv, handle, spec);
            Ok(AgentResponse::LoadSessionResponse(resp))
        }),
        "session/prompt" => match handle_prompt(srv, raw, &id) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        "session/set_mode" => handle_set_mode(srv, raw),
        "session/set_config_option" => handle_set_config(srv, raw),
        _ => Err(AcpError::method_not_found()),
    };
    srv.respond(id, result);
}

fn spawn_session(
    params: &AcpParams,
    cwd: PathBuf,
    session_id: Option<String>,
    history: Vec<Message>,
) -> InteractiveHandle {
    headless::spawn_interactive(InteractiveParams {
        model: params.model.clone(),
        config: params.config.clone(),
        compression: craft_config::CompressionConfig::default(),
        permissions_config: params.permissions_config.clone(),
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        excluded_tools: Vec::new(),
        mcp_handle: params.mcp_handle.clone(),
        initial_wd: cwd,
        session_id,
        initial_history: history,
        yolo: params.yolo,
        system_prompt_override: None,
        append_system_prompt: None,
    })
}

fn install_session(srv: &mut Server, handle: InteractiveHandle, current_model: String) {
    let session_id = handle.session_id.clone();
    let pending = PendingPrompt::default();
    start_event_pump(
        handle.event_rx.clone(),
        handle.session_id.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
    );
    *srv.shared_session.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionInfo {
        session_id: session_id.clone(),
        current_model: current_model.clone(),
    });
    srv.session = Some(SessionState {
        handle,
        current_mode: AgentMode::Build,
        current_model,
        pending_prompt: pending,
    });
}

fn load_history(session_id: &str) -> Result<Vec<Message>, AcpError> {
    let storage = craft_storage::StateDir::resolve()
        .map_err(|e| AcpError::internal_error().data(json_str(&e)))?;
    load_history_from(&storage, session_id)
}

fn load_history_from(
    storage: &craft_storage::StateDir,
    session_id: &str,
) -> Result<Vec<Message>, AcpError> {
    let session: craft_storage::sessions::Session<
        Message,
        craft_providers::TokenUsage,
        craft_agent::ToolOutput,
    > = craft_storage::sessions::Session::load(session_id, storage).map_err(|e| {
        AcpError::resource_not_found(Some(format!("session/{session_id}"))).data(json_str(&e))
    })?;
    Ok(session.messages)
}

fn handle_prompt(srv: &mut Server, raw: &Value, id: &RequestId) -> Result<(), AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = srv.session.as_ref().ok_or_else(no_session)?;

    let (message, images) = extract_prompt_content(&req.prompt);
    let input = AgentInput {
        message,
        mode: session.current_mode.clone(),
        images,
        ..Default::default()
    };

    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    *session.pending_prompt.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
    Ok(())
}

fn handle_set_mode(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionModeRequest = parse_params(raw)?;
    let mode_str = req.mode_id.0.to_string();
    let new_mode = methods::mode_id_to_agent_mode(&mode_str)
        .ok_or_else(|| AcpError::new(-32602, format!("unknown mode: {mode_str}")))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_mode = new_mode;

    let sid = SessionId::from(session.handle.session_id.clone());
    session_update(
        &srv.out_tx,
        &sid,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(mode_str))),
    );
    Ok(AgentResponse::SetSessionModeResponse(
        SetSessionModeResponse::new(),
    ))
}

fn handle_set_config(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionConfigOptionRequest = parse_params(raw)?;
    let config_id = req.config_id.0.as_ref();

    if config_id == methods::MODE_CONFIG_ID {
        return handle_set_mode_config(srv, &req);
    }

    if config_id != methods::MODEL_CONFIG_ID {
        let detail = format!("unknown config option: {}", req.config_id);
        return Err(AcpError::invalid_params().data(json_str(&detail)));
    }

    let spec = req.value.0.to_string();
    let model =
        Model::from_spec(&spec).map_err(|e| AcpError::invalid_params().data(json_str(&e)))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session
        .handle
        .model_tx
        .send(model)
        .map_err(|_| AcpError::new(-32603, "session ended"))?;
    session.current_model = spec.clone();

    if let Some(info) = srv.shared_session.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        info.current_model = spec.clone();
    }

    let mode = srv.session.as_ref().map(|s| &s.current_mode);
    let mode_id = match mode {
        Some(AgentMode::Plan(_)) => "plan",
        _ => "build",
    };
    let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(vec![
            methods::mode_config_option(mode_id),
            methods::model_config_option(&spec, &specs),
        ]),
    ))
}

fn handle_set_mode_config(
    srv: &mut Server,
    req: &SetSessionConfigOptionRequest,
) -> Result<AgentResponse, AcpError> {
    let mode_str = req.value.0.to_string();
    let new_mode = methods::mode_id_to_agent_mode(&mode_str)
        .ok_or_else(|| AcpError::new(-32602, format!("unknown mode: {mode_str}")))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_mode = new_mode;

    let sid = SessionId::from(session.handle.session_id.clone());
    session_update(
        &srv.out_tx,
        &sid,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(mode_str.clone()))),
    );

    let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(vec![
            methods::mode_config_option(&mode_str),
            methods::model_config_option(&session.current_model, &specs),
        ]),
    ))
}

fn handle_notification(srv: &Server, method: &str) {
    match method {
        "session/cancel" => {
            if let Some(session) = &srv.session {
                let _ = session.handle.cancel_tx.try_send(());
            }
        }
        _ => debug!(method, "unknown notification"),
    }
}

fn handle_incoming_response(srv: &Server, raw: &Value) {
    let Some(session) = &srv.session else { return };

    if let Some(result) = raw.get("result")
        && let Ok(resp) = serde_json::from_value::<RequestPermissionResponse>(result.clone())
    {
        let answer = permissions::outcome_to_answer(&resp.outcome);
        let _ = session.handle.answer_tx.send(answer.encode());
    }
}

fn extract_prompt_content(blocks: &[ContentBlock]) -> (String, Vec<ImageSource>) {
    let mut text = String::new();
    let mut images = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text(TextContent { text: t, .. }) => append(&mut text, t),
            ContentBlock::Image(ImageContent {
                data, mime_type, ..
            }) => images.push(ImageSource {
                media_type: image_media_type(mime_type),
                data: Arc::from(data.as_str()),
            }),
            ContentBlock::Resource(res) => {
                if let EmbeddedResourceResource::TextResourceContents(trc) = &res.resource {
                    append(&mut text, &format!("--- {} ---\n{}", trc.uri, trc.text));
                }
            }
            ContentBlock::ResourceLink(rl) => append(&mut text, &format!("[Resource: {}]", rl.uri)),
            _ => {}
        }
    }

    (text, images)
}

fn append(text: &mut String, part: &str) {
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(part);
}

fn image_media_type(mime: &str) -> ImageMediaType {
    match mime {
        "image/png" => ImageMediaType::Png,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        _ => ImageMediaType::Jpeg,
    }
}

fn start_event_pump(
    event_rx: Receiver<Envelope>,
    session_id: String,
    out_tx: Sender<Value>,
    pending: PendingPrompt,
) {
    tokio::spawn(async move {
        let sid = SessionId::from(session_id);
        let mut next_request_id = FIRST_OUTGOING_REQUEST_ID;

        while let Ok(Envelope {
            event, subagent, ..
        }) = event_rx.recv_async().await
        {
            if let Some(info) = &subagent {
                if matches!(
                    event,
                    AgentEvent::Done { .. }
                        | AgentEvent::Error { .. }
                        | AgentEvent::ToolPending { .. }
                        | AgentEvent::SubagentHistory { .. }
                ) {
                    continue;
                }
                let parent_id = &info.parent_tool_use_id;
                match &event {
                    AgentEvent::TextDelta { text } => {
                        let update = translate::text_delta(text);
                        session_update(&out_tx, &sid, update);
                    }
                    AgentEvent::ThinkingDelta { text } => {
                        let update = translate::thinking_delta(text);
                        session_update(&out_tx, &sid, update);
                    }
                    AgentEvent::ToolStart(ts) => {
                        let prefixed_id = format!("{}__{}", parent_id, ts.id);
                        let mut fields =
                            ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
                        if let Some(raw) = &ts.raw_input {
                            fields = fields.raw_input(raw.clone());
                        }
                        let tool_call = ToolCall::new(
                            ToolCallId::from(prefixed_id),
                            format!("{} (subagent)", ts.tool),
                        )
                        .kind(translate::tool_kind(ts.tool.as_ref()))
                        .status(ToolCallStatus::InProgress);
                        session_update(&out_tx, &sid, SessionUpdate::ToolCall(tool_call));
                    }
                    AgentEvent::ToolDone(event) => {
                        let prefixed_id = format!("{}__{}", parent_id, event.id);
                        let status = if event.is_error {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        };
                        let fields = ToolCallUpdateFields::new().status(status);
                        session_update(
                            &out_tx,
                            &sid,
                            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                                ToolCallId::from(prefixed_id),
                                fields,
                            )),
                        );
                    }
                    _ => {}
                }
                continue;
            }

            let update = match event {
                AgentEvent::TextDelta { text } => translate::text_delta(&text),
                AgentEvent::ThinkingDelta { text } => translate::thinking_delta(&text),
                AgentEvent::ToolPending { id, name } => translate::tool_pending(&id, &name),
                AgentEvent::ToolStart(event) => translate::tool_start(&event),
                AgentEvent::ToolOutput { id, content } => translate::tool_output(&id, &content),
                AgentEvent::ToolDone(event) => {
                    if let ToolOutput::TodoList(items) = &event.output {
                        session_update(&out_tx, &sid, translate::todo_list_to_plan(items));
                    }
                    translate::tool_done(&event)
                }
                AgentEvent::BatchProgress(event) => {
                    if event.status != BatchToolStatus::InProgress {
                        continue;
                    }
                    translate::batch_inner_start(&event)
                }
                AgentEvent::PermissionRequest { id, tool, scopes, .. } => {
                    let fields =
                        ToolCallUpdateFields::new().title(format!("{tool}: {}", scopes.join(", ")));
                    let request =
                        AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
                            sid.clone(),
                            ToolCallUpdate::new(ToolCallId::from(id), fields),
                            permissions::permission_options(),
                        ));
                    next_request_id += 1;
                    send(
                        &out_tx,
                        Request {
                            id: RequestId::Number(next_request_id),
                            method: Arc::from(request.method()),
                            params: Some(request),
                        },
                    );
                    continue;
                }
                AgentEvent::Done { stop_reason, .. } => {
                    if let Some(id) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let resp = PromptResponse::new(translate::map_stop_reason(stop_reason));
                        send(
                            &out_tx,
                            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
                        );
                    }
                    continue;
                }
                AgentEvent::Error { message } => {
                    if let Some(id) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        let error = AcpError::internal_error().data(Value::String(message));
                        send(&out_tx, Response::<AgentResponse>::new(id, Err(error)));
                    }
                    continue;
                }
                _ => continue,
            };
            session_update(&out_tx, &sid, update);
        }
    });
}

fn send(out_tx: &Sender<Value>, msg: impl Serialize) {
    match serde_json::to_value(JsonRpcMessage::wrap(msg)) {
        Ok(json) => {
            if out_tx.send(json).is_err() {
                warn!("ACP: failed to send message, channel closed");
            }
        }
        Err(e) => warn!(error = %e, "ACP: failed to serialize message"),
    }
}

fn session_update(out_tx: &Sender<Value>, sid: &SessionId, update: SessionUpdate) {
    let notification =
        AgentNotification::SessionNotification(SessionNotification::new(sid.clone(), update));
    send(
        out_tx,
        Notification {
            method: Arc::from("session/update"),
            params: Some(notification),
        },
    );
}

fn no_session() -> AcpError {
    AcpError::new(-32600, "no active session")
}

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &impl std::fmt::Display) -> Value {
    Value::String(e.to_string())
}

#[cfg(test)]
mod tests {
    use craft_providers::{ContentBlock as MsgBlock, Role, TokenUsage};
    use craft_storage::StateDir;
    use craft_storage::sessions::Session;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn load_history_round_trips_stored_messages() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let messages = vec![
            Message::user("rename foo to bar".into()),
            Message {
                role: Role::Assistant,
                content: vec![MsgBlock::Text {
                    text: "done".into(),
                }],
                display_text: None,
            },
        ];
        let mut session: Session<Message, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.messages = messages.clone();
        session.save(&dir).unwrap();

        let history = load_history_from(&dir, &session.id).unwrap();
        assert_eq!(
            serde_json::to_value(&history).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
    }

    #[test]
    fn load_missing_session_is_resource_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let err = load_history_from(&dir, "missing-id").unwrap_err();
        assert_eq!(err.code, AcpError::resource_not_found(None).code);
    }
}
