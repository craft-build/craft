use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use serde_json::Value;
use tracing::{debug, info, warn};

use super::error::McpError;
use super::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};
use super::transport::{BoxFuture, McpTransport};

type PendingMap = HashMap<u64, flume::Sender<Result<Value, McpError>>>;

const LINE_DELIMITER: u8 = b'\n';

use crate::ChildGuard;

pub struct StdioTransport {
    name: Arc<str>,
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: AtomicU64,
    timeout: Duration,
    alive: Arc<AtomicBool>,
    _reader_task: tokio::task::JoinHandle<()>,
    _stderr_task: tokio::task::JoinHandle<()>,
    _child: ChildGuard,
}

impl StdioTransport {
    pub fn spawn(
        name: &str,
        program: &str,
        args: &[String],
        environment: &HashMap<String, String>,
        timeout: Duration,
    ) -> Result<Self, McpError> {
        let mut std_cmd = std::process::Command::new(program);
        std_cmd.args(args).envs(environment);

        #[cfg(unix)]
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }

        let mut cmd: tokio::process::Command = std_cmd.into();
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| McpError::StartFailed {
            server: name.into(),
            reason: e.to_string(),
        })?;

        let stdin = child.stdin.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stdout".into(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| McpError::StartFailed {
            server: name.into(),
            reason: "no stderr".into(),
        })?;

        let name: Arc<str> = Arc::from(name);
        let alive = Arc::new(AtomicBool::new(true));
        let pending: Arc<Mutex<PendingMap>> = Arc::new(Mutex::new(HashMap::new()));

        let reader_task = {
            let name = Arc::clone(&name);
            let alive = Arc::clone(&alive);
            let pending = Arc::clone(&pending);
            tokio::spawn(async move {
                let result = Self::reader_loop(&name, &mut BufReader::new(stdout), &pending).await;
                if let Err(e) = &result {
                    warn!(server = &*name, error = %e, "MCP reader loop ended");
                }
                alive.store(false, Ordering::Release);
                for (_, sender) in pending.lock().await.drain() {
                    let _ = sender
                        .send_async(Err(McpError::ServerDied {
                            server: (*name).into(),
                        }))
                        .await;
                }
            })
        };

        let stderr_task = {
            let name = Arc::clone(&name);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                warn!(server = &*name, "{trimmed}");
                            }
                        }
                    }
                }
            })
        };

        Ok(Self {
            name,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            alive,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _child: ChildGuard::new(child),
        })
    }

    async fn reader_loop(
        name: &Arc<str>,
        reader: &mut (impl AsyncBufReadExt + Unpin),
        pending: &Mutex<PendingMap>,
    ) -> Result<(), McpError> {
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| McpError::ServerDied {
                    server: format!("{}: read failed: {e}", &**name),
                })?;

            if n == 0 {
                return Err(McpError::ServerDied {
                    server: (**name).into(),
                });
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            match serde_json::from_str::<JsonRpcResponse>(trimmed) {
                Ok(resp) => {
                    if let Some(id) = resp.id {
                        if let Some(sender) = pending.lock().await.remove(&id) {
                            let result = if let Some(err) = resp.error {
                                Err(McpError::RpcError {
                                    server: (**name).into(),
                                    code: err.code,
                                    message: err.message,
                                })
                            } else {
                                Ok(resp.result.unwrap_or(Value::Null))
                            };
                            let _ = sender.send_async(result).await;
                        } else {
                            debug!(server = &**name, id, "response for unknown request id");
                        }
                    } else {
                        debug!(server = &**name, "received notification (no id)");
                    }
                }
                Err(e) => {
                    debug!(server = &**name, error = %e, line = trimmed, "non-JSON-RPC line from server");
                }
            }
        }
    }

    fn server(&self) -> String {
        (*self.name).into()
    }

    async fn write_line(&self, line: &[u8]) -> Result<(), McpError> {
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line)
            .await
            .map_err(|e| McpError::WriteFailed {
                server: self.server(),
                reason: e.to_string(),
            })?;
        stdin.flush().await.map_err(|e| McpError::WriteFailed {
            server: self.server(),
            reason: e.to_string(),
        })
    }

    fn server_died(&self) -> McpError {
        McpError::ServerDied {
            server: self.server(),
        }
    }

    fn serialize(&self, value: &impl serde::Serialize) -> Result<Vec<u8>, McpError> {
        let mut buf = serde_json::to_vec(value).map_err(|e| McpError::InvalidResponse {
            server: self.server(),
            reason: e.to_string(),
        })?;
        buf.push(LINE_DELIMITER);
        Ok(buf)
    }
}

impl McpTransport for StdioTransport {
    fn send_request<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<Value, McpError>> {
        Box::pin(async move {
            if !self.alive.load(Ordering::Acquire) {
                return Err(self.server_died());
            }

            let start = Instant::now();
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let req = JsonRpcRequest::new(id, method, params);

            let (tx, rx) = flume::bounded(1);
            self.pending.lock().await.insert(id, tx);

            if let Err(e) = self.write_line(&self.serialize(&req)?).await {
                self.pending.lock().await.remove(&id);
                return Err(e);
            }

            let result = tokio::select! {
                r = async { rx.recv_async().await.unwrap_or(Err(self.server_died())) } => r,
                _ = tokio::time::sleep(self.timeout) => {
                    Err(McpError::Timeout {
                        server: self.server(),
                        timeout_ms: self.timeout.as_millis() as u64,
                    })
                }
            };

            if result.is_err() {
                self.pending.lock().await.remove(&id);
            } else {
                info!(server = %self.server(), method, id, duration_ms = start.elapsed().as_millis() as u64, "MCP stdio response");
            }

            result
        })
    }

    fn send_notification<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            let notif = JsonRpcNotification::new(method, params);
            self.write_line(&self.serialize(&notif)?).await
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.alive.store(false, Ordering::Release);
        })
    }

    fn server_name(&self) -> &Arc<str> {
        &self.name
    }

    fn transport_kind(&self) -> &'static str {
        "stdio"
    }

    fn child_pids(&self) -> Vec<u32> {
        vec![self._child.id()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use test_case::test_case;

    async fn read_single_response(input: &str) -> Result<Value, McpError> {
        let pending: Mutex<PendingMap> = Mutex::new(HashMap::new());
        let name: Arc<str> = Arc::from("test");

        let (tx, rx) = flume::bounded(1);
        pending.lock().await.insert(1, tx);

        let mut reader = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let _ = StdioTransport::reader_loop(&name, &mut reader, &pending).await;

        rx.try_recv().unwrap_or(Err(McpError::ServerDied {
            server: "no response received".into(),
        }))
    }

    #[test_case("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "lf_terminated")]
    #[test_case("{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\r\n" ; "crlf_terminated")]
    #[test_case("  {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}  \n" ; "whitespace_padded")]
    #[test_case("\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "blank_lines_before")]
    #[test_case("not json\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n" ; "invalid_json_before")]
    #[tokio::test]
    async fn reader_parses_valid_response(input: &str) {
        assert!(read_single_response(input).await.is_ok());
    }

    #[tokio::test]
    async fn reader_returns_rpc_error() {
        let input =
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32600,\"message\":\"bad\"}}\n";
        assert!(matches!(
            read_single_response(input).await,
            Err(McpError::RpcError { code: -32600, .. })
        ));
    }
}