//! Process safety for spawned children: kill + reap on drop so a
//! cancelled future, a timed-out tool call, or an early return can
//! never orphan a process tree.
//!
//! On unix, children are expected to run in their own process group
//! (bash's `spawn` sets `process_group(0)`), so `signal_kill` takes
//! down the whole command tree, not just the direct child.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Child;

/// How long [`ChildGuard::kill_and_reap`] waits for the child to exit
/// after SIGKILL before giving up on reaping.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ChildGuard {
    pid: u32,
    child: Option<Child>,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        let pid = child.id().expect("child was polled");
        Self {
            pid,
            child: Some(child),
        }
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Take the child's stdout pipe (for output readers). The guard keeps
    /// ownership of the child itself.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.as_mut().and_then(|child| child.stdout.take())
    }

    /// Take the child's stderr pipe.
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.as_mut().and_then(|child| child.stderr.take())
    }

    /// Await the child's exit. The slot is cleared on success; a second
    /// call errors with `InvalidInput`.
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        match self.child.as_mut() {
            Some(child) => {
                let result = child.wait().await;
                if result.is_ok() {
                    self.child = None;
                }
                result
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child already reaped",
            )),
        }
    }

    /// Non-blocking exit probe: `None` while the child still runs.
    /// Unlike [`ChildGuard::status`] this never awaits, so it is safe
    /// to call while holding a sync lock (the background-job waiter).
    /// Clears the slot when the child has exited.
    pub fn try_status(&mut self) -> Option<ExitStatus> {
        let exited = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        if exited.is_some() {
            self.child = None;
        }
        exited
    }

    /// Kill the whole process group and wait (up to [`REAP_TIMEOUT`])
    /// for the child to be reaped.
    pub async fn kill_and_reap(&mut self) {
        self.signal_kill();
        if let Some(mut child) = self.child.take() {
            tokio::select! {
                _ = child.wait() => {}
                _ = tokio::time::sleep(REAP_TIMEOUT) => {}
            }
        }
    }

    #[cfg(unix)]
    fn signal_kill(&self) {
        if self.child.is_some() {
            unsafe {
                libc::killpg(self.pid as i32, libc::SIGKILL);
            }
        }
    }

    #[cfg(not(unix))]
    fn signal_kill(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }

    #[cfg(unix)]
    fn reap_nonblocking(&mut self) {
        if self.child.take().is_some() {
            unsafe {
                libc::waitpid(self.pid as i32, std::ptr::null_mut(), libc::WNOHANG);
            }
        }
    }

    #[cfg(not(unix))]
    fn reap_nonblocking(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.try_wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.signal_kill();
        }
        self.reap_nonblocking();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};

    use tokio::process::Child;

    use super::ChildGuard;

    fn spawn_sleep() -> Child {
        let mut std_cmd = std::process::Command::new("sleep");
        std_cmd.arg("60");
        unsafe {
            std_cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut cmd: tokio::process::Command = std_cmd.into();
        cmd.spawn().expect("failed to spawn sleep")
    }

    fn is_alive(pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    fn wait_for_death(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("process {pid} still alive after 2s");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drop_kills_child_process() {
        let child = spawn_sleep();
        let pid = child.id().expect("child was polled");
        assert!(is_alive(pid));
        drop(ChildGuard::new(child));
        wait_for_death(pid);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn kill_and_reap_kills_process() {
        let child = spawn_sleep();
        let pid = child.id().expect("child was polled");
        assert!(is_alive(pid));
        let mut guard = ChildGuard::new(child);
        guard.kill_and_reap().await;
        wait_for_death(pid);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_after_reap_returns_error() {
        let child = spawn_sleep();
        let mut guard = ChildGuard::new(child);
        guard.kill_and_reap().await;
        assert!(guard.status().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_status_reports_exit_and_clears_slot() {
        let mut cmd: tokio::process::Command = std::process::Command::new("true").into();
        let child = cmd.spawn().expect("spawn true");
        let pid = child.id().unwrap();
        let mut guard = ChildGuard::new(child);
        // Poll until the fast process exits.
        let mut exited = None;
        for _ in 0..200 {
            if let Some(status) = guard.try_status() {
                exited = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = exited.expect("true exited");
        assert!(status.success());
        // Slot cleared: a second probe and status() both report nothing.
        assert!(guard.try_status().is_none());
        assert!(guard.status().await.is_err());
        wait_for_death(pid);
    }
}
