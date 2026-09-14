//! Shell command execution: `bash` plus the background-task companions
//! `bash_status`, `bash_watch`, and `bash_kill`.
//!
//! Unlike the filesystem tools these run a child process for potentially
//! minutes, so they implement `PortableTool` manually instead of going
//! through the workspace blocking lock: holding that mutex across a
//! `sleep 60` would freeze every other tool call.
//!
//! Background tasks live in a session-shared registry on [`Workspace`]
//! (same pattern as the todo store), so the tool that spawned a job and
//! the tool that polls it see the same buffers.

use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{Duration, sleep},
};

use crate::child_guard::ChildGuard;

use super::{MAX_OUTPUT_BYTES, Result, Workspace, clip, denied, failure, invalid};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MIN_TIMEOUT_SECS: u64 = 5;
const DEFAULT_WATCH_TIMEOUT_SECS: u64 = 60;
/// Poll cadence for background task supervision and `bash_watch`.
const POLL: Duration = Duration::from_millis(100);
const READ_CHUNK: usize = 4096;

/// `find` targets that would scan an entire disk or system tree. The deny
/// guard keeps the agent from hanging on a full-disk walk.
const DANGEROUS_FIND_ROOTS: [&str; 23] = [
    "/", "/.", "/*", "/~", "/root", "/home", "/Users", "/var", "/usr", "/etc", "/sys", "/proc",
    "/dev", "/opt", "/mnt", "/media", "/srv", "/bin", "/sbin", "/lib", "/System", "/Library",
    "/private",
];

// ---------------------------------------------------------------------------
// Background job registry
// ---------------------------------------------------------------------------

type OutputBuf = Arc<Mutex<String>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgState {
    Running,
    Exited(i32),
    Killed,
}

/// One background task. Every field is shared behind an `Arc` so the
/// spawner, the reader/waiter tasks, and the poll tools all observe the
/// same job without holding any lock across an await.
#[derive(Clone)]
pub(crate) struct BgJob {
    /// Kept for diagnostics; the reference stores it on the job too.
    #[allow(dead_code)]
    pub command: String,
    output: OutputBuf,
    state: Arc<Mutex<BgState>>,
    /// The live child wrapped in a kill-on-drop guard, if it has neither
    /// exited nor been killed. `None` once `bash_kill` takes it or the
    /// waiter reaps it.
    child: Arc<Mutex<Option<ChildGuard>>>,
}

impl BgJob {
    fn status_line(&self) -> String {
        match *self.state.lock().unwrap() {
            BgState::Running => "status: running".into(),
            BgState::Exited(code) => format!("status: exited (exit code: {code})"),
            BgState::Killed => "status: killed".into(),
        }
    }

    fn snapshot_output(&self) -> String {
        truncate_output(&compress_output(&self.output.lock().unwrap().clone()))
    }
}

#[derive(Clone, Default)]
pub(crate) struct BashJobs {
    inner: Arc<Mutex<JobsInner>>,
}

#[derive(Default)]
struct JobsInner {
    next_id: u64,
    jobs: HashMap<String, BgJob>,
}

impl BashJobs {
    fn register(&self, job: BgJob) -> String {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = format!("bg_{}", inner.next_id);
        inner.jobs.insert(id.clone(), job);
        id
    }

    fn get(&self, id: &str) -> Option<BgJob> {
        self.inner.lock().unwrap().jobs.get(id).cloned()
    }
}

/// Supervise a background child: poll for exit without holding the child
/// slot across an await, so `bash_kill` can always take the handle.
fn spawn_waiter(job: &BgJob) {
    let slot = job.child.clone();
    let state = job.state.clone();
    tokio::spawn(async move {
        loop {
            if matches!(*state.lock().unwrap(), BgState::Killed) {
                break;
            }
            let status = slot
                .lock()
                .unwrap()
                .as_mut()
                .and_then(|guard| guard.try_status().map(|s| s.code().unwrap_or(-1)));
            match status {
                Some(code) => {
                    *state.lock().unwrap() = BgState::Exited(code);
                    break;
                }
                None => sleep(POLL).await,
            }
        }
    });
}

/// Append a piped stream into the shared output buffer until EOF.
async fn drain<R: tokio::io::AsyncRead + Unpin>(mut reader: R, buf: OutputBuf) {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buf
                .lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&chunk[..n])),
        }
    }
}

// ---------------------------------------------------------------------------
// Output shaping (reference: compress_output + truncate)
// ---------------------------------------------------------------------------

/// Remove SGR escape sequences (`ESC [ ... m`) the way the reference's
/// `strip_ansi` does.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        if chars.next() != Some('[') {
            out.push('\x1b');
            continue;
        }
        let mut terminated = false;
        for next in chars.by_ref() {
            if next == 'm' {
                terminated = true;
                break;
            }
            if !next.is_ascii_digit() && next != ';' {
                break;
            }
        }
        if !terminated {
            out.push_str("\x1b[");
        }
    }
    out
}

/// Collapse runs of blank lines to a single empty line.
fn compress_output(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let mut lines: Vec<&str> = Vec::new();
    let mut prev_blank = false;
    for line in stripped.lines() {
        if line.trim().is_empty() {
            if !prev_blank {
                lines.push("");
                prev_blank = true;
            }
        } else {
            lines.push(line);
            prev_blank = false;
        }
    }
    lines.join("\n")
}

fn truncate_output(text: &str) -> String {
    let (clipped, truncated) = clip(text, MAX_OUTPUT_BYTES);
    if truncated {
        format!("{clipped}\n... [output truncated]")
    } else {
        clipped.to_string()
    }
}

fn format_exit(output: &str, code: i32) -> String {
    if code == 0 {
        if output.is_empty() {
            "Exit code: 0".into()
        } else {
            output.to_string()
        }
    } else if output.is_empty() {
        format!("Exit code: {code}")
    } else {
        format!("{output}\nExit code: {code}")
    }
}

// ---------------------------------------------------------------------------
// Command shaping (reference: parse_cd_hint, denied_command_reason)
// ---------------------------------------------------------------------------

fn unquote(text: &str) -> &str {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        if (first == b'"' || first == b'\'') && bytes[bytes.len() - 1] == first {
            return &text[1..text.len() - 1];
        }
    }
    text
}

/// Split `DIR && REST` after a leading `cd `, mirroring the reference's
/// `^cd%s+(.-)%s+&&%s+(.+)$`. `&&` must sit between whitespace runs.
fn split_cd_tail(rest: &str) -> Option<(&str, &str)> {
    let (dir, tail) = rest.split_once("&&")?;
    if dir.ends_with(|c: char| c.is_whitespace())
        && tail.starts_with(|c: char| c.is_whitespace())
        && !dir.trim().is_empty()
    {
        Some((dir.trim_end(), tail.trim_start()))
    } else {
        None
    }
}

fn parse_cd_hint(workdir: Option<&str>, command: &str) -> (String, Option<String>) {
    if let Some(dir) = workdir.filter(|dir| !dir.trim().is_empty()) {
        return (command.to_string(), Some(dir.to_string()));
    }
    if let Some(rest) = command
        .strip_prefix("cd ")
        .or_else(|| command.strip_prefix("cd\t"))
        && let Some((dir, tail)) = split_cd_tail(rest)
    {
        return (tail.to_string(), Some(unquote(dir).to_string()));
    }
    (command.to_string(), None)
}

fn denied_command_reason(command: &str) -> Option<String> {
    let cmd = command.trim();
    let root = cmd.strip_prefix("find ").and_then(|rest| {
        rest.split_whitespace()
            .next()
            .filter(|root| !root.starts_with('-'))
    })?;
    if DANGEROUS_FIND_ROOTS.contains(&root) {
        Some(format!(
            "refused: `find` from a filesystem root ({root}) is blocked to avoid hanging on a \
             full-disk scan. Scope `find` to a project subdirectory, or use `glob`/`grep` instead."
        ))
    } else {
        None
    }
}

/// Resolve the effective command and working directory. The `cd DIR &&`
/// form is rewritten to run in `DIR` so the model's habitual phrasing
/// keeps working; explicit `workdir` wins.
fn prepare(workspace: &Workspace, args: &BashArgs) -> Result<(String, std::path::PathBuf)> {
    let (command, dir) = parse_cd_hint(args.workdir.as_deref(), &args.command);
    if command.trim().is_empty() {
        return Err(invalid("command must not be empty"));
    }
    if let Some(reason) = denied_command_reason(&command) {
        return Err(denied(reason));
    }
    let cwd = match dir {
        Some(dir) => {
            let path = workspace.resolve(&dir)?;
            if !path.is_dir() {
                return Err(invalid(format!("workdir is not a directory: {}", dir)));
            }
            path
        }
        None => workspace.root().to_path_buf(),
    };
    Ok((command, cwd))
}

fn spawn(workspace_root: &Path, command: &str, cwd: &Path) -> Result<ChildGuard> {
    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so a kill takes down the whole command tree,
    // not just the bash wrapper (pair with ChildGuard's killpg).
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child: Command = cmd.into();
    let child = child.spawn().map_err(|error| {
        failure(format!(
            "failed to run command in {}: {error}",
            workspace_root.display()
        ))
    })?;
    Ok(ChildGuard::new(child))
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashArgs {
    /// The bash command to execute.
    pub command: String,
    /// Timeout in seconds (default 120, minimum 5).
    pub timeout: Option<u64>,
    /// Working directory, relative to the workspace (default: workspace
    /// root, or the directory in a leading `cd DIR && `).
    pub workdir: Option<String>,
    /// Short description (3-5 words) of what the command does.
    pub description: Option<String>,
    /// Run in background and return a task_id for later polling.
    #[serde(default)]
    pub background: bool,
}

#[derive(Debug)]
pub struct BashOutput {
    pub text: String,
}

impl IntoToolOutput for BashOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

#[derive(Clone)]
pub struct Bash(pub Workspace);

impl Bash {
    async fn execute(workspace: &Workspace, args: BashArgs) -> Result<BashOutput> {
        let (command, cwd) = prepare(workspace, &args)?;
        let timeout_secs = args
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .max(MIN_TIMEOUT_SECS);
        let mut guard = spawn(workspace.root(), &command, &cwd)?;
        let output: OutputBuf = Arc::default();

        if args.background {
            let job = BgJob {
                command,
                output: output.clone(),
                state: Arc::new(Mutex::new(BgState::Running)),
                child: Arc::new(Mutex::new(Some(guard))),
            };
            // The registry holds the live child; the readers drain its pipes
            // into the shared buffer until EOF.
            let mut held = job.child.lock().unwrap().take().unwrap();
            spawn_readers(&mut held, &output);
            job.child.lock().unwrap().replace(held);
            spawn_waiter(&job);
            let id = workspace.bash_jobs.register(job);
            return Ok(BashOutput {
                text: format!(
                    "Background task: {id}\nuse bash_status(task_id=\"{id}\") to check \
                     output\nuse bash_kill(task_id=\"{id}\") to terminate"
                ),
            });
        }
        let mut readers = spawn_readers(&mut guard, &output);

        // Reap the child, then join the readers: `wait` can return while the
        // pipe still holds undrained tail bytes.
        let wait_and_drain = async {
            let status = guard
                .status()
                .await
                .map_err(|error| failure(format!("wait failed: {error}")))?;
            for handle in readers.drain(..) {
                let _ = handle.await;
            }
            Ok(status)
        };
        match tokio::time::timeout(Duration::from_secs(timeout_secs), wait_and_drain).await {
            Ok(status) => {
                let code = status?.code().unwrap_or(-1);
                let text = truncate_output(&compress_output(&output.lock().unwrap().clone()));
                if code == 0 {
                    Ok(BashOutput {
                        text: format_exit(&text, code),
                    })
                } else {
                    Err(failure(format_exit(&text, code)))
                }
            }
            Err(_) => {
                // Kill the whole process group and reap it before reporting;
                // dropping the guard would do the same, but unreaped.
                guard.kill_and_reap().await;
                let partial = truncate_output(&compress_output(&output.lock().unwrap().clone()));
                Err(failure(format!(
                    "{partial}\ntool bash timed out after {timeout_secs}s"
                )))
            }
        }
    }
}

fn spawn_readers(guard: &mut ChildGuard, output: &OutputBuf) -> Vec<tokio::task::JoinHandle<()>> {
    let mut readers = Vec::new();
    if let Some(stdout) = guard.take_stdout() {
        readers.push(tokio::spawn(drain(stdout, output.clone())));
    }
    if let Some(stderr) = guard.take_stderr() {
        readers.push(tokio::spawn(drain(stderr, output.clone())));
    }
    readers
}

impl PortableTool for Bash {
    const NAME: &'static str = "bash";
    type Args = BashArgs;
    type Output = BashOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Executes a non-interactive bash command (`bash -c`) with combined stdout/stderr captured. \
         Runs in the workspace root unless workdir is given or the command starts with \
         `cd DIR && `. Default timeout is 120s (minimum 5): timed-out commands are killed and the \
         partial output is returned. Set background:true to run long tasks in the background and \
         poll them with bash_status, bash_watch, and bash_kill. Use for system commands (git, \
         builds, tests); prefer the file tools for file operations."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BashArgs)).expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        Self::execute(&self.0, args).await
    }
}

// ---------------------------------------------------------------------------
// bash_status / bash_watch / bash_kill
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashStatusArgs {
    /// The task_id returned by bash.
    pub task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashWatchArgs {
    /// The task_id returned by bash.
    pub task_id: String,
    /// Substring to wait for in the task's output.
    pub pattern: Option<String>,
    /// Max seconds to wait (default 60).
    pub timeout: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BashKillArgs {
    /// The task_id returned by bash.
    pub task_id: String,
}

#[derive(Debug)]
pub struct BashStatusOutput {
    pub text: String,
}

impl IntoToolOutput for BashStatusOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

fn unknown_task(id: &str) -> rig_core::tool::ToolExecutionError {
    invalid(format!("unknown task_id \"{id}\""))
}

impl Bash {
    pub(crate) fn status_text(&self, id: &str) -> Result<(String, bool)> {
        let job = self.0.bash_jobs.get(id).ok_or_else(|| unknown_task(id))?;
        let output = job.snapshot_output();
        let text = if output.is_empty() {
            format!("{}\nno output yet", job.status_line())
        } else {
            format!("{}\n{output}", job.status_line())
        };
        let is_error = matches!(*job.state.lock().unwrap(), BgState::Exited(code) if code != 0);
        Ok((text, is_error))
    }
}

#[derive(Clone)]
pub struct BashStatus(pub Workspace);

impl PortableTool for BashStatus {
    const NAME: &'static str = "bash_status";
    type Args = BashStatusArgs;
    type Output = BashStatusOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Check status and current output of a background bash task.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BashStatusArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        let (text, is_error) = Bash(self.0.clone()).status_text(&args.task_id)?;
        if is_error {
            Err(failure(text))
        } else {
            Ok(BashStatusOutput { text })
        }
    }
}

#[derive(Clone)]
pub struct BashWatch(pub Workspace);

impl PortableTool for BashWatch {
    const NAME: &'static str = "bash_watch";
    type Args = BashWatchArgs;
    type Output = BashStatusOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Wait for a substring in a background bash task's output, or for the task to exit. \
         Polls until the pattern matches, the task exits, or the timeout (default 60s) elapses."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BashWatchArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        let job = self
            .0
            .bash_jobs
            .get(&args.task_id)
            .ok_or_else(|| unknown_task(&args.task_id))?;
        let timeout_secs = args.timeout.unwrap_or(DEFAULT_WATCH_TIMEOUT_SECS);
        let pattern = args.pattern;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            let output = job.snapshot_output();
            let exited = match *job.state.lock().unwrap() {
                BgState::Running => None,
                BgState::Exited(code) => Some(code),
                BgState::Killed => Some(-1),
            };
            let matched = pattern
                .as_deref()
                .is_some_and(|pattern| output.contains(pattern));
            if matched {
                let qualifier = match exited {
                    Some(_) => " (task exited)",
                    None => " (task still running)",
                };
                return Ok(BashStatusOutput {
                    text: format!("pattern found{qualifier}\n{output}"),
                });
            }
            if let Some(code) = exited {
                let text = if output.is_empty() {
                    format!("task exited (code: {code})")
                } else {
                    format!("task exited (code: {code})\n{output}")
                };
                return if code == 0 {
                    Ok(BashStatusOutput { text })
                } else {
                    Err(failure(text))
                };
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(BashStatusOutput {
                    text: format!("timed out after {timeout_secs}s (task still running)\n{output}"),
                });
            }
            sleep(POLL).await;
        }
    }
}

#[derive(Clone)]
pub struct BashKill(pub Workspace);

impl PortableTool for BashKill {
    const NAME: &'static str = "bash_kill";
    type Args = BashKillArgs;
    type Output = BashStatusOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Terminate a background bash task.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BashKillArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        let job = self
            .0
            .bash_jobs
            .get(&args.task_id)
            .ok_or_else(|| unknown_task(&args.task_id))?;
        if let BgState::Exited(code) = *job.state.lock().unwrap() {
            return Ok(BashStatusOutput {
                text: format!("task already exited (code: {code})"),
            });
        }
        if let Some(mut guard) = job.child.lock().unwrap().take() {
            // Kill-and-reap off the hot path so the tool responds
            // immediately; the guard's Drop is the backstop.
            tokio::spawn(async move {
                guard.kill_and_reap().await;
            });
        }
        *job.state.lock().unwrap() = BgState::Killed;
        Ok(BashStatusOutput {
            text: format!("task {} killed", args.task_id),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::tool::PortableTool;
    use serde_json::json;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        (dir, workspace)
    }

    async fn invoke<T: PortableTool<Error = rig_core::tool::ToolExecutionError>>(
        tool: &T,
        args: serde_json::Value,
    ) -> Result<T::Output> {
        tool.call(serde_json::from_value(args).unwrap()).await
    }

    fn bash_text(result: Result<BashOutput>) -> String {
        match result {
            Ok(out) => out
                .into_tool_output()
                .unwrap()
                .as_text()
                .unwrap()
                .to_string(),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn parse_cd_hint_variants() {
        let (cmd, dir) = parse_cd_hint(None, "cargo test");
        assert_eq!((cmd.as_str(), dir), ("cargo test", None));
        let (cmd, dir) = parse_cd_hint(Some("sub"), "cargo test");
        assert_eq!((cmd.as_str(), dir.as_deref()), ("cargo test", Some("sub")));
        let (cmd, dir) = parse_cd_hint(None, "cd sub && cargo test");
        assert_eq!(
            (cmd.as_str(), dir.as_deref()),
            ("cargo test", Some("sub")),
            "cd prefix is stripped into a workdir"
        );
        let (cmd, dir) = parse_cd_hint(None, "cd \"my dir\"\t&&\tmake");
        assert_eq!((cmd.as_str(), dir.as_deref()), ("make", Some("my dir")));
        // && without surrounding whitespace is not a cd separator.
        let (cmd, dir) = parse_cd_hint(None, "cd a&&b");
        assert_eq!((cmd.as_str(), dir), ("cd a&&b", None));
        // Explicit workdir wins over the cd hint.
        let (cmd, dir) = parse_cd_hint(Some("x"), "cd y && make");
        assert_eq!((cmd.as_str(), dir.as_deref()), ("cd y && make", Some("x")));
    }

    #[test]
    fn find_from_filesystem_roots_is_denied() {
        for cmd in ["find / -name x", "find  /Users  -type f"] {
            assert!(denied_command_reason(cmd).is_some(), "{cmd}");
        }
        assert!(denied_command_reason("find . -name x").is_none());
        assert!(denied_command_reason("grep -r x /etc").is_none());
        assert!(denied_command_reason("find -name flag").is_none());
    }

    #[test]
    fn compress_output_strips_ansi_and_collapses_blanks() {
        let raw = "\x1b[1mbold\x1b[0m\n\n\n\n\ntail";
        assert_eq!(compress_output(raw), "bold\n\ntail");
        assert_eq!(compress_output("a\n \n \tb"), "a\n\n \tb");
    }

    #[test]
    fn format_exit_shapes_llm_text() {
        assert_eq!(format_exit("", 0), "Exit code: 0");
        assert_eq!(format_exit("out", 0), "out");
        assert_eq!(format_exit("", 3), "Exit code: 3");
        assert_eq!(format_exit("out", 3), "out\nExit code: 3");
    }

    #[tokio::test]
    async fn foreground_success_and_exit_codes() {
        let (_dir, workspace) = workspace();
        let tool = Bash(workspace);
        let out = invoke(&tool, json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert_eq!(out.into_tool_output().unwrap().as_text().unwrap(), "hello");
        let err = invoke(&tool, json!({"command": "echo oops >&2; exit 3"}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "oops\nExit code: 3");
        let empty = invoke(&tool, json!({"command": "true"})).await.unwrap();
        assert_eq!(
            empty.into_tool_output().unwrap().as_text().unwrap(),
            "Exit code: 0"
        );
    }

    #[tokio::test]
    async fn workdir_and_cd_hint_are_honored() {
        let (dir, workspace) = workspace();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let tool = Bash(workspace.clone());
        let out = invoke(&tool, json!({"command": "cd sub && pwd"}))
            .await
            .unwrap();
        let text = out
            .into_tool_output()
            .unwrap()
            .as_text()
            .unwrap()
            .to_string();
        assert!(text.trim_end().ends_with("/sub"), "got {text}");
        let out = invoke(&tool, json!({"command": "pwd", "workdir": "sub"}))
            .await
            .unwrap();
        let text = out
            .into_tool_output()
            .unwrap()
            .as_text()
            .unwrap()
            .to_string();
        assert!(text.trim_end().ends_with("/sub"), "got {text}");
        // Outside the workspace is refused.
        assert!(
            invoke(&tool, json!({"command": "pwd", "workdir": "../"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn timeout_kills_and_returns_partial_output() {
        let (_dir, workspace) = workspace();
        let tool = Bash(workspace);
        let err = invoke(
            &tool,
            json!({"command": "echo partial; sleep 30", "timeout": 5}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("partial"), "{}", err);
        assert!(err.to_string().contains("timed out after 5s"), "{}", err);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn timeout_kills_the_whole_process_group() {
        let (dir, workspace) = workspace();
        let tool = Bash(workspace);
        // Spawn a grandchild `sleep`, record its pid, then hang the
        // parent: the timeout must take down both via the process group.
        let err = invoke(
            &tool,
            json!({
                "command": "sleep 30 & echo $!; sleep 30",
                "timeout": 5
            }),
        )
        .await
        .unwrap_err();
        let pid: i32 = err
            .to_string()
            .lines()
            .find_map(|l| l.trim().parse::<i32>().ok())
            .expect("grandchild pid in partial output");
        assert!(err.to_string().contains("timed out after 5s"), "{err}");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild {pid} survived the timeout kill"
            );
            if !alive {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        drop(dir);
    }

    #[tokio::test]
    async fn denied_find_command_is_refused() {
        let (_dir, workspace) = workspace();
        let err = invoke(&Bash(workspace), json!({"command": "find / -name x"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("refused"), "{}", err);
    }

    #[tokio::test]
    async fn background_task_lifecycle() {
        let (_dir, workspace) = workspace();
        let bash = Bash(workspace.clone());
        let out = invoke(
            &bash,
            json!({"command": "echo hi from bg", "background": true}),
        )
        .await
        .unwrap();
        let text = out
            .into_tool_output()
            .unwrap()
            .as_text()
            .unwrap()
            .to_string();
        let id = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Background task: "))
            .expect("task id line")
            .to_string();
        assert!(text.contains("bash_status"), "{}", text);

        let status = BashStatus(workspace.clone());
        // Poll until the task exits.
        let mut final_text = String::new();
        for _ in 0..100 {
            let out = invoke(&status, json!({"task_id": id})).await.unwrap();
            final_text = out
                .into_tool_output()
                .unwrap()
                .as_text()
                .unwrap()
                .to_string();
            if final_text.contains("exited") {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
        assert!(final_text.contains("exit code: 0"), "status: {final_text}");
        assert!(
            final_text.contains("hi from bg"),
            "output captured: {final_text}"
        );

        // Kill after exit is an idempotent success.
        let kill = BashKill(workspace.clone());
        let out = invoke(&kill, json!({"task_id": id})).await.unwrap();
        assert_eq!(
            out.into_tool_output().unwrap().as_text().unwrap(),
            "task already exited (code: 0)"
        );
    }

    #[tokio::test]
    async fn bash_watch_waits_for_pattern() {
        let (_dir, workspace) = workspace();
        let bash = Bash(workspace.clone());
        let out = invoke(
            &bash,
            json!({"command": "sleep 1; echo ready", "background": true}),
        )
        .await
        .unwrap();
        let text = out
            .into_tool_output()
            .unwrap()
            .as_text()
            .unwrap()
            .to_string();
        let id = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Background task: "))
            .unwrap()
            .to_string();

        let watch = BashWatch(workspace);
        let out = invoke(
            &watch,
            json!({"task_id": id, "pattern": "ready", "timeout": 15}),
        )
        .await
        .unwrap();
        let text = out
            .into_tool_output()
            .unwrap()
            .as_text()
            .unwrap()
            .to_string();
        assert!(text.starts_with("pattern found"), "{}", text);
        assert!(text.contains("ready"), "{}", text);
    }

    #[tokio::test]
    async fn bash_kill_terminates_running_task() {
        let (_dir, workspace) = workspace();
        let bash = Bash(workspace.clone());
        let out = invoke(&bash, json!({"command": "sleep 60", "background": true}))
            .await
            .unwrap();
        let text = out
            .into_tool_output()
            .unwrap()
            .as_text()
            .unwrap()
            .to_string();
        let id = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Background task: "))
            .unwrap()
            .to_string();

        let kill = BashKill(workspace.clone());
        let out = invoke(&kill, json!({"task_id": id})).await.unwrap();
        assert_eq!(
            out.into_tool_output().unwrap().as_text().unwrap(),
            format!("task {id} killed")
        );
        let status = BashStatus(workspace);
        let out = invoke(&status, json!({"task_id": id})).await.unwrap();
        assert!(
            out.into_tool_output()
                .unwrap()
                .as_text()
                .unwrap()
                .contains("status: killed")
        );
    }

    #[tokio::test]
    async fn unknown_task_ids_error() {
        let (_dir, workspace) = workspace();
        for result in [
            invoke(&BashStatus(workspace.clone()), json!({"task_id": "bg_99"})).await,
            invoke(&BashWatch(workspace.clone()), json!({"task_id": "bg_99"})).await,
            invoke(&BashKill(workspace), json!({"task_id": "bg_99"})).await,
        ] {
            assert!(result.is_err());
        }
    }
}
