use std::collections::HashMap;
use std::env;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mlua::{Function, Lua, RegistryKey, Result as LuaResult, Table};

use crate::plugin_permissions::{Permission::{Env, Run}, PluginPermissions};
use crate::runtime::with_task_jobs;
use crate::terminal_backend::{
    JobEvent, TerminalBackend, TerminalHandle, TerminalSpec,
};
#[cfg(test)]
use crate::terminal_backend::LocalTerminal;

struct JobMeta {
    alive: bool,
    on_stdout: Option<RegistryKey>,
    on_stderr: Option<RegistryKey>,
    on_exit: Option<RegistryKey>,
    event_rx: Option<flume::Receiver<JobEvent>>,
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
    ) {
        self.jobs.insert(
            id,
            JobMeta {
                alive: true,
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
        if let Some(meta) = self.jobs.get_mut(&job_id) {
            if meta.alive {
                if let Some(kill) = meta.kill.take() {
                    kill();
                }
            }
        }
    }

    pub fn kill_all(&mut self) {
        for meta in self.jobs.values_mut() {
            if meta.alive {
                if let Some(kill) = meta.kill.take() {
                    kill();
                }
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
}

pub(crate) fn create_fn_table(lua: &Lua, perms: &PluginPermissions) -> LuaResult<Table> {
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
                let (cwd, env, on_stdout, on_stderr, on_exit) = match opts {
                    Some(ref opts) => {
                        let cwd: Option<String> = opts.get("cwd").ok();
                        let env: Option<HashMap<String, String>> = opts
                            .get::<Table>("env")
                            .ok()
                            .map(|t| {
                                t.pairs::<String, String>().filter_map(Result::ok).collect()
                            });
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
                        (cwd, env, on_stdout, on_stderr, on_exit)
                    }
                    None => (None, None, None, None, None),
                };

                let (backend, id) = with_task_jobs(&lua, |store| (store.backend(), store.next_id()));
                let spec = TerminalSpec { cmd, cwd, env };
                let handle = backend.start(spec).await.map_err(mlua::Error::runtime)?;
                with_task_jobs(&lua, |store| {
                    store.register(id, handle, on_stdout, on_stderr, on_exit);
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
                        event = rx.recv_async() => event.ok(),
                        _ = tokio::time::sleep(timeout) => None,
                    };

                    match event {
                        None => return Ok(mlua::Value::Nil),
                        Some(JobEvent::Stdout(line)) => stdout_lines.push(line),
                        Some(JobEvent::Stderr(line)) => stderr_lines.push(line),
                        Some(JobEvent::Exit(code)) => {
                            break code;
                        }
                    }
                };

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
                .map(|paths| {
                    env::split_paths(&paths)
                        .any(|dir| dir.join(&name).is_file())
                })
                .unwrap_or(false)
                || Path::new(&name).is_file();
            Ok(if found { 1 } else { 0 })
        })?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_backend::TerminalSpec;

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
            })
            .await
            .unwrap();
        store.register(id, handle, None, None, None);
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
}
