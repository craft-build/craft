//! The process-global codemode worker pool: warm `craft __codemode-worker`
//! subprocesses shared across every surface (TUI, ACP, `--print`, subagents,
//! the reviewer). Per-run state travels in the `run` frame, so workers stay
//! interchangeable.

use std::ffi::OsString;
use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::diagnose::Failure;
use super::proto::{self, AsyncFrameReader, FromWorker, Message, Reply, ToWorker, WorkerParams};
use super::worker::RECYCLE_AFTER_RUNS;

/// Hard cap on live workers; the parent and its subagents run concurrent
/// `code` calls, and a 5th checkout waits here for one to free up.
const MAX_LIVE_WORKERS: usize = 4;
/// Warm idle workers kept between runs.
const WARM_IDLE_WORKERS: usize = 1;
/// How long a fresh subprocess gets to send its `ready` frame.
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Worker executable override; integration tests point it at
/// `CARGO_BIN_EXE_craft`. Without it (production) the pool re-execs the
/// running binary.
const WORKER_CMD_ENV: &str = "CRAFT_CODEMODE_WORKER_CMD";

/// One live worker — a subprocess handle, or under `cfg(test)` a duplex
/// thread — plus the capacity permit that bounds the live count.
struct Worker {
    handle: Box<dyn WorkerHandle>,
    runs: usize,
    _permit: Option<OwnedSemaphorePermit>,
}

/// The transport-agnostic worker surface: one checked-out run's event
/// stream, reply sink, and teardown. Implemented by the `tokio::process`
/// child (production) and the in-memory duplex (tests).
#[async_trait]
trait WorkerHandle: Send {
    async fn next_event(&mut self) -> io::Result<Message>;
    async fn send(&mut self, frame: &ToWorker) -> io::Result<()>;
    /// Why the worker ended abnormally (exit status or signal), for the
    /// model-facing crash message.
    async fn crash_detail(&mut self) -> String;
    /// Kill and reap (or otherwise dispose of) the worker.
    async fn shutdown(&mut self);
    fn pid(&self) -> Option<u32>;
}

/// A worker checked out for one run. Clean completion calls
/// [`Checkout::complete`] (the worker returns to the pool); any other end
/// drops the checkout, which kills the worker and leaves one warm
/// replacement behind.
pub struct Checkout {
    worker: Option<Worker>,
    pool: &'static Pool,
    settled: bool,
}

impl Checkout {
    pub async fn next_event(&mut self) -> io::Result<Message> {
        self.worker
            .as_mut()
            .expect("checkout settled")
            .handle
            .next_event()
            .await
    }

    pub async fn send_replies(&mut self, replies: Vec<Reply>) -> io::Result<()> {
        let frame = ToWorker::Replies {
            replies: replies
                .into_iter()
                .map(|(id, result)| (id, proto::ReplyOutcome::from_result(result)))
                .collect(),
        };
        self.worker
            .as_mut()
            .expect("checkout settled")
            .handle
            .send(&frame)
            .await
    }

    pub async fn crash_detail(&mut self) -> String {
        self.worker
            .as_mut()
            .expect("checkout settled")
            .handle
            .crash_detail()
            .await
    }

    /// Clean completion: the worker goes back to the pool (or is recycled
    /// once it reaches the run cap).
    pub async fn complete(&mut self) {
        let Some(mut worker) = self.worker.take() else {
            return;
        };
        worker.runs += 1;
        self.settled = true;
        self.pool.checkin(worker).await;
    }
}

impl Drop for Checkout {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let Some(worker) = self.worker.take() else {
            return;
        };
        let pool = self.pool;
        // Reap and respawn warm off the drop path; without a runtime we are
        // tearing the process down anyway and kill_on_drop still applies.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                pool.discard(worker).await;
            });
        }
    }
}

struct PoolState {
    idle: Vec<Worker>,
}

/// The shared pool. Sizing is fixed: constants, no new config knobs.
pub struct Pool {
    state: Mutex<PoolState>,
    capacity: Arc<Semaphore>,
}

static POOL: OnceLock<Pool> = OnceLock::new();

/// The process-global pool, initialized lazily on the first `code` call —
/// codemode is off by default, so nothing spawns at startup.
pub fn pool() -> &'static Pool {
    POOL.get_or_init(|| Pool {
        state: Mutex::new(PoolState { idle: Vec::new() }),
        capacity: Arc::new(Semaphore::new(MAX_LIVE_WORKERS)),
    })
}

impl Pool {
    /// Checks out a worker and starts one run on it.
    pub async fn checkout(&'static self, params: WorkerParams) -> Result<Checkout, Failure> {
        loop {
            // An idle worker can have died while parked (OOM, a stray
            // kill): sending the run frame surfaces that immediately, so
            // discard the corpse and spawn a fresh one instead of failing
            // the run.
            let idle = self.state.lock().unwrap().idle.pop();
            let mut candidate = match idle {
                Some(worker) => worker,
                None => {
                    let permit = self
                        .capacity
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| Failure::crash("codemode worker pool closed"))?;
                    spawn_worker(Some(permit)).await?
                }
            };
            if candidate
                .handle
                .send(&ToWorker::Run(params.clone()))
                .await
                .is_ok()
            {
                RUNS_SENT.fetch_add(1, Ordering::Relaxed);
                return Ok(Checkout {
                    worker: Some(candidate),
                    pool: self,
                    settled: false,
                });
            }
            candidate.handle.shutdown().await;
        }
    }

    async fn checkin(&'static self, mut worker: Worker) {
        if worker.runs >= RECYCLE_AFTER_RUNS {
            // The child self-exits after the cap; the kill covers the race
            // and the wait prevents a zombie.
            worker.handle.shutdown().await;
            self.respawn_warm().await;
            return;
        }
        let overflow = {
            let mut state = self.state.lock().unwrap();
            if state.idle.len() < WARM_IDLE_WORKERS {
                state.idle.push(worker);
                None
            } else {
                Some(worker)
            }
        };
        if let Some(mut worker) = overflow {
            worker.handle.shutdown().await;
        }
    }

    async fn discard(&'static self, mut worker: Worker) {
        worker.handle.shutdown().await;
        self.respawn_warm().await;
    }

    /// Leaves one warm worker behind, as long as capacity allows.
    async fn respawn_warm(&'static self) {
        if !self.state.lock().unwrap().idle.is_empty() {
            return;
        }
        let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
            return;
        };
        if let Ok(worker) = spawn_worker(Some(permit)).await {
            // Scope the guard: it must not be live across the await below.
            let spare = {
                let mut state = self.state.lock().unwrap();
                if state.idle.is_empty() {
                    state.idle.push(worker);
                    None
                } else {
                    Some(worker)
                }
            };
            if let Some(mut worker) = spare {
                // Lost the refill race: dispose of the spare so its pid
                // deregisters instead of leaking into LIVE_PIDS.
                worker.handle.shutdown().await;
            }
        }
    }
}

/// Spawns a fresh worker: the duplex transport under `cfg(test)` without a
/// command override, a real subprocess otherwise.
async fn spawn_worker(permit: Option<OwnedSemaphorePermit>) -> Result<Worker, Failure> {
    #[cfg(test)]
    if std::env::var_os(WORKER_CMD_ENV).is_none() {
        return Ok(Worker {
            handle: Box::new(duplex::spawn().await),
            runs: 0,
            _permit: None,
        });
    }
    process_spawn(resolved_command())
        .await
        .map(|handle| Worker {
            handle: Box::new(handle),
            runs: 0,
            _permit: permit,
        })
}

fn resolved_command() -> Result<OsString, Failure> {
    if let Some(command) = std::env::var_os(WORKER_CMD_ENV) {
        return Ok(command);
    }
    std::env::current_exe()
        .map(std::path::PathBuf::into_os_string)
        .map_err(|error| Failure::crash(format!("could not resolve the codemode worker: {error}")))
}

// --- stdio subprocess transport -------------------------------------------

/// A `craft __codemode-worker` subprocess speaking framed JSON over its
/// stdio pipes.
struct ProcessWorker {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    reader: AsyncFrameReader<tokio::process::ChildStdout>,
    pid: Option<u32>,
}

async fn process_spawn(command: Result<OsString, Failure>) -> Result<ProcessWorker, Failure> {
    let mut command = tokio::process::Command::new(command?);
    command
        .arg("__codemode-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| Failure::crash(format!("could not start codemode worker: {error}")))?;
    let pid = child.id();
    if let Some(pid) = pid {
        register_pid(pid);
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain stderr (the child would block on a full pipe) and surface its
    // lines — panic output — at debug level.
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "codemode", worker = ?pid, "{line}");
        }
    });
    let mut reader = AsyncFrameReader::new(stdout);
    let ready = match tokio::time::timeout(READY_TIMEOUT, reader.read_frame()).await {
        Err(_) => Err(Failure::crash("codemode worker did not start in time")),
        Ok(Err(error)) => Err(Failure::crash(format!(
            "could not start codemode worker: {error}"
        ))),
        Ok(Ok(Some(body))) => match proto::parse_frame::<FromWorker>(&body) {
            Ok(FromWorker::Ready) => Ok(()),
            _ => Err(Failure::crash(
                "codemode worker did not send its ready frame",
            )),
        },
        Ok(Ok(None)) => Err(Failure::crash(
            "codemode worker did not send its ready frame",
        )),
    };
    if let Err(failure) = ready {
        // Reap before failing: the pid registered at spawn must not leak
        // into LIVE_PIDS (kill_on_drop alone leaves no trace of the exit).
        let _ = child.kill().await;
        let _ = child.wait().await;
        if let Some(pid) = pid {
            deregister_pid(pid);
        }
        return Err(failure);
    }
    Ok(ProcessWorker {
        child,
        stdin,
        reader,
        pid,
    })
}

#[async_trait]
impl WorkerHandle for ProcessWorker {
    async fn next_event(&mut self) -> io::Result<Message> {
        let body = self
            .reader
            .read_frame()
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker closed"))?;
        let message = proto::parse_frame::<FromWorker>(&body)?.into_message()?;
        Ok(message)
    }

    async fn send(&mut self, frame: &ToWorker) -> io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut buffer = Vec::new();
        proto::encode_into(&mut buffer, frame)?;
        self.stdin.write_all(&buffer).await?;
        self.stdin.flush().await
    }

    async fn crash_detail(&mut self) -> String {
        let _ = self.child.kill().await;
        match self.child.wait().await {
            Ok(status) => exit_detail(&status),
            Err(_) => "worker stopped unexpectedly".to_owned(),
        }
    }

    async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        if let Some(pid) = self.pid.take() {
            deregister_pid(pid);
        }
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }
}

fn exit_detail(status: &std::process::ExitStatus) -> String {
    #[cfg(unix)]
    if let Some(signal) = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    } {
        return format!("signal {signal}");
    }
    format!("exit status {status}")
}

// --- in-memory duplex transport (tests) ------------------------------------

#[cfg(test)]
mod duplex;

// --- debug/test hooks (the Phase 4 integration tests run outside this
// --- crate's cfg(test), so these stay compiled in) --------------------------

/// Live subprocess pids, for leak and crash-injection assertions.
static LIVE_PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Run frames successfully sent, so tests can wait for a run to be in
/// flight before injecting a crash.
static RUNS_SENT: AtomicU64 = AtomicU64::new(0);

fn register_pid(pid: u32) {
    LIVE_PIDS.lock().unwrap().push(pid);
}

fn deregister_pid(pid: u32) {
    LIVE_PIDS.lock().unwrap().retain(|live| *live != pid);
}

/// Test hook: SIGKILL every live worker subprocess, simulating an engine
/// crash at the OS level. Inert unless a worker-command override is
/// configured (the integration-test setup arms it); production never does.
#[doc(hidden)]
pub fn debug_kill_all() {
    if std::env::var_os(WORKER_CMD_ENV).is_none() {
        return;
    }
    #[cfg(unix)]
    for pid in LIVE_PIDS.lock().unwrap().iter().copied() {
        // ESRCH for an already-dead pid is fine.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
}

/// Test hook: total run frames sent since process start.
#[doc(hidden)]
pub fn debug_runs_sent() -> u64 {
    RUNS_SENT.load(Ordering::Relaxed)
}

/// Test hook: pids of the warm idle workers (warm-reuse assertions).
#[doc(hidden)]
pub fn debug_idle_pids() -> Vec<u32> {
    pool()
        .state
        .lock()
        .unwrap()
        .idle
        .iter()
        .filter_map(|worker| worker.handle.pid())
        .collect()
}
