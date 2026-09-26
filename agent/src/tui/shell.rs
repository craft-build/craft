//! Bash bang-mode: composer input starting `!` (visible) or `!!` (hidden)
//! runs a shell command directly, streamed into a bash tool card.
//! Ported from the reference `craft-ui/src/app/shell.rs` (F.2).

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;

use crate::child_guard::ChildGuard;
use crate::history;
use crate::run::CancelToken;

use super::provider::{AgentEvent, LineKind, ToolCallData, ToolKind, ToolLine};

const SHELL_TIMEOUT: Duration = Duration::from_secs(300);
const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(100);
/// Reference `DEFAULT_MAX_OUTPUT_BYTES` (craft-config).
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
/// Reference `DEFAULT_MAX_OUTPUT_LINES` (craft-config).
const MAX_OUTPUT_LINES: usize = 2000;

/// A composer input recognized as a bang-mode shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellPrefix {
    /// Bytes of `!` / `!!` plus the optional single separating space; the
    /// remainder of the line is the command.
    pub prefix_len: usize,
    pub command: String,
    /// `!` runs visibly (result enters model history); `!!` stays hidden.
    pub visible: bool,
}

/// Parse `! command` / `!! command` input. Returns `None` for anything else
/// (no sigil, empty command, leading whitespace before the sigil).
pub(crate) fn parse_shell_prefix(text: &str) -> Option<ShellPrefix> {
    let (sigil_len, visible) = if text.starts_with("!!") {
        (2, false)
    } else if text.starts_with('!') {
        (1, true)
    } else {
        return None;
    };
    let rest = &text[sigil_len..];
    let prefix_len = if rest.starts_with(' ') {
        sigil_len + 1
    } else {
        sigil_len
    };
    let command = rest.trim();
    if command.is_empty() {
        return None;
    }
    Some(ShellPrefix {
        prefix_len,
        command: command.to_owned(),
        visible,
    })
}

fn card(id: &str, command: &str, lines: Vec<ToolLine>) -> AgentEvent {
    AgentEvent::ToolCall(ToolCallData {
        id: id.to_owned(),
        kind: ToolKind::Bash {
            cmd: command.to_owned(),
        },
        lines,
        awaiting_approval: false,
    })
}

fn body_lines(output: &str, is_error: bool) -> Vec<ToolLine> {
    let kind = if is_error {
        LineKind::Del
    } else {
        LineKind::Context
    };
    output.lines().map(|l| ToolLine::new(kind, l)).collect()
}

/// Execute one bang-mode command: emit the start card, stream the output,
/// finalize the card, and — for visible runs — queue the `I ran: …` user
/// message the next turn will see.
pub(crate) async fn run_shell(
    id: String,
    command: String,
    visible: bool,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancelToken,
    results: Arc<Mutex<Vec<history::Message>>>,
) {
    let _ = tx.send(card(&id, &command, Vec::new()));
    let result = run_command(&command, &id, &tx, &cancel).await;
    let (output, is_error) = match result {
        Ok(out) => (out, false),
        Err(err) => (err, true),
    };
    let _ = tx.send(card(&id, &command, body_lines(&output, is_error)));
    if visible {
        let label = if is_error { "Error" } else { "Output" };
        let msg = history::Message::user(format!("I ran: $ {command}\n\n{label}:\n{output}"));
        results.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
    }
}

async fn run_command(
    command: &str,
    id: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    cancel: &CancelToken,
) -> Result<String, String> {
    let mut cmd = TokioCommand::new("bash");
    cmd.arg("-c")
        .arg(command)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so ChildGuard's killpg takes the command's children
    // with it on cancel/timeout (the reference uses setsid for this).
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn: {e}"))?;

    let (line_tx, line_rx) = mpsc::unbounded_channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        spawn_line_reader(BufReader::new(stdout), line_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_line_reader(BufReader::new(stderr), line_tx.clone());
    }
    let mut guard = ChildGuard::new(child);
    drop(line_tx);
    let mut line_rx = line_rx;

    let mut output = String::new();
    let mut line_count: usize = 0;
    let mut truncated = false;
    let mut last_flush = Instant::now();
    let deadline = Instant::now() + SHELL_TIMEOUT;

    macro_rules! race_deadline {
        ($future:expr) => {
            tokio::select! {
                biased;
                result = $future => result,
                _ = tokio::time::sleep_until(deadline.into()) => {
                    Err(format!("timed out after {}s", SHELL_TIMEOUT.as_secs()))
                }
                _ = cancel.wait() => Err("cancelled".to_string()),
            }
        };
    }

    loop {
        let line = race_deadline!(async { Ok(line_rx.recv().await) });
        match line {
            Ok(Some(line)) => {
                if !truncated {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(&line);
                    line_count += 1;
                    if output.len() > MAX_OUTPUT_BYTES || line_count >= MAX_OUTPUT_LINES {
                        truncated = true;
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                guard.kill_and_reap().await;
                return Err(e);
            }
        }

        if last_flush.elapsed() >= STREAM_FLUSH_INTERVAL && !output.is_empty() {
            flush_output(tx, id, command, &output);
            last_flush = Instant::now();
        }
    }

    let status =
        race_deadline!(async { guard.status().await.map_err(|e| format!("wait error: {e}")) });
    match status {
        Ok(status) => {
            flush_output(tx, id, command, &output);
            if truncated {
                output.push_str("\n[truncated]");
            }
            if !status.success() {
                if output.is_empty() {
                    return Err(format!("exited with code {}", status.code().unwrap_or(-1)));
                }
                return Err(output);
            }
            Ok(output)
        }
        Err(e) => {
            guard.kill_and_reap().await;
            Err(e)
        }
    }
}

fn flush_output(tx: &mpsc::UnboundedSender<AgentEvent>, id: &str, command: &str, output: &str) {
    let _ = tx.send(card(id, command, body_lines(output, false)));
}

fn spawn_line_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    reader: BufReader<R>,
    tx: mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefix(command: &str, prefix_len: usize, visible: bool) -> Option<ShellPrefix> {
        Some(ShellPrefix {
            prefix_len,
            command: command.into(),
            visible,
        })
    }

    #[test]
    fn parse_shell_prefix_cases() {
        assert_eq!(parse_shell_prefix("! ls"), prefix("ls", 2, true));
        assert_eq!(parse_shell_prefix("!! ls"), prefix("ls", 3, false));
        assert_eq!(
            parse_shell_prefix("! cargo test --release"),
            prefix("cargo test --release", 2, true)
        );
        assert_eq!(
            parse_shell_prefix("!! cargo build"),
            prefix("cargo build", 3, false)
        );
        assert_eq!(parse_shell_prefix("! "), None);
        assert_eq!(parse_shell_prefix("!"), None);
        assert_eq!(parse_shell_prefix("!!"), None);
        assert_eq!(parse_shell_prefix("!! "), None);
        assert_eq!(parse_shell_prefix("hello ! world"), None);
        assert_eq!(parse_shell_prefix(" ! ls"), None);
        assert_eq!(parse_shell_prefix("!echo hi"), prefix("echo hi", 1, true));
        assert_eq!(parse_shell_prefix("!!echo hi"), prefix("echo hi", 2, false));
        assert_eq!(parse_shell_prefix("!  ls"), prefix("ls", 2, true));
    }

    /// End-to-end: `run_shell` streams the start and finished bash cards and
    /// queues the `I ran: …` message only for visible runs.
    #[tokio::test]
    async fn run_shell_streams_cards_and_queues_visible_result() {
        use crate::run::cancel_channel;
        use crate::tui::provider::AgentEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = cancel_channel();
        let results = Arc::new(Mutex::new(Vec::new()));

        run_shell(
            "shell-1".into(),
            "echo hi".into(),
            true,
            tx,
            cancel,
            Arc::clone(&results),
        )
        .await;

        let start = rx.recv().await.unwrap();
        assert!(matches!(
            start,
            AgentEvent::ToolCall(ref c) if c.id == "shell-1" && c.lines.is_empty()
        ));
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            done = Some(ev);
        }
        match done.expect("final card") {
            AgentEvent::ToolCall(c) => {
                assert!(matches!(&c.kind, ToolKind::Bash { cmd } if cmd == "echo hi"));
                assert_eq!(
                    c.lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>(),
                    ["hi"]
                );
            }
            other => panic!("unexpected final event: {other:?}"),
        }
        let queued = results.lock().unwrap().clone();
        assert_eq!(queued.len(), 1);
        assert!(
            queued[0]
                .text()
                .starts_with("I ran: $ echo hi\n\nOutput:\nhi")
        );
    }

    #[tokio::test]
    async fn run_shell_hidden_and_errors_do_not_queue_results() {
        use crate::run::cancel_channel;

        let (tx, _rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = cancel_channel();
        let results = Arc::new(Mutex::new(Vec::new()));

        run_shell(
            "shell-1".into(),
            "exit 3".into(),
            false,
            tx,
            cancel,
            Arc::clone(&results),
        )
        .await;

        assert!(results.lock().unwrap().is_empty());
    }

    /// A cancelled token kills a long-running command: `run_shell` returns
    /// promptly with an error card instead of sleeping out the command.
    /// Matches the reference: the `Done` path (including the visible-run
    /// result queue) still runs with `is_error` set.
    #[tokio::test]
    async fn cancelled_shell_kills_the_command() {
        use crate::run::cancel_channel;
        use crate::tui::provider::AgentEvent;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let (flag, cancel) = cancel_channel();
        flag.set(true); // cancelled before it starts, as after an Esc press
        let results = Arc::new(Mutex::new(Vec::new()));

        let started = Instant::now();
        run_shell(
            "shell-1".into(),
            "sleep 30".into(),
            true,
            tx,
            cancel,
            Arc::clone(&results),
        )
        .await;
        assert!(
            started.elapsed() < SHELL_TIMEOUT,
            "cancellation must not wait out the command"
        );

        let _ = rx.recv().await; // start card
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            done = Some(ev);
        }
        match done.expect("final card") {
            AgentEvent::ToolCall(c) => {
                assert!(matches!(
                    c.lines.as_slice(),
                    [line] if line.kind == LineKind::Del && line.text == "cancelled"
                ));
            }
            other => panic!("unexpected final event: {other:?}"),
        }
        assert_eq!(results.lock().unwrap().len(), 1);
    }
}
