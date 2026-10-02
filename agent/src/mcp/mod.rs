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
mod manager;
pub mod oauth;
pub mod request;
// Pinned because both `session.rs` (pending deletion) and `session/mod.rs`
// exist; drop the attribute once `session.rs` is gone.
pub mod session;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use serde_json::Value;
use tokio::sync::{mpsc, watch};

use self::config::{McpServerInfo, McpServerStatus, ServerConfig};
use self::error::McpError;
pub use self::manager::{
    start, start_connected, start_with_config, start_with_config_events, start_with_events,
};
pub use self::request::{ElicitOutcome, McpServerRequest};
pub use self::session::McpEvents;
use self::session::{McpSession, PromptInfo, ResourceInfo, ToolInfo};
pub use self::session::{McpToolImage, McpToolOutput};

pub(crate) const SEPARATOR: &str = ".";
pub const WIRE_SEPARATOR: &str = "__";
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
    /// Server-declared `readOnlyHint` (Phase 3): feeds the permission engine.
    pub read_only_hint: Option<bool>,
    /// Server-declared `destructiveHint` (Phase 3): feeds the permission engine.
    pub destructive_hint: Option<bool>,
}

struct McpToolDef {
    qualified_name: Arc<str>,
    raw_name: String,
    description: String,
    input_schema: Value,
    read_only_hint: Option<bool>,
    destructive_hint: Option<bool>,
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
    resources: Vec<ResourceInfo>,
}

impl ServerEntry {
    async fn clear_connection(&mut self) {
        if let Some(old) = self.session.take() {
            old.shutdown().await;
        }
        self.tools.clear();
        self.prompts.clear();
        self.resources.clear();
    }

    fn populate(&mut self, result: StartResult) {
        let StartResult {
            session,
            tool_infos,
            prompt_infos,
            resource_infos,
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
                read_only_hint: info.read_only_hint,
                destructive_hint: info.destructive_hint,
            })
            .collect();
        self.prompts = prompt_infos
            .into_iter()
            .map(|info| McpPromptDef::from_info(&self.name, info))
            .collect();
        self.resources = resource_infos;
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
    /// Read targets keyed by `(server, uri)`, so `McpHandle::read_resource`
    /// routes without scanning the snapshot.
    resources: HashMap<(String, String), ResourceRef>,
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

struct ResourceRef {
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

/// One published MCP resource, flattened with its owning server so the UI
/// and the read tool's verbatim-URI lookup can consume it without a join.
#[derive(Clone)]
pub struct McpResourceInfo {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
}

#[derive(Clone, Default)]
pub struct McpSnapshot {
    pub infos: Vec<McpServerInfo>,
    pub prompts: Vec<McpPromptInfo>,
    pub resources: Vec<McpResourceInfo>,
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
    /// Receiver for server-initiated requests; taken by the TUI loop, dropped
    /// by headless callers so handlers deny cleanly.
    server_requests: Arc<Mutex<Option<mpsc::UnboundedReceiver<McpServerRequest>>>>,
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

    /// Published snapshot generation; annotation sync uses it to skip
    /// re-registration when the tool set has not changed.
    pub fn generation(&self) -> u64 {
        self.snapshot.load().generation
    }

    pub fn reader(&self) -> McpSnapshotReader {
        McpSnapshotReader(Arc::clone(&self.snapshot))
    }

    /// Take the receiver for server-initiated requests (sampling,
    /// elicitation). The TUI provider loop calls this once and drains it
    /// alongside its commands; a second take yields `None`.
    pub fn take_server_requests(&self) -> Option<mpsc::UnboundedReceiver<McpServerRequest>> {
        self.server_requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Drop the request receiver: no frontend will answer, so handlers deny
    /// server-initiated requests instead of parking on a channel nobody
    /// drains. Headless entry points call this.
    pub fn drop_server_requests(&self) {
        self.take_server_requests();
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

    pub async fn call_tool(
        &self,
        qualified_name: &str,
        args: &Value,
    ) -> Result<McpToolOutput, McpError> {
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

    /// Phase 6: [`Self::call_tool`] under the turn's cancellation token —
    /// a cancelled turn tells the server to stop the in-flight call.
    pub async fn call_tool_cancellable(
        &self,
        qualified_name: &str,
        args: &Value,
        cancel: &crate::run::CancelToken,
    ) -> Result<McpToolOutput, McpError> {
        let (raw_name, session) = {
            let idx = self.index.load();
            let Some(t) = idx.tools.get(qualified_name) else {
                return Err(McpError::UnknownTool {
                    name: qualified_name.to_string(),
                });
            };
            (t.raw_name.clone(), Arc::clone(&t.session))
        };
        session.call_tool_cancellable(&raw_name, args, cancel).await
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

    /// `resources/read` on the named server (Phase 5). The (server, uri)
    /// pair must exist in the published index — reads of unlisted uris are
    /// refused the same way unknown tool names are.
    pub async fn read_resource(&self, server: &str, uri: &str) -> Result<String, McpError> {
        let session = {
            let idx = self.index.load();
            let Some(r) = idx.resources.get(&(server.to_string(), uri.to_string())) else {
                return Err(McpError::UnknownResource {
                    uri: format!("{server}{SEPARATOR}{uri}"),
                });
            };
            Arc::clone(&r.session)
        };
        session.read_resource(uri).await
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

struct StartResult {
    session: Arc<dyn McpSession>,
    tool_infos: Vec<ToolInfo>,
    prompt_infos: Vec<PromptInfo>,
    resource_infos: Vec<ResourceInfo>,
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
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
