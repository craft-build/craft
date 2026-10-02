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
use crate::run::cancel::CancelTrigger;

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

/// Per-session bookkeeping for bang-mode runs, ported from the reference
/// `ShellState` (`craft-ui/src/app/shell.rs`): monotonically numbered
/// `shell-N` ids, the set of in-flight runs, one cancel trigger per
/// in-flight run, and the visible-run results queued for the next turn.
#[derive(Default)]
pub(crate) struct ShellState {
    id_counter: u64,
    active_ids: std::collections::HashSet<String>,
    /// One trigger per in-flight run, keyed by its id. A trigger cancels
    /// on drop, so removing a finished run's entry doubles as the cleanup
    /// that ends its parent-link task.
    triggers: std::collections::HashMap<String, CancelTrigger>,
    pending_results: Vec<history::Message>,
}

impl ShellState {
    /// Reserve the next run id; active until [`Self::finish_run`].
    pub(crate) fn reserve_id(&mut self) -> String {
        self.id_counter += 1;
        let id = format!("shell-{}", self.id_counter);
        self.active_ids.insert(id.clone());
        id
    }

    /// Ids of runs still in flight.
    #[allow(dead_code)] // parity with the reference API; exercised by tests
    pub(crate) fn active_ids(&self) -> &std::collections::HashSet<String> {
        &self.active_ids
    }

    /// Register a run's cancel trigger (its per-run child token's setting
    /// half, linked under the parent interrupt flag).
    pub(crate) fn add_trigger(&mut self, id: &str, trigger: CancelTrigger) {
        self.triggers.insert(id.to_owned(), trigger);
    }

    /// Retire one finished run: free its id and drop its trigger. The run
    /// is over, so the token cancel the drop performs is harmless — it
    /// only ends the parent-link task.
    pub(crate) fn finish_run(&mut self, id: &str) {
        self.active_ids.remove(id);
        self.triggers.remove(id);
    }

    /// Cancel every in-flight run (loop teardown, session reset).
    pub(crate) fn cancel_all(&mut self) {
        for (_, trigger) in self.triggers.drain() {
            trigger.cancel();
        }
        self.active_ids.clear();
    }

    /// Queue a visible run's `I ran: …` message for the next turn.
    pub(crate) fn push_result(&mut self, msg: history::Message) {
        self.pending_results.push(msg);
    }

    /// Take every queued visible-run result.
    pub(crate) fn drain_results(&mut self) -> Vec<history::Message> {
        std::mem::take(&mut self.pending_results)
    }

    /// Drop queued results without delivering them (`/clear`, `/reset`).
    pub(crate) fn clear_results(&mut self) {
        self.pending_results.clear();
    }
}

fn card(id: &str, command: &str, lines: Vec<ToolLine>) -> AgentEvent {
    AgentEvent::ToolCall(ToolCallData {
        id: id.to_owned(),
        kind: ToolKind::Bash {
            cmd: command.to_owned(),
        },
        lines,
        awaiting_approval: false,
        image: None,
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
/// message the next turn will see. The run is retired in `shell` on every
/// exit path (id released, trigger dropped).
pub(crate) async fn run_shell(
    id: String,
    command: String,
    visible: bool,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancelToken,
    shell: Arc<Mutex<ShellState>>,
) {
    let _ = tx.send(card(&id, &command, Vec::new()));
    let result = run_command(&command, &id, &tx, &cancel).await;
    let (output, is_error) = match result {
        Ok(out) => (out, false),
        Err(err) => (err, true),
    };
    let _ = tx.send(card(&id, &command, body_lines(&output, is_error)));
    let mut shell_guard = shell.lock().unwrap_or_else(|e| e.into_inner());
    if visible {
        let label = if is_error { "Error" } else { "Output" };
        let msg = history::Message::user(format!("I ran: $ {command}\n\n{label}:\n{output}"));
        shell_guard.push_result(msg);
    }
    shell_guard.finish_run(&id);
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
                // Keep the streamed output: the card and the `I ran:` record
                // still show what the command produced before the cut.
                return Err(if output.is_empty() {
                    e
                } else {
                    format!("{e}\n{output}")
                });
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
            Err(if output.is_empty() {
                e
            } else {
                format!("{e}\n{output}")
            })
        }
    }
}

fn flush_output(tx: &mpsc::UnboundedSender<AgentEvent>, id: &str, command: &str, output: &str) {
    let _ = tx.send(card(id, command, body_lines(output, false)));
}

/// Hard cap on one stored output line (finding 60): a single endless
/// line (e.g. `yes | tr -d '\n'`) would otherwise grow the buffered line
/// — and the accumulated output — without bound.
const MAX_LINE_BYTES: usize = 16 * 1024;
/// Marker appended where a line was hard-capped.
const LINE_TRUNC_MARKER: &str = "… [truncated]";

/// Copy child output lines into the channel. Lines are hard-capped at
/// [`MAX_LINE_BYTES`] (with a marker appended) and the remainder of an
/// oversized line is discarded without storing it, so a newline-free
/// flood can't grow memory. Lines are decoded lossily: binary output no
/// longer kills the reader (the old `lines()` loop did).
fn spawn_line_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: BufReader<R>,
    tx: mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        loop {
            let mut bytes = Vec::new();
            // Bounded read: `take` stops buffering past the cap.
            let mut limited = tokio::io::AsyncReadExt::take(&mut reader, MAX_LINE_BYTES as u64 + 1);
            match limited.read_until(b'\n', &mut bytes).await {
                Ok(0) => break, // EOF with nothing buffered
                Ok(_) => {
                    let capped = bytes.len() > MAX_LINE_BYTES;
                    if capped {
                        // Drain the rest of the oversized line up to its
                        // newline without storing it; anything after the
                        // newline stays buffered for the next iteration.
                        let mut sink = Vec::new();
                        loop {
                            sink.clear();
                            match reader.read_until(b'\n', &mut sink).await {
                                Ok(0) => break,
                                Ok(_) if sink.ends_with(b"\n") => break,
                                Ok(_) => {}
                                Err(_) => break,
                            }
                        }
                        bytes.truncate(MAX_LINE_BYTES);
                    } else if bytes.ends_with(b"\n") {
                        bytes.pop();
                        if bytes.ends_with(b"\r") {
                            bytes.pop();
                        }
                    }
                    let mut line = String::from_utf8_lossy(&bytes).into_owned();
                    if capped {
                        line.push_str(LINE_TRUNC_MARKER);
                    }
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
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
        let shell = Arc::new(Mutex::new(ShellState::default()));

        run_shell(
            "shell-1".into(),
            "echo hi".into(),
            true,
            tx,
            cancel,
            Arc::clone(&shell),
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
        let queued = shell.lock().unwrap().drain_results();
        assert_eq!(queued.len(), 1);
        assert!(
            queued[0]
                .text()
                .starts_with("I ran: $ echo hi\n\nOutput:\nhi")
        );
        assert!(
            shell.lock().unwrap().active_ids().is_empty(),
            "the finished run is retired"
        );
    }

    #[tokio::test]
    async fn run_shell_hidden_and_errors_do_not_queue_results() {
        use crate::run::cancel_channel;

        let (tx, _rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = cancel_channel();
        let shell = Arc::new(Mutex::new(ShellState::default()));

        run_shell(
            "shell-1".into(),
            "exit 3".into(),
            false,
            tx,
            cancel,
            Arc::clone(&shell),
        )
        .await;

        assert!(shell.lock().unwrap().drain_results().is_empty());
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
        let shell = Arc::new(Mutex::new(ShellState::default()));

        let started = Instant::now();
        run_shell(
            "shell-1".into(),
            "sleep 30".into(),
            true,
            tx,
            cancel,
            Arc::clone(&shell),
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
        assert_eq!(shell.lock().unwrap().drain_results().len(), 1);
    }

    #[test]
    fn shell_state_ids_lifecycle_and_results() {
        let mut state = ShellState::default();
        let a = state.reserve_id();
        let b = state.reserve_id();
        assert_ne!(a, b, "concurrent runs must get distinct ids");
        assert_eq!(state.active_ids().len(), 2);

        state.finish_run(&a);
        assert!(!state.active_ids().contains(&a));
        assert!(state.active_ids().contains(&b));

        state.push_result(history::Message::user("I ran: x"));
        assert_eq!(state.drain_results().len(), 1);
        assert!(state.drain_results().is_empty(), "drain takes everything");
        state.push_result(history::Message::user("dropped"));
        state.clear_results();
        assert!(state.drain_results().is_empty());
    }

    /// `cancel_all` stops every in-flight run's token; a finished run's
    /// trigger is already gone and cannot be re-cancelled.
    #[tokio::test]
    async fn shell_state_cancel_all_cancels_in_flight_runs() {
        use crate::run::cancel::cancel_pair;

        let mut state = ShellState::default();
        let id_a = state.reserve_id();
        let id_b = state.reserve_id();
        let (trig_a, tok_a) = cancel_pair();
        let (trig_b, tok_b) = cancel_pair();
        state.add_trigger(&id_a, trig_a);
        state.add_trigger(&id_b, trig_b);

        state.finish_run(&id_a); // retired: dropping its trigger ends the
        // link task (the token cancel it performs is post-run cleanup)
        tok_a.wait().await;
        assert!(tok_a.cancelled());

        state.cancel_all();
        tok_b.wait().await;
        assert!(tok_b.cancelled(), "the in-flight run is stopped");
        assert!(state.active_ids().is_empty());
    }

    /// Finding 60: a multi-megabyte single line (no newline) is stored
    /// capped at `MAX_LINE_BYTES` with the truncation marker, and the
    /// reader task still terminates at EOF.
    #[tokio::test]
    async fn endless_single_line_is_capped_and_marked() {
        let chunk = vec![b'y'; 4 * 1024 * 1024]; // multi-MB, no '\n'
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_line_reader(BufReader::new(std::io::Cursor::new(chunk)), tx);
        let line = rx.recv().await.expect("capped line");
        assert_eq!(line.len(), MAX_LINE_BYTES + LINE_TRUNC_MARKER.len());
        assert!(line.ends_with(LINE_TRUNC_MARKER));
        assert!(line[..MAX_LINE_BYTES].chars().all(|c| c == 'y'));
        assert!(
            rx.recv().await.is_none(),
            "drained to EOF: the channel closes with the task"
        );
    }

    /// Ordinary lines still pass through untouched, CRLF stripped to LF.
    #[tokio::test]
    async fn reader_yields_normal_lines_unchanged() {
        let input = b"one\ntwo\r\nthree".to_vec();
        let (tx, mut rx) = mpsc::unbounded_channel();
        spawn_line_reader(BufReader::new(std::io::Cursor::new(input)), tx);
        let mut got = Vec::new();
        while let Some(l) = rx.recv().await {
            got.push(l);
        }
        assert_eq!(got, ["one", "two", "three"]);
    }
}
