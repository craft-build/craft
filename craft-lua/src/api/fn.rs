use std::collections::{HashMap, VecDeque};
use std::env;
use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_agent::SessionMailbox;
use craft_storage::id::CraftId;
use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::fs::expand_tilde;
use crate::api::util::command::{UiAction, ui_roundtrip, ui_send};
use crate::api::util::pair::{err_pair, try_pair};
use crate::plugin_permissions::{
    Permission::{Env, Run},
    PluginPermissions,
};
use crate::runtime::{SharedSandboxConfig, with_bg_jobs, with_task_jobs};
#[cfg(test)]
use crate::terminal_backend::LocalTerminal;
use crate::terminal_backend::{JobEvent, TerminalBackend, TerminalHandle, TerminalSpec};

const DEFAULT_TAIL: usize = 20;
const MAX_TAIL_LINES: usize = 1024;
const MAX_COMPLETED_SESSION_JOBS: usize = 256;
const DEFAULT_WAIT_MS: u64 = 30_000;
const JOB_NOT_FOUND_ERR: &str = "job: not found";
const BLANK_NAME_ERR: &str = "jobstart: name must be non-blank";
/// How often a parked `jobwait` re-checks whether its job's session ended:
/// session jobs can live inside the waiting task's own store, where the
/// SessionEnd reap cannot reach them.
const JOBWAIT_SESSION_POLL: Duration = Duration::from_millis(50);

/// Sessions the host ended. A `jobwait` parked on a session job inside the
/// task that started it polls this, because the SessionEnd reap only walks
/// the background map while that task is still alive.
#[derive(Default)]
pub(crate) struct EndedSessions(std::cell::RefCell<std::collections::HashSet<CraftId>>);

impl EndedSessions {
    pub(crate) fn mark(lua: &Lua, session: CraftId) {
        if let Some(store) = lua.app_data_ref::<EndedSessions>() {
            store.0.borrow_mut().insert(session);
        }
    }

    fn contains(lua: &Lua, session: CraftId) -> bool {
        lua.app_data_ref::<EndedSessions>()
            .is_some_and(|store| store.0.borrow().contains(&session))
    }
}

/// Kill the job when the host ended its session, so a parked `jobwait`
/// returns instead of waiting out its timeout.
fn kill_if_session_ended(lua: &Lua, job_id: u32, plugin: &str) {
    let ended = with_task_jobs(lua, |store| {
        store
            .session_of(job_id, plugin)
            .is_some_and(|session| EndedSessions::contains(lua, session))
    });
    if ended {
        with_task_jobs(lua, |store| store.kill(job_id, plugin));
    }
}

#[derive(Clone)]
struct JobNotify {
    session: CraftId,
    wake: bool,
    on_success: bool,
}

fn build_sandbox_profile(lua: &Lua, cwd: Option<&Path>) -> Option<craft_sandbox::SandboxProfile> {
    let shared = lua.app_data_ref::<SharedSandboxConfig>()?;
    let config = shared.load();
    if !config.enabled || matches!(config.mode, craft_config::SandboxMode::Off) {
        return None;
    }
    let workspace = cwd
        .and_then(|p| p.canonicalize().ok())
        .or_else(|| env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let mode = match config.mode {
        craft_config::SandboxMode::WorkspaceWrite => craft_sandbox::SandboxMode::WorkspaceWrite,
        craft_config::SandboxMode::ReadOnly => craft_sandbox::SandboxMode::ReadOnly,
        craft_config::SandboxMode::DangerFullAccess => craft_sandbox::SandboxMode::DangerFullAccess,
        craft_config::SandboxMode::Off => return None,
    };
    let network = if config.network {
        craft_sandbox::NetworkPolicy::Allowed
    } else {
        craft_sandbox::NetworkPolicy::Denied
    };
    Some(craft_sandbox::SandboxProfile {
        mode,
        network,
        workspace,
        writable_roots: Vec::new(),
    })
}

pub(crate) struct JobMeta {
    pub(crate) alive: bool,
    pub(crate) background: bool,
    plugin: Arc<str>,
    command: String,
    /// Per-plugin handle, so a reloaded plugin can find this job again
    /// without matching on the command string. Only live jobs hold it.
    name: Option<Arc<str>>,
    pid: u32,
    started: Instant,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    pub(crate) event_rx: Option<flume::Receiver<JobEvent>>,
    kill: Option<Box<dyn FnOnce() + Send>>,
    stdout_tail: VecDeque<String>,
    stderr_tail: VecDeque<String>,
    tail_cap: usize,
    session: Option<CraftId>,
    notify: Option<JobNotify>,
    exit_code: Option<i32>,
    /// Frozen at exit so elapsed time stops counting once the process is gone.
    elapsed_secs: Option<u64>,
    /// Exit code owed to a callback attached after the process already died,
    /// served by [`JobStore::next_event`] as a synthetic event.
    replay_exit: Option<i32>,
    /// Set by the first [`JobStore::complete`], so a replayed exit does not
    /// run the completion bookkeeping twice.
    completed: bool,
}

impl JobMeta {
    pub(crate) fn owned_by(&self, plugin: &str) -> bool {
        self.plugin.as_ref() == plugin
    }

    fn record_line(&mut self, stdout: bool, line: &str) {
        if self.tail_cap == 0 {
            return;
        }
        let tail = if stdout {
            &mut self.stdout_tail
        } else {
            &mut self.stderr_tail
        };
        if tail.len() >= self.tail_cap {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
    }

    fn has_pending(&self) -> bool {
        self.replay_exit.is_some() || self.event_rx.as_ref().is_some_and(|rx| !rx.is_empty())
    }
}

/// What a `jobattach` opts table says about one callback slot.
enum CallbackUpdate {
    Keep,
    Clear,
    Set(RegistryKey),
}

pub(crate) struct CallbackUpdates {
    on_stdout: CallbackUpdate,
    on_stderr: CallbackUpdate,
    on_exit: CallbackUpdate,
}

pub(crate) struct JobSnapshot {
    pub id: u32,
    pub command: String,
    pub name: Option<Arc<str>>,
    pub session: Option<CraftId>,
    pub pid: u32,
    pub elapsed_secs: u64,
    pub exit_code: Option<i32>,
    pub stdout_lines: Vec<String>,
    pub stderr_lines: Vec<String>,
}

impl JobSnapshot {
    fn from_job(id: u32, job: &JobMeta, tails: bool) -> Self {
        Self {
            id,
            command: job.command.clone(),
            name: job.name.clone(),
            session: job.session,
            pid: job.pid,
            elapsed_secs: job
                .elapsed_secs
                .unwrap_or_else(|| job.started.elapsed().as_secs()),
            exit_code: job.exit_code,
            stdout_lines: if tails {
                job.stdout_tail.iter().cloned().collect()
            } else {
                Vec::new()
            },
            stderr_lines: if tails {
                job.stderr_tail.iter().cloned().collect()
            } else {
                Vec::new()
            },
        }
    }
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
    backend: Arc<dyn TerminalBackend>,
    completed_order: VecDeque<u32>,
    /// Id served by the last [`JobStore::next_event`], so the next scan
    /// starts past it.
    scan_cursor: u32,
}

fn list_jobs(
    jobs: &HashMap<u32, JobMeta>,
    session: Option<CraftId>,
    plugin: &str,
) -> Vec<JobSnapshot> {
    jobs.iter()
        .filter(|(_, job)| job.owned_by(plugin))
        .filter(|(_, job)| session.is_none_or(|s| job.session == Some(s)))
        .filter(|(_, job)| job.session.is_some() || job.exit_code.is_none())
        .map(|(&id, job)| JobSnapshot::from_job(id, job, false))
        .collect()
}

impl JobStore {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_backend(Arc::new(LocalTerminal))
    }

    pub fn with_backend(backend: Arc<dyn TerminalBackend>) -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
            backend,
            completed_order: VecDeque::new(),
            scan_cursor: 0,
        }
    }

    pub fn next_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn backend(&self) -> Arc<dyn TerminalBackend> {
        Arc::clone(&self.backend)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &mut self,
        id: u32,
        handle: TerminalHandle,
        command: String,
        name: Option<Arc<str>>,
        plugin: Arc<str>,
        session: Option<CraftId>,
        on_stdout: Option<RegistryKey>,
        on_stderr: Option<RegistryKey>,
        on_exit: Option<RegistryKey>,
        background: bool,
    ) {
        self.jobs.insert(
            id,
            JobMeta {
                alive: true,
                background,
                plugin,
                session,
                command,
                name,
                pid: handle.pid,
                started: Instant::now(),
                on_stdout,
                on_stderr,
                on_exit,
                event_rx: Some(handle.events),
                kill: Some(handle.kill),
                stdout_tail: VecDeque::new(),
                stderr_tail: VecDeque::new(),
                tail_cap: DEFAULT_TAIL,
                notify: None,
                exit_code: None,
                elapsed_secs: None,
                replay_exit: None,
                completed: false,
            },
        );
    }

    pub fn has_alive_jobs(&self) -> bool {
        self.jobs.values().any(|j| j.alive)
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn callback_key(&self, job_id: u32, event: &JobEvent) -> Option<&RegistryKey> {
        let meta = self.jobs.get(&job_id)?;
        match event {
            JobEvent::Stdout(_) => meta.on_stdout.as_ref(),
            JobEvent::Stderr(_) => meta.on_stderr.as_ref(),
            JobEvent::Exit(_) => meta.on_exit.as_ref(),
        }
    }

    pub fn take_receiver(
        &mut self,
        job_id: u32,
        plugin: &str,
    ) -> Option<flume::Receiver<JobEvent>> {
        let meta = self.jobs.get_mut(&job_id)?;
        meta.owned_by(plugin).then(|| meta.event_rx.take())?
    }

    /// Pop one queued event. Deliberately one at a time: a batch pulled into
    /// a caller-owned buffer is lost if that caller is dropped mid-delivery,
    /// and with it the tail and exit code the event carries.
    ///
    /// Round-robins over the jobs holding events: taking the first match
    /// every time would let a job printing faster than we deliver starve the
    /// rest.
    pub fn next_event(&mut self) -> Option<(u32, JobEvent)> {
        let mut past_cursor = None;
        let mut lowest = None;
        for (&id, meta) in &self.jobs {
            if !meta.has_pending() {
                continue;
            }
            if id > self.scan_cursor {
                past_cursor = Some(past_cursor.map_or(id, |seen: u32| seen.min(id)));
            }
            lowest = Some(lowest.map_or(id, |seen: u32| seen.min(id)));
        }
        let id = past_cursor.or(lowest)?;
        self.scan_cursor = id;
        let job = self.jobs.get_mut(&id)?;
        if let Some(code) = job.replay_exit.take() {
            return Some((id, JobEvent::Exit(code)));
        }
        Some((id, job.event_rx.as_ref()?.try_recv().ok()?))
    }

    /// Removes a finished job and frees its callback registry values.
    #[cfg(test)]
    pub fn finish(&mut self, lua: &Lua, job_id: u32) {
        self.remove(lua, job_id);
    }

    fn configure(&mut self, id: u32, notify: Option<JobNotify>, tail: Option<usize>) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if let Some(cap) = tail {
            job.tail_cap = cap.min(MAX_TAIL_LINES);
        }
        job.notify = notify;
    }

    pub fn record_event(&mut self, job_id: u32, event: &JobEvent) {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return;
        };
        match event {
            JobEvent::Stdout(line) => job.record_line(true, line),
            JobEvent::Stderr(line) => job.record_line(false, line),
            JobEvent::Exit(code) => {
                job.exit_code = Some(*code);
                job.elapsed_secs
                    .get_or_insert_with(|| job.started.elapsed().as_secs());
            }
        }
    }

    /// Session-owned jobs stay inspectable after exit; every other job is
    /// removed the way it always was.
    pub fn complete(&mut self, lua: &Lua, job_id: u32, code: i32) {
        complete_job(lua, &mut self.jobs, &mut self.completed_order, job_id, code);
    }

    #[cfg(test)]
    pub fn kill_session(&mut self, lua: &Lua, session: CraftId) {
        kill_session_jobs(lua, &mut self.jobs, session);
    }

    /// Re-arm callbacks on an existing job, the way a reloaded plugin picks up
    /// a session job it started before. An `on_exit` attached to an already
    /// exited job is owed a replay, queued rather than fired inline so the
    /// attaching plugin finishes its own setup first.
    pub fn attach(
        &mut self,
        lua: &Lua,
        job_id: u32,
        plugin: &str,
        updates: CallbackUpdates,
    ) -> bool {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return false;
        };
        if !job.owned_by(plugin) {
            return false;
        }
        let attaching_exit = matches!(updates.on_exit, CallbackUpdate::Set(_));
        apply_update(lua, &mut job.on_stdout, updates.on_stdout);
        apply_update(lua, &mut job.on_stderr, updates.on_stderr);
        apply_update(lua, &mut job.on_exit, updates.on_exit);
        if attaching_exit && let Some(code) = job.exit_code {
            job.replay_exit = Some(code);
        }
        true
    }

    pub fn snapshot(&self, job_id: u32, plugin: &str) -> Option<JobSnapshot> {
        let job = self.jobs.get(&job_id)?;
        job.owned_by(plugin)
            .then(|| JobSnapshot::from_job(job_id, job, true))
    }

    /// List jobs this plugin can see. Task and plugin jobs leave the map on
    /// exit; session-owned jobs stay so exited ids stay findable. Tails live
    /// on `snapshot` / `jobinfo`.
    pub fn list(&self, session: Option<CraftId>, plugin: &str) -> Vec<JobSnapshot> {
        list_jobs(&self.jobs, session, plugin)
    }

    /// Drop an exited job this plugin can see. Running jobs are left alone.
    pub fn forget(&mut self, lua: &Lua, job_id: u32, plugin: &str) {
        let Some(job) = self.jobs.get(&job_id) else {
            return;
        };
        if !job.owned_by(plugin) || job.exit_code.is_none() {
            return;
        }
        remove_job_from(lua, &mut self.jobs, job_id);
        self.completed_order.retain(|&id| id != job_id);
    }

    pub fn session_of(&self, job_id: u32, plugin: &str) -> Option<CraftId> {
        let job = self.jobs.get(&job_id)?;
        job.owned_by(plugin).then_some(job.session)?
    }

    /// Id of the **live** job this plugin can see holding {name}. Exited jobs
    /// keep their name for display but stop answering here, so a plugin that
    /// adopts by name restarts a dead job instead of adopting a corpse.
    pub fn find_named(&self, name: &str, plugin: &str) -> Option<u32> {
        self.jobs
            .iter()
            .find(|(_, job)| {
                job.exit_code.is_none() && job.name.as_deref() == Some(name) && job.owned_by(plugin)
            })
            .map(|(&id, _)| id)
    }

    fn visible(&self, job_id: u32, plugin: &str) -> bool {
        self.jobs
            .get(&job_id)
            .is_some_and(|job| job.owned_by(plugin))
    }

    pub fn kill(&mut self, job_id: u32, plugin: &str) {
        if let Some(meta) = self.jobs.get_mut(&job_id)
            && meta.owned_by(plugin)
            && meta.alive
            && meta.exit_code.is_none()
            && let Some(kill) = meta.kill.take()
        {
            kill();
        }
    }

    #[cfg(test)]
    fn remove(&mut self, lua: &Lua, job_id: u32) {
        remove_job_from(lua, &mut self.jobs, job_id);
    }

    pub fn kill_all(&mut self) {
        for meta in self.jobs.values_mut() {
            if meta.alive
                && !meta.background
                && let Some(kill) = meta.kill.take()
            {
                kill();
            }
        }
    }

    pub fn clear(&mut self, lua: &Lua) {
        for (_, meta) in self.jobs.drain() {
            for key in [meta.on_stdout, meta.on_stderr, meta.on_exit]
                .into_iter()
                .flatten()
            {
                lua.remove_registry_value(key).ok();
            }
        }
    }

    pub fn drain_background(&mut self) -> HashMap<u32, JobMeta> {
        let bg_ids: Vec<u32> = self
            .jobs
            .iter()
            .filter(|(_, m)| m.background)
            .map(|(&id, _)| id)
            .collect();
        let mut drained = HashMap::with_capacity(bg_ids.len());
        for id in bg_ids {
            if let Some(meta) = self.jobs.remove(&id) {
                drained.insert(id, meta);
            }
        }
        drained
    }

    pub fn absorb(&mut self, jobs: HashMap<u32, JobMeta>) {
        for (id, meta) in jobs {
            self.jobs.insert(id, meta);
        }
    }

    pub fn put_receiver(&mut self, job_id: u32, rx: flume::Receiver<JobEvent>) {
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            meta.event_rx = Some(rx);
        }
    }
}

impl Drop for JobStore {
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// Kills and removes a single job from a raw job map, freeing its
/// callback registry values. Shared by [`JobStore::remove`] and the
/// plugin-unload cleanup over the global background map. The kill is
/// skipped for jobs that already exited: the wait thread reaped the
/// process, so its pid may have been recycled.
pub(crate) fn remove_job_from(lua: &Lua, jobs: &mut HashMap<u32, JobMeta>, job_id: u32) {
    if let Some(mut meta) = jobs.remove(&job_id) {
        if meta.exit_code.is_none()
            && let Some(kill) = meta.kill.take()
        {
            kill();
        }
        for key in [meta.on_stdout, meta.on_stderr, meta.on_exit]
            .into_iter()
            .flatten()
        {
            lua.remove_registry_value(key).ok();
        }
    }
}

/// Kills every background job owned by {plugin} in a raw job map, except
/// session-owned jobs, which survive the unload with their callbacks
/// dropped. Used on plugin unload against the persistent [`BgJobMap`] (see
/// `runtime`), where there is no live [`TaskScope`] to drain through.
pub(crate) fn kill_plugin_jobs(lua: &Lua, jobs: &mut HashMap<u32, JobMeta>, plugin: &str) {
    let ids: Vec<u32> = jobs
        .iter()
        .filter_map(|(&id, m)| {
            (m.background && m.owned_by(plugin) && m.session.is_none()).then_some(id)
        })
        .collect();
    for id in ids {
        remove_job_from(lua, jobs, id);
    }
    detach_session_callbacks(lua, jobs, plugin);
}

pub(crate) fn kill_session_jobs(lua: &Lua, jobs: &mut HashMap<u32, JobMeta>, session: CraftId) {
    let ids: Vec<u32> = jobs
        .iter()
        .filter(|(_, m)| m.session == Some(session))
        .map(|(&id, _)| id)
        .collect();
    for id in ids {
        remove_job_from(lua, jobs, id);
    }
}

fn detach_session_callbacks(lua: &Lua, jobs: &mut HashMap<u32, JobMeta>, plugin: &str) {
    for meta in jobs.values_mut() {
        if meta.session.is_some() && meta.owned_by(plugin) {
            drop_callbacks(lua, meta);
        }
    }
}

fn drop_callbacks(lua: &Lua, job: &mut JobMeta) {
    for key in [&mut job.on_stdout, &mut job.on_stderr, &mut job.on_exit]
        .into_iter()
        .filter_map(Option::take)
    {
        lua.remove_registry_value(key).ok();
    }
}

fn apply_update(lua: &Lua, slot: &mut Option<RegistryKey>, update: CallbackUpdate) {
    let replacement = match update {
        CallbackUpdate::Keep => return,
        CallbackUpdate::Clear => None,
        CallbackUpdate::Set(key) => Some(key),
    };
    if let Some(old) = mem::replace(slot, replacement) {
        lua.remove_registry_value(old).ok();
    }
}

/// Attach to a job wherever it lives: the current task store first, then the
/// background map an exited session job retired to when its task ended. A
/// job adopted from the background map moves into the task store so this
/// task's pump serves its replayed exit.
fn attach_job(lua: &Lua, job_id: u32, plugin: &str, updates: CallbackUpdates) -> bool {
    if with_task_jobs(lua, |store| store.visible(job_id, plugin)) {
        return with_task_jobs(lua, |store| store.attach(lua, job_id, plugin, updates));
    }
    crate::runtime::adopt_bg_job(lua, job_id, plugin)
        && with_task_jobs(lua, |store| store.attach(lua, job_id, plugin, updates))
}

/// Exit path shared by the task store and the background map. Posts the
/// mailbox notify for session jobs, then keeps them inspectable (capped)
/// while every other job is removed as before.
pub(crate) fn complete_job(
    lua: &Lua,
    jobs: &mut HashMap<u32, JobMeta>,
    completed_order: &mut VecDeque<u32>,
    job_id: u32,
    code: i32,
) {
    let Some(job) = jobs.get_mut(&job_id) else {
        return;
    };
    // A replayed exit reaches here a second time; only the callbacks the
    // replay just used still need releasing.
    if job.completed {
        drop_callbacks(lua, job);
        return;
    }
    job.completed = true;
    job.exit_code = Some(code);
    job.elapsed_secs
        .get_or_insert_with(|| job.started.elapsed().as_secs());
    if let Some(notify) = job.notify.clone()
        && (code != 0 || notify.on_success)
    {
        let message = format!("[job {job_id}] \"{}\" exited with code {code}", job.command);
        if let Err(e) = SessionMailbox::notify(notify.session, message, notify.wake) {
            tracing::warn!(error = %e, job_id, "session job notify failed");
        }
    }
    if job.session.is_some() {
        drop_callbacks(lua, job);
        completed_order.push_back(job_id);
        while completed_order.len() > MAX_COMPLETED_SESSION_JOBS {
            let oldest = completed_order.pop_front().unwrap();
            if jobs.get(&oldest).is_some_and(|j| j.exit_code.is_some()) {
                remove_job_from(lua, jobs, oldest);
            }
        }
        return;
    }
    remove_job_from(lua, jobs, job_id);
}

/// Drain pending events from session-owned jobs living in a raw job map
/// (the background map between tasks), so exit codes, tails, and mailbox
/// notifies settle without a task to pump them.
pub(crate) fn pump_session_jobs(lua: &Lua, jobs: &mut HashMap<u32, JobMeta>) {
    let ids: Vec<u32> = jobs
        .iter()
        .filter(|(_, m)| m.session.is_some())
        .map(|(&id, _)| id)
        .collect();
    for id in ids {
        let Some(meta) = jobs.get_mut(&id) else {
            continue;
        };
        let mut exited = None;
        while let Some(event) = meta.event_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match event {
                JobEvent::Stdout(line) => meta.record_line(true, &line),
                JobEvent::Stderr(line) => meta.record_line(false, &line),
                JobEvent::Exit(code) => exited = Some(code),
            }
        }
        if let Some(code) = exited {
            let mut completed = VecDeque::new();
            complete_job(lua, jobs, &mut completed, id, code);
        }
    }
    let mut exited: Vec<u32> = jobs
        .iter()
        .filter(|(_, m)| m.session.is_some() && m.exit_code.is_some())
        .map(|(&id, _)| id)
        .collect();
    exited.sort_unstable();
    if exited.len() > MAX_COMPLETED_SESSION_JOBS {
        let excess = exited.len() - MAX_COMPLETED_SESSION_JOBS;
        for id in exited.into_iter().take(excess) {
            remove_job_from(lua, jobs, id);
        }
    }
}

pub(crate) fn create_fn_table(
    lua: &Lua,
    plugin: Arc<str>,
    perms: &PluginPermissions,
    tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    let perms = perms.clone();

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobstart",
        lua.create_async_function(move |lua, (cmd, opts): (String, Option<Table>)| {
            let p = p.clone();
            let owner = owner.clone();
            async move {
                if !p.is_allowed(Run) {
                    return Err(crate::plugin_permissions::denied_error(Run));
                }
                let (
                    cwd,
                    env,
                    name,
                    on_stdout,
                    on_stderr,
                    on_exit,
                    want_sandbox,
                    background,
                    session,
                    notify,
                    tail,
                ) = match opts {
                    Some(ref opts) => {
                        let cwd: Option<String> = opts.get("cwd").ok();
                        let env: Option<HashMap<String, String>> = opts
                            .get::<Table>("env")
                            .ok()
                            .map(|t| t.pairs::<String, String>().filter_map(Result::ok).collect());
                        let on_stdout = opts
                            .get::<Function>("on_stdout")
                            .ok()
                            .map(|f| lua.create_registry_value(f))
                            .transpose()?;
                        let on_stderr = opts
                            .get::<Function>("on_stderr")
                            .ok()
                            .map(|f| lua.create_registry_value(f))
                            .transpose()?;
                        let on_exit = opts
                            .get::<Function>("on_exit")
                            .ok()
                            .map(|f| lua.create_registry_value(f))
                            .transpose()?;
                        let want_sandbox: bool = opts.get("sandbox").unwrap_or(false);
                        let name = job_name(opts)?;
                        let background: bool = opts.get("background").unwrap_or(false);
                        let session: Option<String> = opts.get("session").ok();
                        let session = session
                            .map(|raw| {
                                raw.parse::<CraftId>().map_err(
                                    |e: craft_storage::id::CraftIdParseError| {
                                        mlua::Error::runtime(e.to_string())
                                    },
                                )
                            })
                            .transpose()?;
                        let notify = parse_notify(opts, session)?;
                        let tail: Option<usize> = opts.get("tail").ok();
                        if let Some(n) = tail
                            && n > MAX_TAIL_LINES
                        {
                            return Err(mlua::Error::runtime(format!(
                                "jobstart: tail must be in 0..={MAX_TAIL_LINES}"
                            )));
                        }
                        (
                            cwd,
                            env,
                            name,
                            on_stdout,
                            on_stderr,
                            on_exit,
                            want_sandbox,
                            background || session.is_some(),
                            session,
                            notify,
                            tail,
                        )
                    }
                    None => (
                        None, None, None, None, None, None, false, false, None, None, None,
                    ),
                };

                if let Some(ref name) = name
                    && let Some(held) = with_task_jobs(&lua, |store| store.find_named(name, &owner))
                {
                    return Err(mlua::Error::runtime(format!(
                        "jobstart: name {name:?} is already held by live job {held}"
                    )));
                }

                let (backend, id) =
                    with_task_jobs(&lua, |store| (store.backend(), store.next_id()));
                let cwd = cwd.as_deref().map(expand_tilde);
                if let Some(ref dir) = cwd
                    && !dir.is_dir()
                {
                    return Err(mlua::Error::runtime(format!(
                        "cwd is not a directory: {}",
                        dir.display()
                    )));
                }
                let sandbox = if want_sandbox {
                    build_sandbox_profile(&lua, cwd.as_deref())
                } else {
                    None
                };
                let spec = TerminalSpec {
                    cmd: cmd.clone(),
                    cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
                    env,
                    sandbox,
                };
                let handle = backend.start(spec).await.map_err(mlua::Error::runtime)?;
                with_task_jobs(&lua, |store| {
                    store.register(
                        id, handle, cmd, name, owner, session, on_stdout, on_stderr, on_exit,
                        background,
                    );
                    store.configure(id, notify, tail);
                });
                Ok(id)
            }
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobstop",
        lua.create_function(move |lua, job_id: u32| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            with_task_jobs(lua, |store| store.kill(job_id, &owner));
            Ok(())
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobwait",
        lua.create_async_function(move |lua, (job_id, timeout_ms): (u32, Option<u64>)| {
            let p = p.clone();
            let owner = owner.clone();
            async move {
                if !p.is_allowed(Run) {
                    return Err(crate::plugin_permissions::denied_error(Run));
                }
                if let Some(snap) = with_task_jobs(&lua, |store| store.snapshot(job_id, &owner))
                    && let Some(code) = snap.exit_code
                {
                    let result = lua.create_table()?;
                    result.set("stdout", snap.stdout_lines.join("\n"))?;
                    result.set("stderr", snap.stderr_lines.join("\n"))?;
                    result.set("exit_code", code)?;
                    result.set("truncated", true)?;
                    return Ok(mlua::Value::Table(result));
                }
                let rx = with_task_jobs(&lua, |store| store.take_receiver(job_id, &owner))
                    .ok_or_else(|| mlua::Error::runtime("unknown job id or already waited"))?;

                let deadline = tokio::time::Instant::now()
                    + Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_WAIT_MS));

                let mut stdout_lines = Vec::new();
                let mut stderr_lines = Vec::new();

                let exit_code = loop {
                    let event = tokio::select! {
                        biased;
                        event = rx.recv_async() => event.ok(),
                        _ = tokio::time::sleep_until(deadline) => None,
                        _ = tokio::time::sleep(JOBWAIT_SESSION_POLL) => {
                            kill_if_session_ended(&lua, job_id, &owner);
                            continue;
                        }
                    };

                    let Some(event) = event else {
                        with_task_jobs(&lua, |store| store.put_receiver(job_id, rx));
                        return Ok(mlua::Value::Nil);
                    };
                    // A failing callback must not abort the wait: the event is
                    // already recorded and the exit still needs collecting.
                    if let Err(e) = deliver_job_event(&lua, job_id, &event).await {
                        tracing::warn!(
                            job_id,
                            error = %crate::runtime::strip_traceback(&e),
                            "jobwait callback failed"
                        );
                    }
                    match event {
                        JobEvent::Stdout(line) => stdout_lines.push(line),
                        JobEvent::Stderr(line) => stderr_lines.push(line),
                        JobEvent::Exit(code) => break code,
                    }
                };

                with_task_jobs(&lua, |store| store.put_receiver(job_id, rx));

                let result = lua.create_table()?;
                result.set("stdout", stdout_lines.join("\n"))?;
                result.set("stderr", stderr_lines.join("\n"))?;
                result.set("exit_code", exit_code)?;
                result.set("truncated", false)?;
                Ok(mlua::Value::Table(result))
            }
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobinfo",
        lua.create_function(move |lua, job_id: u32| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            let snap = with_task_jobs(lua, |store| store.snapshot(job_id, &owner)).or_else(|| {
                with_bg_jobs(lua, |jobs| {
                    jobs.get(&job_id)
                        .filter(|job| job.owned_by(&owner))
                        .map(|job| JobSnapshot::from_job(job_id, job, true))
                })
                .flatten()
            });
            match snap {
                Some(snap) => Ok((Some(snapshot_table(lua, &snap, true)?), None)),
                None => Ok(err_pair(JOB_NOT_FOUND_ERR)),
            }
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobattach",
        lua.create_function(move |lua, (job_id, opts): (u32, Table)| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            let updates = CallbackUpdates {
                on_stdout: callback_update(lua, &opts, "on_stdout")?,
                on_stderr: callback_update(lua, &opts, "on_stderr")?,
                on_exit: callback_update(lua, &opts, "on_exit")?,
            };
            if attach_job(lua, job_id, &owner, updates) {
                Ok((Some(true), None))
            } else {
                Ok(err_pair(JOB_NOT_FOUND_ERR))
            }
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "joblist",
        lua.create_function(move |lua, session: Option<String>| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            let filter = match session.map(|raw| raw.parse::<CraftId>()) {
                Some(Ok(id)) => Some(id),
                Some(Err(e)) => return Ok(err_pair(e.to_string())),
                None => None,
            };
            let mut snaps = with_task_jobs(lua, |store| store.list(filter, &owner));
            if let Some(bg) = with_bg_jobs(lua, |jobs| list_jobs(jobs, filter, &owner)) {
                snaps.extend(bg);
            }
            snaps.sort_unstable_by_key(|snap| snap.id);
            let result = lua.create_table()?;
            for (i, snap) in snaps.iter().enumerate() {
                result.set(i + 1, snapshot_table(lua, snap, false)?)?;
            }
            Ok((Some(Value::Table(result)), None::<String>))
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobforget",
        lua.create_function(move |lua, job_id: u32| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            with_task_jobs(lua, |store| store.forget(lua, job_id, &owner));
            Ok(())
        })?,
    )?;

    let p = perms.clone();
    let owner = plugin.clone();
    t.set(
        "jobfind",
        lua.create_function(move |lua, name: String| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            match with_task_jobs(lua, |store| store.find_named(&name, &owner)) {
                Some(id) => Ok((Some(id), None)),
                None => Ok(err_pair(JOB_NOT_FOUND_ERR)),
            }
        })?,
    )?;

    let p = perms;
    t.set(
        "executable",
        lua.create_function(move |_, name: String| {
            if !p.is_allowed(Env) {
                return Err(crate::plugin_permissions::denied_error(Env));
            }
            let found = env::var_os("PATH")
                .map(|paths| env::split_paths(&paths).any(|dir| dir.join(&name).is_file()))
                .unwrap_or(false)
                || Path::new(&name).is_file();
            Ok(if found { 1 } else { 0 })
        })?,
    )?;

    let win_tx = tx.clone();
    t.set(
        "winsaveview",
        lua.create_async_function(move |lua, ()| {
            let win_tx = win_tx.clone();
            async move {
                let view = try_pair!(
                    ui_roundtrip(win_tx.as_ref(), |reply_tx| UiAction::WinSaveView {
                        reply_tx,
                    })
                    .await
                );
                let table = lua.create_table()?;
                table.set("topline", i64::from(view.scroll_top) + 1)?;
                table.set("line_count", view.line_count)?;
                table.set("height", view.height)?;
                table.set("auto_scroll", view.auto_scroll)?;
                Ok((Some(table), None))
            }
        })?,
    )?;

    let rest_tx = tx;
    t.set(
        "winrestview",
        lua.create_async_function(move |_, view: Table| {
            let rest_tx = rest_tx.clone();
            async move {
                let topline = view.get::<Option<i64>>("topline")?.unwrap_or(1);
                let scroll_top = topline.saturating_sub(1).clamp(0, i64::MAX) as u32;
                try_pair!(ui_send(
                    rest_tx.as_ref(),
                    UiAction::WinRestView { scroll_top }
                ));
                Ok((Some(true), None))
            }
        })?,
    )?;

    Ok(t)
}

fn parse_notify(opts: &Table, session: Option<CraftId>) -> LuaResult<Option<JobNotify>> {
    let Some(session) = session else {
        if opts.contains_key("notify")? {
            return Err(mlua::Error::runtime("jobstart: notify requires a session"));
        }
        return Ok(None);
    };
    match opts.get::<Value>("notify")? {
        Value::Nil => Ok(None),
        Value::Boolean(false) => Ok(None),
        Value::Boolean(true) => Ok(Some(JobNotify {
            session,
            wake: true,
            on_success: true,
        })),
        Value::Table(t) => Ok(Some(JobNotify {
            session,
            wake: t.get("wake").unwrap_or(true),
            on_success: t.get("on_success").unwrap_or(true),
        })),
        _ => Err(mlua::Error::runtime(
            "jobstart: notify must be a boolean or table",
        )),
    }
}

fn job_name(opts: &Table) -> LuaResult<Option<Arc<str>>> {
    let Some(name) = opts.get::<Option<String>>("name")? else {
        return Ok(None);
    };
    if name.trim().is_empty() {
        return Err(mlua::Error::runtime(BLANK_NAME_ERR));
    }
    Ok(Some(Arc::from(name)))
}

fn callback_update(lua: &Lua, opts: &Table, key: &str) -> LuaResult<CallbackUpdate> {
    match opts.get::<Value>(key)? {
        Value::Nil => Ok(CallbackUpdate::Keep),
        Value::Boolean(false) => Ok(CallbackUpdate::Clear),
        Value::Function(callback) => Ok(CallbackUpdate::Set(lua.create_registry_value(callback)?)),
        _ => Err(mlua::Error::runtime(format!(
            "jobattach: {key} must be a function or false"
        ))),
    }
}

fn snapshot_table(lua: &Lua, snap: &JobSnapshot, tails: bool) -> LuaResult<Table> {
    let row = lua.create_table()?;
    row.set("id", snap.id)?;
    row.set("command", snap.command.as_str())?;
    row.set("name", snap.name.as_deref())?;
    row.set("pid", snap.pid)?;
    row.set("session", snap.session.map(|s| s.to_string()))?;
    row.set("elapsed_secs", snap.elapsed_secs)?;
    row.set(
        "status",
        if snap.exit_code.is_some() {
            "exited"
        } else {
            "running"
        },
    )?;
    row.set("exit_code", snap.exit_code)?;
    if tails {
        let stdout = lua.create_table()?;
        for (i, line) in snap.stdout_lines.iter().enumerate() {
            stdout.set(i + 1, line.as_str())?;
        }
        row.set("stdout_lines", stdout)?;
        let stderr = lua.create_table()?;
        for (i, line) in snap.stderr_lines.iter().enumerate() {
            stderr.set(i + 1, line.as_str())?;
        }
        row.set("stderr_lines", stderr)?;
    }
    Ok(row)
}

/// Fire the job's Lua callback for {event} (if any), finishing the job
/// on exit before invoking a callback that may raise. Shared by
/// `jobwait` and the async dispatch loop so both deliver events
/// identically.
///
/// The callback runs in a coroutine so it may suspend (the `craft.fs.*`
/// helpers park on blocking IO); resumed inline from a poll loop it would
/// die with "attempt to yield across metamethod / C-call boundary" on its
/// first suspension.
pub(crate) async fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let callback = with_task_jobs(lua, |store| {
        store.record_event(job_id, event);
        store
            .callback_key(job_id, event)
            .and_then(|key| lua.registry_value::<Function>(key).ok())
    });
    if let JobEvent::Exit(code) = event {
        with_task_jobs(lua, |store| store.complete(lua, job_id, *code));
    }
    if let Some(callback) = callback {
        let arg: Value = match event {
            JobEvent::Stdout(line) | JobEvent::Stderr(line) => {
                Value::String(lua.create_string(line)?)
            }
            JobEvent::Exit(code) => Value::Integer(*code as i64),
        };
        callback.call_async::<()>((job_id, arg)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::{NO_UI_ERR, WinView};
    use crate::terminal_backend::TerminalSpec;
    use test_case::test_case;

    fn make_store() -> JobStore {
        JobStore::new()
    }

    const TEST_PLUGIN: &str = "test-plugin";

    fn owner() -> Arc<str> {
        Arc::from(TEST_PLUGIN)
    }

    async fn start_echo(store: &mut JobStore) -> u32 {
        let backend = store.backend();
        let id = store.next_id();
        let handle = backend
            .start(TerminalSpec {
                cmd: "echo hello".into(),
                cwd: None,
                env: None,
                sandbox: None,
            })
            .await
            .unwrap();
        store.register(
            id,
            handle,
            "echo hello".into(),
            None,
            owner(),
            None,
            None,
            None,
            None,
            false,
        );
        id
    }

    #[tokio::test]
    async fn start_invalid_cwd_returns_error() {
        let backend = LocalTerminal;
        let result = backend
            .start(TerminalSpec {
                cmd: "echo hello".into(),
                cwd: Some("/nonexistent_dir_abc_xyz_123".into()),
                env: None,
                sandbox: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn finishing_a_job_removes_it() {
        let lua = Lua::new();
        let mut store = make_store();
        assert!(!store.has_alive_jobs());

        let id = start_echo(&mut store).await;
        assert!(store.has_alive_jobs());

        store.finish(&lua, id);
        assert!(!store.has_alive_jobs());
    }

    #[tokio::test]
    async fn noop_on_nonexistent_or_dead_jobs() {
        let mut store = make_store();
        store.kill(999, TEST_PLUGIN);

        let id = start_echo(&mut store).await;
        store.kill(id, TEST_PLUGIN);

        assert!(store.callback_key(999, &JobEvent::Exit(0)).is_none());
    }

    #[tokio::test]
    async fn take_receiver_lifecycle() {
        let mut store = make_store();
        assert!(store.take_receiver(999, TEST_PLUGIN).is_none());

        let id = start_echo(&mut store).await;
        assert!(
            store.take_receiver(id, "other-plugin").is_none(),
            "another plugin must not access the job"
        );
        assert!(store.take_receiver(id, TEST_PLUGIN).is_some());
        assert!(
            store.take_receiver(id, TEST_PLUGIN).is_none(),
            "second take should fail (receiver already moved)"
        );
    }

    #[tokio::test]
    async fn plugin_owner_can_be_accessed_only_by_its_plugin() {
        let mut store = make_store();
        let backend = store.backend();
        let id = store.next_id();
        let handle = backend
            .start(TerminalSpec {
                cmd: "echo hello".into(),
                cwd: None,
                env: None,
                sandbox: None,
            })
            .await
            .unwrap();
        store.register(
            id,
            handle,
            "echo hello".into(),
            None,
            owner(),
            None,
            None,
            None,
            None,
            false,
        );

        assert!(store.take_receiver(id, "other-plugin").is_none());
        assert!(store.take_receiver(id, TEST_PLUGIN).is_some());
    }

    #[tokio::test]
    async fn callback_key_returns_none_without_callbacks() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        assert!(
            store
                .callback_key(id, &JobEvent::Stdout("x".into()))
                .is_none()
        );
        assert!(
            store
                .callback_key(id, &JobEvent::Stderr("x".into()))
                .is_none()
        );
        assert!(store.callback_key(id, &JobEvent::Exit(0)).is_none());
    }

    #[tokio::test]
    async fn take_receiver_delivers_events() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        let rx = store.take_receiver(id, TEST_PLUGIN).unwrap();

        let mut got_exit = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(JobEvent::Exit(_)) => {
                    got_exit = true;
                    break;
                }
                Ok(_) => continue,
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(got_exit, "should receive exit event for completed job");
    }

    #[tokio::test]
    async fn next_event_collects_from_all_jobs() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;

        let events = collect_until_exit(id, || store.next_event());
        assert!(
            events.iter().any(|(jid, _)| *jid == id),
            "should receive events for the started job"
        );
    }

    #[tokio::test]
    async fn snapshot_reports_live_tails_and_hides_inaccessible_jobs() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        store.record_event(id, &JobEvent::Stdout("hello".into()));
        store.record_event(id, &JobEvent::Stderr("warn".into()));

        let snap = store.snapshot(id, TEST_PLUGIN).unwrap();
        assert_eq!(snap.command, "echo hello");
        assert_eq!(snap.stdout_lines, ["hello"]);
        assert_eq!(snap.stderr_lines, ["warn"]);
        assert!(snap.exit_code.is_none());
        assert!(store.snapshot(id, "other-plugin").is_none());
        assert!(store.snapshot(999, TEST_PLUGIN).is_none());
    }

    #[tokio::test]
    async fn tail_cap_drops_oldest_lines() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        store.configure(id, None, Some(2));
        store.record_event(id, &JobEvent::Stdout("a".into()));
        store.record_event(id, &JobEvent::Stdout("b".into()));
        store.record_event(id, &JobEvent::Stdout("c".into()));
        let snap = store.snapshot(id, TEST_PLUGIN).unwrap();
        assert_eq!(snap.stdout_lines, ["b", "c"]);
    }

    #[tokio::test]
    async fn list_is_live_and_plugin_scoped() {
        let lua = Lua::new();
        let mut store = make_store();
        let id = start_echo(&mut store).await;

        let listed: Vec<u32> = store.list(None, TEST_PLUGIN).iter().map(|s| s.id).collect();
        assert_eq!(listed, [id]);
        assert!(store.list(None, "other-plugin").is_empty());

        store.record_event(id, &JobEvent::Exit(0));
        store.finish(&lua, id);
        assert!(store.list(None, TEST_PLUGIN).is_empty());
        assert!(store.snapshot(id, TEST_PLUGIN).is_none());
    }

    async fn start_session_job(store: &mut JobStore, cmd: &str, session: CraftId) -> u32 {
        let backend = store.backend();
        let id = store.next_id();
        let handle = backend
            .start(TerminalSpec {
                cmd: cmd.into(),
                cwd: None,
                env: None,
                sandbox: None,
            })
            .await
            .unwrap();
        store.register(
            id,
            handle,
            cmd.into(),
            None,
            owner(),
            Some(session),
            None,
            None,
            None,
            true,
        );
        id
    }

    const JOB_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
    const JOB_POLL_INTERVAL: Duration = Duration::from_millis(10);
    const NEVER_EXITED: &str = "job never reported its exit";

    /// Pull events until {id} reports its exit, so assertions run against a
    /// job that is certainly done.
    fn collect_until_exit(
        id: u32,
        mut next: impl FnMut() -> Option<(u32, JobEvent)>,
    ) -> Vec<(u32, JobEvent)> {
        let deadline = Instant::now() + JOB_EXIT_TIMEOUT;
        let mut events = Vec::new();
        loop {
            while let Some(event) = next() {
                events.push(event);
            }
            if events
                .iter()
                .any(|(job_id, event)| *job_id == id && matches!(event, JobEvent::Exit(_)))
            {
                return events;
            }
            assert!(Instant::now() < deadline, "{NEVER_EXITED}");
            std::thread::sleep(JOB_POLL_INTERVAL);
        }
    }

    async fn wait_for_exit(store: &mut JobStore, id: u32) {
        for (_, event) in collect_until_exit(id, || store.next_event()) {
            store.record_event(id, &event);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_job_survives_plugin_detach_and_answers_after_exit() {
        let lua = Lua::new();
        let session = CraftId::generate();
        let mailbox = SessionMailbox::register(session);
        let mut store = make_store();
        let id = start_session_job(&mut store, "echo hi; echo err >&2; exit 3", session).await;
        store.configure(
            id,
            Some(JobNotify {
                session,
                wake: false,
                on_success: true,
            }),
            Some(5),
        );

        detach_session_callbacks(&lua, &mut store.jobs, TEST_PLUGIN);
        assert!(store.jobs.contains_key(&id), "detach must keep the job");

        wait_for_exit(&mut store, id).await;
        store.complete(&lua, id, 3);

        let snap = store.snapshot(id, TEST_PLUGIN).expect("peek after exit");
        assert_eq!(snap.exit_code, Some(3));
        assert!(
            snap.stdout_lines.iter().any(|l| l.contains("hi")),
            "stdout tail: {:?}",
            snap.stdout_lines
        );
        assert!(
            snap.stderr_lines.iter().any(|l| l.contains("err")),
            "stderr tail: {:?}",
            snap.stderr_lines
        );
        let notes = mailbox.drain();
        assert!(
            notes.iter().any(|m| m
                .user_text()
                .is_some_and(|t| t.contains("exited with code 3"))),
            "expected exit notify"
        );

        store.kill_session(&lua, session);
        assert!(store.snapshot(id, TEST_PLUGIN).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_session_does_not_touch_other_sessions() {
        let lua = Lua::new();
        let a = CraftId::generate();
        let b = CraftId::generate();
        let mut store = make_store();
        let first = start_session_job(&mut store, "sleep 30", a).await;
        let second = start_session_job(&mut store, "sleep 30", b).await;
        store.kill_session(&lua, a);
        assert!(store.snapshot(first, TEST_PLUGIN).is_none());
        assert!(store.snapshot(second, TEST_PLUGIN).is_some());
        store.kill_session(&lua, b);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exited_session_job_freezes_elapsed_at_exit() {
        let lua = Lua::new();
        let session = CraftId::generate();
        let mut store = make_store();
        let id = start_session_job(&mut store, "sleep 0.1", session).await;
        wait_for_exit(&mut store, id).await;
        store.complete(&lua, id, 0);
        let at_exit = store.snapshot(id, TEST_PLUGIN).unwrap().elapsed_secs;
        std::thread::sleep(Duration::from_millis(200));
        let later = store.snapshot(id, TEST_PLUGIN).unwrap().elapsed_secs;
        assert_eq!(at_exit, later, "elapsed must freeze once the job exits");
        store.kill_session(&lua, session);
    }

    #[tokio::test]
    async fn list_omits_tails() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        store.record_event(id, &JobEvent::Stdout("hello".into()));
        let listed = store.list(None, TEST_PLUGIN);
        assert!(
            listed
                .iter()
                .any(|s| s.id == id && s.stdout_lines.is_empty() && s.stderr_lines.is_empty()),
            "joblist must not clone tails"
        );
        assert_eq!(
            store.snapshot(id, TEST_PLUGIN).unwrap().stdout_lines,
            ["hello"]
        );
    }

    fn stub_job() -> JobMeta {
        JobMeta {
            alive: false,
            background: false,
            plugin: owner(),
            command: "stub".into(),
            name: None,
            pid: 0,
            started: Instant::now(),
            on_stdout: None,
            on_stderr: None,
            on_exit: None,
            event_rx: None,
            kill: None,
            stdout_tail: VecDeque::new(),
            stderr_tail: VecDeque::new(),
            tail_cap: DEFAULT_TAIL,
            session: None,
            notify: None,
            exit_code: None,
            elapsed_secs: None,
            replay_exit: None,
            completed: false,
        }
    }

    fn exit_updates(key: RegistryKey) -> CallbackUpdates {
        CallbackUpdates {
            on_stdout: CallbackUpdate::Keep,
            on_stderr: CallbackUpdate::Keep,
            on_exit: CallbackUpdate::Set(key),
        }
    }

    fn noop_key(lua: &Lua) -> RegistryKey {
        let noop = lua.create_function(|_, ()| Ok(())).unwrap();
        lua.create_registry_value(noop).unwrap()
    }

    fn exited_session_job(lua: &Lua, store: &mut JobStore, session: CraftId, code: i32) {
        let mut job = stub_job();
        job.session = Some(session);
        store.jobs.insert(1, job);
        store.record_event(1, &JobEvent::Exit(code));
        store.complete(lua, 1, code);
    }

    #[tokio::test]
    async fn attaching_on_exit_after_the_exit_replays_it_once() {
        const CODE: i32 = 5;
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, CraftId::generate(), CODE);
        let elapsed = store.jobs[&1].elapsed_secs;

        assert!(store.attach(&lua, 1, TEST_PLUGIN, exit_updates(noop_key(&lua))));

        let (id, event) = store.next_event().expect("replayed exit");
        assert_eq!(id, 1);
        assert!(matches!(event, JobEvent::Exit(CODE)));
        assert!(
            store.next_event().is_none(),
            "the replay must be served once"
        );

        store.record_event(1, &event);
        store.complete(&lua, 1, CODE);
        assert_eq!(
            store.completed_order.len(),
            1,
            "a replayed exit must not re-push the job onto the completed list"
        );
        assert_eq!(store.jobs[&1].elapsed_secs, elapsed);
        assert!(
            store.callback_key(1, &JobEvent::Exit(CODE)).is_none(),
            "completing again must still release the replayed callback"
        );
    }

    #[tokio::test]
    async fn attach_is_refused_for_jobs_this_plugin_cannot_see() {
        let lua = Lua::new();
        let mut store = make_store();
        exited_session_job(&lua, &mut store, CraftId::generate(), 0);

        assert!(!store.attach(&lua, 1, "other-plugin", exit_updates(noop_key(&lua))));
        assert!(!store.attach(&lua, 999, TEST_PLUGIN, exit_updates(noop_key(&lua))));
        assert!(
            store.next_event().is_none(),
            "a refused attach must not queue a replay"
        );
    }

    #[tokio::test]
    async fn attach_keeps_absent_callbacks_and_clears_on_false() {
        let lua = Lua::new();
        let mut store = make_store();
        let mut job = stub_job();
        job.on_stdout = Some(noop_key(&lua));
        store.jobs.insert(1, job);

        assert!(store.attach(
            &lua,
            1,
            TEST_PLUGIN,
            CallbackUpdates {
                on_stdout: CallbackUpdate::Clear,
                on_stderr: CallbackUpdate::Set(noop_key(&lua)),
                on_exit: CallbackUpdate::Keep,
            },
        ));
        assert!(
            store
                .callback_key(1, &JobEvent::Stdout(String::new()))
                .is_none(),
            "false must clear the current callback"
        );
        assert!(
            store
                .callback_key(1, &JobEvent::Stderr(String::new()))
                .is_some(),
            "a function must replace the current callback"
        );
        assert!(
            store.callback_key(1, &JobEvent::Exit(0)).is_none(),
            "an absent key must leave the (missing) callback alone"
        );
    }

    #[tokio::test]
    async fn a_name_is_held_by_the_live_job_only() {
        const NAME: &str = "log-tail";
        let lua = Lua::new();
        let mut store = make_store();
        let mut job = stub_job();
        job.session = Some(CraftId::generate());
        job.name = Some(Arc::from(NAME));
        store.jobs.insert(1, job);

        assert_eq!(store.find_named(NAME, TEST_PLUGIN), Some(1));
        assert_eq!(store.find_named(NAME, "other-plugin"), None);
        assert_eq!(store.find_named("absent", TEST_PLUGIN), None);

        store.record_event(1, &JobEvent::Exit(0));
        store.complete(&lua, 1, 0);

        assert_eq!(
            store.find_named(NAME, TEST_PLUGIN),
            None,
            "an exited job must release its name so the next start is not blocked"
        );
        assert_eq!(
            store.snapshot(1, TEST_PLUGIN).unwrap().name.as_deref(),
            Some(NAME),
            "the name stays on the row that explains the exit"
        );
    }

    #[tokio::test]
    async fn next_event_round_robins_so_a_chatty_job_cannot_starve_its_sibling() {
        const QUEUED_PER_JOB: usize = 2;
        let mut store = make_store();
        for id in [1, 2] {
            let (tx, rx) = flume::unbounded();
            for _ in 0..QUEUED_PER_JOB {
                tx.send(JobEvent::Stdout("spam".into())).unwrap();
            }
            let mut job = stub_job();
            job.event_rx = Some(rx);
            store.jobs.insert(id, job);
        }

        let served: Vec<u32> = std::iter::from_fn(|| store.next_event())
            .map(|(id, _)| id)
            .collect();

        assert_eq!(served, [1, 2, 1, 2]);
    }

    #[tokio::test]
    async fn forget_drops_exited_session_jobs_only() {
        let lua = Lua::new();
        let session = CraftId::generate();
        let mut store = make_store();
        let mut job = stub_job();
        job.session = Some(session);
        store.jobs.insert(1, job);

        store.forget(&lua, 1, TEST_PLUGIN);
        assert!(
            store.snapshot(1, TEST_PLUGIN).is_some(),
            "running job must stay"
        );

        store.record_event(1, &JobEvent::Exit(0));
        store.complete(&lua, 1, 0);
        assert!(
            store
                .list(Some(session), TEST_PLUGIN)
                .iter()
                .any(|s| s.id == 1)
        );

        store.forget(&lua, 1, "other-plugin");
        assert!(
            store.snapshot(1, TEST_PLUGIN).is_some(),
            "other plugin cannot forget"
        );

        store.forget(&lua, 1, TEST_PLUGIN);
        assert!(store.snapshot(1, TEST_PLUGIN).is_none());
        assert!(store.list(Some(session), TEST_PLUGIN).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exited_session_job_stays_listed_and_kill_is_a_noop() {
        let lua = Lua::new();
        let session = CraftId::generate();
        let mut store = make_store();
        let id = start_session_job(&mut store, "sleep 0.1", session).await;
        wait_for_exit(&mut store, id).await;
        store.complete(&lua, id, 0);
        store.kill(id, TEST_PLUGIN);
        let snap = store
            .snapshot(id, TEST_PLUGIN)
            .expect("exited session job must stay inspectable");
        assert_eq!(snap.exit_code, Some(0));
        assert!(
            store
                .list(Some(session), TEST_PLUGIN)
                .iter()
                .any(|listed| listed.id == id && listed.exit_code == Some(0)),
            "exited session job must stay listed"
        );
        store.kill_session(&lua, session);
    }

    #[tokio::test]
    async fn next_event_is_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        let _rx = store.take_receiver(id, TEST_PLUGIN).unwrap();

        assert!(
            store.next_event().is_none(),
            "a checked out receiver yields no events to the pump"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_store_kills_its_jobs() {
        let mut store = make_store();
        let id = store.next_id();
        let handle = store
            .backend()
            .start(TerminalSpec {
                cmd: "sleep 30".into(),
                cwd: None,
                env: None,
                sandbox: None,
            })
            .await
            .expect("job started");
        store.register(
            id,
            handle,
            "echo hello".into(),
            None,
            owner(),
            None,
            None,
            None,
            None,
            false,
        );
        assert!(
            store.has_alive_jobs(),
            "job should be alive before the drop"
        );
        let rx = store
            .take_receiver(id, TEST_PLUGIN)
            .expect("receiver present");

        drop(store);

        let mut got_kill = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(JobEvent::Exit(_)) => {
                    got_kill = true;
                    break;
                }
                Ok(_) => continue,
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(
            got_kill,
            "dropping the store must kill the job, not orphan it"
        );
    }

    fn lua_with_view(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_fn_table(&lua, owner(), &PluginPermissions::trusted(), tx).unwrap();
        lua.globals().set("f", t).unwrap();
        lua
    }

    #[tokio::test]
    async fn winsaveview_without_ui_returns_error_pair() {
        let lua = lua_with_view(None);
        let (val, err): (Value, Option<String>) = lua
            .load("return f.winsaveview()")
            .eval_async()
            .await
            .unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[tokio::test]
    async fn winrestview_without_ui_returns_error_pair() {
        let lua = lua_with_view(None);
        let (val, err): (Value, Option<String>) = lua
            .load("return f.winrestview({ topline = 3 })")
            .eval_async()
            .await
            .unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[tokio::test]
    async fn winsaveview_reports_the_viewport_one_based() {
        const SCROLL_TOP: u32 = 6;
        const LINE_COUNT: u32 = 100;
        const HEIGHT: u16 = 24;

        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        tokio::spawn(async move {
            let UiAction::WinSaveView { reply_tx } = rx.recv_async().await.unwrap() else {
                panic!("expected winsaveview request");
            };
            reply_tx
                .send(WinView {
                    scroll_top: SCROLL_TOP,
                    line_count: LINE_COUNT,
                    height: HEIGHT,
                    auto_scroll: false,
                })
                .unwrap();
        });
        let (view, err): (Table, Option<String>) = lua
            .load("return f.winsaveview()")
            .eval_async()
            .await
            .unwrap();
        assert_eq!(err, None);
        assert_eq!(view.get::<u32>("topline").unwrap(), SCROLL_TOP + 1);
        assert_eq!(view.get::<u32>("line_count").unwrap(), LINE_COUNT);
        assert_eq!(view.get::<u16>("height").unwrap(), HEIGHT);
        assert!(!view.get::<bool>("auto_scroll").unwrap());
    }

    #[test_case("{ topline = 12 }", 11 ; "explicit_topline")]
    #[test_case("{}", 0 ; "missing_topline_defaults_to_first_line")]
    #[test_case("{ topline = -5 }", 0 ; "below_range_clamps_to_first_line")]
    #[tokio::test]
    async fn winrestview_forwards_zero_based_scroll_top(arg: &'static str, expected: u32) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        let (ok, err): (bool, Option<String>) = lua
            .load(format!("return f.winrestview({arg})"))
            .eval_async()
            .await
            .unwrap();
        assert!(ok);
        assert_eq!(err, None);
        let UiAction::WinRestView { scroll_top } = rx.recv_async().await.unwrap() else {
            panic!("expected winrestview request");
        };
        assert_eq!(scroll_top, expected);
    }
}
