use std::collections::HashMap;
use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use flume::Receiver;
use shell_words::join as shell_join;

const READER_BUF_SIZE: usize = 8 * 1024;

#[derive(Clone)]
pub enum JobEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

/// How the process is spawned: through a shell, or straight from an argv the
/// plugin built itself (no quoting rules to get wrong).
pub enum JobCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl From<&str> for JobCommand {
    fn from(cmd: &str) -> Self {
        Self::Shell(cmd.to_string())
    }
}

impl JobCommand {
    fn build(&self) -> Command {
        match self {
            Self::Shell(cmd) => shell_command(cmd),
            Self::Argv(argv) => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
        }
    }

    /// Only for `jobinfo` / `joblist` rows: an argv job spawns from the vec,
    /// so nothing ever re-parses this.
    pub fn display(&self) -> String {
        match self {
            Self::Shell(cmd) => cmd.clone(),
            Self::Argv(argv) => shell_join(argv),
        }
    }
}

/// Where one of the job's streams goes.
pub enum Redirect {
    /// Piped to a reader thread, so callbacks and tails see the lines.
    Capture,
    Discard,
    /// Appended to a file by the child itself: no reader thread, no events,
    /// no tail. Durability instead of reaction.
    File(PathBuf),
}

impl Redirect {
    fn stdio(&self) -> Result<Stdio, String> {
        match self {
            Self::Capture => Ok(Stdio::piped()),
            Self::Discard => Ok(Stdio::null()),
            Self::File(path) => File::options()
                .create(true)
                .append(true)
                .open(path)
                .map(Stdio::from)
                .map_err(|e| format!("cannot open {}: {e}", path.display())),
        }
    }
}

pub struct TerminalSpec {
    pub cmd: JobCommand,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub sandbox: Option<craft_sandbox::SandboxProfile>,
    pub stdout: Redirect,
    pub stderr: Redirect,
}

pub struct TerminalHandle {
    pub pid: u32,
    pub events: Receiver<JobEvent>,
    pub kill: Box<dyn FnOnce() + Send>,
    /// Flipped by the wait thread the moment the process is reaped, so the
    /// kill closure stops signalling a pid the kernel may have handed out
    /// again. Narrower than an exit code, which is only observable once the
    /// `Exit` event reaches the pump.
    pub reaped: Arc<AtomicBool>,
}

pub type TerminalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<TerminalHandle, String>> + Send + 'a>>;

pub trait TerminalBackend: Send + Sync {
    fn start<'a>(&'a self, spec: TerminalSpec) -> TerminalFuture<'a>;
}

pub struct LocalTerminal;

impl TerminalBackend for LocalTerminal {
    fn start<'a>(&'a self, spec: TerminalSpec) -> TerminalFuture<'a> {
        Box::pin(async move { spawn_local_process(spec) })
    }
}

pub fn local_backend() -> Arc<dyn TerminalBackend> {
    Arc::new(LocalTerminal)
}

fn spawn_local_process(spec: TerminalSpec) -> Result<TerminalHandle, String> {
    let mut command = spec.cmd.build();

    if let Some(ref profile) = spec.sandbox
        && profile.mode != craft_sandbox::SandboxMode::Off
    {
        if !craft_sandbox::available() {
            return Err(
                "sandbox enabled but backing binary not found; refusing to run unsandboxed"
                    .to_string(),
            );
        }
        craft_sandbox::apply(&mut command, profile)
            .map_err(|e| format!("sandbox apply failed; refusing to run unsandboxed: {e}"))?;
    }

    command
        .stdout(spec.stdout.stdio()?)
        .stderr(spec.stderr.stdio()?)
        .stdin(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid()?;
                Ok(())
            });
        }
    }

    if let Some(ref dir) = spec.cwd {
        command.current_dir(dir);
    }
    if let Some(ref env_map) = spec.env {
        for (k, v) in env_map {
            command.env(k, v);
        }
    }

    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();

    let (event_tx, event_rx) = flume::unbounded();
    let stdout_handle = spawn_reader(
        child.stdout.take(),
        "job-stdout",
        JobEvent::Stdout,
        &event_tx,
    )?;
    let stderr_handle = spawn_reader(
        child.stderr.take(),
        "job-stderr",
        JobEvent::Stderr,
        &event_tx,
    )?;

    let exit_tx = event_tx;
    let reaped = Arc::new(AtomicBool::new(false));
    let wait_reaped = Arc::clone(&reaped);
    thread::Builder::new()
        .name("job-wait".into())
        .spawn(move || {
            // Reaping frees the pid, and that pid is the process group the
            // kill closure signals. The readers only return once every
            // descendant dropped the pipes, so joining them first keeps a
            // kill target around for as long as the job is really alive.
            if let Some(h) = stdout_handle {
                let _ = h.join();
            }
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            wait_reaped.store(true, Ordering::Relaxed);
            let _ = exit_tx.send(JobEvent::Exit(code));
        })
        .map_err(|e| e.to_string())?;

    let kill_reaped = Arc::clone(&reaped);
    let kill: Box<dyn FnOnce() + Send> = Box::new(move || {
        // Signalling a reaped pid would hit whoever the kernel handed it to
        // next, so stop at the flag. Until then the child is a zombie, and a
        // zombie group leader keeps its pid and pgid off the free list, so
        // the group is still the right target. The flag carries no data of
        // its own, hence Relaxed.
        if !kill_reaped.load(Ordering::Relaxed) {
            kill_process(pid);
        }
    });

    Ok(TerminalHandle {
        pid,
        events: event_rx,
        kill,
        reaped,
    })
}

fn spawn_reader<R, F>(
    stream: Option<R>,
    name: &'static str,
    variant: F,
    tx: &flume::Sender<JobEvent>,
) -> Result<Option<thread::JoinHandle<()>>, String>
where
    R: std::io::Read + Send + 'static,
    F: Fn(String) -> JobEvent + Send + 'static,
{
    let Some(stream) = stream else {
        return Ok(None);
    };
    let tx = tx.clone();
    let handle = thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for line in BufReader::with_capacity(READER_BUF_SIZE, stream)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(variant(line)).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Some(handle))
}

fn shell_command(cmd: &str) -> Command {
    #[cfg(unix)]
    {
        let mut c = Command::new("bash");
        c.arg("-c").arg(cmd);
        c
    }
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd.exe");
        c.arg("/C").arg(cmd);
        c
    }
}

fn kill_process(pid: u32) {
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        let raw = match i32::try_from(pid) {
            Ok(raw) => raw,
            Err(_) => return,
        };
        if let Some(pid) = Pid::from_raw(raw) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn local_backend_runs_echo() {
        let backend = LocalTerminal;
        let spec = TerminalSpec {
            cmd: "echo hello".into(),
            cwd: None,
            env: None,
            sandbox: None,
            stdout: Redirect::Capture,
            stderr: Redirect::Capture,
        };
        let handle = backend.start(spec).await.unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got_exit = false;
        while std::time::Instant::now() < deadline {
            match handle.events.recv_timeout(Duration::from_millis(200)) {
                Ok(JobEvent::Exit(code)) => {
                    assert_eq!(code, 0);
                    got_exit = true;
                    break;
                }
                Ok(_) => {}
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(got_exit);
    }

    #[tokio::test]
    async fn local_backend_invalid_cwd_errors() {
        let backend = LocalTerminal;
        let spec = TerminalSpec {
            cmd: "echo hi".into(),
            cwd: Some("/nonexistent_dir_abc_xyz_123".into()),
            env: None,
            sandbox: None,
            stdout: Redirect::Capture,
            stderr: Redirect::Capture,
        };
        assert!(backend.start(spec).await.is_err());
    }

    #[test]
    fn an_argv_row_reads_back_as_the_same_argv() {
        const ARGV: [&str; 2] = ["echo", "a; echo pwned"];
        let row = JobCommand::Argv(ARGV.map(String::from).to_vec()).display();
        assert_eq!(
            shell_words::split(&row).unwrap(),
            ARGV,
            "the row a user reads must quote what the shell would have eaten"
        );
    }
}
