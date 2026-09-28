//! MCP client manager: owns server sessions and routes tool calls (B.11).
//!
//! Tool names are namespaced as `server.tool` internally and `server__tool` on
//! the wire (LLM APIs reject dots). All mutable state lives in the `run` task,
//! which owns `McpManagerInner` exclusively. Reads go through two lock-free
//! `ArcSwap`s: a `ToolIndex` for tool calls and an `McpSnapshot` for the UI, so
//! a slow tool call never blocks a toggle and vice versa.
//!
//! Servers connect inside `run`, not in `start`, so a slow `initialize` never
//! delays the caller's first frame. Whoever needs the tools waits on
//! `McpHandle::ready`.

pub mod config;
pub mod error;
pub mod oauth;
pub mod session;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use self::config::{
    McpConfig, McpConfigErrors, McpServerInfo, McpServerStatus, OauthClientConfig, ServerConfig,
    Transport, load_config, parse_server, transport_kind,
};
use self::error::McpError;
use self::session::{McpSession, PromptInfo, ToolInfo, start_session};

const SEPARATOR: &str = ".";
const WIRE_SEPARATOR: &str = "__";
pub const UNKNOWN_MCP: &str = "unknown_mcp";

/// Convert internal qualified name (`server.tool`) to wire format (`server__tool`)
/// for LLM provider APIs that reject dots in tool names.
///
/// Lossless: server names can't contain `__` (only alphanumeric + `-`),
/// so the first `__` in the wire name is always the separator boundary.
pub fn wire_tool_name(qualified: &str) -> String {
    qualified.replacen(SEPARATOR, WIRE_SEPARATOR, 1)
}

/// Convert wire format (`server__tool`) back to internal qualified name
/// (`server.tool`). Only the first `__` is the separator — tool names may
/// contain underscores.
pub fn internal_tool_name(wire: &str) -> String {
    wire.replacen(WIRE_SEPARATOR, SEPARATOR, 1)
}

const MCP_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// One MCP tool as the model sees it: wire name, description, JSON schema.
#[derive(Clone)]
pub struct McpToolDescriptor {
    pub wire_name: String,
    pub qualified_name: String,
    pub description: String,
    pub parameters: Value,
}

struct McpToolDef {
    qualified_name: Arc<str>,
    raw_name: String,
    description: String,
    input_schema: Value,
}

struct McpPromptDef {
    qualified_name: String,
    raw_name: String,
    description: String,
    arguments: Vec<session::PromptArgument>,
}

impl McpPromptDef {
    fn from_info(server_name: &str, info: PromptInfo) -> Self {
        Self {
            qualified_name: format!("{server_name}{SEPARATOR}{}", info.name),
            raw_name: info.name,
            description: info.description.unwrap_or_default(),
            arguments: info.arguments,
        }
    }

    fn to_info(&self, server_name: &str) -> McpPromptInfo {
        McpPromptInfo {
            display_name: format!("{server_name}:{}", self.raw_name),
            qualified_name: self.qualified_name.clone(),
            description: self.description.clone(),
            arguments: self
                .arguments
                .iter()
                .map(|a| McpPromptArg {
                    name: a.name.clone(),
                    description: a.description.clone().unwrap_or_default(),
                    required: a.required,
                })
                .collect(),
        }
    }
}

struct ServerEntry {
    name: String,
    config: Option<ServerConfig>,
    transport_kind: &'static str,
    origin: PathBuf,
    status: McpServerStatus,
    session: Option<Arc<dyn McpSession>>,
    tools: Vec<McpToolDef>,
    prompts: Vec<McpPromptDef>,
}

impl ServerEntry {
    async fn clear_connection(&mut self) {
        if let Some(old) = self.session.take() {
            old.shutdown().await;
        }
        self.tools.clear();
        self.prompts.clear();
    }

    fn populate(&mut self, result: StartResult) {
        let StartResult {
            session,
            tool_infos,
            prompt_infos,
        } = result;
        self.tools = tool_infos
            .into_iter()
            .filter(|info| {
                if !crate::permissions::is_valid_wire_name(&info.name) {
                    tracing::warn!(tool = %info.name, server = %self.name, "skipping tool with invalid name");
                    return false;
                }
                let wire_len = self.name.len() + 2 + info.name.len();
                if wire_len > 64 {
                    tracing::warn!(
                        tool = %info.name,
                        server = %self.name,
                        wire_len,
                        "skipping tool — wire name exceeds 64 char LLM API limit"
                    );
                    return false;
                }
                true
            })
            .map(|info| McpToolDef {
                qualified_name: intern(format!("{}{SEPARATOR}{}", self.name, info.name)),
                raw_name: info.name,
                description: info.description,
                input_schema: info.input_schema,
            })
            .collect();
        self.prompts = prompt_infos
            .into_iter()
            .map(|info| McpPromptDef::from_info(&self.name, info))
            .collect();
        self.session = Some(session);
        self.status = McpServerStatus::Running;
    }
}

struct McpManagerInner {
    entries: Vec<ServerEntry>,
    generation: u64,
}

#[derive(Default)]
struct ToolIndex {
    tools: HashMap<Arc<str>, ToolRef>,
    prompts: HashMap<String, PromptRef>,
    descriptors: Vec<McpToolDescriptor>,
}

struct ToolRef {
    raw_name: String,
    session: Arc<dyn McpSession>,
}

struct PromptRef {
    raw_name: String,
    session: Arc<dyn McpSession>,
}

#[derive(Clone)]
pub struct McpPromptInfo {
    pub display_name: String,
    pub qualified_name: String,
    pub description: String,
    pub arguments: Vec<McpPromptArg>,
}

#[derive(Clone)]
pub struct McpPromptArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

#[derive(Clone, Default)]
pub struct McpSnapshot {
    pub infos: Vec<McpServerInfo>,
    pub prompts: Vec<McpPromptInfo>,
    pub generation: u64,
}

/// Read-only view of the latest published `McpSnapshot`. Handing this out
/// instead of the raw `ArcSwap` keeps outside code from publishing snapshots
/// of its own.
#[derive(Clone)]
pub struct McpSnapshotReader(Arc<ArcSwap<McpSnapshot>>);

impl McpSnapshotReader {
    pub fn empty() -> Self {
        Self::from_snapshot(McpSnapshot::default())
    }

    pub fn from_snapshot(snapshot: McpSnapshot) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(snapshot)))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<McpSnapshot>> {
        self.0.load()
    }

    pub fn load_full(&self) -> Arc<McpSnapshot> {
        self.0.load_full()
    }
}

pub enum McpCommand {
    Toggle {
        server: String,
        enabled: bool,
    },
    Reconnect {
        server: String,
    },
    /// Drain every running session and stop the loop. The loop acks on `ack`
    /// once every shutdown has finished, so callers can wait with a timeout.
    Shutdown {
        ack: tokio::sync::oneshot::Sender<()>,
    },
}

#[derive(Clone)]
pub struct McpHandle {
    cmd_tx: mpsc::UnboundedSender<McpCommand>,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    ready_rx: watch::Receiver<bool>,
}

impl McpHandle {
    pub fn send(&self, cmd: McpCommand) {
        if self.cmd_tx.send(cmd).is_err() {
            tracing::warn!("MCP command loop is gone");
        }
    }

    /// Resolves once every enabled server has connected or failed. Await it
    /// before building a request's tool list, or an early prompt ships without
    /// the MCP tools.
    pub async fn ready(&self) {
        let mut rx = self.ready_rx.clone();
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn reader(&self) -> McpSnapshotReader {
        McpSnapshotReader(Arc::clone(&self.snapshot))
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.index.load().tools.contains_key(name)
    }

    pub fn interned_name(&self, name: &str) -> Arc<str> {
        self.index
            .load()
            .tools
            .get_key_value(name)
            .map(|(k, _)| Arc::clone(k))
            .unwrap_or_else(|| Arc::from(UNKNOWN_MCP))
    }

    pub async fn call_tool(&self, qualified_name: &str, args: &Value) -> Result<String, McpError> {
        let (raw_name, session) = {
            let idx = self.index.load();
            let Some(t) = idx.tools.get(qualified_name) else {
                return Err(McpError::UnknownTool {
                    name: qualified_name.to_string(),
                });
            };
            (t.raw_name.clone(), Arc::clone(&t.session))
        };
        session.call_tool(&raw_name, args).await
    }

    pub async fn get_prompt(
        &self,
        qualified_name: &str,
        arguments: &HashMap<String, String>,
    ) -> Result<Vec<session::PromptMessage>, McpError> {
        let (raw_name, session) = {
            let idx = self.index.load();
            let Some(p) = idx.prompts.get(qualified_name) else {
                return Err(McpError::UnknownPrompt {
                    name: qualified_name.to_string(),
                });
            };
            (p.raw_name.clone(), Arc::clone(&p.session))
        };
        session.get_prompt(&raw_name, arguments).await
    }

    /// Model-facing descriptors for every published MCP tool.
    pub fn tool_descriptors(&self) -> Vec<McpToolDescriptor> {
        self.index.load().descriptors.clone()
    }

    pub async fn shutdown(&self) {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.send(McpCommand::Shutdown { ack: ack_tx });
        if tokio::time::timeout(MCP_SHUTDOWN_TIMEOUT, ack_rx)
            .await
            .is_err()
        {
            tracing::warn!("MCP shutdown timed out after {MCP_SHUTDOWN_TIMEOUT:?}");
        }
    }
}

/// Returns as soon as the config is read, so nothing with a screen waits on a
/// slow `initialize`. Await `McpHandle::ready` before touching the tool index.
pub async fn start(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    tracing::info!(cwd = %cwd.display(), "starting MCP");
    let cwd = cwd.to_owned();
    let (config, config_errors) = tokio::task::spawn_blocking(move || load_config(&cwd))
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to load config");
            (McpConfig::default(), McpConfigErrors::new(PathBuf::new()))
        });
    (start_with_config(config), config_errors)
}

/// `start` for callers with no frame to protect, who want the tools up front.
/// The wait is bounded (10s, mirroring the TUI's turn gate) so one hung server
/// cannot stall a headless run; a server that lands later simply misses the
/// initial tool registration.
pub async fn start_connected(cwd: &Path) -> (Option<McpHandle>, McpConfigErrors) {
    const CONNECTED_BUDGET: Duration = Duration::from_secs(10);
    let (handle, config_errors) = start(cwd).await;
    if let Some(handle) = &handle {
        if tokio::time::timeout(CONNECTED_BUDGET, handle.ready())
            .await
            .is_err()
        {
            tracing::warn!(
                budget = ?CONNECTED_BUDGET,
                "MCP servers did not settle in time; continuing without the stragglers"
            );
        }
    }
    (handle, config_errors)
}

pub fn start_with_config(config: McpConfig) -> Option<McpHandle> {
    if config.is_empty() {
        tracing::info!("no MCP servers configured, skipping");
        return None;
    }

    let inner = parse_entries(config);

    let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
    let index: Arc<ArcSwap<ToolIndex>> = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
    publish(&inner, &index, &snapshot);

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = watch::channel(false);
    let handle = McpHandle {
        cmd_tx,
        index: Arc::clone(&index),
        snapshot: Arc::clone(&snapshot),
        ready_rx,
    };

    tracing::info!(total = inner.entries.len(), "MCP servers connecting");
    tokio::spawn(run(inner, index, snapshot, cmd_rx, ready_tx));
    Some(handle)
}

/// Connect results ride the same loop as commands, so a `Shutdown` arriving
/// mid-connect never waits for a slow `initialize`.
enum Step {
    Connected(usize, Result<StartResult, McpError>),
    Command(McpCommand),
}

async fn run(
    mut inner: McpManagerInner,
    index: Arc<ArcSwap<ToolIndex>>,
    snapshot: Arc<ArcSwap<McpSnapshot>>,
    mut cmd_rx: mpsc::UnboundedReceiver<McpCommand>,
    ready_tx: watch::Sender<bool>,
) {
    let (connected_tx, mut connected_rx) = mpsc::unbounded_channel();
    // Held, not detached: dropping a pending connect aborts the task, which
    // drops the session it owns and kills the child process group it spawned.
    // The sender stays alive for the loop's lifetime so refreshes can also
    // ride this channel instead of blocking the command loop.
    let mut connects = spawn_connects(&inner, &connected_tx);
    let mut ready_released = false;
    let mut ack: Option<tokio::sync::oneshot::Sender<()>> = None;

    loop {
        release_ready(&inner, &ready_tx, &mut ready_released);
        let step = tokio::select! {
            result = connected_rx.recv() => match result {
                Some((i, r)) => Step::Connected(i, r),
                None => match cmd_rx.recv().await {
                    Some(cmd) => Step::Command(cmd),
                    None => break,
                },
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(cmd) => Step::Command(cmd),
                None => break,
            },
        };

        match step {
            Step::Connected(i, result) => {
                // A toggle or reconnect that ran mid-connect owns the entry
                // now, so a late result must not resurrect it. Dropping it
                // kills the session it carries.
                if inner.entries[i].status == McpServerStatus::Connecting {
                    let _ = apply_start_result(&mut inner.entries[i], result, "start");
                }
            }
            Step::Command(McpCommand::Toggle { server, enabled }) => {
                handle_toggle(&mut inner, &server, enabled, &connected_tx, &mut connects).await;
            }
            Step::Command(McpCommand::Reconnect { server }) => {
                handle_reconnect(&mut inner, &server, &connected_tx, &mut connects).await;
            }
            Step::Command(McpCommand::Shutdown { ack: tx }) => {
                ack = Some(tx);
                break;
            }
        }
        inner.generation += 1;
        publish(&inner, &index, &snapshot);
    }
    drop(connects);
    drop(connected_tx);
    shutdown_all(&mut inner).await;
    inner.generation += 1;
    publish(&inner, &index, &snapshot);
    release_ready(&inner, &ready_tx, &mut ready_released);
    if let Some(tx) = ack {
        let _ = tx.send(());
    }
}

/// Nothing left in `Connecting` means every server landed and the last publish
/// already carried its tools, so waiters can go.
fn release_ready(inner: &McpManagerInner, ready: &watch::Sender<bool>, released: &mut bool) {
    if *released
        || inner
            .entries
            .iter()
            .any(|e| e.status == McpServerStatus::Connecting)
    {
        return;
    }
    *released = true;
    let _ = ready.send(true);
    tracing::info!(
        running = inner.entries.iter().filter(|e| e.session.is_some()).count(),
        total = inner.entries.len(),
        "MCP servers initialized"
    );
}

async fn handle_toggle(
    inner: &mut McpManagerInner,
    server_name: &str,
    enabled: bool,
    connected_tx: &mpsc::UnboundedSender<(usize, Result<StartResult, McpError>)>,
    connects: &mut tokio::task::JoinSet<()>,
) {
    if let Some(path) = inner
        .entries
        .iter()
        .find(|e| e.name == server_name)
        .map(|e| e.origin.clone())
    {
        spawn_persist_enabled(path, server_name.to_owned(), enabled);
    }

    if enabled {
        begin_refresh(inner, server_name, connected_tx, connects).await;
    } else if let Some(entry) = inner.entries.iter_mut().find(|e| e.name == server_name) {
        entry.clear_connection().await;
        entry.status = McpServerStatus::Disabled;
    }

    tracing::info!(server = server_name, enabled, "MCP toggle complete");
}

/// Restart the server with its stored config.
async fn handle_reconnect(
    inner: &mut McpManagerInner,
    server_name: &str,
    connected_tx: &mpsc::UnboundedSender<(usize, Result<StartResult, McpError>)>,
    connects: &mut tokio::task::JoinSet<()>,
) {
    let Some(entry) = inner.entries.iter().find(|e| e.name == server_name) else {
        tracing::warn!(server = server_name, "reconnect for unknown server");
        return;
    };
    if entry.status == McpServerStatus::Disabled {
        tracing::info!(
            server = server_name,
            "ignoring reconnect for disabled server"
        );
        return;
    }
    begin_refresh(inner, server_name, connected_tx, connects).await;
    tracing::info!(server = server_name, "MCP reconnect complete");
}

async fn shutdown_all(inner: &mut McpManagerInner) {
    for entry in &mut inner.entries {
        entry.clear_connection().await;
        if entry.status != McpServerStatus::Disabled {
            entry.status = McpServerStatus::Failed("shutdown".into());
        }
    }
    tracing::info!("MCP command loop shutting down");
}

/// Tear the old session down and wipe tools/prompts, mark the entry
/// `Connecting`, then reconnect in the background — through the same channel
/// the initial connects use, so no command ever awaits a server handshake
/// inline and a `Shutdown` arriving mid-refresh still preempts it. A failed
/// start leaves the entry empty instead of holding zombie tool references
/// into a dead session.
async fn begin_refresh(
    inner: &mut McpManagerInner,
    server_name: &str,
    connected_tx: &mpsc::UnboundedSender<(usize, Result<StartResult, McpError>)>,
    connects: &mut tokio::task::JoinSet<()>,
) {
    let Some(idx) = inner.entries.iter().position(|e| e.name == server_name) else {
        tracing::warn!(server = server_name, "refresh for unknown server");
        return;
    };
    let Some(config) = inner.entries[idx].config.clone() else {
        tracing::warn!(server = server_name, "refresh for server with no config");
        return;
    };
    {
        let entry = &mut inner.entries[idx];
        entry.status = McpServerStatus::Connecting;
        entry.clear_connection().await;
    }
    spawn_connect(idx, config, connected_tx, connects);
    tracing::info!(server = server_name, "MCP refresh started");
}

/// One background connect task reporting through `connected_tx` as it lands.
fn spawn_connect(
    idx: usize,
    config: ServerConfig,
    connected_tx: &mpsc::UnboundedSender<(usize, Result<StartResult, McpError>)>,
    set: &mut tokio::task::JoinSet<()>,
) {
    let tx = connected_tx.clone();
    set.spawn(async move {
        let _ = tx.send((idx, connect_result(&config).await));
    });
}

/// Connect and gather tool/prompt listings. Runs on a background task; every
/// await here is bounded by the server's configured timeout.
async fn connect_result(config: &ServerConfig) -> Result<StartResult, McpError> {
    let session = start_session(config).await?;
    let tool_infos = session.list_tools().await.unwrap_or_default();
    let prompt_infos = session.list_prompts().await.unwrap_or_default();
    Ok(StartResult {
        session,
        tool_infos,
        prompt_infos,
    })
}

fn status_from_err(e: &McpError) -> McpServerStatus {
    if let McpError::HttpError {
        status: 401,
        reason,
        ..
    } = e
    {
        McpServerStatus::NeedsAuth {
            url: Some(reason.clone()),
        }
    } else {
        McpServerStatus::Failed(e.to_string())
    }
}

struct StartResult {
    session: Arc<dyn McpSession>,
    tool_infos: Vec<ToolInfo>,
    prompt_infos: Vec<PromptInfo>,
}

/// The only place read-side state is updated. Every mutation in the command
/// loop ends here.
fn publish(inner: &McpManagerInner, index: &ArcSwap<ToolIndex>, snapshot: &ArcSwap<McpSnapshot>) {
    let mut tools = HashMap::new();
    let mut prompts = HashMap::new();
    let mut descriptors = Vec::new();
    let mut server_infos = Vec::with_capacity(inner.entries.len());
    let mut prompt_infos = Vec::new();

    for entry in &inner.entries {
        let url = entry
            .config
            .as_ref()
            .and_then(|c| transport_url(&c.transport));
        let oauth = entry
            .config
            .as_ref()
            .and_then(|c| transport_oauth(&c.transport));

        if let Some(ref session) = entry.session
            && entry.status != McpServerStatus::Disabled
        {
            for t in &entry.tools {
                tools.insert(
                    Arc::clone(&t.qualified_name),
                    ToolRef {
                        raw_name: t.raw_name.clone(),
                        session: Arc::clone(session),
                    },
                );
                descriptors.push(McpToolDescriptor {
                    wire_name: wire_tool_name(&t.qualified_name),
                    qualified_name: t.qualified_name.to_string(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                });
            }
            for p in &entry.prompts {
                prompts.insert(
                    p.qualified_name.clone(),
                    PromptRef {
                        raw_name: p.raw_name.clone(),
                        session: Arc::clone(session),
                    },
                );
                prompt_infos.push(p.to_info(&entry.name));
            }
        }

        server_infos.push(McpServerInfo {
            name: entry.name.clone(),
            transport_kind: entry.transport_kind,
            tool_count: entry.tools.len(),
            prompt_count: entry.prompts.len(),
            status: entry.status.clone(),
            config_path: entry.origin.clone(),
            url,
            oauth,
        });
    }

    index.store(Arc::new(ToolIndex {
        tools,
        prompts,
        descriptors,
    }));
    snapshot.store(Arc::new(McpSnapshot {
        infos: server_infos,
        prompts: prompt_infos,
        generation: inner.generation,
    }));
}

fn transport_url(transport: &Transport) -> Option<String> {
    match transport {
        Transport::Http { url, .. } => Some(url.clone()),
        Transport::Stdio { .. } => None,
    }
}

fn transport_oauth(transport: &Transport) -> Option<OauthClientConfig> {
    match transport {
        Transport::Http { oauth, .. } => oauth.clone(),
        Transport::Stdio { .. } => None,
    }
}

fn parse_entries(config: McpConfig) -> McpManagerInner {
    let origins = config.origins;
    let mut entries = Vec::with_capacity(config.mcp.len());

    for (name, raw) in config.mcp {
        let transport_kind = transport_kind(&raw.transport);
        let origin = origins.get(&name).cloned().unwrap_or_default();
        let disabled = !raw.enabled;
        let (config, status) = match parse_server(name.clone(), raw) {
            Ok(sc) if disabled => (Some(sc), McpServerStatus::Disabled),
            Ok(sc) => (Some(sc), McpServerStatus::Connecting),
            Err(e) => {
                tracing::warn!(server = %name, error = %e, "invalid MCP server config");
                (None, McpServerStatus::Failed(e.to_string()))
            }
        };
        entries.push(ServerEntry {
            name,
            config,
            transport_kind,
            origin,
            status,
            session: None,
            tools: Vec::new(),
            prompts: Vec::new(),
        });
    }

    McpManagerInner {
        entries,
        generation: 0,
    }
}

/// One task per enabled server, each reporting back as it lands, so `run`
/// publishes a fast server's tools without waiting for the slowest.
fn spawn_connects(
    inner: &McpManagerInner,
    tx: &mpsc::UnboundedSender<(usize, Result<StartResult, McpError>)>,
) -> tokio::task::JoinSet<()> {
    let mut set = tokio::task::JoinSet::new();
    for (i, config) in inner
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status == McpServerStatus::Connecting)
        .filter_map(|(i, e)| e.config.clone().map(|c| (i, c)))
    {
        spawn_connect(i, config, tx, &mut set);
    }
    set
}

fn apply_start_result(
    entry: &mut ServerEntry,
    result: Result<StartResult, McpError>,
    action: &'static str,
) -> Result<(), McpError> {
    match result {
        Ok(start) => {
            entry.populate(start);
            Ok(())
        }
        Err(e) => {
            entry.status = status_from_err(&e);
            if !matches!(entry.status, McpServerStatus::NeedsAuth { .. }) {
                tracing::warn!(server = %entry.name, action, error = %e, "MCP server start failed");
            }
            Err(e)
        }
    }
}

fn spawn_persist_enabled(path: PathBuf, name: String, enabled: bool) {
    let log_name = name.clone();
    tokio::spawn(async move {
        if let Err(e) =
            tokio::task::spawn_blocking(move || config::persist_enabled(&path, &name, enabled))
                .await
                .unwrap_or_else(|e| {
                    Err(McpError::Config {
                        message: format!("spawn_blocking failed: {e}"),
                    })
                })
        {
            tracing::warn!(error = %e, server = %log_name, "failed to persist MCP toggle");
        }
    });
}

/// Dedup cache for qualified MCP tool names. The set is bounded (finite per
/// session) and `Arc<str>` means entries get freed when the cache drops.
pub(crate) fn intern(name: String) -> Arc<str> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<str>>>> = OnceLock::new();
    let mut map = CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(&name) {
        return Arc::clone(existing);
    }
    let arc: Arc<str> = Arc::from(name.as_str());
    map.insert(name, Arc::clone(&arc));
    arc
}

/// Stands in for a live MCP session in tests: publishes the given tools
/// (qualified `server.tool` names) behind a session that fails every call
/// with `unknown MCP tool`, so a test can prove a call reached MCP routing
/// instead of dying at name lookup.
#[cfg(test)]
pub(crate) mod test_support {
    use super::session::BoxFuture;
    use super::*;
    use serde_json::json;

    struct FailingSession;

    impl McpSession for FailingSession {
        fn server_name(&self) -> &str {
            "stub"
        }
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn call_tool(&self, _name: &str, _args: &Value) -> BoxFuture<'_, Result<String, McpError>> {
            Box::pin(async {
                Err(McpError::UnknownTool {
                    name: String::new(),
                })
            })
        }
        fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn get_prompt(
            &self,
            _name: &str,
            _arguments: &HashMap<String, String>,
        ) -> BoxFuture<'_, Result<Vec<session::PromptMessage>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn shutdown(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    pub(crate) fn stub_handle(tools: &[(&str, &str)]) -> McpHandle {
        let session: Arc<dyn McpSession> = Arc::new(FailingSession);
        let mut index = ToolIndex::default();
        for (qualified, description) in tools {
            let (_server, raw) = qualified
                .split_once(SEPARATOR)
                .unwrap_or((qualified, "tool"));
            index.tools.insert(
                intern(qualified.to_string()),
                ToolRef {
                    raw_name: (*raw).to_string(),
                    session: Arc::clone(&session),
                },
            );
            index.descriptors.push(McpToolDescriptor {
                wire_name: wire_tool_name(qualified),
                qualified_name: (*qualified).to_string(),
                description: (*description).to_string(),
                parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            });
        }
        let index = Arc::new(ArcSwap::from_pointee(index));
        McpHandle {
            cmd_tx: mpsc::unbounded_channel().0,
            snapshot: Arc::new(ArcSwap::from_pointee(McpSnapshot::default())),
            index,
            ready_rx: watch::channel(true).1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::session::BoxFuture;
    use super::test_support::stub_handle;
    use super::*;
    use config::{RawHttpFields, RawServerConfig, RawStdioFields, RawTransport};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const DEFAULT_TIMEOUT_MS: u64 = 30_000;
    const MISSING_PROGRAM: &str = "/nonexistent/definitely-not-here";

    fn stdio_raw(cmd: &[&str]) -> RawServerConfig {
        RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Stdio(RawStdioFields {
                command: cmd.iter().map(|s| s.to_string()).collect(),
                environment: HashMap::new(),
            }),
        }
    }

    fn make_config(entries: Vec<(&str, RawServerConfig)>) -> McpConfig {
        let mut mcp = HashMap::new();
        let mut origins = HashMap::new();
        for (name, cfg) in entries {
            origins.insert(name.to_string(), PathBuf::from("/test/config.toml"));
            mcp.insert(name.to_string(), cfg);
        }
        McpConfig { mcp, origins }
    }

    const TOOL_NAME: &str = "srv.tool";
    const WIRE_TOOL_NAME: &str = "srv__tool";

    #[test]
    fn wire_name_round_trip() {
        assert_eq!(wire_tool_name("srv.echo_tool"), "srv__echo_tool");
        assert_eq!(internal_tool_name("srv__echo_tool"), "srv.echo_tool");
        // Only the first `__` is the separator.
        assert_eq!(internal_tool_name("srv__deep__name"), "srv.deep__name");
    }

    /// Counts shutdowns, signals on `call_entered` the moment a `tools/call`
    /// begins, and holds the call inside `call_gate` until tests release it.
    struct FakeSession {
        name: Arc<str>,
        shutdowns: AtomicUsize,
        call_entered: tokio::sync::mpsc::UnboundedSender<()>,
        call_gate: tokio::sync::Mutex<()>,
    }

    impl FakeSession {
        fn new() -> Arc<Self> {
            let (call_entered, _) = tokio::sync::mpsc::unbounded_channel();
            Arc::new(Self {
                name: Arc::from("fake"),
                shutdowns: AtomicUsize::new(0),
                call_entered,
                call_gate: tokio::sync::Mutex::new(()),
            })
        }

        fn shutdowns(&self) -> usize {
            self.shutdowns.load(Ordering::SeqCst)
        }
    }

    impl McpSession for FakeSession {
        fn server_name(&self) -> &str {
            &self.name
        }
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn call_tool(&self, _name: &str, _args: &Value) -> BoxFuture<'_, Result<String, McpError>> {
            Box::pin(async {
                let _ = self.call_entered.send(());
                let _g = self.call_gate.lock().await;
                Ok("ok".into())
            })
        }
        fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn get_prompt(
            &self,
            _name: &str,
            _arguments: &HashMap<String, String>,
        ) -> BoxFuture<'_, Result<Vec<session::PromptMessage>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn shutdown(&self) -> BoxFuture<'_, ()> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    fn fake_entry(name: &str, session: Arc<dyn McpSession>) -> ServerEntry {
        let qualified = intern(format!("{name}{SEPARATOR}tool"));
        ServerEntry {
            name: name.into(),
            config: None,
            transport_kind: "fake",
            origin: PathBuf::new(),
            status: McpServerStatus::Running,
            session: Some(session),
            tools: vec![McpToolDef {
                qualified_name: qualified,
                raw_name: "tool".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }],
            prompts: Vec::new(),
        }
    }

    fn bad_stdio_config(name: &str) -> ServerConfig {
        ServerConfig {
            name: name.into(),
            timeout: Duration::from_secs(1),
            transport: Transport::Stdio {
                program: MISSING_PROGRAM.into(),
                args: vec![],
                environment: HashMap::new(),
            },
        }
    }

    /// Build `inner`, publish it into fresh `ArcSwap`s, and return a live
    /// `McpHandle` pointing at the same state so tests can hit both the
    /// mutation and the read path.
    fn setup(entries: Vec<ServerEntry>) -> (McpManagerInner, McpHandle) {
        let inner = McpManagerInner {
            entries,
            generation: 0,
        };
        let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
        let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
        publish(&inner, &index, &snapshot);
        let handle = McpHandle {
            cmd_tx: mpsc::unbounded_channel().0,
            index,
            snapshot,
            ready_rx: watch::channel(true).1,
        };
        (inner, handle)
    }

    /// `ready` carries the correctness of connecting in the background: a
    /// prompt typed during startup must not ship before the servers settle.
    #[tokio::test]
    async fn ready_settles_every_server_status() {
        assert!(start_with_config(McpConfig::default()).is_none());

        let mut disabled = stdio_raw(&["unused-disabled-cmd"]);
        disabled.enabled = false;
        let mut http_missing = RawServerConfig {
            enabled: true,
            timeout: DEFAULT_TIMEOUT_MS,
            transport: RawTransport::Http(RawHttpFields {
                url: "https://invalid.invalid/mcp".into(),
                headers: HashMap::new(),
                oauth: None,
            }),
        };
        http_missing.timeout = 500;
        let config = make_config(vec![
            ("disabled-srv", disabled),
            ("unparseable-srv", stdio_raw(&[])),
            ("unspawnable-srv", stdio_raw(&[MISSING_PROGRAM])),
        ]);
        let handle = start_with_config(config).unwrap();
        handle.ready().await;

        let infos = handle.reader().load().infos.clone();
        let status = |name: &str| {
            &infos
                .iter()
                .find(|i| i.name == name)
                .unwrap_or_else(|| panic!("{name} must be published"))
                .status
        };
        let failed = |name: &str| matches!(status(name), McpServerStatus::Failed(_));
        assert!(failed("unparseable-srv"));
        assert!(failed("unspawnable-srv"));
        assert_eq!(*status("disabled-srv"), McpServerStatus::Disabled);
    }

    /// `sleep` spawns fine and never answers `initialize`, so its connect only
    /// ends on the request timeout, far past the shutdown one. Shutdown has to
    /// preempt it, or quitting during startup hangs.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_preempts_an_in_flight_connect() {
        const BLOCKED: &str = "shutdown must not wait for an in-flight connect";
        let config = make_config(vec![("slow-srv", stdio_raw(&["sleep", "60"]))]);
        let handle = start_with_config(config).unwrap();
        let started = std::time::Instant::now();
        handle.shutdown().await;
        assert!(started.elapsed() < MCP_SHUTDOWN_TIMEOUT, "{BLOCKED}");
    }

    /// If a refresh fails, the entry must end up empty. A zombie tool left
    /// behind would be handed to the model on the next turn and then try to
    /// call into a dead session. Exercises the same pieces the background
    /// refresh path composes: clear + Connecting, connect, apply.
    #[tokio::test]
    async fn failed_refresh_clears_entry() {
        let t = FakeSession::new();
        let (mut inner, _) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
        let bad_config = bad_stdio_config("srv");
        inner.entries[0].config = Some(bad_config);

        {
            let entry = &mut inner.entries[0];
            entry.status = McpServerStatus::Connecting;
            entry.clear_connection().await;
        }
        let result = connect_result(inner.entries[0].config.as_ref().unwrap()).await;
        assert!(result.is_err());
        apply_start_result(&mut inner.entries[0], result, "refresh").unwrap_err();

        let entry = &inner.entries[0];
        assert_eq!(t.shutdowns(), 1);
        assert!(entry.tools.is_empty());
        assert!(entry.prompts.is_empty());
        assert!(entry.session.is_none());
        assert!(matches!(entry.status, McpServerStatus::Failed(_)));
    }

    /// A refresh must not run inline: while one is in flight, a Shutdown must
    /// still preempt it (same guarantee the initial connects give).
    #[tokio::test]
    async fn shutdown_preempts_an_in_flight_refresh() {
        let t = FakeSession::new();
        let (mut inner, _) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);
        inner.entries[0].config = Some(ServerConfig {
            name: "srv".into(),
            timeout: Duration::from_secs(30),
            transport: Transport::Stdio {
                program: "sleep".into(),
                args: vec!["60".into()],
                environment: HashMap::new(),
            },
        });

        let (tx, rx) = mpsc::unbounded_channel();
        let mut connects = tokio::task::JoinSet::new();
        let started = std::time::Instant::now();
        begin_refresh(&mut inner, "srv", &tx, &mut connects).await;
        drop(tx);
        drop(connects);
        drop(rx);
        assert!(
            started.elapsed() < MCP_SHUTDOWN_TIMEOUT,
            "refresh start must not await the handshake"
        );
        assert_eq!(inner.entries[0].status, McpServerStatus::Connecting);
    }

    #[tokio::test]
    async fn disable_purges_entry_and_published_view() {
        let t = FakeSession::new();
        let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

        assert!(handle.has_tool(TOOL_NAME));
        let descriptors = handle.tool_descriptors();
        assert_eq!(descriptors[0].wire_name, WIRE_TOOL_NAME);

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut connects = tokio::task::JoinSet::new();
        handle_toggle(&mut inner, "srv", false, &tx, &mut connects).await;
        publish(&inner, &handle.index, &handle.snapshot);

        let entry = &inner.entries[0];
        assert_eq!(t.shutdowns(), 1);
        assert!(entry.tools.is_empty());
        assert!(entry.session.is_none());
        assert_eq!(entry.status, McpServerStatus::Disabled);
        assert!(!handle.has_tool(TOOL_NAME));
        assert!(handle.tool_descriptors().is_empty());
    }

    /// Regression (reference semantics): a slow tool call must not block a
    /// publish. The call is parked on `call_gate` while `publish` runs.
    #[tokio::test]
    async fn slow_tool_call_does_not_block_publish() {
        let t = FakeSession::new();
        let (mut inner, handle) = setup(vec![fake_entry("srv", Arc::clone(&t) as _)]);

        let held = t.call_gate.lock().await;
        let call_handle = {
            let handle = handle.clone();
            tokio::spawn(async move {
                handle
                    .call_tool(TOOL_NAME, &serde_json::json!({}))
                    .await
                    .unwrap()
            })
        };

        // Give the call a moment to park inside the session.
        tokio::time::sleep(Duration::from_millis(50)).await;
        inner.generation += 1;
        publish(&inner, &handle.index, &handle.snapshot);
        assert_eq!(handle.snapshot.load().generation, 1);

        drop(held);
        call_handle.await.unwrap();
    }

    /// Commands must actually be awaited in the loop: a toggle-enable on a
    /// disabled server drives it Connecting → Failed through the real
    /// command loop (regression: a refactor once constructed the handler
    /// future without polling it, so commands silently did nothing).
    #[tokio::test]
    async fn toggle_enable_runs_a_background_refresh() {
        let mut raw = stdio_raw(&[MISSING_PROGRAM]);
        raw.timeout = 1_000;
        raw.enabled = false;
        let config = make_config(vec![("ghost", raw)]);
        let handle = start_with_config(config).unwrap();
        handle.ready().await;
        assert_eq!(
            handle.reader().load().infos[0].status,
            McpServerStatus::Disabled
        );

        handle.send(McpCommand::Toggle {
            server: "ghost".into(),
            enabled: true,
        });
        for _ in 0..200 {
            let status = handle.reader().load().infos[0].status.clone();
            if matches!(status, McpServerStatus::Failed(_)) {
                handle.shutdown().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("toggle-enable never reached Failed");
    }

    #[tokio::test]
    async fn shutdown_command_drains_and_acks() {
        let (t1, t2) = (FakeSession::new(), FakeSession::new());
        let inner = McpManagerInner {
            entries: vec![
                fake_entry("a", Arc::clone(&t1) as _),
                fake_entry("b", Arc::clone(&t2) as _),
            ],
            generation: 0,
        };
        let index = Arc::new(ArcSwap::from_pointee(ToolIndex::default()));
        let snapshot = Arc::new(ArcSwap::from_pointee(McpSnapshot::default()));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let loop_task = tokio::spawn(run(
            inner,
            Arc::clone(&index),
            Arc::clone(&snapshot),
            cmd_rx,
            watch::channel(false).0,
        ));

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        cmd_tx.send(McpCommand::Shutdown { ack: ack_tx }).unwrap();
        ack_rx.await.unwrap();
        loop_task.await.unwrap();

        assert_eq!(t1.shutdowns(), 1);
        assert_eq!(t2.shutdowns(), 1);
        assert!(snapshot.load().infos.iter().all(|i| i.tool_count == 0));
    }

    #[test]
    fn overlong_wire_names_are_filtered_at_populate() {
        let session: Arc<dyn McpSession> = FakeSession::new();
        let mut entry = ServerEntry {
            name: "averyveryverylongservername".into(),
            config: None,
            transport_kind: "fake",
            origin: PathBuf::new(),
            status: McpServerStatus::Connecting,
            session: None,
            tools: Vec::new(),
            prompts: Vec::new(),
        };
        let long_tool = "x".repeat(60);
        entry.populate(StartResult {
            session,
            tool_infos: vec![ToolInfo {
                name: long_tool,
                description: String::new(),
                input_schema: serde_json::json!({}),
            }],
            prompt_infos: Vec::new(),
        });
        assert!(entry.tools.is_empty(), "64-char wire limit must filter");
    }

    #[test]
    fn stub_handle_publishes_wire_names() {
        let handle = stub_handle(&[("srv.tool", "a tool")]);
        assert!(handle.has_tool("srv.tool"));
        let descriptors = handle.tool_descriptors();
        assert_eq!(descriptors[0].wire_name, "srv__tool");
        assert_eq!(descriptors[0].parameters["type"], "object");
    }

    #[test]
    fn http_401_maps_to_needs_auth() {
        let status = status_from_err(&McpError::HttpError {
            server: "srv".into(),
            status: 401,
            reason: "Bearer realm=...".into(),
        });
        assert!(matches!(
            status,
            McpServerStatus::NeedsAuth { url: Some(ref u) } if u.contains("realm")
        ));
        let other = status_from_err(&McpError::Timeout {
            server: "srv".into(),
            timeout_ms: 10,
        });
        assert!(matches!(other, McpServerStatus::Failed(_)));
    }

    /// End-to-end over stdio against a minimal in-process MCP server (python3
    /// speaking newline-delimited JSON-RPC): connect, list, call, timeout path.
    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_end_to_end_against_a_mock_server() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no python3 on this host; skip rather than fail
        }
        const SCRIPT: &str = r#"
import json, sys, time
def send(msg): sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method, rid = req.get("method"), req.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{},"prompts":{}},"serverInfo":{"name":"mock","version":"1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"echo","description":"Echo a message","inputSchema":{"type":"object","properties":{"message":{"type":"string"}}}}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"prompts":[{"name":"greet","description":"Greet","arguments":[{"name":"who","required":True}]}]}})
    elif method == "tools/call":
        name = req["params"]["name"]
        if name == "echo":
            msg = req["params"].get("arguments", {}).get("message", "")
            send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":f"echo: {msg}"}],"isError":False}})
        elif name == "boom":
            send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":"exploded"}],"isError":True}})
        elif name == "hang":
            time.sleep(30)
        else:
            send({"jsonrpc":"2.0","id":rid,"error":{"code":-32602,"message":"unknown tool"}})
    elif method == "prompts/get":
        send({"jsonrpc":"2.0","id":rid,"result":{"messages":[{"role":"user","content":{"type":"text","text":"hello prompt"}}]}})
"#;
        let config = ServerConfig {
            name: "mock".into(),
            timeout: Duration::from_secs(5),
            transport: Transport::Stdio {
                program: "python3".into(),
                args: vec!["-u".into(), "-c".into(), SCRIPT.into()],
                environment: HashMap::new(),
            },
        };
        let session = start_session(&config).await.expect("connect mock server");

        let tools = session.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let text = session
            .call_tool("echo", &serde_json::json!({"message": "hi"}))
            .await
            .unwrap();
        assert_eq!(text, "echo: hi");

        // `isError: true` surfaces as an error carrying the tool's text.
        let err = session
            .call_tool("boom", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exploded"), "got: {err}");

        // Prompts flow through the same session.
        let prompts = session.list_prompts().await.unwrap();
        assert_eq!(prompts.len(), 1);
        let messages = session.get_prompt("greet", &HashMap::new()).await.unwrap();
        assert_eq!(messages[0].text.as_deref(), Some("hello prompt"));

        // A hanging tool call must hit the per-request timeout, not hang forever.
        let start = std::time::Instant::now();
        let err = session
            .call_tool("hang", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Timeout { .. }), "got: {err}");
        assert!(start.elapsed() < Duration::from_secs(10));

        session.shutdown().await;
    }
}
