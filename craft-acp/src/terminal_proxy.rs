//! `TerminalBackend` implementation that runs commands through the ACP client
//! via `terminal/create`, `terminal/output`, `terminal/wait_for_exit`, and
//! `terminal/release`. Used when the client advertises `terminal: true` in
//! `ClientCapabilities`. Otherwise `LocalTerminal` is used.
//!
//! ACP merges stdout and stderr into a single buffer that's polled (not
//! streamed). This proxy emits everything as `JobEvent::Stdout`; the
//! `JobEvent::Stderr` variant is unreachable under ACP transport.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::ConnectionTo;
use agent_client_protocol::role::acp::Client;
use agent_client_protocol::schema::CreateTerminalRequest;
use agent_client_protocol::schema::EnvVariable;
use agent_client_protocol::schema::KillTerminalRequest;
use agent_client_protocol::schema::ReleaseTerminalRequest;
use agent_client_protocol::schema::SessionId;
use agent_client_protocol::schema::TerminalId;
use agent_client_protocol::schema::TerminalOutputRequest;
use craft_lua::TerminalBackend;
use craft_lua::TerminalEvent;
use craft_lua::TerminalHandle;
use craft_lua::TerminalSpec;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::oneshot;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const SHELL_FLAG: &str = "-c";
const SHELL_BIN: &str = "sh";
const CREATE_FAILED: &str = "acp terminal/create failed";

pub struct AcpTerminal {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
}

impl AcpTerminal {
    pub fn new(cx: ConnectionTo<Client>, session_id: SessionId) -> Self {
        Self { cx, session_id }
    }
}

impl TerminalBackend for AcpTerminal {
    fn start<'a>(
        &'a self,
        spec: TerminalSpec,
    ) -> Pin<Box<dyn Future<Output = Result<TerminalHandle, String>> + Send + 'a>> {
        Box::pin(async move {
            let req = CreateTerminalRequest::new(self.session_id.clone(), SHELL_BIN)
                .args(vec![SHELL_FLAG.to_owned(), spec.cmd])
                .env(env_vars(spec.env))
                .cwd(spec.cwd.map(PathBuf::from));

            let resp = self
                .cx
                .send_request(req)
                .block_task()
                .await
                .map_err(|e| format!("{CREATE_FAILED}: {e}"))?;

            let (event_tx, event_rx) = flume::unbounded::<TerminalEvent>();
            let (kill_tx, kill_rx) = oneshot::channel::<()>();

            tokio::spawn(poll_loop(
                self.cx.clone(),
                self.session_id.clone(),
                resp.terminal_id,
                event_tx,
                kill_rx,
            ));

            let kill: Box<dyn FnOnce() + Send> = Box::new(move || {
                let _ = kill_tx.send(());
            });

            Ok(TerminalHandle {
                events: event_rx,
                kill,
            })
        })
    }
}

fn env_vars(env: Option<std::collections::HashMap<String, String>>) -> Vec<EnvVariable> {
    env.unwrap_or_default()
        .into_iter()
        .map(|(name, value)| EnvVariable::new(name, value))
        .collect()
}

async fn poll_loop(
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    terminal_id: TerminalId,
    event_tx: flume::Sender<TerminalEvent>,
    mut kill_rx: oneshot::Receiver<()>,
) {
    let mut seen: usize = 0;
    let mut pending = String::new();
    let mut killed = false;

    loop {
        if !killed
            && let Ok(()) = kill_rx.try_recv()
        {
            issue_kill(&cx, &session_id, &terminal_id).await;
            killed = true;
        }

        let req = TerminalOutputRequest::new(session_id.clone(), terminal_id.clone());
        match cx.send_request(req).block_task().await {
            Ok(resp) => {
                if resp.output.len() > seen {
                    let new_bytes = &resp.output[seen..];
                    seen = resp.output.len();
                    pending.push_str(new_bytes);
                    flush_lines(&mut pending, &event_tx, false);
                }
                if let Some(status) = resp.exit_status {
                    flush_lines(&mut pending, &event_tx, true);
                    let code = status.exit_code.map(|c| c as i32).unwrap_or(-1);
                    let _ = event_tx.send(TerminalEvent::Exit(code));
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "acp terminal/output failed; ending poll loop");
                let _ = event_tx.send(TerminalEvent::Exit(-1));
                break;
            }
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }

    issue_release(&cx, &session_id, &terminal_id).await;
}

fn flush_lines(pending: &mut String, tx: &flume::Sender<TerminalEvent>, drain_partial: bool) {
    while let Some(idx) = pending.find('\n') {
        let line: String = pending.drain(..=idx).collect();
        let trimmed = line.trim_end_matches(['\n', '\r']).to_owned();
        if tx.send(TerminalEvent::Stdout(trimmed)).is_err() {
            return;
        }
    }
    if drain_partial && !pending.is_empty() {
        let line = std::mem::take(pending);
        let _ = tx.send(TerminalEvent::Stdout(line));
    }
}

async fn issue_kill(cx: &ConnectionTo<Client>, session_id: &SessionId, terminal_id: &TerminalId) {
    let req = KillTerminalRequest::new(session_id.clone(), terminal_id.clone());
    if let Err(e) = cx.send_request(req).block_task().await {
        tracing::warn!(error = %e, "acp terminal/kill failed");
    }
}

async fn issue_release(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    terminal_id: &TerminalId,
) {
    let req = ReleaseTerminalRequest::new(session_id.clone(), terminal_id.clone());
    if let Err(e) = cx.send_request(req).block_task().await {
        tracing::warn!(error = %e, "acp terminal/release failed");
    }
}
