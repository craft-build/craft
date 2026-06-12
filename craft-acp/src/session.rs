//! Per-session state and prompt-loop runner for the ACP server.
//!
//! Each session keeps its own `History`, `PluginHost`, tool registry and
//! optional MCP handle so successive `session/prompt` calls can build on
//! prior turns.

use agent_client_protocol::ConnectionTo;
use agent_client_protocol::role::acp::Client;
use agent_client_protocol::schema::ContentBlock;
use agent_client_protocol::schema::CurrentModeUpdate;
use agent_client_protocol::schema::FileSystemCapabilities;
use agent_client_protocol::schema::PromptResponse;
use agent_client_protocol::schema::RequestPermissionRequest;
use agent_client_protocol::schema::SessionId;
use agent_client_protocol::schema::SessionMode;
use agent_client_protocol::schema::SessionModeId;
use agent_client_protocol::schema::SessionModeState;
use agent_client_protocol::schema::SessionNotification;
use agent_client_protocol::schema::SessionUpdate;
use agent_client_protocol::schema::StopReason;
use agent_client_protocol::util::internal_error;
use craft_agent::Agent;
use craft_agent::AgentConfig;
use craft_agent::AgentInput;
use craft_agent::AgentMode;
use craft_agent::AgentParams;
use craft_agent::AgentRunParams;
use craft_agent::Envelope;
use craft_agent::EventSender;
use craft_agent::PermissionsConfig;
use craft_agent::ToolOutputLines;
use craft_agent::agent::History;
use craft_agent::cancel::CancelToken;
use craft_agent::cancel::CancelTrigger;
use craft_agent::mcp::McpHandle;
use craft_agent::permissions::PermissionAnswer;
use craft_agent::permissions::PermissionManager;
use craft_agent::tools::DescriptionContext;
use craft_agent::tools::FileReadTracker;
use craft_agent::tools::FsBackend;
use craft_agent::tools::LocalFs;
use craft_agent::tools::ToolFilter;
use craft_agent::tools::ToolRegistry;
use craft_lua::LocalTerminal;
use craft_lua::PluginHost;
use craft_lua::TerminalBackend;
use craft_providers::Timeouts;
use craft_providers::model::Model;
use craft_providers::provider::Provider;
use craft_providers::provider::{self};
use craft_storage::StateDir;
use std::collections::HashMap;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

use crate::event_bridge;
use crate::fs_proxy::AcpFs;
use crate::permissions;
use crate::prompt;
use crate::terminal_proxy::AcpTerminal;

const SESSION_NOT_FOUND_MSG: &str = "session not found";
const UNKNOWN_MODE_MSG: &str = "unknown mode id";

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";

/// Shared runtime context for all sessions: provider config, model, timeouts.
///
/// Built once at `run_stdio` startup; cloned (via Arc) into every prompt task.
#[derive(Clone)]
pub struct Runtime {
    pub model: Model,
    pub config: AgentConfig,
    pub compression: craft_config::CompressionConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: Timeouts,
    pub state_dir: StateDir,
    pub fs_caps: Arc<RwLock<FileSystemCapabilities>>,
    pub terminal_cap: Arc<RwLock<bool>>,
    pub plugins_config: craft_config::PluginsConfig,
}

/// Per-session in-memory state. All state survives across prompts.
pub struct SessionState {
    pub cwd: PathBuf,
    pub mode: AgentMode,
    pub history: History,
    pub cancel_trigger: Option<CancelTrigger>,
    pub fs: Option<Arc<dyn FsBackend>>,
    pub registry: Option<Arc<ToolRegistry>>,
    pub plugin_host: Option<PluginHost>,
    pub mcp_handle: Option<McpHandle>,
}

impl SessionState {
    pub fn fresh(cwd: PathBuf) -> Self {
        Self {
            cwd,
            mode: AgentMode::Build,
            history: History::new(Vec::new()),
            cancel_trigger: None,
            fs: None,
            registry: None,
            plugin_host: None,
            mcp_handle: None,
        }
    }
}

/// Map of session id (the `Arc<str>` inside `SessionId`) to state.
pub type Sessions = Arc<Mutex<HashMap<Arc<str>, SessionState>>>;

/// Allocate a new session entry. Returns a freshly generated session id.
pub async fn new_session(sessions: &Sessions, cwd: PathBuf) -> Arc<str> {
    let id: Arc<str> = Arc::from(uuid::Uuid::new_v4().to_string());
    sessions.lock().await.insert(id.clone(), SessionState::fresh(cwd));
    id
}

/// Insert a pre-populated session (used by `session/load` to reattach
/// persisted history under a freshly assigned id).
pub async fn insert_loaded_session(sessions: &Sessions, id: Arc<str>, state: SessionState) {
    sessions.lock().await.insert(id, state);
}

/// ACP `SessionModeState` advertised to the client at session creation.
///
/// `build` is the default; `plan` activates plan-mode mutation gating.
pub fn available_modes() -> SessionModeState {
    SessionModeState::new(
        SessionModeId::new(MODE_BUILD),
        vec![
            SessionMode::new(MODE_BUILD, "Build"),
            SessionMode::new(MODE_PLAN, "Plan"),
        ],
    )
}

/// Switch the session's mode. Cancels any in-flight prompt before switching
/// and emits a `current_mode_update` notification.
pub async fn set_mode(
    sessions: &Sessions,
    runtime: &Runtime,
    session_id: &SessionId,
    mode_id: &str,
    cx: &ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    let new_mode = build_mode(mode_id, &runtime.state_dir)?;
    let trigger = {
        let mut map = sessions.lock().await;
        let Some(state) = map.get_mut(&session_id.0) else {
            return Err(internal_error(SESSION_NOT_FOUND_MSG));
        };
        state.mode = new_mode;
        state.cancel_trigger.take()
    };
    if let Some(trigger) = trigger {
        trigger.cancel();
    }
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(
            mode_id.to_owned(),
        ))),
    ))?;
    Ok(())
}

fn build_mode(
    mode_id: &str,
    state_dir: &StateDir,
) -> Result<AgentMode, agent_client_protocol::Error> {
    match mode_id {
        MODE_BUILD => Ok(AgentMode::Build),
        MODE_PLAN => {
            let path = craft_storage::plans::new_plan_path(state_dir)
                .map_err(|e| internal_error(format!("allocate plan path: {e}")))?;
            Ok(AgentMode::Plan(path))
        }
        other => Err(internal_error(format!("{UNKNOWN_MODE_MSG}: {other}"))),
    }
}

/// Run a single prompt against a session. Streams `SessionUpdate` notifications
/// to the client and returns the final `PromptResponse`.
pub async fn run_prompt(
    sessions: &Sessions,
    runtime: &Runtime,
    session_id: SessionId,
    prompt_blocks: Vec<ContentBlock>,
    cx: ConnectionTo<Client>,
) -> Result<PromptResponse, agent_client_protocol::Error> {
    let lowered = prompt::lower(&prompt_blocks)?;
    let id_arc: Arc<str> = session_id.0.clone();

    ensure_session_resources(sessions, runtime, &cx, &session_id).await?;

    let (cwd, mode, history, fs, registry, mcp_handle) = {
        let mut map = sessions.lock().await;
        let Some(state) = map.get_mut(&id_arc) else {
            return Err(internal_error(SESSION_NOT_FOUND_MSG));
        };
        let cwd = state.cwd.clone();
        let mode = state.mode.clone();
        let history = mem::replace(&mut state.history, History::new(Vec::new()));
        let fs = state.fs.clone().expect("fs initialised");
        let registry = state.registry.clone().expect("registry initialised");
        let mcp_handle = state.mcp_handle.clone();
        (cwd, mode, history, fs, registry, mcp_handle)
    };

    let (cancel_trigger, cancel_token) = CancelToken::new();
    {
        let mut map = sessions.lock().await;
        if let Some(state) = map.get_mut(&id_arc) {
            state.cancel_trigger = Some(cancel_trigger);
        }
    }

    let (raw_tx, event_rx) = flume::unbounded::<Envelope>();
    let (response_tx, response_rx) = flume::unbounded::<String>();
    let response_rx = Arc::new(Mutex::new(response_rx));
    let runtime_clone = runtime.clone();
    let session_id_str = id_arc.to_string();

    let agent_task = tokio::spawn(run_agent(
        runtime_clone,
        cwd,
        mode,
        history,
        lowered.text,
        lowered.images,
        cancel_token,
        raw_tx,
        session_id_str,
        response_rx,
        fs,
        registry,
        mcp_handle,
    ));

    while let Ok(envelope) = event_rx.recv_async().await {
        dispatch_event(&envelope, &session_id, &cx, &response_tx)?;
        if let craft_agent::AgentEvent::Error { message } = &envelope.event {
            tracing::warn!(session_id = %id_arc, error = message, "agent reported error");
        }
    }

    let (stop_reason, history_after) = match agent_task.await {
        Ok((reason, history)) => (reason, history),
        Err(join_err) => {
            tracing::error!(error = %join_err, "agent task panicked");
            (StopReason::Refusal, History::new(Vec::new()))
        }
    };

    {
        let mut map = sessions.lock().await;
        if let Some(state) = map.get_mut(&id_arc) {
            state.cancel_trigger = None;
            state.history = history_after;
        }
    }

    Ok(PromptResponse::new(stop_reason))
}

/// Lazily build per-session resources (fs backend, plugin host, MCP handle).
/// Idempotent: re-entries skip work that has already been done.
async fn ensure_session_resources(
    sessions: &Sessions,
    runtime: &Runtime,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
) -> Result<(), agent_client_protocol::Error> {
    let needs_init = {
        let map = sessions.lock().await;
        let state = map
            .get(&session_id.0)
            .ok_or_else(|| internal_error(SESSION_NOT_FOUND_MSG))?;
        state.fs.is_none() || state.registry.is_none()
    };
    if !needs_init {
        return Ok(());
    }

    let cwd = {
        let map = sessions.lock().await;
        map.get(&session_id.0)
            .ok_or_else(|| internal_error(SESSION_NOT_FOUND_MSG))?
            .cwd
            .clone()
    };

    let fs = build_fs_backend(runtime, cx, session_id.clone()).await;
    let (registry, plugin_host) = build_plugin_host(runtime, cx, session_id, &cwd).await;
    let (mcp_handle, mcp_errors) = craft_agent::mcp::start(&cwd).await;
    if !mcp_errors.is_empty() {
        tracing::warn!(?mcp_errors, "MCP config had errors");
    }

    let mut map = sessions.lock().await;
    let state = map
        .get_mut(&session_id.0)
        .ok_or_else(|| internal_error(SESSION_NOT_FOUND_MSG))?;
    state.fs = Some(fs);
    state.registry = Some(registry);
    state.plugin_host = Some(plugin_host);
    state.mcp_handle = mcp_handle;
    Ok(())
}

/// Cancel the in-flight prompt for a session. No-op if there is none.
pub async fn cancel(sessions: &Sessions, session_id: &SessionId) {
    let mut map = sessions.lock().await;
    if let Some(state) = map.get_mut(&session_id.0)
        && let Some(trigger) = state.cancel_trigger.take()
    {
        trigger.cancel();
    }
}

/// Dispatch a single `Envelope`: emit any `SessionUpdate`s and, for permission
/// requests, spawn an ACP request to the client whose response is forwarded
/// to `response_tx` for the `PermissionManager` to consume.
fn dispatch_event(
    envelope: &Envelope,
    session_id: &SessionId,
    cx: &ConnectionTo<Client>,
    response_tx: &flume::Sender<String>,
) -> Result<(), agent_client_protocol::Error> {
    if let craft_agent::AgentEvent::PermissionRequest { id, tool, scopes, .. } = &envelope.event {
        spawn_permission_request(cx, session_id, response_tx, id, tool, scopes)?;
        return Ok(());
    }
    let translation = event_bridge::translate(&envelope.event);
    for update in translation.updates {
        cx.send_notification(SessionNotification::new(session_id.clone(), update))?;
    }
    Ok(())
}

fn spawn_permission_request(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    response_tx: &flume::Sender<String>,
    tool_call_id: &str,
    tool: &str,
    scopes: &[String],
) -> Result<(), agent_client_protocol::Error> {
    let request = RequestPermissionRequest::new(
        session_id.clone(),
        permissions::tool_call_update(tool_call_id, tool, scopes),
        permissions::options(),
    );
    let response_tx = response_tx.clone();
    let log_id = tool_call_id.to_owned();
    cx.send_request(request).on_receiving_result(async move |result| {
        let answer = match result {
            Ok(resp) => permissions::outcome_to_answer(&resp.outcome),
            Err(err) => {
                tracing::warn!(error = %err, tool_call_id = %log_id, "permission request failed; denying");
                PermissionAnswer::Deny.encode()
            }
        };
        if let Err(e) = response_tx.send(answer) {
            tracing::warn!(error = %e, "permission response channel closed");
        }
        Ok(())
    })
}

/// Build and run the agent inside the spawned prompt task. Returns the
/// resolved ACP `StopReason` plus the post-run `History` for the session
/// to pick up next turn.
#[allow(clippy::too_many_arguments)]
async fn run_agent(
    runtime: Runtime,
    cwd: PathBuf,
    mode: AgentMode,
    history: History,
    prompt: String,
    images: Vec<craft_providers::ImageSource>,
    cancel: CancelToken,
    event_tx_raw: flume::Sender<Envelope>,
    session_id: String,
    user_response_rx: Arc<Mutex<flume::Receiver<String>>>,
    fs: Arc<dyn FsBackend>,
    registry: Arc<ToolRegistry>,
    mcp_handle: Option<McpHandle>,
) -> (StopReason, History) {
    let vars = craft_agent::template::env_vars();
    let instructions = craft_agent::agent::load_instructions(&vars.apply("{cwd}"));
    let filter = ToolFilter::from_config(&runtime.config, &[]);
    let ctx = DescriptionContext { filter: &filter };
    let mut tools = registry.definitions(&vars, &ctx, runtime.model.supports_tool_examples());
    if let Some(ref mcp) = mcp_handle {
        mcp.extend_tools(&mut tools);
    }
    let prompt_slots = craft_agent::prompt::ResolvedSlots::default();
    let compact = runtime.config.small_model.should_activate(runtime.model.context_window)
        && runtime.config.small_model.compact_prompt;
    let system =
        craft_agent::agent::build_system_prompt(&vars, &mode, &instructions.text, &prompt_slots, compact);

    let event_tx = EventSender::new(event_tx_raw, 0);

    let provider: Arc<dyn Provider> = match provider::from_model(&runtime.model, runtime.timeouts).await {
        Ok(p) => Arc::from(p),
        Err(e) => {
            tracing::error!(error = %e, "provider init failed");
            let _ = event_tx.send(craft_agent::AgentEvent::Error {
                message: e.user_message(),
            });
            return (StopReason::Refusal, history);
        }
    };

    let agent = Agent::new(
        AgentParams {
            provider,
            model: runtime.model,
            config: runtime.config,
            tool_output_lines: ToolOutputLines::default(),
            permissions: Arc::new(PermissionManager::new(
                runtime.permissions_config,
                cwd.clone(),
            )),
            session_id: Some(session_id),
            timeouts: runtime.timeouts,
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::new(prompt_slots),
            compression: runtime.compression,
            findings_store: Some(craft_agent::FindingsStore::new_shared()),
            fs,
            doom: Arc::new(std::sync::Mutex::new(craft_agent::DoomTracker::new())),
        },
        AgentRunParams {
            history,
            system,
            event_tx,
            tools,
        },
    )
    .with_loaded_instructions(instructions.loaded)
    .with_user_response_rx(user_response_rx)
    .with_cancel(cancel)
    .with_mcp(mcp_handle);

    let outcome = agent
        .run(AgentInput {
            message: prompt,
            mode,
            images,
            ..Default::default()
        })
        .await;

    (event_bridge::stop_reason(&outcome.result), outcome.history)
}

/// Pick the right `FsBackend`: route through the ACP client when it
/// advertised both `fs.read_text_file` and `fs.write_text_file`, otherwise
/// fall back to `LocalFs`.
async fn build_fs_backend(
    runtime: &Runtime,
    cx: &ConnectionTo<Client>,
    session_id: SessionId,
) -> Arc<dyn FsBackend> {
    let caps = runtime.fs_caps.read().await.clone();
    if caps.read_text_file && caps.write_text_file {
        Arc::new(AcpFs::new(cx.clone(), session_id))
    } else {
        Arc::new(LocalFs)
    }
}

/// Build a per-session Lua plugin host plus its tool registry. The terminal
/// backend is `AcpTerminal` when the client advertises `terminal: true`,
/// otherwise `LocalTerminal`. Failures fall back to a disabled host with the
/// native-only registry so a broken plugin never breaks the prompt.
async fn build_plugin_host(
    runtime: &Runtime,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    cwd: &std::path::Path,
) -> (Arc<ToolRegistry>, PluginHost) {
    let registry = Arc::new(ToolRegistry::with_natives());
    let terminal_supported = *runtime.terminal_cap.read().await;
    let backend: Arc<dyn TerminalBackend> = if terminal_supported {
        Arc::new(AcpTerminal::new(cx.clone(), session_id.clone()))
    } else {
        Arc::new(LocalTerminal)
    };
    let mut host =
        match PluginHost::with_terminal_backend(Arc::clone(&registry), None, backend) {
            Ok(h) => h,
            Err(err) => {
                tracing::warn!(error = %err, "plugin host init failed; using disabled host");
                return (registry, PluginHost::disabled());
            }
        };
    if let Err(err) = host.load_init_files(cwd) {
        tracing::warn!(error = %err, "load_init_files failed; continuing without user init.lua");
    }
    if let Err(err) = host.load_builtins(&runtime.plugins_config) {
        tracing::warn!(error = %err, "load_builtins failed; continuing with native tools only");
    }
    (registry, host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn new_session_inserts_state() {
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        let id = new_session(&sessions, PathBuf::from("/tmp")).await;
        let map = sessions.lock().await;
        let state = map.get(&id).expect("session inserted");
        assert_eq!(state.mode, AgentMode::Build);
        assert!(state.history.as_slice().is_empty());
    }

    #[test]
    fn available_modes_advertises_build_default_and_plan() {
        let modes = available_modes();
        assert_eq!(&*modes.current_mode_id.0, MODE_BUILD);
        let ids: Vec<&str> = modes
            .available_modes
            .iter()
            .map(|m| m.id.0.as_ref())
            .collect();
        assert_eq!(ids, vec![MODE_BUILD, MODE_PLAN]);
    }

    #[test]
    fn build_mode_build_returns_build() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mode = build_mode(MODE_BUILD, &dir).unwrap();
        assert_eq!(mode, AgentMode::Build);
    }

    #[test]
    fn build_mode_plan_allocates_plan_path_under_state_dir() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let mode = build_mode(MODE_PLAN, &dir).unwrap();
        let AgentMode::Plan(path) = mode else {
            panic!("expected plan mode");
        };
        assert!(path.starts_with(tmp.path()));
        assert_eq!(path.extension().and_then(|s| s.to_str()), Some("md"));
    }

    #[test]
    fn build_mode_unknown_id_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());
        let err = build_mode("orchestra", &dir).unwrap_err();
        let data = err.data.expect("data attached").to_string();
        assert!(data.contains(UNKNOWN_MODE_MSG), "data was: {data}");
    }
}
