use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol_schema::v1::{
    AgentNotification, AgentRequest, AgentResponse, ClientCapabilities, CloseSessionRequest,
    CloseSessionResponse, ConfigOptionUpdate, ContentBlock, CreateTerminalRequest,
    CreateTerminalResponse, CurrentModeUpdate, EmbeddedResourceResource, EnvVariable,
    Error as AcpError, ImageContent, InitializeRequest, JsonRpcMessage, KillTerminalRequest,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, McpServer, NewSessionRequest,
    Notification, PromptRequest, PromptResponse, ReadTextFileRequest, ReadTextFileResponse,
    ReleaseTerminalRequest, Request, RequestId, RequestPermissionRequest,
    RequestPermissionResponse, Response, ResumeSessionRequest, SessionConfigOptionValue, SessionId,
    SessionInfo as AcpSessionInfo, SessionInfoUpdate, SessionModeId, SessionNotification,
    SessionUpdate, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse, StopReason, TerminalId, TerminalOutputRequest,
    TerminalOutputResponse, TextContent, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    WriteTextFileRequest, WriteTextFileResponse,
};
use color_eyre::eyre::Context;
use craft_agent::headless::{self, InteractiveHandle, InteractiveParams};
use craft_agent::mcp;
use craft_agent::mcp::config::{McpConfig, ServerConfig, Transport};
use craft_agent::tools::{FsBackend, FsFuture, LocalFs};
use craft_agent::types::{AgentEvent, BatchToolStatus};
use craft_agent::{AgentInput, AgentMode, Envelope, ImageMediaType, ImageSource};
use craft_config::ModelPolicy;
use craft_lua::{
    LocalTerminal, TerminalBackend, TerminalEvent, TerminalFuture, TerminalHandle, TerminalSpec,
    terminal_backend::{JobCommand, Redirect},
};
use craft_providers::model::Model;
use craft_providers::{Message, TokenUsage, add_cost, settle_session};
use craft_storage::id::{CraftId, SessionRef};
use craft_storage::sessions::StoredTokenUsage;
use flume::{Receiver, Sender};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::{AcpParams, elicitation, mcp as acp_mcp, methods, permissions, translate};

const FIRST_OUTGOING_REQUEST_ID: i64 = 1000;
const DELEGATION_TIMEOUT: Duration = Duration::from_secs(60);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const TODO_WRITE_TOOL: &str = "todo_write";
const TODO_UPDATE_METHOD_LEGACY: &str = "session/todo_update";
const TODO_UPDATE_METHOD: &str = "_craft/session/todo_update";

/// What the client still owes us. `asks` holds every outstanding request
/// that blocks a tool (permission or elicitation); tools run concurrently,
/// so answers are matched by request id instead of assuming a single ask.
#[derive(Default)]
pub(crate) struct Pending {
    pub(crate) prompt: Option<RequestId>,
    asks: HashMap<i64, AskKind>,
}

pub(crate) enum AskKind {
    Permission,
    Elicitation,
}

pub(crate) type PendingState = Arc<Mutex<Pending>>;
type ModelSpecs = Arc<Mutex<Vec<String>>>;
pub(crate) type PendingRequests = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

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

    fn apply(&self, caps: &ClientCapabilities) {
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
            let _: WriteTextFileResponse = serde_json::from_value(v).map_err(|e| e.to_string())?;
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

pub(crate) async fn send_delegated(
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
        if !self.caps.terminal.load(Ordering::Relaxed)
            || !matches!(
                (&spec.stdout, &spec.stderr),
                (Redirect::Capture, Redirect::Capture)
            )
        {
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

            let (program, args) = match &spec.cmd {
                JobCommand::Shell(cmd) => (
                    shell_program().to_string(),
                    vec![shell_arg().to_string(), cmd.clone()],
                ),
                JobCommand::Argv(argv) => (argv[0].clone(), argv[1..].to_vec()),
            };
            let mut create = CreateTerminalRequest::new(sid.clone(), program);
            create.args = args;
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
                pid: 0,
                events: event_rx,
                kill,
            })
        })
    }
}

impl AcpTerminal {
    fn spawn_poller(&self, sid: String, terminal_id: TerminalId, event_tx: Sender<TerminalEvent>) {
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
                        if event_tx
                            .send(TerminalEvent::Stdout(line.to_string()))
                            .is_err()
                        {
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

pub(crate) struct SessionState {
    pub(crate) handle: InteractiveHandle,
    pub(crate) mcp: Option<craft_agent::McpHandle>,
    pub(crate) current_mode: AgentMode,
    pub(crate) current_model: String,
    pub(crate) current_thinking: String,
    pub(crate) pending: PendingState,
    pub(crate) title_sent: bool,
    pub(crate) cwd: PathBuf,
}

struct SessionInfo {
    session_id: String,
    current_model: String,
    thinking: String,
    yolo: bool,
    auto_review: bool,
}

type SharedSession = Arc<Mutex<Option<SessionInfo>>>;

pub(crate) struct Server {
    pub(crate) out_tx: Sender<Value>,
    model_specs: ModelSpecs,
    model_policy: Arc<ModelPolicy>,
    shared_session: SharedSession,
    pending_requests: PendingRequests,
    client_caps: Arc<ClientCaps>,
    next_request_id: Arc<AtomicI64>,
    session: Option<SessionState>,
    lua: craft_lua::EventHandle,
}

impl Server {
    fn respond(&self, id: RequestId, result: Result<AgentResponse, AcpError>) {
        send(&self.out_tx, Response::new(id, result));
    }

    /// Respond with a raw JSON `Value` result. Used by `_craft/*` extension
    /// methods whose payloads aren't part of the typed `AgentResponse` enum.
    fn respond_value(&self, id: RequestId, result: Result<Value, AcpError>) {
        match result {
            Ok(value) => send(
                &self.out_tx,
                json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            ),
            Err(e) => send(&self.out_tx, Response::<AgentResponse>::new(id, Err(e))),
        }
    }

    /// Active session's cwd, for command discovery / wiki / repomap.
    pub(crate) fn session_cwd(&self) -> Option<&Path> {
        self.session.as_ref().map(|s| s.cwd.as_path())
    }

    /// Borrow the active session mutably for `_craft/*` handlers that need to
    /// drive the agent (compact, command/run, meta/prompt).
    pub(crate) fn session_mut(&mut self) -> Option<&mut SessionState> {
        self.session.as_mut()
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
    let bg_policy = Arc::clone(&params.model_policy);

    let _bg_fetch = tokio::spawn(async move {
        craft_providers::provider::fetch_all_models(
            &bg_policy,
            |batch| {
                if batch.models.is_empty() {
                    return;
                }
                let mut specs = bg_specs.lock().unwrap_or_else(|e| e.into_inner());
                let known = specs.len();
                for spec in batch.models {
                    if !specs.contains(&spec) {
                        specs.push(spec);
                    }
                }
                if specs.len() == known {
                    return;
                }
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
                            methods::thinking_config_option(&info.thinking),
                            methods::yolo_config_option(info.yolo),
                            methods::auto_review_config_option(info.auto_review),
                        ])),
                    );
                }
            },
            None,
        )
        .await;
    });

    let mut server = Server {
        out_tx,
        model_specs,
        model_policy: Arc::clone(&params.model_policy),
        shared_session,
        pending_requests,
        client_caps,
        next_request_id,
        session: None,
        lua: params.plugin_host.event_handle(),
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

    // The client is gone (stdin EOF, e.g. the desktop app exited). Tear the
    // session down, then hard-exit: the runtime's blocking pool (lua plugin
    // host threads, watchers) can otherwise keep the process alive and orphan
    // it. Session history is persisted continuously, so this is safe mid-turn.
    if let Some(session) = server.session.take() {
        teardown_session(&server.out_tx, &server.lua, session).await;
    }
    drop(server);
    let _ = writer_task.await;
    std::process::exit(0);
}

fn request_id(v: &Value) -> RequestId {
    serde_json::from_value(v.clone()).unwrap_or(RequestId::Null)
}

async fn handle_request(
    srv: &mut Server,
    method: &str,
    id: RequestId,
    raw: &Value,
    params: &AcpParams,
) {
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
                Err(e) => {
                    srv.respond(id, Err(e));
                    return;
                }
            };
            let mcp_servers = req.mcp_servers.clone();
            let fs = build_delegated_fs(srv);
            let cwd = req.cwd.clone();
            let (handle, mcp) =
                spawn_session(params, req.cwd, None, Vec::new(), &mcp_servers, fs).await;
            let spec = params.model.spec();
            let resp = {
                let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
                methods::new_session_response(handle.session_id.as_str()).config_options(vec![
                    methods::mode_config_option(methods::MODE_BUILD),
                    methods::model_config_option(&spec, &specs),
                    methods::thinking_config_option("off"),
                    methods::yolo_config_option(params.yolo),
                    methods::auto_review_config_option(params.permissions_config.auto_review),
                ])
            };
            install_session(srv, handle, mcp, spec, "off".to_string(), cwd, None).await;
            Ok(AgentResponse::NewSessionResponse(resp))
        }
        "session/load" => {
            let req = match parse_params::<LoadSessionRequest>(raw) {
                Ok(r) => r,
                Err(e) => {
                    srv.respond(id, Err(e));
                    return;
                }
            };
            let session_ref: SessionRef = match req.session_id.0.parse() {
                Ok(r) => r,
                Err(_) => {
                    srv.respond(
                        id,
                        Err(AcpError::resource_not_found(Some(
                            req.session_id.0.to_string(),
                        ))),
                    );
                    return;
                }
            };
            let mut loaded = match load_history(session_ref.id()) {
                Ok(h) => h,
                Err(e) => {
                    srv.respond(id, Err(e));
                    return;
                }
            };
            let sid = SessionId::from(session_ref.to_string());
            let home = craft_storage::paths::home();
            let replay_cwd = loaded.recorded_cwd.as_deref().unwrap_or(&req.cwd);
            for update in translate::replay_history(&loaded.history, replay_cwd, home.as_deref()) {
                session_update(&srv.out_tx, &sid, update);
            }
            let mcp_servers = req.mcp_servers.clone();
            let fs = build_delegated_fs(srv);
            let cwd = req.cwd.clone();
            let (handle, mcp) = spawn_session(
                params,
                req.cwd,
                Some(session_ref),
                loaded.history,
                &mcp_servers,
                fs,
            )
            .await;
            let spec = params.model.spec();
            let resp = {
                let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
                methods::load_session_response().config_options(vec![
                    methods::mode_config_option(methods::MODE_BUILD),
                    methods::model_config_option(&spec, &specs),
                    methods::thinking_config_option(&loaded.thinking),
                    methods::yolo_config_option(params.yolo),
                    methods::auto_review_config_option(params.permissions_config.auto_review),
                ])
            };
            // Priced against the model the session recorded, not the one
            // selected now (which may cost 10x more or less). Later turns add
            // their own exact cost.
            let recorded_model =
                Model::from_spec(&loaded.model).unwrap_or_else(|_| params.model.clone());
            let restored_cost = settle_session(
                &loaded.usage,
                &mut loaded.by_model,
                &recorded_model,
                RESTORED_FAST,
            );
            install_session(srv, handle, mcp, spec, loaded.thinking, cwd, restored_cost).await;
            Ok(AgentResponse::LoadSessionResponse(resp))
        }
        "session/resume" => {
            let req = match parse_params::<ResumeSessionRequest>(raw) {
                Ok(r) => r,
                Err(e) => {
                    srv.respond(id, Err(e));
                    return;
                }
            };
            let session_ref: SessionRef = match req.session_id.0.parse() {
                Ok(r) => r,
                Err(_) => {
                    srv.respond(
                        id,
                        Err(AcpError::resource_not_found(Some(
                            req.session_id.0.to_string(),
                        ))),
                    );
                    return;
                }
            };
            let mut loaded = match load_history(session_ref.id()) {
                Ok(h) => h,
                Err(e) => {
                    srv.respond(id, Err(e));
                    return;
                }
            };
            let mcp_servers = req.mcp_servers.clone();
            let fs = build_delegated_fs(srv);
            let cwd = req.cwd.clone();
            let (handle, mcp) = spawn_session(
                params,
                req.cwd,
                Some(session_ref),
                loaded.history,
                &mcp_servers,
                fs,
            )
            .await;
            let spec = params.model.spec();
            let resp = {
                let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
                methods::resume_session_response().config_options(vec![
                    methods::mode_config_option(methods::MODE_BUILD),
                    methods::model_config_option(&spec, &specs),
                    methods::thinking_config_option(&loaded.thinking),
                    methods::yolo_config_option(params.yolo),
                    methods::auto_review_config_option(params.permissions_config.auto_review),
                ])
            };
            let recorded_model =
                Model::from_spec(&loaded.model).unwrap_or_else(|_| params.model.clone());
            let restored_cost = settle_session(
                &loaded.usage,
                &mut loaded.by_model,
                &recorded_model,
                RESTORED_FAST,
            );
            install_session(srv, handle, mcp, spec, loaded.thinking, cwd, restored_cost).await;
            Ok(AgentResponse::ResumeSessionResponse(resp))
        }
        "session/list" => handle_list_sessions(raw),
        "session/close" => handle_close_session(srv, raw).await,
        "session/prompt" => match handle_prompt(srv, raw, &id, params) {
            Ok(()) => return,
            Err(e) => Err(e),
        },
        "session/set_mode" => handle_set_mode(srv, raw),
        "session/set_config_option" => handle_set_config(srv, raw),
        method if method.starts_with("_craft/") => {
            match crate::commands::dispatch(srv, method, raw).await {
                Ok(value) => {
                    srv.respond_value(id, Ok(value));
                    return;
                }
                Err(e) => Err(e),
            }
        }
        _ => Err(AcpError::method_not_found()),
    };
    srv.respond(id, result);
}

async fn spawn_session(
    params: &AcpParams,
    cwd: PathBuf,
    session_id: Option<SessionRef>,
    history: Vec<Message>,
    client_mcp_servers: &[McpServer],
    fs: Arc<dyn FsBackend>,
) -> (InteractiveHandle, Option<craft_agent::McpHandle>) {
    let mcp_handle = build_mcp_handle(&params.mcp_config, client_mcp_servers).await;
    let handle = headless::spawn_interactive(InteractiveParams {
        model: params.model.clone(),
        config: params.config.clone(),
        compression: craft_config::CompressionConfig::default(),
        model_policy: Arc::clone(&params.model_policy),
        permissions_config: params.permissions_config.clone(),
        timeouts: params.timeouts,
        prompt_slots: Arc::clone(&params.prompt_slots),
        excluded_tools: Vec::new(),
        mcp_handle: mcp_handle.clone(),
        initial_wd: cwd,
        session_id,
        initial_history: history,
        yolo: params.yolo,
        auto_review: params.permissions_config.auto_review,
        system_prompt_override: None,
        append_system_prompt: None,
        fs,
        plugin_rules: Arc::clone(&params.plugin_rules),
    });
    let mcp = mcp_handle.clone();
    (handle, mcp)
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
    let handle = mcp::start_with_config(merged);
    if let Some(handle) = &handle {
        handle.ready().await;
    }
    handle
}

fn merge_configs(local: &McpConfig, client: &[ServerConfig]) -> McpConfig {
    let mut merged = local.clone();
    for cfg in client {
        // `mcp.toml` wins on name, so a client-injected server can never swap
        // out the credentials the user configured or revive one they disabled.
        if merged.mcp.contains_key(&cfg.name) {
            warn!(server = %cfg.name, "client MCP server already configured locally");
            continue;
        }
        let raw = craft_agent::mcp::config::RawServerConfig {
            enabled: true,
            timeout: cfg.timeout.as_millis() as u64,
            transport: match &cfg.transport {
                Transport::Stdio {
                    program,
                    args,
                    environment,
                } => {
                    let mut command = vec![program.clone()];
                    command.extend(args.iter().cloned());
                    craft_agent::mcp::config::RawTransport::Stdio(
                        craft_agent::mcp::config::RawStdioFields {
                            command,
                            environment: environment.clone(),
                        },
                    )
                }
                Transport::Http { url, headers, .. } => {
                    craft_agent::mcp::config::RawTransport::Http(
                        craft_agent::mcp::config::RawHttpFields {
                            url: url.clone(),
                            headers: headers.clone(),
                            oauth: None,
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

async fn install_session(
    srv: &mut Server,
    handle: InteractiveHandle,
    mcp: Option<craft_agent::McpHandle>,
    current_model: String,
    thinking: String,
    cwd: PathBuf,
    initial_cost: Option<f64>,
) {
    if let Some(prev) = srv.session.take() {
        teardown_session(&srv.out_tx, &srv.lua, prev).await;
    }
    let session_id = handle.session_id.to_string();
    let pending = PendingState::default();
    start_event_pump(
        handle.event_rx.clone(),
        handle.session_id.clone(),
        srv.out_tx.clone(),
        Arc::clone(&pending),
        Arc::clone(&srv.next_request_id),
        cwd.clone(),
        craft_storage::paths::home(),
        initial_cost,
    );
    *srv.shared_session.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionInfo {
        session_id: session_id.clone(),
        current_model: current_model.clone(),
        thinking: thinking.clone(),
        yolo: handle.permissions.is_yolo(),
        auto_review: handle.permissions.is_auto_review(),
    });
    srv.session = Some(SessionState {
        handle,
        mcp,
        current_mode: AgentMode::Build,
        current_model,
        current_thinking: thinking,
        pending,
        title_sent: false,
        cwd,
    });
}

fn resolve_pending_cancelled(out_tx: &Sender<Value>, pending: PendingState) {
    if let Some(id) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prompt
        .take()
    {
        let resp = PromptResponse::new(StopReason::Cancelled);
        send(
            out_tx,
            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
        );
    }
}

async fn teardown_session(
    out_tx: &Sender<Value>,
    lua: &craft_lua::EventHandle,
    session: SessionState,
) {
    lua.end_session_async(session.handle.session_id.id()).await;
    resolve_pending_cancelled(out_tx, Arc::clone(&session.pending));
    let _ = session.handle.cancel_tx.try_send(());
    session.handle.task.abort();
    if let Some(mcp) = session.mcp {
        mcp.shutdown().await;
    }
}

/// ACP has no fast-mode toggle, so a restored total is priced at standard rates.
const RESTORED_FAST: bool = false;

#[derive(Debug)]
struct LoadedHistory {
    history: Vec<Message>,
    recorded_cwd: Option<PathBuf>,
    usage: TokenUsage,
    by_model: HashMap<String, StoredTokenUsage>,
    model: String,
    thinking: String,
}

fn load_history(session_id: CraftId) -> Result<LoadedHistory, AcpError> {
    let storage = craft_storage::StateDir::resolve()
        .map_err(|e| AcpError::internal_error().data(json_str(&e)))?;
    load_history_from(&storage, session_id)
}

/// History plus the absolute cwd the session recorded in its header. Tool
/// inputs from a past run resolve against that cwd, not the client's current
/// one; a non-absolute recording falls back to the caller's cwd.
fn load_history_from(
    storage: &craft_storage::StateDir,
    session_id: CraftId,
) -> Result<LoadedHistory, AcpError> {
    let session: craft_storage::sessions::Session<
        Message,
        craft_providers::TokenUsage,
        craft_agent::ToolOutput,
    > = craft_storage::sessions::Session::load(session_id, storage).map_err(|e| {
        AcpError::resource_not_found(Some(format!("session/{session_id}"))).data(json_str(&e))
    })?;
    let recorded = if Path::new(&session.cwd).is_absolute() {
        Some(PathBuf::from(&session.cwd))
    } else {
        None
    };
    let usage = session.token_usage;
    let by_model = session.usage_by_model().clone();
    let model = session.model.clone();
    let thinking = session
        .meta
        .thinking
        .map(|t| t.setting_string())
        .unwrap_or_else(|| "off".to_string());
    Ok(LoadedHistory {
        history: session.take_messages(),
        recorded_cwd: recorded,
        usage,
        by_model,
        model,
        thinking,
    })
}

fn handle_prompt(
    srv: &mut Server,
    raw: &Value,
    id: &RequestId,
    _params: &AcpParams,
) -> Result<(), AcpError> {
    let req: PromptRequest = parse_params(raw)?;
    let session = srv.session.as_mut().ok_or_else(no_session)?;
    if session
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prompt
        .is_some()
    {
        return Err(AcpError::invalid_request()
            .data(json_str("a prompt is already in progress for this session")));
    }

    let (message, images) = extract_prompt_content(&req.prompt);
    {
        let sid = SessionId::from(session.handle.session_id.to_string());
        if !message.is_empty() {
            session_update(&srv.out_tx, &sid, translate::user_message_chunk(&message));
            if !session.title_sent {
                let title =
                    craft_storage::sessions::generate_title(&[Message::user(message.clone())]);
                session_update(
                    &srv.out_tx,
                    &sid,
                    SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title)),
                );
                session.title_sent = true;
            }
        }
        for source in &images {
            session_update(&srv.out_tx, &sid, translate::user_image_chunk(source));
        }
    }

    let input = AgentInput {
        message,
        mode: session.current_mode.clone(),
        images,
        thinking: parse_thinking(&session.current_thinking),
        ..Default::default()
    };

    session
        .handle
        .input_tx
        .send(input)
        .map_err(|_| AcpError::internal_error().data(json_str("session ended")))?;
    session
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prompt = Some(id.clone());
    Ok(())
}

fn apply_mode(srv: &mut Server, mode_str: &str) -> Result<(), AcpError> {
    let new_mode = methods::mode_id_to_agent_mode(mode_str).ok_or_else(|| {
        AcpError::invalid_params().data(json_str(&format!("unknown mode: {mode_str}")))
    })?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_mode = new_mode;

    let sid = SessionId::from(session.handle.session_id.to_string());
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

fn config_value_str(req: &SetSessionConfigOptionRequest) -> Result<&str, AcpError> {
    match &req.value {
        SessionConfigOptionValue::ValueId { value } => Ok(&value.0),
        _ => {
            Err(AcpError::invalid_params().data(json_str("expected a select config option value")))
        }
    }
}

fn handle_set_config(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: SetSessionConfigOptionRequest = parse_params(raw)?;
    let config_id = req.config_id.0.as_ref();

    if config_id == methods::MODE_CONFIG_ID {
        return handle_set_mode_config(srv, &req);
    }

    if config_id == methods::YOLO_CONFIG_ID {
        return handle_set_yolo_config(srv, &req);
    }

    if config_id == methods::AUTO_REVIEW_CONFIG_ID {
        return handle_set_auto_review_config(srv, &req);
    }

    if config_id == methods::THINKING_CONFIG_ID {
        return handle_set_thinking_config(srv, &req);
    }

    if config_id != methods::MODEL_CONFIG_ID {
        let detail = format!("unknown config option: {}", req.config_id);
        return Err(AcpError::invalid_params().data(json_str(&detail)));
    }

    let spec = config_value_str(&req)?.to_string();
    if !srv.model_policy.allows(&spec) {
        return Err(AcpError::invalid_params().data(json_str(&"model is not allowed by policy")));
    }
    let model =
        Model::from_spec(&spec).map_err(|e| AcpError::invalid_params().data(json_str(&e)))?;

    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session
        .handle
        .model_tx
        .send(model)
        .map_err(|_| AcpError::internal_error().data(json_str("session ended")))?;
    session.current_model = spec.clone();

    if let Some(info) = srv
        .shared_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        info.current_model = spec.clone();
    }

    config_option_response(srv)
}

fn handle_set_thinking_config(
    srv: &mut Server,
    req: &SetSessionConfigOptionRequest,
) -> Result<AgentResponse, AcpError> {
    let value = config_value_str(req)?.to_string();
    if craft_storage::sessions::StoredThinking::parse_setting(&value).is_err() {
        return Err(AcpError::invalid_params().data(json_str("unknown thinking level")));
    }
    let session = srv.session.as_mut().ok_or_else(no_session)?;
    session.current_thinking = value.clone();
    if let Some(info) = srv
        .shared_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        info.thinking = value;
    }
    config_option_response(srv)
}

fn handle_set_mode_config(
    srv: &mut Server,
    req: &SetSessionConfigOptionRequest,
) -> Result<AgentResponse, AcpError> {
    let mode_str = config_value_str(req)?.to_string();
    apply_mode(srv, &mode_str)?;
    config_option_response(srv)
}

fn handle_set_yolo_config(
    srv: &mut Server,
    req: &SetSessionConfigOptionRequest,
) -> Result<AgentResponse, AcpError> {
    let want = config_value_str(req)? == "true";
    let session = srv.session.as_mut().ok_or_else(no_session)?;
    if want != session.handle.permissions.is_yolo() {
        session.handle.permissions.toggle_yolo();
    }
    if let Some(info) = srv
        .shared_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        info.yolo = want;
    }
    config_option_response(srv)
}

fn handle_set_auto_review_config(
    srv: &mut Server,
    req: &SetSessionConfigOptionRequest,
) -> Result<AgentResponse, AcpError> {
    let want = config_value_str(req)? == "true";
    let session = srv.session.as_mut().ok_or_else(no_session)?;
    if want != session.handle.permissions.is_auto_review() {
        session.handle.permissions.toggle_auto_review();
    }
    if let Some(info) = srv
        .shared_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        info.auto_review = want;
    }
    config_option_response(srv)
}

/// Build the full `set_config_option` response from the live session's
/// current mode/model/permission state. Shared by the mode, yolo, and
/// auto-review config handlers so the returned options stay in lockstep.
fn config_option_response(srv: &Server) -> Result<AgentResponse, AcpError> {
    let session = srv.session.as_ref().ok_or_else(no_session)?;
    let mode_id = match &session.current_mode {
        AgentMode::Plan(_) => "plan",
        AgentMode::Flow(_) => "flow",
        AgentMode::Build => "build",
    };
    let current_model = session.current_model.clone();
    let thinking = session.current_thinking.clone();
    let yolo = session.handle.permissions.is_yolo();
    let auto_review = session.handle.permissions.is_auto_review();
    let specs = srv.model_specs.lock().unwrap_or_else(|e| e.into_inner());
    Ok(AgentResponse::SetSessionConfigOptionResponse(
        SetSessionConfigOptionResponse::new(vec![
            methods::mode_config_option(mode_id),
            methods::model_config_option(&current_model, &specs),
            methods::thinking_config_option(&thinking),
            methods::yolo_config_option(yolo),
            methods::auto_review_config_option(auto_review),
        ]),
    ))
}

pub(crate) fn parse_thinking(value: &str) -> craft_providers::ThinkingConfig {
    craft_storage::sessions::StoredThinking::parse_setting(value)
        .map(Into::into)
        .unwrap_or_default()
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
            AcpSessionInfo::new(s.id.to_string(), s.cwd)
                .title(s.title)
                .updated_at(epoch_to_iso8601(s.updated_at))
        })
        .collect();
    Ok(AgentResponse::ListSessionsResponse(
        ListSessionsResponse::new(sessions),
    ))
}

async fn handle_close_session(srv: &mut Server, raw: &Value) -> Result<AgentResponse, AcpError> {
    let req: CloseSessionRequest = parse_params(raw)?;
    if srv
        .session
        .as_ref()
        .is_some_and(|s| s.handle.session_id.as_str() == req.session_id.0.as_ref())
    {
        if let Some(session) = srv.session.take() {
            teardown_session(&srv.out_tx, &srv.lua, session).await;
        }
        *srv.shared_session.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    Ok(AgentResponse::CloseSessionResponse(
        CloseSessionResponse::new(),
    ))
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
                // Any answers still in flight belong to the cancelled turn,
                // so forget their ids and let them be dropped on arrival.
                session
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .asks
                    .clear();
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

    let Some(id) = raw.get("id").and_then(Value::as_i64) else {
        return;
    };
    let kind = session
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .asks
        .remove(&id);
    let Some(kind) = kind else {
        warn!(id, "response for an unknown request id");
        return;
    };
    match kind {
        AskKind::Permission => {
            let _ = session
                .handle
                .answer_tx
                .send(permission_answer(raw).encode());
        }
        // The waiting question dispatch decodes this; an error response
        // parses to nothing and counts as a dismissal.
        AskKind::Elicitation => {
            let answer = elicitation::answer_from_response(
                &raw.get("result")
                    .cloned()
                    .unwrap_or(Value::Null)
                    .to_string(),
            );
            let _ = session
                .handle
                .answer_tx
                .send(craft_agent::tools::question::encode_answer(&answer));
        }
    }
}

/// A response we cannot read still has to answer the agent, or the tool waits
/// on a permission that will never come.
fn permission_answer(raw: &Value) -> craft_agent::permissions::PermissionAnswer {
    match raw
        .get("result")
        .map(|result| serde_json::from_value::<RequestPermissionResponse>(result.clone()))
    {
        Some(Ok(resp)) => permissions::outcome_to_answer(&resp.outcome),
        _ => craft_agent::permissions::PermissionAnswer::Deny,
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

#[allow(clippy::too_many_arguments)]
fn start_event_pump(
    event_rx: Receiver<Envelope>,
    session_id: SessionRef,
    out_tx: Sender<Value>,
    pending: PendingState,
    next_request_id: Arc<AtomicI64>,
    cwd: PathBuf,
    home: Option<PathBuf>,
    initial_cost: Option<f64>,
) {
    tokio::spawn(async move {
        let sid = SessionId::from(session_id.to_string());
        let mut cost_total = initial_cost;
        let mut sub_buffers: HashMap<String, String> = HashMap::new();

        while let Ok(Envelope {
            event, subagent, ..
        }) = event_rx.recv_async().await
        {
            // Subagent stream events stay out of the transcript, but their
            // turns still spend session money.
            if let AgentEvent::TurnComplete(tc) = &event {
                add_cost(&mut cost_total, tc.cost);
            }
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
                        let buf = sub_buffers.entry(parent_id.clone()).or_default();
                        buf.push_str(text);
                        session_update(
                            &out_tx,
                            &sid,
                            translate::subagent_content_update(parent_id, buf),
                        );
                    }
                    AgentEvent::ThinkingDelta { .. } => continue,
                    AgentEvent::ToolStart(ts) => {
                        let buf = sub_buffers.entry(parent_id.clone()).or_default();
                        buf.push_str(&translate::subagent_breadcrumb(&ts.summary));
                        session_update(
                            &out_tx,
                            &sid,
                            translate::subagent_content_update(parent_id, buf),
                        );
                    }
                    AgentEvent::ToolDone(event) if event.is_error => {
                        let buf = sub_buffers.entry(parent_id.clone()).or_default();
                        buf.push_str("\n  ");
                        buf.push_str(translate::SUBAGENT_FAILURE_MARKER);
                        buf.push('\n');
                        session_update(
                            &out_tx,
                            &sid,
                            translate::subagent_content_update(parent_id, buf),
                        );
                    }
                    // Asks must reach the client: the subagent blocks on the
                    // session's shared answer channel, so dropping one wedges
                    // the whole turn.
                    AgentEvent::PermissionRequest {
                        id, tool, scopes, ..
                    } => forward_permission_ask(
                        &out_tx,
                        &pending,
                        &next_request_id,
                        &sid,
                        id,
                        &tool.to_string(),
                        scopes,
                    ),
                    AgentEvent::QuestionRequest { id, questions } => forward_question_ask(
                        &out_tx,
                        &pending,
                        &next_request_id,
                        &sid,
                        id,
                        questions,
                    ),
                    _ => {}
                }
                continue;
            }

            if let Some(todos) = todo_update_from_event(&event) {
                emit_todo_update(&out_tx, &sid, todos);
            }

            let update = match event {
                AgentEvent::TextDelta { text } => translate::text_delta(&text),
                AgentEvent::ThinkingDelta { text } => translate::thinking_delta(&text),
                AgentEvent::ToolPending { id, name } => translate::tool_pending(&id, &name),
                AgentEvent::ToolStart(event) => {
                    translate::tool_start(&event, &cwd, home.as_deref())
                }
                AgentEvent::ToolOutput { id, content } => translate::tool_output(&id, &content),
                AgentEvent::ToolDone(event) => translate::tool_done(&event, &cwd, home.as_deref()),
                AgentEvent::BatchProgress(event) => {
                    if event.status != BatchToolStatus::InProgress {
                        continue;
                    }
                    translate::batch_inner_start(&event)
                }
                AgentEvent::PermissionRequest {
                    id, tool, scopes, ..
                } => {
                    forward_permission_ask(
                        &out_tx,
                        &pending,
                        &next_request_id,
                        &sid,
                        &id,
                        &tool.to_string(),
                        &scopes,
                    );
                    continue;
                }
                AgentEvent::QuestionRequest { id, questions } => {
                    forward_question_ask(
                        &out_tx,
                        &pending,
                        &next_request_id,
                        &sid,
                        &id,
                        &questions,
                    );
                    continue;
                }
                AgentEvent::Done { reason, .. } => {
                    let id = pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .prompt
                        .take();
                    if let Some(id) = id {
                        let resp = PromptResponse::new(translate::map_done_reason(reason));
                        send(
                            &out_tx,
                            Response::new(id, Ok(AgentResponse::PromptResponse(resp))),
                        );
                    } else {
                        notify_turn_done(&out_tx, &sid);
                    }
                    continue;
                }
                AgentEvent::Error { message } => {
                    let id = pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .prompt
                        .take();
                    if let Some(id) = id {
                        let error = AcpError::internal_error().data(Value::String(message));
                        send(&out_tx, Response::<AgentResponse>::new(id, Err(error)));
                    } else {
                        notify_turn_done(&out_tx, &sid);
                    }
                    continue;
                }
                AgentEvent::TurnComplete(tc) => translate::usage_update(&tc, cost_total),
                AgentEvent::FlowProgress { progress } => {
                    match translate::flow_progress(&progress) {
                        Some(u) => u,
                        None => continue,
                    }
                }
                AgentEvent::AdvisorNote { severity, message } => {
                    translate::advisor_note(&severity, &message)
                }
                AgentEvent::Info { message } => translate::info(&message),
                AgentEvent::AutoReviewStart { id, .. } => translate::auto_review_start(&id),
                AgentEvent::AutoReviewDecision {
                    id,
                    verdict,
                    risk,
                    rationale,
                    ..
                } => translate::auto_review_decision(&id, &verdict, &risk, &rationale),
                _ => continue,
            };
            session_update(&out_tx, &sid, update);
        }
    });
}

pub(crate) fn send(out_tx: &Sender<Value>, msg: impl Serialize) {
    match serde_json::to_value(JsonRpcMessage::wrap(msg)) {
        Ok(json) => {
            if out_tx.send(json).is_err() {
                warn!("ACP: failed to send message, channel closed");
            }
        }
        Err(e) => warn!(error = %e, "ACP: failed to serialize message"),
    }
}

pub(crate) fn session_update(out_tx: &Sender<Value>, sid: &SessionId, update: SessionUpdate) {
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

/// If this event is a `todo_write` tool start, extract the structured `todos`
/// array so the ACP server can forward it as a `session/todo_update`
/// notification (ACP has no dedicated todo `SessionUpdate` variant).
fn todo_update_from_event(event: &AgentEvent) -> Option<&serde_json::Value> {
    match event {
        AgentEvent::ToolStart(ts) if ts.tool.as_ref() == TODO_WRITE_TOOL => {
            ts.raw_input.as_ref().and_then(|v| v.get("todos"))
        }
        _ => None,
    }
}

/// Tell the client a turn that was started fire-and-forget (via
/// `_craft/command/run` or `_craft/meta/prompt`, which have no `session/prompt`
/// request to resolve) has ended, so it can clear its busy state.
fn notify_turn_done(out_tx: &Sender<Value>, sid: &SessionId) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "_craft/session/turn_done",
        "params": { "sessionId": sid.0 }
    });
    let _ = out_tx.send(msg);
}

fn emit_todo_update(out_tx: &Sender<Value>, sid: &SessionId, todos: &serde_json::Value) {
    // During the `_craft/` migration we emit both the legacy non-prefixed name
    // (kept so older clients keep rendering todos) and the new spec-blessed
    // `_craft/` prefixed name. The legacy emitter is dropped in the release
    // after desktop ships the new reader.
    let params = serde_json::json!({ "sessionId": sid, "todos": todos });
    for method in [TODO_UPDATE_METHOD, TODO_UPDATE_METHOD_LEGACY] {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        if out_tx.send(msg).is_err() {
            warn!("ACP: failed to send todo_update, channel closed");
        }
    }
}

/// Sends a request the client must answer and records it as an outstanding
/// ask, registered before sending so the response can never race past us.
fn ask_client(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    next_request_id: &AtomicI64,
    kind: AskKind,
    request: AgentRequest,
) -> i64 {
    let id = next_request_id.fetch_add(1, Ordering::Relaxed) + 1;
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .asks
        .insert(id, kind);
    send(
        out_tx,
        Request {
            id: RequestId::Number(id),
            method: Arc::from(request.method()),
            params: Some(request),
        },
    );
    id
}

fn forward_permission_ask(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    next_request_id: &Arc<AtomicI64>,
    sid: &SessionId,
    id: &str,
    tool: &str,
    scopes: &[String],
) {
    let fields = ToolCallUpdateFields::new().title(format!("{tool}: {}", scopes.join(", ")));
    let request = AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
        sid.clone(),
        ToolCallUpdate::new(ToolCallId::new(id), fields),
        permissions::permission_options(),
    ));
    ask_client(
        out_tx,
        pending,
        next_request_id,
        AskKind::Permission,
        request,
    );
}

fn forward_question_ask(
    out_tx: &Sender<Value>,
    pending: &PendingState,
    next_request_id: &Arc<AtomicI64>,
    sid: &SessionId,
    id: &str,
    questions: &[craft_agent::types::QuestionSpec],
) {
    // The standardized `elicitation/create` form is the only question channel;
    // every client we ship against supports it.
    if let Ok(request) = elicitation::form_request(sid.0.as_ref(), Some(id.to_string()), questions)
    {
        ask_client(
            out_tx,
            pending,
            next_request_id,
            AskKind::Elicitation,
            AgentRequest::CreateElicitationRequest(request),
        );
    }
}

pub(crate) fn no_session() -> AcpError {
    AcpError::invalid_request().data(json_str("no active session"))
}

pub(crate) fn parse_params<T: serde::de::DeserializeOwned>(raw: &Value) -> Result<T, AcpError> {
    serde_json::from_value(raw.get("params").cloned().unwrap_or(Value::Null))
        .map_err(|e| AcpError::invalid_params().data(json_str(&e)))
}

fn json_str(e: &(impl std::fmt::Display + ?Sized)) -> Value {
    Value::String(e.to_string())
}

#[cfg(test)]
mod tests {
    use craft_agent::ToolOutput;
    use craft_agent::permissions::PermissionAnswer;
    use craft_agent::types::ToolStartEvent;
    use craft_providers::{ContentBlock as MsgBlock, Role, TokenUsage};
    use craft_storage::StateDir;
    use craft_storage::sessions::Session;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const ANSWERED_ID: i64 = 1001;
    const UNKNOWN_ID: i64 = 1002;

    fn allow_once(id: i64) -> Value {
        serde_json::json!({
            "id": id,
            "result": { "outcome": { "outcome": "selected", "optionId": "allow_once" } },
        })
    }

    #[test_case(allow_once(ANSWERED_ID), PermissionAnswer::AllowOnce ; "selected_option")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "outcome": { "outcome": "cancelled" } } }), PermissionAnswer::Deny ; "cancelled_outcome")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "result": { "nonsense": true } }), PermissionAnswer::Deny ; "unparsable_result")]
    #[test_case(serde_json::json!({ "id": ANSWERED_ID, "error": { "code": -32603 } }), PermissionAnswer::Deny ; "jsonrpc_error")]
    fn permission_answer_maps_response(raw: Value, expected: PermissionAnswer) {
        assert_eq!(permission_answer(&raw), expected);
    }

    fn server_awaiting_answer() -> (Server, Receiver<String>) {
        server_with_ask(AskKind::Permission)
    }

    fn server_with_ask(kind: AskKind) -> (Server, Receiver<String>) {
        server_with_asks(HashMap::from([(ANSWERED_ID, kind)]))
    }

    fn server_with_asks(asks: HashMap<i64, AskKind>) -> (Server, Receiver<String>) {
        let (answer_tx, answer_rx) = flume::unbounded();
        let (event_tx, event_rx) = flume::unbounded();
        let handle = InteractiveHandle {
            event_rx,
            raw_event_tx: event_tx,
            tool_names: Vec::new(),
            input_tx: flume::unbounded().0,
            answer_tx,
            cancel_tx: flume::unbounded().0,
            model_tx: flume::unbounded().0,
            session_id: SessionRef::from(CraftId::generate()),
            permissions: Arc::new(craft_agent::permissions::PermissionManager::new(
                craft_config::PermissionsConfig::default(),
                PathBuf::from("/project"),
                Arc::default(),
            )),
            task: tokio::spawn(async {}),
        };
        let server = Server {
            out_tx: flume::unbounded().0,
            model_specs: Arc::new(Mutex::new(Vec::new())),
            model_policy: Arc::new(craft_config::ModelPolicy::default()),
            shared_session: Arc::new(Mutex::new(None)),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            client_caps: Arc::new(ClientCaps::new()),
            next_request_id: Arc::new(AtomicI64::new(FIRST_OUTGOING_REQUEST_ID)),
            session: Some(SessionState {
                handle,
                mcp: None,
                current_mode: AgentMode::Build,
                current_model: String::new(),
                current_thinking: "off".to_string(),
                pending: Arc::new(Mutex::new(Pending {
                    asks,
                    ..Default::default()
                })),
                title_sent: false,
                cwd: PathBuf::from("/project"),
            }),
            lua: craft_lua::EventHandle::disconnected_for_test(),
        };
        (server, answer_rx)
    }

    #[tokio::test]
    async fn only_the_outstanding_request_id_is_answered() {
        let (srv, answer_rx) = server_awaiting_answer();

        handle_incoming_response(&srv, &allow_once(UNKNOWN_ID));
        assert!(answer_rx.is_empty(), "an unknown id is dropped");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert_eq!(
            answer_rx.try_recv().ok(),
            Some(PermissionAnswer::AllowOnce.encode())
        );

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(
            answer_rx.is_empty(),
            "a replayed answer cannot land on the next request"
        );
    }

    #[tokio::test]
    async fn elicitation_response_forwards_the_encoded_answer() {
        let (srv, answer_rx) = server_with_ask(AskKind::Elicitation);
        let raw = serde_json::json!({
            "id": ANSWERED_ID,
            "result": { "action": "accept", "content": { "q1": "axum" } },
        });

        handle_incoming_response(&srv, &raw);
        let forwarded = answer_rx.try_recv().unwrap();
        let answer = craft_agent::tools::question::decode_answer(&forwarded).unwrap();
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["axum".to_string()]]);
    }

    #[tokio::test]
    async fn cancel_drops_the_outstanding_permission_request() {
        let (srv, answer_rx) = server_awaiting_answer();
        handle_notification(&srv, "session/cancel");

        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));
        assert!(answer_rx.is_empty(), "the cancelled turn owns that answer");
    }

    #[tokio::test]
    async fn concurrent_asks_each_get_their_answer() {
        let (srv, answer_rx) = server_with_asks(HashMap::from([
            (ANSWERED_ID, AskKind::Permission),
            (UNKNOWN_ID, AskKind::Elicitation),
        ]));

        handle_incoming_response(
            &srv,
            &serde_json::json!({
                "id": UNKNOWN_ID,
                "result": { "action": "accept", "content": { "q1": "axum" } },
            }),
        );
        handle_incoming_response(&srv, &allow_once(ANSWERED_ID));

        let elicitation = answer_rx.try_recv().unwrap();
        assert!(elicitation.contains("axum"), "first answer was not dropped");
        assert_eq!(
            answer_rx.try_recv().ok(),
            Some(PermissionAnswer::AllowOnce.encode())
        );
        assert!(answer_rx.is_empty());
    }

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
                ..Default::default()
            },
        ];
        let mut session: Session<Message, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.replace_messages(messages.clone());
        session.token_usage = TokenUsage {
            input: 1_000,
            output: 200,
            ..Default::default()
        };
        session.save(&dir).unwrap();

        let loaded = load_history_from(&dir, session.id.id()).unwrap();
        assert_eq!(loaded.model, "anthropic/test-model");
        assert_eq!(loaded.thinking, "off");
        assert_eq!(
            serde_json::to_value(&loaded.history).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
        assert_eq!(loaded.recorded_cwd, Some(PathBuf::from("/project")));
        assert_eq!(loaded.usage, session.token_usage);
    }

    /// Resuming must bill what the session actually paid. If `by_model` came
    /// back empty or lost its recorded costs, ACP would re-price the restored
    /// total against today's table and disagree with the TUI.
    #[test]
    fn load_history_prices_a_resumed_session_at_what_it_paid() {
        const SELECTED_SPEC: &str = "anthropic/claude-sonnet-4-5";
        /// Neither resolves in the price tables, so nothing can re-price a
        /// restored session back onto the recorded number by luck.
        const RETIRED_SPEC: &str = "retired-vendor/retired-model-9000";
        const RETIRED_MODEL_ID: &str = "retired-model-9000";
        const RECORDED_COST: f64 = 1.25;

        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, ToolOutput> =
            Session::new(RETIRED_SPEC, "/project");
        session.token_usage = TokenUsage {
            input: 1_000_000,
            output: 200_000,
            ..Default::default()
        };
        session.add_model_usage(
            RETIRED_MODEL_ID,
            StoredTokenUsage {
                input: 1_000_000,
                output: 200_000,
                cost: Some(RECORDED_COST),
                ..Default::default()
            },
        );
        session.save(&dir).unwrap();

        let mut loaded = load_history_from(&dir, session.id.id()).unwrap();
        assert_eq!(
            loaded.by_model[RETIRED_MODEL_ID].cost,
            Some(RECORDED_COST),
            "the per-model breakdown survives the file"
        );

        // Mirrors `session/load`: the recorded spec no longer parses, so the
        // selected model stands in, and that must not change the bill.
        let recorded_model = Model::from_spec(&loaded.model)
            .unwrap_or_else(|_| Model::from_spec(SELECTED_SPEC).expect("a shipped model"));
        assert_eq!(
            settle_session(
                &loaded.usage,
                &mut loaded.by_model,
                &recorded_model,
                RESTORED_FAST
            ),
            Some(RECORDED_COST)
        );
    }

    #[test]
    fn load_history_restores_stored_thinking() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "/project");
        session.meta.thinking = Some(craft_storage::sessions::StoredThinking::Effort {
            level: craft_storage::sessions::Effort::High,
        });
        session.save(&dir).unwrap();

        let thinking = load_history_from(&dir, session.id.id()).unwrap().thinking;
        assert_eq!(thinking, "high");
    }

    #[test]
    fn load_history_records_absolute_cwd_only() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mut session: Session<Message, TokenUsage, ToolOutput> =
            Session::new("anthropic/test-model", "relative/project");
        session.save(&dir).unwrap();
        let loaded = load_history_from(&dir, session.id.id()).unwrap();
        assert_eq!(loaded.recorded_cwd, None);
    }

    #[test]
    fn load_missing_session_is_resource_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let err = load_history_from(&dir, CraftId::generate()).unwrap_err();
        assert_eq!(err.code, AcpError::resource_not_found(None).code);
    }

    #[test]
    fn merge_configs_keeps_local_server_on_name_conflict() {
        let local_raw = craft_agent::mcp::config::RawServerConfig {
            enabled: false,
            timeout: 1234,
            transport: craft_agent::mcp::config::RawTransport::Stdio(
                craft_agent::mcp::config::RawStdioFields {
                    command: vec!["/local/mcp".into()],
                    environment: HashMap::new(),
                },
            ),
        };
        let local = McpConfig {
            mcp: HashMap::from([("shared".into(), local_raw)]),
            origins: HashMap::new(),
        };
        let client = ServerConfig {
            name: "shared".into(),
            timeout: Duration::from_secs(30),
            transport: Transport::Stdio {
                program: "/client/mcp".into(),
                args: vec![],
                environment: HashMap::new(),
            },
        };

        let merged = merge_configs(&local, &[client]);
        let raw = merged.mcp.get("shared").unwrap();
        assert!(
            !raw.enabled,
            "a client-injected server must not revive a disabled local one"
        );
        assert_eq!(raw.timeout, 1234);

        let fresh = merge_configs(
            &McpConfig {
                mcp: HashMap::new(),
                origins: HashMap::new(),
            },
            &[ServerConfig {
                name: "shared".into(),
                timeout: Duration::from_secs(30),
                transport: Transport::Stdio {
                    program: "/client/mcp".into(),
                    args: vec![],
                    environment: HashMap::new(),
                },
            }],
        );
        assert!(fresh.mcp.get("shared").unwrap().enabled);
        assert_eq!(
            fresh.origins.get("shared"),
            Some(&PathBuf::from("acp-client"))
        );
    }

    fn tool_start(name: &str, raw_input: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "tu-1".into(),
            tool: Arc::from(name),
            summary: "summary".into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: Some(raw_input),
            output: None,
        }))
    }

    #[test]
    fn todo_update_extracts_todos_from_todo_write_tool_start() {
        let event = tool_start(
            "todo_write",
            serde_json::json!({
                "todos": [
                    { "id": "T1", "content": "task one", "status": "pending" }
                ]
            }),
        );
        let todos = todo_update_from_event(&event).unwrap();
        assert_eq!(todos[0]["id"], "T1");
        assert_eq!(todos[0]["status"], "pending");
    }

    #[test]
    fn todo_update_ignores_non_todo_write_tools() {
        let event = tool_start("bash", serde_json::json!({ "command": "ls" }));
        assert!(todo_update_from_event(&event).is_none());
    }

    #[test]
    fn todo_update_ignores_todo_write_without_raw_input() {
        let event = AgentEvent::ToolStart(Box::new(ToolStartEvent {
            id: "tu-1".into(),
            tool: Arc::from("todo_write"),
            summary: "summary".into(),
            render_header: None,
            annotation: None,
            input: None,
            raw_input: None,
            output: None,
        }));
        assert!(todo_update_from_event(&event).is_none());
    }

    #[tokio::test]
    async fn event_pump_forwards_advisor_events_as_thought_updates() {
        let (event_tx, event_rx) = flume::unbounded::<craft_agent::Envelope>();
        let (out_tx, out_rx) = flume::unbounded::<Value>();
        start_event_pump(
            event_rx,
            SessionRef::from(CraftId::generate()),
            out_tx,
            Arc::new(Mutex::new(Pending::default())),
            Arc::new(AtomicI64::new(FIRST_OUTGOING_REQUEST_ID)),
            PathBuf::from("/project"),
            None,
            None,
        );

        let envelope = |event: AgentEvent| craft_agent::Envelope {
            event,
            subagent: None,
            run_id: 0,
        };
        event_tx
            .send(envelope(AgentEvent::AdvisorNote {
                severity: "concern".into(),
                message: "missing error handling".into(),
            }))
            .unwrap();
        event_tx
            .send(envelope(AgentEvent::Info {
                message: "advisor reviewing recent activity…".into(),
            }))
            .unwrap();

        let note = out_rx.recv_async().await.unwrap();
        assert_eq!(note["method"], "session/update");
        let note_update = &note["params"]["update"];
        assert_eq!(note_update["sessionUpdate"], "agent_thought_chunk");
        let note_text = note_update["content"]["text"].as_str().unwrap();
        assert!(note_text.contains("advisor"), "got {note_text}");
        assert!(
            note_text.contains("missing error handling"),
            "got {note_text}"
        );

        let info = out_rx.recv_async().await.unwrap();
        assert_eq!(
            info["params"]["update"]["sessionUpdate"],
            "agent_thought_chunk"
        );
        let info_text = info["params"]["update"]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(info_text.contains("advisor reviewing"), "got {info_text}");
    }

    #[test]
    fn emit_todo_update_sends_expected_payload() {
        let (tx, rx) = flume::unbounded::<Value>();
        let sid = SessionId::from("sess-1".to_string());
        let todos = serde_json::json!([{ "id": "T1", "content": "x", "status": "completed" }]);
        emit_todo_update(&tx, &sid, &todos);
        let msg = rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert_eq!(msg["method"], TODO_UPDATE_METHOD);
        assert_eq!(msg["params"]["sessionId"], "sess-1");
        assert_eq!(msg["params"]["todos"][0]["id"], "T1");
    }
}
