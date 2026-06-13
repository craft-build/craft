use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::{
    AgentNotification, AgentRequest, AgentResponse, CloseSessionRequest, CloseSessionResponse,
    ConfigOptionUpdate, ContentBlock, CreateTerminalRequest, CreateTerminalResponse,
    CurrentModeUpdate, EmbeddedResourceResource, EnvVariable, Error as AcpError, ImageContent,
    InitializeRequest, JsonRpcMessage, KillTerminalRequest, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, McpServer, NewSessionRequest, Notification,
    PromptRequest, PromptResponse, ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest,
    Request, RequestId, RequestPermissionRequest, RequestPermissionResponse, ResumeSessionRequest,
    Response, SessionId, SessionInfo as AcpSessionInfo, SessionInfoUpdate, SessionModeId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse,
    TerminalId, TerminalOutputRequest, TerminalOutputResponse, TextContent, ToolCall, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, WriteTextFileRequest,
    WriteTextFileResponse,
};
use color_eyre::eyre::Context;
use craft_agent::headless::{self, InteractiveHandle, InteractiveParams};
use craft_agent::mcp::config::{McpConfig, ServerConfig, Transport};
use craft_agent::mcp;
use craft_agent::tools::{FsBackend, FsFuture, LocalFs};
use craft_agent::types::{AgentEvent, BatchToolStatus, ToolOutput};
use craft_agent::{AgentInput, AgentMode, Envelope, ImageMediaType, ImageSource};
use craft_lua::{LocalTerminal, TerminalBackend, TerminalEvent, TerminalFuture, TerminalHandle, TerminalSpec};
use craft_providers::Message;
use craft_providers::model::Model;
use flume::{Receiver, Sender};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::{AcpParams, mcp as acp_mcp, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
const DELEGATION_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(50);

type PendingPrompt = Arc<Mutex<Option<RequestId>>>;
type ModelSpecs = Arc<Mutex<Vec<String>>>;
type PendingRequests = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

struct ClientCaps {
    fs_read: AtomicBool,
    fs_write: AtomicBool,
    terminal: AtomicBool,
}

impl ClientCaps {
    fn new() -> Self {
        Self {
            fs_read: AtomicBool::new(false),
            fs_write: AtomicBool::new(false),
            terminal: AtomicBool::new(false),
        }
    }

    fn apply(&self, caps: &agent_client_protocol_schema::ClientCapabilities) {
        self.fs_read
            .store(caps.fs.read_text_file, Ordering::Relaxed);
        self.fs_write
            .store(caps.fs.write_text_file, Ordering::Relaxed);
        self.terminal.store(caps.terminal, Ordering::Relaxed);
    }
}

struct AcpFs {
    caps: Arc<ClientCaps>,
    out_tx: Sender<Value>,
    pending: PendingRequests,
    next_id: Arc<AtomicI64>,
    shared_session: SharedSession,
}

impl AcpFs {
    fn session_id(&self) -> Result<String, String> {
        self.shared_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|i| i.session_id.clone())
            .ok_or_else(|| "no active session for fs delegation".to_string())
    }
}

impl FsBackend for AcpFs {
    fn read_text_file<'a>(&'a self, path: &'a Path) -> FsFuture<'a, String> {
        if !self.caps.fs_read.load(Ordering::Relaxed) {
            return LocalFs.read_text_file(path);
        }
        let path = path.to_path_buf();
        Box::pin(async move {
            let sid = self.session_id()?;
            let request = AgentRequest::ReadTextFileRequest(ReadTextFileRequest::new(
                SessionId::from(sid),
                path,
            ));
            let v = send_delegated(&self.out_tx, &self.pending, &self.next_id, request).await?;
            let resp: ReadTextFileResponse =
                serde_json::from_value(v).map_err(|e| e.to_string())?;
            Ok(resp.content)
        })
    }

    fn write_text_file<'a>(&'a self, path: &'a Path, contents: &'a str) -> FsFuture<'a, ()> {
        if !self.caps.fs_write.load(Ordering::Relaxed) {
            return LocalFs.write_text_file(path, contents);
        }
        let path = path.to_path_buf();
        let contents = contents.to_owned();
        Box::pin(async move {
            let sid = self.session_id()?;
            let request = AgentRequest::WriteTextFileRequest(WriteTextFileRequest::new(
                SessionId::from(sid),
                path,
                contents,
            ));
            let v = send_delegated(&self.out_tx, &self.pending, &self.next_id, request).await?;
            let _: WriteTextFileResponse =
                serde_json::from_value(v).map_err(|e| e.to_string())?;
            Ok(())
        })
    }
}

async fn recv_delegated(rx: oneshot::Receiver<Value>) -> Result<Value, String> {
    let raw = tokio::time::timeout(DELEGATION_TIMEOUT, rx)
        .await
        .map_err(|_| "client request timed out".to_string())?
        .map_err(|_| "client dropped response channel".to_string())?;
    if let Some(err) = raw.get("error") {
        return Err(format!("client error: {err}"));
    }
    Ok(raw.get("result").cloned().unwrap_or(Value::Null))
}

async fn send_delegated(
    out_tx: &Sender<Value>,
    pending: &PendingRequests,
    next_id: &AtomicI64,
    request: AgentRequest,
) -> Result<Value, String> {
    let id = next_id.fetch_add(1, Ordering::Relaxed) + 1;
    let (tx, rx) = oneshot::channel();
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, tx);
    send(
        out_tx,
        Request {
            id: RequestId::Number(id),
            method: Arc::from(request.method()),
            params: Some(request),
        },
    );
    let result = recv_delegated(rx).await;
    if result.is_err() {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    }
    result
}

struct AcpTerminal {
    caps: Arc<ClientCaps>,
    out_tx: Sender<Value>,
    pending: PendingRequests,
    next_id: Arc<AtomicI64>,
    shared_session: SharedSession,
}

impl AcpTerminal {
    fn session_id(&self) -> Result<String, String> {
        self.shared_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|i| i.session_id.clone())
            .ok_or_else(|| "no active session for terminal delegation".to_string())
    }
}

impl TerminalBackend for AcpTerminal {
    fn start<'a>(&'a self, spec: TerminalSpec) -> TerminalFuture<'a> {
        if !self.caps.terminal.load(Ordering::Relaxed) {
            return LocalTerminal.start(spec);
        }
        Box::pin(async move {
            let sid = self.session_id()?;
            let env: Vec<EnvVariable> = spec
                .env
                .as_ref()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| EnvVariable::new(k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();

            let mut create = CreateTerminalRequest::new(sid.clone(), shell_program());
            create.args = vec![shell_arg().to_string(), spec.cmd.clone()];
            create.env = env;
            create.cwd = spec.cwd.clone().map(PathBuf::from);

            let create_val = send_delegated(
                &self.out_tx,
                &self.pending,
                &self.next_id,
                AgentRequest::CreateTerminalRequest(create),
            )
            .await?;
            let terminal_id: TerminalId =
                serde_json::from_value::<CreateTerminalResponse>(create_val)
                    .map_err(|e| e.to_string())?
                    .terminal_id;

            let (event_tx, event_rx) = flume::unbounded::<TerminalEvent>();
            self.spawn_poller(sid.clone(), terminal_id.clone(), event_tx);

            let kill_out = self.out_tx.clone();
            let kill_pending = Arc::clone(&self.pending);
            let kill_next = Arc::clone(&self.next_id);
            let kill_sid = sid;
            let kill_tid = terminal_id;
            let kill: Box<dyn FnOnce() + Send> = Box::new(move || {
                let out_tx = kill_out.clone();
                let pending = Arc::clone(&kill_pending);
                let next = Arc::clone(&kill_next);
                let sid = kill_sid.clone();
                let tid = kill_tid.clone();
                tokio::spawn(async move {
                    let _ = send_delegated(
                        &out_tx,
                        &pending,
                        &next,
                        AgentRequest::KillTerminalRequest(KillTerminalRequest::new(sid, tid)),
                    )
                    .await;
                });
            });

            Ok(TerminalHandle {
                events: event_rx,
                kill,
            })
        })
    }
}

impl AcpTerminal {
    fn spawn_poller(
        &self,
        sid: String,
        terminal_id: TerminalId,
        event_tx: Sender<TerminalEvent>,
    ) {
        let out_tx = self.out_tx.clone();
        let pending = Arc::clone(&self.pending);
        let next = Arc::clone(&self.next_id);
        tokio::spawn(async move {
            let mut sent = 0usize;
            loop {
                let resp = match send_delegated(
                    &out_tx,
                    &pending,
                    &next,
                    AgentRequest::TerminalOutputRequest(TerminalOutputRequest::new(
                        sid.clone(),
                        terminal_id.clone(),
                    )),
                )
                .await
                {
                    Ok(v) => serde_json::from_value::<TerminalOutputResponse>(v).ok(),
                    Err(_) => None,
                };
                if let Some(resp) = resp {
                    let start = sent.min(resp.output.len());
                    for line in resp.output[start..].lines() {
                        if event_tx.send(TerminalEvent::Stdout(line.to_string())).is_err() {
                            release_terminal(&out_tx, &pending, &next, &sid, &terminal_id).await;
                            return;
                        }
                    }
                    sent = resp.output.len();
                    if let Some(exit) = resp.exit_status {
                        let code = exit.exit_code.map(|c| c as i32).unwrap_or(-1);
                        let _ = event_tx.send(TerminalEvent::Exit(code));
                        release_terminal(&out_tx, &pending, &next, &sid, &terminal_id).await;
                        break;
                    }
                }
                tokio::time::sleep(TERMINAL_POLL_INTERVAL).await;
            }
        });
    }
}

async fn release_terminal(
    out_tx: &Sender<Value>,
    pending: &PendingRequests,
    next: &AtomicI64,
    sid: &str,
    terminal_id: &TerminalId,
) {
    let _ = send_delegated(
        out_tx,
        pending,
        next,
        AgentRequest::ReleaseTerminalRequest(ReleaseTerminalRequest::new(
            SessionId::from(sid.to_string()),
            terminal_id.clone(),
        )),
    )
    .await;
}

fn shell_program() -> &'static str {
    #[cfg(unix)]
    {
        "sh"
    }
    #[cfg(not(unix))]
    {
        "cmd.exe"
    }
}

fn shell_arg() -> &'static str {
    #[cfg(unix)]
    {
        "-c"
    }
    #[cfg(not(unix))]
    {
        "/C"
    }
}

struct SessionState {
    handle: InteractiveHandle,
    current_mode: AgentMode,
    current_model: String,
    pending_prompt: PendingPrompt,
    title_sent: bool,
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
    pending_requests: PendingRequests,
    client_caps: Arc<ClientCaps>,
    next_request_id: Arc<AtomicI64>,
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
    let pending_requests: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
    let client_caps = Arc::new(ClientCaps::new());
    let next_request_id = Arc::new(AtomicI64::new(FIRST_OUTGOING_REQUEST_ID));

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
        pending_requests,
        client_caps,
        next_request_id,
        session: None,
    };

    let acp_terminal: Arc<dyn TerminalBackend> = Arc::new(AcpTerminal {
        caps: Arc::clone(&server.client_caps),
        out_tx: server.out_tx.clone(),
        pending: Arc::clone(&server.pending_requests),
        next_id: Arc::clone(&server.next_request_id),
        shared_session: Arc::clone(&server.shared_session),
    });
    if let Err(e) = params.plugin_host.set_terminal_backend(acp_terminal) {
        warn!(error = %e, "failed to install ACP terminal backend");
    }

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
                Some(id) => handle_request(&mut server, method, id, &raw, &params).await,
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

async fn handle_request(srv: &mut Server, method: &str, id: RequestId, raw: &Value, params: &AcpParams) {
    let result: Result<AgentResponse, AcpError> = match method {
        "initialize" => {
            if let Ok(req) = parse_params::<InitializeRequest>(raw) {
                srv.client_caps.apply(&req.client_capabilities);
            }
            Ok(AgentResponse::InitializeResponse(
                methods::initialize_response(),
            ))
        }
        "session/new" => {
            let req = match parse_params::<NewSessionRequest>(raw) {
                Ok(r) => r,
                Err(e) => { srv.respond(id, Err(e)); return; }
            };
            let mcp_servers = req.mcp_servers.clone();
            let fs = build_delegated_fs(srv);
            let handle =
                spawn_session(params, req.cwd, None, Vec::new(), &mcp_servers, fs).await;
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
            Ok(AgentResponse::NewSessionResponse(resp))
        }
        "session/load" => {
            let req = match parse_params::<LoadSessionRequest>(raw) {
                Ok(r) => r,
                Err(e) => { srv.respond(id, Err(e)); return; }
            };
            let session_id = req.session_id.0.to_string();
            let history = match load_history(&session_id) {
                Ok(h) => h,
                Err(e) => { srv.respond(id, Err(e)); return; }
            };
            let sid = SessionId::from(session_id.clone());
            for update in translate::replay_history(&history) {
                session_update(&srv.out_tx, &sid, update);
            }
            let mcp_servers = req.mcp_servers.clone();
            let fs = build_delegated_fs(srv);
            let handle =
                spawn_session(params, req.cwd, Some(session_id), history, &mcp_servers, fs).await;
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
        }
        "session/resume" => {
            let req = match parse_params::<ResumeSessionRequest>(raw) {
                Ok(r) => r,
                Err(e) => { srv.respond(id, Err(e)); return; }
            };
            let session_id = req.session_id.0.to_string();
            let history = match load_history(&session_id) {
                Ok(h) => h,
                Err(e) => { srv.respond(id, Err(e)); return; }
            };
            let mcp_servers = req.mcp_servers.clone();
            let fs = build_delegated_fs(srv);
            let handle =
                spawn_session(params, req.cwd, Some(session_id), history, &mcp_servers, fs).await;
            let spec = params.model.spec();
            let resp = {
                let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
                methods::resume_session_response()
                    .config_options(vec![
                        methods::mode_config_option(methods::MODE_BUILD),
                        methods::model_config_option(&spec, &specs),
                    ])
            };
            install_session(srv, handle, spec);
            Ok(AgentResponse::ResumeSessionResponse(resp))
        }
        "session/list" => handle_list_sessions(raw),
        "session/close" => handle_close_session(srv, raw),
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

async fn spawn_session(
    params: &AcpParams,
    cwd: PathBuf,
    session_id: Option<String>,
    history: Vec<Message>,
    client_mcp_servers: &[McpServer],
    fs: Arc<dyn FsBackend>,
) -> InteractiveHandle {
    let mcp_handle = build_mcp_handle(&params.mcp_config, client_mcp_servers).await;
    headless::spawn_interactive(InteractiveParams {
        model: params.model.clone(),
        config: params.config.clone(),
        compression: craft_config::CompressionConfig::default(),
        permissions_config: params.permissions_config.clone(),
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        excluded_tools: Vec::new(),
        mcp_handle,
        initial_wd: cwd,
        session_id,
        initial_history: history,
        yolo: params.yolo,
        system_prompt_override: None,
        append_system_prompt: None,
        fs,
    })
}

fn build_delegated_fs(srv: &Server) -> Arc<dyn FsBackend> {
    Arc::new(AcpFs {
        caps: Arc::clone(&srv.client_caps),
        out_tx: srv.out_tx.clone(),
        pending: Arc::clone(&srv.pending_requests),
        next_id: Arc::clone(&srv.next_request_id),
        shared_session: Arc::clone(&srv.shared_session),
    })
}

async fn build_mcp_handle(
    local_config: &McpConfig,
    client_servers: &[McpServer],
) -> Option<craft_agent::McpHandle> {
    let client_configs = acp_mcp::convert_acp_servers(client_servers);
    if local_config.is_empty() && client_configs.is_empty() {
        return None;
    }
    let merged = merge_configs(local_config, &client_configs);
    mcp::start_with_config(merged).await
}

fn merge_configs(local: &McpConfig, client: &[ServerConfig]) -> McpConfig {
    let mut merged = local.clone();
    for cfg in client {
        let raw = craft_agent::mcp::config::RawServerConfig {
            enabled: true,
            timeout: cfg.timeout.as_millis() as u64,
            transport: match &cfg.transport {
                Transport::Stdio { program, args, environment } => {
                    let mut command = vec![program.clone()];
                    command.extend(args.iter().cloned());
                    craft_agent::mcp::config::RawTransport::Stdio(
                        craft_agent::mcp::config::RawStdioFields {
                            command,
                            environment: environment.clone(),
                        },
                    )
                }
                Transport::Http { url, headers } => {
                    craft_agent::mcp::config::RawTransport::Http(
                        craft_agent::mcp::config::RawHttpFields {
                            url: url.clone(),
                            headers: headers.clone(),
                        },
                    )
                }
            },
        };
        merged
            .origins
            .insert(cfg.name.clone(), PathBuf::from("acp-client"));
        merged.mcp.insert(cfg.name.clone(), raw);
    }
    merged
}

fn install_session(srv: &mut Server, handle: InteractiveHandle, current_model: String) {
    if let Some(prev) = srv.session.take() {
        teardown_session(&srv.out_tx, prev);
    }
    let session_id = handle.session_id.clone();
    let pending = PendingPrompt::default();
    start_event_pump(
        handle.event_rx.clone(),
        handle.session_id.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        Arc::clone(&srv.next_request_id),
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
        title_sent: false,
    });
}

fn resolve_pending_cancelled(out_tx: &Sender<Value>, pending: PendingPrompt) {
    if let Some(id) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let resp = PromptResponse::new(agent_client_protocol_schema::StopReason::Cancelled);
        send(
            out_tx,
            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
        );
    }
}

fn teardown_session(out_tx: &Sender<Value>, session: SessionState) {
    resolve_pending_cancelled(out_tx, Arc::clone(&session.pending_prompt));
    let _ = session.handle.cancel_tx.try_send(());
    session.handle.task.abort();
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
    let session = srv.session.as_mut().ok_or_else(no_session)?;
    if session
        .pending_prompt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
    {
        return Err(AcpError::invalid_request()
            .data(json_str("a prompt is already in progress for this session")));
    }

    let (message, images) = extract_prompt_content(&req.prompt);
    if !message.is_empty() {
        let sid = SessionId::from(session.handle.session_id.clone());
        session_update(&srv.out_tx, &sid, translate::user_message_chunk(&message));
        if !session.title_sent {
            let title = craft_storage::sessions::generate_title(&[Message::user(message.clone())]);
            session_update(
                &srv.out_tx,
                &sid,
                SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
            );
            session.title_sent = true;
        }
    }
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
        .map_err(|_| AcpError::internal_error().data(json_str("session ended")))?;
    *session.pending_prompt.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
    Ok(())
}

fn apply_mode(srv: &mut Server, mode_str: &str) -> Result<(), AcpError> {
    let new_mode = methods::mode_id_to_agent_mode(mode_str).ok_or_else(|| {
        AcpError::invalid_params().data(json_str(&format!("unknown mode: {mode_str}")))
    })?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_mode = new_mode;

    let sid = SessionId::from(session.handle.session_id.clone());
    session_update(
        &srv.out_tx,
        &sid,
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::from(
            mode_str.to_string(),
        ))),
    );
    Ok(())
}

fn handle_set_mode(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionModeRequest = parse_params(raw)?;
    let mode_str = req.mode_id.0.to_string();
    apply_mode(srv, &mode_str)?;
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
        .map_err(|_| AcpError::internal_error().data(json_str("session ended")))?;
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
    apply_mode(srv, &mode_str)?;

    let current_model = srv
        .session
        .as_ref()
        .map(|s| s.current_model.clone())
        .unwrap_or_default();
    let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(vec![
            methods::mode_config_option(&mode_str),
            methods::model_config_option(&current_model, &specs),
        ]),
    ))
}

fn handle_list_sessions(raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: ListSessionsRequest = parse_params(raw)?;
    let storage = craft_storage::StateDir::resolve()
        .map_err(|e| AcpError::internal_error().data(json_str(&e)))?;
    let cwd_filter = req.cwd.as_deref().and_then(std::path::Path::to_str);
    let summaries = craft_storage::sessions::Session::<
        craft_providers::Message,
        craft_providers::TokenUsage,
        craft_agent::ToolOutput,
    >::list(cwd_filter, &storage)
    .map_err(|e| AcpError::internal_error().data(json_str(&e)))?;

    let sessions = summaries
        .into_iter()
        .map(|s| {
            AcpSessionInfo::new(s.id, s.cwd)
                .title(s.title)
                .updated_at(epoch_to_iso8601(s.updated_at))
        })
        .collect();
    Ok(AgentResponse::ListSessionsResponse(ListSessionsResponse::new(
        sessions,
    )))
}

fn handle_close_session(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: CloseSessionRequest = parse_params(raw)?;
    if srv
        .session
        .as_ref()
        .is_some_and(|s| s.handle.session_id == req.session_id.0.as_ref())
    {
        if let Some(session) = srv.session.take() {
            teardown_session(&srv.out_tx, session);
        }
        *srv.shared_session.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    Ok(AgentResponse::CloseSessionResponse(CloseSessionResponse::new()))
}

fn epoch_to_iso8601(epoch_secs: u64) -> Option<String> {
    time::OffsetDateTime::from_unix_timestamp(epoch_secs as i64)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
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
    if let Some(id) = raw.get("id").and_then(Value::as_i64) {
        let sender = srv
            .pending_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        if let Some(sender) = sender {
            let _ = sender.send(raw.clone());
            return;
        }
    }

    let Some(session) = &srv.session else { return };
    if let Some(result) = raw.get("result")
        && let Ok(resp) = serde_json::from_value::<RequestPermissionResponse>(result.clone())
    {
        let answer = permissions::outcome_to_answer(&resp.outcome);
        let _ = session.handle.answer_tx.send(answer.encode());
    } else if raw.get("error").is_some() {
        let _ = session
            .handle
            .answer_tx
            .send(craft_agent::permissions::PermissionAnswer::Deny.encode());
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
    next_request_id: Arc<AtomicI64>,
) {
    tokio::spawn(async move {
        let sid = SessionId::from(session_id);

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
                    let req_id = next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
                    send(
                        &out_tx,
                        Request {
                            id: RequestId::Number(req_id),
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
    AcpError::invalid_request().data(json_str("no active session"))
}

fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &(impl std::fmt::Display + ?Sized)) -> Value {
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
