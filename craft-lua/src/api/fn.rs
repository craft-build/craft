use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table, Value};

use crate::api::fs::expand_tilde;
use crate::api::util::command::{UiAction, err_pair, ui_roundtrip, ui_send};
use crate::plugin_permissions::{
    Permission::{Env, Run},
    PluginPermissions,
};
use crate::runtime::{SharedSandboxConfig, with_task_jobs};
#[cfg(test)]
use crate::terminal_backend::LocalTerminal;
use crate::terminal_backend::{JobEvent, TerminalBackend, TerminalHandle, TerminalSpec};

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
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    pub(crate) event_rx: Option<flume::Receiver<JobEvent>>,
    kill: Option<Box<dyn FnOnce() + Send>>,
}

pub(crate) struct JobStore {
    jobs: HashMap<u32, JobMeta>,
    next_id: u32,
    backend: Arc<dyn TerminalBackend>,
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

    pub fn register(
        &mut self,
        id: u32,
        handle: TerminalHandle,
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
                on_stdout,
                on_stderr,
                on_exit,
                event_rx: Some(handle.events),
                kill: Some(handle.kill),
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

    pub fn take_receiver(&mut self, job_id: u32) -> Option<flume::Receiver<JobEvent>> {
        let meta = self.jobs.get_mut(&job_id)?;
        meta.event_rx.take()
    }

    pub fn drain_events(&self, buf: &mut Vec<(u32, JobEvent)>) {
        buf.clear();
        for (&id, meta) in &self.jobs {
            if let Some(ref rx) = meta.event_rx {
                while let Ok(event) = rx.try_recv() {
                    buf.push((id, event));
                }
            }
        }
    }

    pub fn mark_dead(&mut self, job_id: u32) {
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            meta.alive = false;
        }
    }

    pub fn kill(&mut self, job_id: u32) {
        if let Some(meta) = self.jobs.get_mut(&job_id)
            && meta.alive
            && let Some(kill) = meta.kill.take()
        {
            kill();
        }
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

pub(crate) fn create_fn_table(
    lua: &Lua,
    perms: &PluginPermissions,
    tx: Option<flume::Sender<UiAction>>,
) -> LuaResult<Table> {
    let t = lua.create_table()?;
    let perms = perms.clone();

    let p = perms.clone();
    t.set(
        "jobstart",
        lua.create_async_function(move |lua, (cmd, opts): (String, Option<Table>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(Run) {
                    return Err(crate::plugin_permissions::denied_error(Run));
                }
                let (cwd, env, on_stdout, on_stderr, on_exit, want_sandbox, background) = match opts
                {
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
                        let background: bool = opts.get("background").unwrap_or(false);
                        (
                            cwd,
                            env,
                            on_stdout,
                            on_stderr,
                            on_exit,
                            want_sandbox,
                            background,
                        )
                    }
                    None => (None, None, None, None, None, false, false),
                };

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
                    cmd,
                    cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
                    env,
                    sandbox,
                };
                let handle = backend.start(spec).await.map_err(mlua::Error::runtime)?;
                with_task_jobs(&lua, |store| {
                    store.register(id, handle, on_stdout, on_stderr, on_exit, background);
                });
                Ok(id)
            }
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "jobstop",
        lua.create_function(move |lua, job_id: u32| {
            if !p.is_allowed(Run) {
                return Err(crate::plugin_permissions::denied_error(Run));
            }
            with_task_jobs(lua, |store| store.kill(job_id));
            Ok(())
        })?,
    )?;

    let p = perms.clone();
    t.set(
        "jobwait",
        lua.create_async_function(move |lua, (job_id, timeout_ms): (u32, Option<u64>)| {
            let p = p.clone();
            async move {
                if !p.is_allowed(Run) {
                    return Err(crate::plugin_permissions::denied_error(Run));
                }
                let rx = with_task_jobs(&lua, |store| store.take_receiver(job_id))
                    .ok_or_else(|| mlua::Error::runtime("unknown job id or already waited"))?;

                let timeout = Duration::from_millis(timeout_ms.unwrap_or(30_000));

                let mut stdout_lines = Vec::new();
                let mut stderr_lines = Vec::new();

                let exit_code = loop {
                    let event = tokio::select! {
                        biased;
                        event = rx.recv_async() => event.ok(),
                        _ = tokio::time::sleep(timeout) => None,
                    };

                    let Some(event) = event else {
                        with_task_jobs(&lua, |store| store.put_receiver(job_id, rx));
                        return Ok(mlua::Value::Nil);
                    };
                    deliver_job_event(&lua, job_id, &event)?;
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
                Ok(mlua::Value::Table(result))
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
                let view = match ui_roundtrip(win_tx.as_ref(), |reply_tx| UiAction::WinSaveView {
                    reply_tx,
                })
                .await
                {
                    Ok(view) => view,
                    Err(e) => return Ok(err_pair(e)),
                };
                let table = lua.create_table()?;
                table.set("topline", i64::from(view.scroll_top) + 1)?;
                table.set("line_count", view.line_count)?;
                table.set("height", view.height)?;
                table.set("auto_scroll", view.auto_scroll)?;
                Ok((Value::Table(table), None))
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
                let scroll_top = topline.saturating_sub(1).clamp(0, i64::from(u16::MAX)) as u16;
                match ui_send(rest_tx.as_ref(), UiAction::WinRestView { scroll_top }) {
                    Ok(()) => Ok((Value::Boolean(true), None)),
                    Err(e) => Ok(err_pair(e)),
                }
            }
        })?,
    )?;

    Ok(t)
}

/// Fire the job's Lua callback for {event} (if any) and mark the job
/// dead on exit. Shared by `jobwait` and the async dispatch loop so
/// both deliver events identically.
pub(crate) fn deliver_job_event(lua: &Lua, job_id: u32, event: &JobEvent) -> LuaResult<()> {
    let callback = with_task_jobs(lua, |store| {
        store
            .callback_key(job_id, event)
            .and_then(|key| lua.registry_value::<Function>(key).ok())
    });
    if let Some(callback) = callback {
        let arg: Value = match event {
            JobEvent::Stdout(line) | JobEvent::Stderr(line) => {
                Value::String(lua.create_string(line)?)
            }
            JobEvent::Exit(code) => Value::Integer(*code as i64),
        };
        callback.call::<()>((job_id, arg))?;
    }
    if let JobEvent::Exit(_) = event {
        with_task_jobs(lua, |store| store.mark_dead(job_id));
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
        store.register(id, handle, None, None, None, false);
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
    async fn has_alive_jobs_tracks_state() {
        let mut store = make_store();
        assert!(!store.has_alive_jobs());

        let id = start_echo(&mut store).await;
        assert!(store.has_alive_jobs());

        store.mark_dead(id);
        assert!(!store.has_alive_jobs());
    }

    #[tokio::test]
    async fn noop_on_nonexistent_or_dead_jobs() {
        let mut store = make_store();
        store.mark_dead(999);
        store.kill(999);

        let id = start_echo(&mut store).await;
        store.mark_dead(id);
        store.kill(id);

        assert!(store.callback_key(999, &JobEvent::Exit(0)).is_none());
    }

    #[tokio::test]
    async fn take_receiver_lifecycle() {
        let mut store = make_store();
        assert!(store.take_receiver(999).is_none());

        let id = start_echo(&mut store).await;
        assert!(store.take_receiver(id).is_some());
        assert!(
            store.take_receiver(id).is_none(),
            "second take should fail (receiver already moved)"
        );
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
        let rx = store.take_receiver(id).unwrap();

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
    async fn drain_events_collects_from_all_jobs() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;

        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            store.drain_events(&mut buf);
            if buf
                .iter()
                .any(|(jid, e)| *jid == id && matches!(e, JobEvent::Exit(_)))
            {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("should receive exit event for completed job");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[tokio::test]
    async fn drain_events_empty_after_take() {
        let mut store = make_store();
        let id = start_echo(&mut store).await;
        let _rx = store.take_receiver(id).unwrap();

        let mut buf = Vec::new();
        store.drain_events(&mut buf);
        assert!(
            buf.is_empty(),
            "drained receiver yields no events via drain_events"
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
        store.register(id, handle, None, None, None, false);
        assert!(
            store.has_alive_jobs(),
            "job should be alive before the drop"
        );
        let rx = store.take_receiver(id).expect("receiver present");

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
        let t = create_fn_table(&lua, &PluginPermissions::trusted(), tx).unwrap();
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
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_view(Some(tx));
        tokio::spawn(async move {
            let UiAction::WinSaveView { reply_tx } = rx.recv_async().await.unwrap() else {
                panic!("expected winsaveview request");
            };
            reply_tx
                .send(WinView {
                    scroll_top: 6,
                    line_count: 100,
                    height: 24,
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
        assert_eq!(view.get::<u16>("topline").unwrap(), 7);
        assert_eq!(view.get::<u16>("line_count").unwrap(), 100);
        assert_eq!(view.get::<u16>("height").unwrap(), 24);
        assert!(!view.get::<bool>("auto_scroll").unwrap());
    }

    #[test_case("{ topline = 12 }", 11 ; "explicit_topline")]
    #[test_case("{}", 0 ; "missing_topline_defaults_to_first_line")]
    #[test_case("{ topline = -5 }", 0 ; "below_range_clamps_to_first_line")]
    #[tokio::test]
    async fn winrestview_forwards_zero_based_scroll_top(arg: &'static str, expected: u16) {
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
