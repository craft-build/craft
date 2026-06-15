use std::collections::HashMap;
use std::future::Future;
use std::io::{BufRead, BufReader};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

use flume::Receiver;

const READER_BUF_SIZE: usize = 8 * 1024;

#[derive(Clone)]
pub enum JobEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

pub struct TerminalSpec {
    pub cmd: String,
    pub cwd: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub sandbox: Option<craft_sandbox::SandboxProfile>,
}

pub struct TerminalHandle {
    pub events: Receiver<JobEvent>,
    pub kill: Box<dyn FnOnce() + Send>,
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
    let mut command = shell_command(&spec.cmd);

    if let Some(ref profile) = spec.sandbox {
        if profile.mode != craft_sandbox::SandboxMode::Off {
            if !craft_sandbox::available() {
                return Err(
                    "sandbox enabled but backing binary not found; refusing to run unsandboxed"
                        .to_string(),
                );
            }
            craft_sandbox::apply(&mut command, profile)
                .map_err(|e| format!("sandbox apply failed; refusing to run unsandboxed: {e}"))?;
        }
    }

    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                let ret = libc::setsid();
                if ret == -1 {
                    return Err(std::io::Error::last_os_error());
                }
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

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (event_tx, event_rx) = flume::unbounded();

    let stdout_handle = spawn_reader(stdout, "job-stdout", JobEvent::Stdout, &event_tx)?;
    let stderr_handle = spawn_reader(stderr, "job-stderr", JobEvent::Stderr, &event_tx)?;

    let exit_tx = event_tx;
    thread::Builder::new()
        .name("job-wait".into())
        .spawn(move || {
            let code = child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            if let Some(h) = stdout_handle {
                let _ = h.join();
            }
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            let _ = exit_tx.send(JobEvent::Exit(code));
        })
        .map_err(|e| e.to_string())?;

    let kill: Box<dyn FnOnce() + Send> = Box::new(move || kill_process(pid));

    Ok(TerminalHandle {
        events: event_rx,
        kill,
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
        let mut c = Command::new("sh");
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
    unsafe {
        libc::killpg(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        const PROCESS_TERMINATE: u32 = 0x0001;
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn TerminateProcess(handle: *mut std::ffi::c_void, exit_code: u32) -> i32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
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
        };
        assert!(backend.start(spec).await.is_err());
    }
}
