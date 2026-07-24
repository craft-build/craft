//! One `craft acp` child process, speaking Agent Client Protocol over
//! newline-delimited JSON-RPC on stdio.
//!
//! Wire-format correctness lives here (field names, method names); the rest
//! of the crate treats requests/responses/notifications as opaque
//! `serde_json::Value`s and lets the frontend interpret ACP's `SessionUpdate`
//! shapes directly, since the UI needs to render them anyway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// `session/prompt` resolves only when the agent's turn ends (tool calls,
/// subagents, Flow stages...), which can legitimately run for many minutes.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(60 * 30);
const SESSION_UPDATE_EVENT: &str = "acp://session-update";
const PERMISSION_REQUEST_EVENT: &str = "acp://permission-request";
const QUESTION_REQUEST_EVENT: &str = "acp://question";
const CLOSED_EVENT: &str = "acp://closed";
const TODO_UPDATE_EVENT: &str = "acp://todo-update";

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("failed to launch `{0}`: {1}")]
    Spawn(String, std::io::Error),
    #[error("craft process ended before responding")]
    Disconnected,
    #[error("request timed out")]
    Timeout,
    #[error("agent returned an error: {0}")]
    Agent(Value),
    #[error("malformed response: {0}")]
    Decode(#[from] serde_json::Error),
}

type PendingMap = std::sync::Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

/// Where and how to launch the `craft acp` process. `Local` spawns the
/// resolved binary directly; `Ssh` forwards ACP over an `ssh` connection so
/// the tab drives a remote agent instead of a local one.
#[derive(Debug)]
pub enum LaunchTarget {
    Local {
        craft_binary: PathBuf,
    },
    Ssh {
        host: String,
        remote_craft: Option<String>,
    },
}

pub struct AcpClient {
    stdin_tx: flume::Sender<Value>,
    pending: PendingMap,
    next_id: AtomicI64,
    child: Mutex<Option<Child>>,
}

impl AcpClient {
    /// Spawns the `craft acp` process (locally or over `ssh`) and starts the
    /// reader/writer tasks. Events for this client are tagged with `tab_id` so
    /// the frontend can route them to the right session tab.
    pub fn spawn(
        app: AppHandle,
        tab_id: String,
        target: &LaunchTarget,
        cwd: &Path,
        yolo: bool,
    ) -> Result<Self, ClientError> {
        let label = launch_label(target);
        let mut cmd = build_command(target, cwd, yolo);

        let mut child = cmd.spawn().map_err(|e| ClientError::Spawn(label, e))?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (stdin_tx, stdin_rx) = flume::unbounded::<Value>();
        tokio::spawn(async move {
            while let Ok(msg) = stdin_rx.recv_async().await {
                let Ok(mut line) = serde_json::to_vec(&msg) else {
                    continue;
                };
                line.push(b'\n');
                if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "craft-acp-stderr", "{line}");
            }
        });

        let pending: PendingMap = Default::default();
        let pending_reader = std::sync::Arc::clone(&pending);
        let reader_tab_id = tab_id.clone();
        let reader_app = app.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    handle_incoming(&reader_app, &reader_tab_id, value, &pending_reader);
                } else {
                    tracing::warn!(tab_id = %reader_tab_id, "invalid JSON from craft acp: {line}");
                }
            }
            let _ = reader_app.emit(CLOSED_EVENT, json!({ "tabId": reader_tab_id }));
        });

        Ok(Self {
            stdin_tx,
            pending,
            next_id: AtomicI64::new(1),
            child: Mutex::new(Some(child)),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.call_with_timeout(method, params, REQUEST_TIMEOUT)
            .await
    }

    async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if self.stdin_tx.send(msg).is_err() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(ClientError::Disconnected);
        }
        let raw = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|_| ClientError::Disconnected)?;
        if let Some(err) = raw.get("error") {
            return Err(ClientError::Agent(err.clone()));
        }
        Ok(raw.get("result").cloned().unwrap_or(Value::Null))
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.stdin_tx.send(msg);
    }

    pub async fn initialize(&self) -> Result<Value, ClientError> {
        self.call(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": false, "writeTextFile": false }, "terminal": false },
                "clientInfo": { "name": "craft-desktop", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await
    }

    pub async fn new_session(&self, cwd: &Path) -> Result<Value, ClientError> {
        self.call("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await
    }

    pub async fn load_session(&self, session_id: &str, cwd: &Path) -> Result<Value, ClientError> {
        self.call(
            "session/load",
            json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
        )
        .await
    }

    pub async fn list_sessions(&self, cwd: Option<&Path>) -> Result<Value, ClientError> {
        self.call("session/list", json!({ "cwd": cwd })).await
    }

    /// Resolves only once the agent's turn ends (see `PROMPT_TIMEOUT`).
    /// Callers should spawn this rather than await it inline from a command
    /// handler; `session/update` notifications carry the live content while
    /// the turn is in flight, and the resolution here is just the final
    /// stop-reason signal.
    pub async fn send_prompt(&self, session_id: &str, text: &str) -> Result<Value, ClientError> {
        self.call_with_timeout(
            "session/prompt",
            json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": text }] }),
            PROMPT_TIMEOUT,
        )
        .await
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<Value, ClientError> {
        self.call(
            "session/set_mode",
            json!({ "sessionId": session_id, "modeId": mode_id }),
        )
        .await
    }

    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<Value, ClientError> {
        self.call(
            "session/set_config_option",
            json!({ "sessionId": session_id, "configId": config_id, "value": value }),
        )
        .await
    }

    pub fn cancel(&self, session_id: &str) {
        self.notify("session/cancel", json!({ "sessionId": session_id }));
    }

    /// Enumerate the server's command palette (builtins + custom commands
    /// discovered from the session cwd). Wrapped in a struct so the frontend
    /// can route each entry by `strategy` without re-implementing discovery.
    pub async fn list_commands(&self) -> Result<Value, ClientError> {
        self.call("_craft/listCommands", json!({})).await
    }

    /// Dispatch a `_craft/*` request. Used by the desktop command palette for
    /// commands whose `strategy` is `craft_request` (compact, btw, cd,
    /// command/run, meta/prompt, wiki/*, map/*, etc.). The method string is
    /// server-validated to start with `_craft/` so this can't be used to send
    /// arbitrary JSON-RPC.
    pub async fn craft_command(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        if !method.starts_with("_craft/") {
            return Err(ClientError::Agent(json!({
                "message": "craft_command requires a _craft/ method"
            })));
        }
        self.call(method, params).await
    }

    /// Replies to an incoming `session/request_permission` request. `option_id`
    /// is `None` to send a `cancelled` outcome (e.g. the user closed the prompt).
    pub fn respond_permission(&self, request_id: Value, option_id: Option<&str>) {
        let outcome = match option_id {
            Some(id) => json!({ "outcome": "selected", "optionId": id }),
            None => json!({ "outcome": "cancelled" }),
        };
        let msg = json!({ "jsonrpc": "2.0", "id": request_id, "result": { "outcome": outcome } });
        let _ = self.stdin_tx.send(msg);
    }

    pub fn respond_question(&self, request_id: Value, result: Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": request_id, "result": result });
        let _ = self.stdin_tx.send(msg);
    }

    pub fn kill(&self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = child.start_kill();
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.kill();
    }
}

fn handle_incoming(app: &AppHandle, tab_id: &str, value: Value, pending: &PendingMap) {
    if value.get("result").is_some() || value.get("error").is_some() {
        if let Some(id) = value.get("id").and_then(Value::as_i64) {
            let sender = pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            if let Some(sender) = sender {
                let _ = sender.send(value);
            }
        }
        return;
    }

    let Some(method) = value
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match (method.as_str(), value.get("id").cloned()) {
        ("session/update", None) => {
            // `params` here is the `SessionNotification` wrapper
            // (`{sessionId, update}`), not the `SessionUpdate` itself —
            // unwrap it so the frontend gets the `sessionUpdate`-tagged
            // object directly.
            let inner = params.get("update").cloned().unwrap_or(Value::Null);
            let _ = app.emit(
                SESSION_UPDATE_EVENT,
                json!({ "tabId": tab_id, "update": inner }),
            );
        }
        ("session/request_permission", Some(request_id)) => {
            let _ = app.emit(
                PERMISSION_REQUEST_EVENT,
                json!({ "tabId": tab_id, "requestId": request_id, "params": params }),
            );
        }
        ("session/todo_update", None) | ("_craft/session/todo_update", None) => {
            let todos = params.get("todos").cloned().unwrap_or(Value::Array(vec![]));
            let _ = app.emit(
                TODO_UPDATE_EVENT,
                json!({ "tabId": tab_id, "todos": todos }),
            );
        }
        ("session/question", Some(request_id)) => {
            let _ = app.emit(
                QUESTION_REQUEST_EVENT,
                json!({ "tabId": tab_id, "requestId": request_id, "params": params }),
            );
        }
        (other, id) => {
            tracing::debug!(
                tab_id,
                other,
                has_id = id.is_some(),
                "unhandled ACP message"
            );
        }
    }
}

fn launch_label(target: &LaunchTarget) -> String {
    match target {
        LaunchTarget::Local { craft_binary } => format!("{} acp", craft_binary.display()),
        LaunchTarget::Ssh { host, .. } => format!("ssh {host} (remote craft acp)"),
    }
}

fn build_command(target: &LaunchTarget, cwd: &Path, yolo: bool) -> Command {
    match target {
        LaunchTarget::Local { craft_binary } => {
            let mut cmd = Command::new(craft_binary);
            cmd.arg("acp").arg("--cwd").arg(cwd);
            if yolo {
                cmd.arg("--yolo");
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            cmd
        }
        LaunchTarget::Ssh { host, remote_craft } => {
            let craft = remote_craft.as_deref().unwrap_or("craft");
            let remote = ssh_remote_command(craft, &cwd.to_string_lossy(), yolo);
            let mut cmd = Command::new("ssh");
            cmd.args(["-T", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=30"])
                .arg(host)
                .arg("--")
                .arg(remote);
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            cmd
        }
    }
}

const SQ: char = '\u{27}';
const BS: char = '\u{5c}';

/// Single-quote a string for a POSIX remote shell, escaping embedded single
/// quotes as the `'\''` sequence. Used to build the `--cwd` argument passed
/// through `ssh`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push(SQ);
    for ch in s.chars() {
        if ch == SQ {
            out.push(SQ);
            out.push(BS);
            out.push(SQ);
            out.push(SQ);
        } else {
            out.push(ch);
        }
    }
    out.push(SQ);
    out
}

/// Builds the remote command `ssh` executes: `exec <craft> acp --cwd '<path>' [--yolo]`.
fn ssh_remote_command(craft: &str, cwd: &str, yolo: bool) -> String {
    let mut s = format!("exec {craft} acp --cwd {}", shell_quote(cwd));
    if yolo {
        s.push_str(" --yolo");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("abc", "'abc'"; "plain")]
    #[test_case("a b c", "'a b c'"; "spaces")]
    fn shell_quote_cases(input: &str, expected: &str) {
        assert_eq!(shell_quote(input), expected);
    }

    #[test]
    fn shell_quote_embedded_quote() {
        let expected: String = [SQ, 'a', SQ, BS, SQ, SQ, 'b', SQ].into_iter().collect();
        assert_eq!(shell_quote("a'b"), expected);
    }

    #[test_case("craft", "/home/user/proj", false, "exec craft acp --cwd '/home/user/proj'"; "no_yolo")]
    #[test_case("craft", "/tmp/x", true, "exec craft acp --cwd '/tmp/x' --yolo"; "yolo")]
    #[test_case("/home/u/.cargo/bin/craft", "/home/u/my proj", false, "exec /home/u/.cargo/bin/craft acp --cwd '/home/u/my proj'"; "spaces_in_path")]
    fn ssh_remote_command_cases(craft: &str, cwd: &str, yolo: bool, expected: &str) {
        assert_eq!(ssh_remote_command(craft, cwd, yolo), expected);
    }
}
