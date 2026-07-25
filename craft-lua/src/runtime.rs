use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_int;
use std::panic::catch_unwind;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex, Weak};

use arc_swap::ArcSwap;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use event_listener::Event;

use craft_agent::cancel::CancelToken;
use craft_agent::tools::{
    HeaderResult, PermissionScopes, RegistryError, Tool, ToolRegistry, ToolSource,
};
use craft_agent::{BufferSnapshot, SharedBuf, SnapshotLine, SnapshotSpan, SpanStyle};
use include_dir::Dir;
use mlua::{Chunk, ChunkMode, Compiler, Function, Lua, RegistryKey, Table, Value as LuaValue, ffi};
use serde_json::Value;

use craft_config::RawConfig;

use crate::api::autocmd::AutocmdStore;
use crate::api::create_craft_global;
use crate::api::r#fn::{JobMeta, JobStore, deliver_job_event};
use crate::api::keymap::KeymapReader;
use crate::api::keymap::{KeymapStore, KeymapWriter};
use crate::api::options::{PluginOptionSpecs, PluginOpts, collect_plugin_options};
use crate::api::slot::SlotStore;
use crate::api::tool::{
    LuaOutputFormat, LuaTool, PendingTool, PendingTools, PermissionScopeSpec, ToolCallReply,
};
use crate::api::ui::HintStore;
use crate::api::ui::buf::{BufHandle, BufferStore};
use crate::api::util::command::{CommandHandlerMap, HintWriter, publish_command_snapshot};
use crate::api::util::command::{LuaCommandReader, LuaCommandWriter, UiAction};
use crate::api::util::convert::json_to_lua;
use crate::api::util::ctx::LuaCtx;
use crate::api::util::setup::ConfigStore;
use crate::error::PluginError;
use crate::plugin_permissions::PluginPermissions;
use crate::terminal_backend::TerminalBackend;

const INTERRUPT_SHUTDOWN_MSG: &str = "plugin interrupted: host shutting down";
const INTERRUPT_CANCELLED_MSG: &str = "plugin interrupted: task cancelled";
const INTERRUPT_DEADLINE_MSG: &str = "plugin interrupted: deadline exceeded";
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NIL_WITHOUT_FINISH_MSG: &str =
    "handler returned nil without calling ctx:finish() or starting jobs";
pub(crate) const CANCELLED_MSG: &str = "cancelled";
const MAX_INFLIGHT_TOOLS: usize = 64;
const GC_STEP_INTERVAL: usize = 4;
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_millis(10);
const OPT_LEVEL_JIT: u8 = 2;
const OPT_LEVEL_DEBUGGABLE: u8 = 1;
const DEBUG_INFO_FULL: u8 = 2;
const ASYNC_RUN_DEFAULT_DEADLINE: Duration = Duration::from_secs(60);
/// Async tasks spawned during restore may spawn further tasks; cap the rounds.
const RESTORE_SPAWN_ROUNDS: usize = 8;
/// Keeps a buggy plugin's restore task from freezing the lua loop.
const RESTORE_ASYNC_DEADLINE: Duration = Duration::from_secs(10);
const TURN_END_EVENT: &str = "TurnEnd";

fn strip_traceback(err: &mlua::Error) -> String {
    match err {
        mlua::Error::CallbackError { cause, .. } => strip_traceback(cause),
        other => other.to_string(),
    }
}

pub type LoadResult = Result<(), PluginError>;

/// Shared sandbox config readable from the VM thread, updatable from the host.
pub type SharedSandboxConfig = Arc<ArcSwap<craft_config::SandboxConfig>>;

type BgJobMap = HashMap<u32, JobMeta>;

pub(crate) enum HintContent {
    Static(String),
    Callback(RegistryKey),
}

pub(crate) struct PromptHintRegistration {
    pub(crate) prompts: Option<Vec<craft_agent::prompt::PromptId>>,
    pub(crate) slot: craft_agent::prompt::Slot,
    pub(crate) content: HintContent,
}

pub(crate) type PromptHintCallbacks = BTreeMap<Arc<str>, Vec<PromptHintRegistration>>;

/// Load and clear requests drain in-flight tools first so we never
/// mutate a plugin environment while a tool call is still running.
pub enum Request {
    /// Plugins are loaded, so native codegen may start using idle time.
    /// Sent last so it never interleaves with the loads themselves.
    WarmJit,
    LoadSource {
        name: Arc<str>,
        source: String,
        plugin_dir: Option<PathBuf>,
        permissions: PluginPermissions,
        opts: PluginOpts,
        reply: flume::Sender<LoadResult>,
    },
    CallTool {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        ctx: Box<LuaCtx>,
        deadline: Option<Instant>,
        reply: flume::Sender<ToolCallReply>,
        live: Option<LiveCtx>,
    },
    ComputeHeader {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        reply: flume::Sender<HeaderResult>,
    },
    ComputePermissionScopes {
        plugin: Arc<str>,
        tool: Arc<str>,
        input: Value,
        reply: flume::Sender<Option<PermissionScopes>>,
    },
    ClearPlugin {
        plugin: Arc<str>,
        reply: flume::Sender<()>,
    },
    RunInitLua {
        source: String,
        source_name: String,
        plugin_dir: Option<PathBuf>,
        reply: flume::Sender<Result<Option<RawConfig>, PluginError>>,
    },
    RunCommand {
        plugin: Arc<str>,
        command: Arc<str>,
        args: String,
    },
    CollectPromptSlots {
        reply: flume::Sender<craft_agent::prompt::ResolvedSlots>,
    },
    CollectPluginOptions {
        reply: flume::Sender<PluginOptionSpecs>,
    },
    SetTerminalBackend {
        backend: Arc<dyn TerminalBackend>,
    },
    Shutdown,
    RestoreToolAsync {
        item: RestoreItem,
        event_tx: craft_agent::EventSender,
    },
    RestoreComplete {
        flag: Arc<AtomicBool>,
    },
    FireAutocmd {
        event: String,
        data: Value,
    },
    RunKeybindCallback {
        id: u64,
    },
    RunHook {
        event: String,
        tool: String,
        input: Value,
        output: String,
        is_error: bool,
        reply: flume::Sender<crate::api::hooks::HookReply>,
    },
    SetSandboxConfig {
        config: craft_config::SandboxConfig,
    },
}

/// Everything needed to re-run a lua restore callback. Used by session
/// restore and theme re-bake so both paths share a single struct.
pub struct RestoreItem {
    pub tool: Arc<str>,
    pub tool_use_id: String,
    pub output: String,
    pub input: Value,
    pub is_error: bool,
    pub tool_output_lines: craft_config::ToolOutputLines,
    pub theme_gen: Option<u64>,
    pub expanded: bool,
}

pub(crate) struct RestoreReply {
    pub body: Option<BufferSnapshot>,
    pub header: Option<BufferSnapshot>,
}

/// The UI restores tool bodies from these events; a send can only fail when
/// the receiver is gone, but that still loses the snapshot, so it gets a log.
pub(crate) fn send_render_event(
    event_tx: &craft_agent::EventSender,
    tool_id: &str,
    what: &str,
    event: craft_agent::AgentEvent,
) {
    if event_tx.send(event).is_err() {
        tracing::warn!(tool_id, what, "tool render event dropped: channel closed");
    }
}

impl RestoreReply {
    pub(crate) fn emit(
        self,
        tool_use_id: &str,
        theme_gen: Option<u64>,
        event_tx: &craft_agent::EventSender,
    ) {
        if let Some(snapshot) = self.body {
            send_render_event(
                event_tx,
                tool_use_id,
                "body_snapshot",
                craft_agent::AgentEvent::ToolSnapshot {
                    id: tool_use_id.to_owned(),
                    snapshot,
                    theme_gen,
                },
            );
        }
        if let Some(snapshot) = self.header {
            send_render_event(
                event_tx,
                tool_use_id,
                "header_snapshot",
                craft_agent::AgentEvent::ToolHeaderSnapshot {
                    id: tool_use_id.to_owned(),
                    snapshot,
                    theme_gen,
                },
            );
        }
    }
}

#[derive(Clone)]
pub struct LiveCtx {
    pub event_tx: craft_agent::EventSender,
    pub tool_use_id: String,
}

/// The `Mutex` is never contended (Lua is single-threaded) but
/// `Lua::app_data` requires `Send + Sync` with the `send` feature.
pub(crate) struct TaskCell {
    pub(crate) cancel: CancelToken,
    pub(crate) deadline: Cell<Option<Instant>>,
    pub(crate) deadline_secs: Cell<Option<u64>>,
    pub(crate) jobs: JobStore,
    pub(crate) bufs: BufferStore,
    pub(crate) live: Option<LiveCtx>,
    pub(crate) click: Option<RegistryKey>,
    pub(crate) inline_spawn: Option<Vec<PendingAsyncTask>>,
    /// Set by `TaskScope::new`; `enqueue_async_task` upgrades it so queued
    /// `craft.async.run` tasks share ownership of `bufs`. See [`BufsClaim`].
    bufs_claim: Weak<BufsClaim>,
}

impl TaskCell {
    fn new(
        cancel: CancelToken,
        deadline: Option<Instant>,
        live: Option<LiveCtx>,
        backend: Arc<dyn TerminalBackend>,
    ) -> Self {
        Self {
            cancel,
            deadline: Cell::new(deadline),
            deadline_secs: Cell::new(None),
            jobs: JobStore::with_backend(backend),
            bufs: BufferStore::new(),
            live,
            click: None,
            inline_spawn: None,
            bufs_claim: Weak::new(),
        }
    }
}

pub(crate) type TaskHandle = Arc<Mutex<TaskCell>>;

pub(crate) fn lock_cell(handle: &TaskHandle) -> std::sync::MutexGuard<'_, TaskCell> {
    handle.lock().unwrap_or_else(|e| e.into_inner())
}

/// The buf streamed to the UI on async-task completion. Resolved lazily so an
/// in-flight highlight rewrite lands before the snapshot; craft has no explicit
/// root-buf override, so this is `bufs.live_buf()`.
fn resolve_root_buf(handle: &TaskHandle) -> Option<Arc<SharedBuf>> {
    lock_cell(handle).bufs.live_buf().cloned()
}

/// Sole place the `--no-jit` flag touches VM state. Called once at VM
/// creation, before any chunk (init.lua included) is compiled, and hands
/// back the compiler so bundled modules compile the same way. Jit off
/// drops to the O1 interpreter with full debug info, the combination
/// that keeps the most usable backtraces.
///
/// Native codegen stays off at load time either way: mlua runs it inline
/// from `Lua::load`, and doing that for every plugin was the single
/// largest cost of startup. Loaded chunks go to a [`CodegenQueue`]
/// instead.
fn install_compiler(lua: &Lua, jit: bool) -> Compiler {
    lua.enable_jit(false);
    let compiler = if jit {
        Compiler::new().set_optimization_level(OPT_LEVEL_JIT)
    } else {
        Compiler::new()
            .set_optimization_level(OPT_LEVEL_DEBUGGABLE)
            .set_debug_level(DEBUG_INFO_FULL)
    };
    lua.set_compiler(compiler.clone());
    compiler
}

/// Many plugins require the same bundled module, and each one needs a
/// separate instance because the module closes over the plugin's
/// `craft`. Only the instantiation has to repeat, so the source is
/// compiled to bytecode once per VM rather than once per plugin.
#[derive(Clone)]
struct BundledModules {
    dirs: &'static [&'static Dir<'static>],
    compiler: Compiler,
    bytecode: Arc<Mutex<HashMap<String, Arc<Vec<u8>>>>>,
}

impl BundledModules {
    fn bytecode(&self, rel_path: &str) -> Result<Option<Arc<Vec<u8>>>, mlua::Error> {
        let mut cache = self.bytecode.lock().expect("bytecode cache");
        if let Some(cached) = cache.get(rel_path) {
            return Ok(Some(Arc::clone(cached)));
        }
        let Some(source) = self
            .dirs
            .iter()
            .find_map(|dir| dir.get_file(rel_path).and_then(|f| f.contents_utf8()))
        else {
            return Ok(None);
        };
        let compiled = Arc::new(self.compiler.compile(source)?);
        cache.insert(rel_path.to_owned(), Arc::clone(&compiled));
        Ok(Some(compiled))
    }
}

/// Chunks awaiting native codegen, `None` when jit is off. Compiling a
/// chunk's main function also compiles every function nested in it, and
/// the native code lives on the shared proto, so closures already handed
/// to the tool registry get faster too.
type CodegenQueue = Option<Arc<Mutex<Vec<Function>>>>;

fn queue_codegen(queue: &CodegenQueue, func: &Function) {
    if let Some(queue) = queue {
        queue.lock().expect("codegen queue").push(func.clone());
    }
}

struct ModuleLoader {
    bundled: BundledModules,
    lua_dir: Option<PathBuf>,
    env: Table,
    codegen: CodegenQueue,
    loaded: Table,
    loading: Table,
}

impl ModuleLoader {
    /// Bundled modules are tried first, so a plugin cannot shadow
    /// `craft.truncate` and friends with a file of its own.
    fn plugin_source(&self, rel_path: &str, modname: &str) -> Result<Option<String>, mlua::Error> {
        let Some(dir) = self.lua_dir.as_ref() else {
            return Ok(None);
        };
        let normalized = dir
            .join(rel_path)
            .components()
            .fold(PathBuf::new(), |mut acc, c| {
                match c {
                    std::path::Component::ParentDir => {
                        acc.pop();
                    }
                    std::path::Component::CurDir => {}
                    _ => acc.push(c),
                }
                acc
            });
        if !normalized.starts_with(dir) {
            return Err(mlua::Error::runtime(format!(
                "require: '{modname}' outside sandbox"
            )));
        }
        Ok(std::fs::read_to_string(&normalized).ok())
    }

    fn bind(&self, chunk: Chunk<'_>, modname: &str) -> Result<Function, mlua::Error> {
        chunk
            .set_name(modname)
            .set_environment(self.env.clone())
            .into_function()
    }

    /// Bundled modules load as bytecode from the shared cache. Plugin
    /// files load as source, so Luau reports syntax errors against the
    /// file the user wrote.
    fn load(&self, lua: &Lua, modname: &str) -> Result<LuaValue, mlua::Error> {
        let rel_path = modname.replace('.', "/") + ".lua";
        let func = match self.bundled.bytecode(&rel_path)? {
            Some(bytecode) => self.bind(
                lua.load(bytecode.as_slice()).set_mode(ChunkMode::Binary),
                modname,
            )?,
            None => {
                let Some(source) = self.plugin_source(&rel_path, modname)? else {
                    return Err(mlua::Error::runtime(format!(
                        "require '{modname}': module not found"
                    )));
                };
                self.bind(lua.load(source.as_str()), modname)?
            }
        };
        queue_codegen(&self.codegen, &func);
        func.call(())
    }

    fn require(&self, lua: &Lua, modname: &str) -> Result<LuaValue, mlua::Error> {
        if modname.is_empty() {
            return Err(mlua::Error::runtime(
                "require: module name must be non-empty",
            ));
        }

        if let Ok(cached) = self.loaded.get::<LuaValue>(modname)
            && cached != LuaValue::Nil
        {
            return Ok(cached);
        }

        if self.loading.get::<bool>(modname).unwrap_or(false) {
            return Ok(LuaValue::Boolean(true));
        }

        // Cleared on every path, so a failed require never leaves the
        // module wedged as "in progress".
        self.loading.set(modname, true)?;
        let result = self.load(lua, modname);
        self.loading.set(modname, LuaValue::Nil)?;
        let result = result?;

        let stored = if result == LuaValue::Nil {
            LuaValue::Boolean(true)
        } else {
            result.clone()
        };
        self.loaded.set(modname, stored)?;
        Ok(result)
    }
}

type InterruptFn = unsafe extern "C-unwind" fn(*mut ffi::lua_State, c_int);

/// The poker thread and the VM thread race on this field, so the write
/// must be atomic to stay defined behavior on the Rust side.
fn store_interrupt(state: *mut ffi::lua_State, cb: Option<InterruptFn>) {
    let raw = cb.map_or(ptr::null_mut(), |f| f as *mut ());
    unsafe {
        let slot = &raw mut (*ffi::lua_callbacks(state)).interrupt;
        AtomicPtr::from_ptr(slot.cast::<*mut ()>()).store(raw, Ordering::Release);
    }
}

/// Shutdown flag mirrored into app data so the watchdog interrupt can
/// re-check it on the Lua thread.
struct ShutdownFlag(Arc<AtomicBool>);

/// Cancellation watchdog. A resident mlua interrupt fires at every
/// safepoint and costs ~100ns a pop, which ate most of the codegen win
/// (see `benches/luau_perf.rs`). So the VM runs with no interrupt at
/// all, and this thread arms a one-shot native one every poll tick.
/// Luau documents `lua_callbacks(L)->interrupt` as safe to assign from
/// another thread, and the VM only pays a null check per safepoint.
/// The callback re-checks shutdown/cancel/deadline on the Lua thread
/// before raising, so a stale poke never kills the wrong task.
struct Watchdog {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn spawn(lua: &Lua, shutdown: Arc<AtomicBool>) -> Self {
        lua.set_app_data(ShutdownFlag(shutdown));
        let main_state =
            lua.exec_raw_lua(|raw| unsafe { ffi::lua_mainthread(raw.state()) }) as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stop = Arc::clone(&stop);
            // Keeps the VM alive while this thread can still write to it,
            // even if a refactor reorders drops.
            let keep_alive = lua.clone();
            move || {
                let _keep_alive = keep_alive;
                loop {
                    thread::park_timeout(WATCHDOG_POLL_INTERVAL);
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    store_interrupt(main_state as *mut ffi::lua_State, Some(watchdog_interrupt));
                }
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

/// One-shot interrupt armed by [`Watchdog`]: disarms itself, re-checks the
/// kill conditions, and raises a plain string error that unwinds like any
/// Lua error. Must not raise during GC (`gc >= 0`), same rule mlua follows.
unsafe extern "C-unwind" fn watchdog_interrupt(state: *mut ffi::lua_State, gc: c_int) {
    if gc >= 0 {
        return;
    }
    store_interrupt(state, None);
    // A Rust panic must not unwind into the VM; treat it as "no kill".
    let msg = catch_unwind(|| interrupt_reason(state)).unwrap_or(None);
    if let Some(msg) = msg {
        unsafe {
            // A safepoint frame may have zero free slots; grow before pushing
            // (raw pushes assert a free slot). On failure the next poke retries.
            if ffi::lua_checkstack(state, 1) == 0 {
                return;
            }
            ffi::lua_pushlstring(state, msg.as_ptr().cast(), msg.len());
            ffi::lua_error(state);
        }
    }
}

fn interrupt_reason(state: *mut ffi::lua_State) -> Option<&'static str> {
    let lua = unsafe { Lua::get_or_init_from_ptr(state) };
    if lua
        .app_data_ref::<ShutdownFlag>()
        .is_some_and(|f| f.0.load(Ordering::Relaxed))
    {
        return Some(INTERRUPT_SHUTDOWN_MSG);
    }
    let handle = lua.app_data_ref::<TaskHandle>()?;
    let cell = lock_cell(&handle);
    if cell.cancel.is_cancelled() {
        Some(INTERRUPT_CANCELLED_MSG)
    } else if cell.deadline.get().is_some_and(|d| Instant::now() > d) {
        Some(INTERRUPT_DEADLINE_MSG)
    } else {
        None
    }
}

/// Publishes a `TaskCell` into `Lua::app_data` for the duration of a
/// task, and restores the previous one on drop. Async work must go
/// through `scope_future` because concurrent tasks on the same executor
/// overwrite `app_data` between yields.
pub(crate) struct TaskScope {
    lua: Lua,
    handle: TaskHandle,
    prev: Option<TaskHandle>,
    /// Dropped after `Drop::drop` runs, so jobs die before bufs can clear.
    _bufs_claim: Arc<BufsClaim>,
}

impl TaskScope {
    pub(crate) fn new(lua: &Lua, cell: TaskCell) -> Self {
        let handle: TaskHandle = Arc::new(Mutex::new(cell));
        let claim = Arc::new(BufsClaim(Arc::clone(&handle)));
        lock_cell(&handle).bufs_claim = Arc::downgrade(&claim);
        if let Some(bg) = lua.app_data_ref::<RefCell<BgJobMap>>() {
            let live_bg: HashMap<_, _> = bg.borrow_mut().drain().filter(|(_, m)| m.alive).collect();
            if !live_bg.is_empty() {
                lock_cell(&handle).jobs.absorb(live_bg);
            }
        }
        let prev = lua.set_app_data::<TaskHandle>(Arc::clone(&handle));
        Self {
            lua: lua.clone(),
            handle,
            prev,
            _bufs_claim: claim,
        }
    }

    pub(crate) fn detached(lua: &Lua) -> Self {
        Self::new(
            lua,
            TaskCell::new(
                CancelToken::none(),
                None,
                None,
                Arc::new(crate::terminal_backend::LocalTerminal),
            ),
        )
    }

    pub(crate) fn handle(&self) -> &TaskHandle {
        &self.handle
    }

    pub(crate) fn scope_future<F>(&self, inner: F) -> ScopedFuture<F> {
        ScopedFuture {
            lua: self.lua.clone(),
            handle: Arc::clone(&self.handle),
            inner,
        }
    }
}

/// Runs an async system callback under a [detached] scope so callers
/// can't forget to set one up.
///
/// Job callbacks (`on_stdout` etc.) are pumped whenever {fut} is
/// suspended, so a handler parked in e.g. `win:recv()` still streams
/// job output, like Neovim firing callbacks from its idle event loop.
///
/// [detached]: TaskScope::detached
pub(crate) async fn run_detached<F: std::future::Future>(lua: &Lua, fut: F) -> F::Output {
    let scope = TaskScope::detached(lua);
    let handle = Arc::clone(scope.handle());
    let pump = async {
        let mut event_buf = Vec::new();
        loop {
            lock_cell(&handle).jobs.drain_events(&mut event_buf);
            for (job_id, event) in event_buf.drain(..) {
                if let Err(e) = deliver_job_event(lua, job_id, &event) {
                    tracing::warn!(error = %strip_traceback(&e), "detached job callback failed");
                }
            }
            tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
        }
    };
    let out = tokio::select! {
        biased;
        out = scope.scope_future(fut) => out,
        _ = pump => unreachable!("pump never completes"),
    };
    drop(scope);
    out
}

impl Drop for TaskScope {
    fn drop(&mut self) {
        {
            let bg_jobs = lock_cell(&self.handle).jobs.drain_background();
            if !bg_jobs.is_empty()
                && let Some(bg) = self.lua.app_data_ref::<RefCell<BgJobMap>>()
            {
                bg.borrow_mut().extend(bg_jobs);
            }
            let mut cell = lock_cell(&self.handle);
            cell.jobs.kill_all();
            cell.jobs.clear(&self.lua);
            if let Some(k) = cell.click.take() {
                let _ = self.lua.remove_registry_value(k);
            }
        }
        match self.prev.take() {
            Some(p) => {
                self.lua.set_app_data(p);
            }
            None => {
                self.lua.remove_app_data::<TaskHandle>();
            }
        }
    }
}

/// Re-publishes the task handle around every `poll` so each concurrent
/// task on the shared Lua instance sees its own `TaskCell`.
pub(crate) struct ScopedFuture<F> {
    lua: Lua,
    handle: TaskHandle,
    inner: F,
}

impl<F: std::future::Future> std::future::Future for ScopedFuture<F> {
    type Output = F::Output;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: `inner` is structurally pinned; `lua`/`handle` are
        // never moved out.
        let this = unsafe { self.get_unchecked_mut() };
        let prev = this
            .lua
            .set_app_data::<TaskHandle>(Arc::clone(&this.handle));
        let result = unsafe { std::pin::Pin::new_unchecked(&mut this.inner) }.poll(cx);
        match prev {
            Some(p) => {
                this.lua.set_app_data(p);
            }
            None => {
                this.lua.remove_app_data::<TaskHandle>();
            }
        }
        result
    }
}

pub(crate) fn active_task(lua: &Lua) -> TaskHandle {
    lua.app_data_ref::<TaskHandle>()
        .map(|r| Arc::clone(&*r))
        .expect("task accessor called outside a task scope")
}

pub(crate) fn with_task_jobs<R>(lua: &Lua, f: impl FnOnce(&mut JobStore) -> R) -> R {
    let cell = active_task(lua);
    let mut guard = lock_cell(&cell);
    f(&mut guard.jobs)
}

pub(crate) fn active_terminal_backend(lua: &Lua) -> Arc<dyn TerminalBackend> {
    lua.app_data_ref::<Arc<dyn TerminalBackend>>()
        .map(|b| Arc::clone(&*b))
        .unwrap_or_else(|| Arc::new(crate::terminal_backend::LocalTerminal))
}

pub(crate) fn with_task_bufs<R>(lua: &Lua, f: impl FnOnce(&mut BufferStore) -> R) -> R {
    f(&mut lock_cell(&active_task(lua)).bufs)
}

#[cfg(test)]
pub(crate) fn with_live_ctx<R>(lua: &Lua, f: impl FnOnce(&LiveCtx) -> R) -> Option<R> {
    let handle = lua.app_data_ref::<TaskHandle>()?;
    lock_cell(&handle).live.as_ref().map(f)
}

pub(crate) fn enqueue_async_task(lua: &Lua, work_fn: RegistryKey) -> Result<(), mlua::Error> {
    let handle = lua.app_data_ref::<TaskHandle>();
    let (cancel, live_ctx) = match &handle {
        Some(h) => {
            let cell = lock_cell(h);
            (cell.cancel.clone(), cell.live.clone())
        }
        None => (CancelToken::none(), None),
    };

    let mut task = PendingAsyncTask {
        work_fn,
        cancel,
        deadline: Some(Instant::now() + ASYNC_RUN_DEFAULT_DEADLINE),
        live_ctx,
        owner: None,
    };

    if let Some(h) = &handle {
        let mut cell = lock_cell(h);
        // Inline tasks live inside the cell, so a claim there would be a
        // strong Arc cycle; they run before the scope drops anyway.
        if let Some(inline) = cell.inline_spawn.as_mut() {
            inline.push(task);
            return Ok(());
        }
        task.owner = cell.bufs_claim.upgrade();
    }

    let queue = lua
        .app_data_ref::<SpawnQueue>()
        .ok_or_else(|| mlua::Error::runtime("spawn queue not initialized"))?;
    queue.borrow_mut().push(task);
    Ok(())
}

/// Caps concurrent coroutines so they don't blow the Lua stack or starve
/// the executor. Also serves as a drain barrier for load/clear ops.
struct InflightGate {
    lua: Lua,
    count: Cell<usize>,
    ops_since_gc: Cell<usize>,
    event: Event,
}

impl InflightGate {
    fn new(lua: Lua) -> Self {
        Self {
            lua,
            count: Cell::new(0),
            ops_since_gc: Cell::new(0),
            event: Event::new(),
        }
    }

    fn increment(&self) {
        self.count.set(self.count.get() + 1);
    }

    fn decrement(&self) {
        self.count.set(self.count.get().saturating_sub(1));
        self.event.notify(usize::MAX);
        let ops = self.ops_since_gc.get() + 1;
        if ops >= GC_STEP_INTERVAL {
            self.ops_since_gc.set(0);
            self.lua.gc_step().ok();
        } else {
            self.ops_since_gc.set(ops);
        }
    }

    async fn wait_below(&self, limit: usize) {
        loop {
            if self.count.get() < limit {
                return;
            }
            let listener = self.event.listen();
            if self.count.get() < limit {
                return;
            }
            listener.await;
        }
    }

    async fn drain(&self) {
        self.wait_below(1).await;
    }
}

struct GateGuard<'a> {
    gate: &'a InflightGate,
}

impl<'a> GateGuard<'a> {
    fn new(gate: &'a InflightGate) -> Self {
        gate.increment();
        Self { gate }
    }
}

impl Drop for GateGuard<'_> {
    fn drop(&mut self) {
        self.gate.decrement();
    }
}

pub(crate) struct PendingAsyncTask {
    pub work_fn: RegistryKey,
    pub cancel: CancelToken,
    pub deadline: Option<Instant>,
    pub live_ctx: Option<LiveCtx>,
    pub owner: Option<Arc<BufsClaim>>,
}

/// Shared ownership of a task's `bufs`: the scope holds one clone, each queued
/// `craft.async.run` task holds one, so the `Arc` strong count is the single
/// source of truth for liveness. Dropping the last clone clears the store,
/// breaking Lua GC watcher/click cycles. Root buf is resolved lazily because it
/// may not exist at enqueue time.
pub(crate) struct BufsClaim(TaskHandle);

impl BufsClaim {
    fn root_buf(&self) -> Option<Arc<SharedBuf>> {
        resolve_root_buf(&self.0)
    }
}

impl Drop for BufsClaim {
    fn drop(&mut self) {
        lock_cell(&self.0).bufs.clear();
    }
}

pub(crate) type SpawnQueue = RefCell<Vec<PendingAsyncTask>>;

async fn run_work_fn(
    lua: &Lua,
    work_fn: &RegistryKey,
    deadline: Option<Instant>,
) -> Result<LuaValue, mlua::Error> {
    let func: Function = lua.registry_value(work_fn)?;
    let fut = lua.create_thread(func)?.into_async::<LuaValue>(())?;
    match deadline {
        Some(dl) => tokio::select! {
            result = fut => result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(dl)) => {
                Err(mlua::Error::runtime("timeout"))
            }
        },
        None => fut.await,
    }
}

fn drain_spawn_queue(lua: &Lua, gate: &Rc<InflightGate>) {
    let tasks: Vec<PendingAsyncTask> = {
        let Some(queue) = lua.app_data_ref::<SpawnQueue>() else {
            return;
        };
        let mut q = queue.borrow_mut();
        if q.is_empty() {
            return;
        }
        q.drain(..).collect()
    };

    for task in tasks {
        if task.cancel.is_cancelled() {
            tracing::debug!(
                tool_id = task.live_ctx.as_ref().map(|l| l.tool_use_id.as_str()),
                "async.run: cancelled before spawn"
            );
            lua.remove_registry_value(task.work_fn).ok();
            continue;
        }

        let lua = lua.clone();
        let g = Rc::clone(gate);

        tokio::task::spawn_local(async move {
            let _gate_guard = GateGuard::new(&g);

            let scope = TaskScope::new(
                &lua,
                TaskCell::new(
                    task.cancel.clone(),
                    task.deadline,
                    task.live_ctx.clone(),
                    active_terminal_backend(&lua),
                ),
            );
            let run = scope.scope_future(run_work_fn(&lua, &task.work_fn, task.deadline));

            let result = run.await;
            if let Err(e) = &result {
                let tool_id = task.live_ctx.as_ref().map(|l| l.tool_use_id.as_str());
                tracing::debug!(error = %e, tool_id, "async.run: task failed");
            }

            if let Some(ref live) = task.live_ctx
                && let Some(buf) = task.owner.as_ref().and_then(|c| c.root_buf())
            {
                // Always `read`, not `read_if_dirty`: the dirty flag is
                // consume-once and the UI polls each frame, so the flag
                // races. Re-emitting identical content is harmless.
                send_render_event(
                    &live.event_tx,
                    &live.tool_use_id,
                    "async_snapshot",
                    craft_agent::AgentEvent::ToolSnapshot {
                        id: live.tool_use_id.clone(),
                        snapshot: craft_agent::BufferSnapshot::from_arc(buf.read()),
                        theme_gen: None,
                    },
                );
            }

            drop(scope);
            lua.remove_registry_value(task.work_fn).ok();
            drain_spawn_queue(&lua, &g);
        });
    }
}

/// Barrier for load/clear ops: drains queued `craft.async.run` tasks and
/// waits for every in-flight task, looping until both are quiescent. A bare
/// `gate.drain()` is not enough: a click handler that runs during the drain
/// can enqueue an async job into the spawn queue, which only the dispatcher
/// loop would spawn - after the barrier already passed.
async fn drain_barrier(lua: &Lua, gate: &Rc<InflightGate>) {
    loop {
        drain_spawn_queue(lua, gate);
        gate.drain().await;
        let empty = lua
            .app_data_ref::<SpawnQueue>()
            .map(|q| q.borrow().is_empty())
            .unwrap_or(true);
        if empty {
            return;
        }
    }
}

struct ToolKeys {
    handler: RegistryKey,
    header: Option<RegistryKey>,
    restore: Option<RegistryKey>,
    permission_scopes: Option<RegistryKey>,
}

type PluginMap = Rc<RefCell<HashMap<Arc<str>, HashMap<Arc<str>, ToolKeys>>>>;

/// Plugins run sandboxed: `require`/`io`/`package` are removed, and
/// `os`/`debug` go through Luau's built-in restrictions.
struct LuaRuntime {
    /// Held for its Drop (joins the poker thread). Field order doesn't
    /// matter: the thread keeps its own `Lua` clone alive.
    _watchdog: Watchdog,
    lua: Lua,
    pending: PendingTools,
    plugins: PluginMap,
    registry: Arc<ToolRegistry>,
    tx: flume::Sender<Request>,
    shutdown: Arc<AtomicBool>,
    bundled: BundledModules,
    codegen_queue: CodegenQueue,
    ui_action_tx: Option<flume::Sender<UiAction>>,
    embed_tx: Option<crate::api::embed::EmbedChannel>,
    terminal_backend: Arc<dyn TerminalBackend>,
}

impl LuaRuntime {
    #[allow(clippy::too_many_arguments)]
    fn new(
        registry: Arc<ToolRegistry>,
        tx: flume::Sender<Request>,
        shutdown: Arc<AtomicBool>,
        bundled_dirs: &'static [&'static Dir<'static>],
        ui_action_tx: Option<flume::Sender<UiAction>>,
        command_writer: LuaCommandWriter,
        keymap_writer: KeymapWriter,
        hint_writer: HintWriter,
        embed_tx: Option<crate::api::embed::EmbedChannel>,
        terminal_backend: Arc<dyn TerminalBackend>,
        jit: bool,
    ) -> Result<Self, PluginError> {
        let lua = Lua::new();
        let compiler = install_compiler(&lua, jit);
        let pending: PendingTools = Arc::new(Mutex::new(Vec::new()));

        let watchdog = Watchdog::spawn(&lua, Arc::clone(&shutdown));

        let globals = lua.globals();
        for name in &["require", "io", "package"] {
            globals
                .set(*name, LuaValue::Nil)
                .map_err(|e| PluginError::Lua {
                    plugin: "<init>".to_owned(),
                    source: e,
                })?;
        }
        drop(globals);
        lua.sandbox(true).map_err(|e| PluginError::Lua {
            plugin: "<init>".to_owned(),
            source: e,
        })?;

        lua.set_app_data(CommandHandlerMap::new());
        lua.set_app_data(SpawnQueue::default());
        lua.set_app_data(command_writer);
        lua.set_app_data(PromptHintCallbacks::default());
        lua.set_app_data(PluginOptionSpecs::default());
        lua.set_app_data(HintStore::new());
        lua.set_app_data(crate::api::hooks::HookHandlerMap::new());
        lua.set_app_data(AutocmdStore::default());
        lua.set_app_data(SlotStore::default());
        lua.set_app_data(KeymapStore::new());
        lua.set_app_data(keymap_writer);
        lua.set_app_data(hint_writer);
        lua.set_app_data::<SharedSandboxConfig>(Arc::new(ArcSwap::from_pointee(
            craft_config::SandboxConfig::default(),
        )));
        lua.set_app_data::<Arc<dyn TerminalBackend>>(Arc::clone(&terminal_backend));
        lua.set_app_data::<RefCell<BgJobMap>>(RefCell::new(BgJobMap::new()));

        Ok(Self {
            _watchdog: watchdog,
            lua,
            pending,
            plugins: Rc::new(RefCell::new(HashMap::new())) as PluginMap,
            registry,
            tx,
            shutdown,
            bundled: BundledModules {
                dirs: bundled_dirs,
                compiler,
                bytecode: Arc::default(),
            },
            codegen_queue: jit.then(Arc::default),
            ui_action_tx,
            embed_tx,
            terminal_backend,
        })
    }

    /// Returns false when there is nothing left to compile, so the
    /// caller can stop polling.
    fn codegen_step(&self) -> bool {
        let Some(queue) = self.codegen_queue.as_ref() else {
            return false;
        };
        let Some(func) = queue.lock().expect("codegen queue").pop() else {
            return false;
        };
        let compiled = unsafe {
            self.lua
                .exec_raw::<()>(func, |state| ffi::luau_codegen_compile(state, -1))
        };
        if let Err(e) = compiled {
            tracing::debug!(error = %e, "native codegen failed");
        }
        true
    }

    fn drop_plugin_keys(&mut self, name: &str) {
        if let Some(mut store) = self.lua.app_data_mut::<PluginOptionSpecs>() {
            store.remove(name);
        }
        if let Some(mut store) = self.lua.app_data_mut::<AutocmdStore>() {
            store.clear_plugin(name);
        }
        if let Some(mut store) = self.lua.app_data_mut::<SlotStore>() {
            store.clear_plugin(name);
        }
        if let Some(keys) = self.plugins.borrow_mut().remove(name) {
            for (_, tk) in keys {
                if let Err(e) = self.lua.remove_registry_value(tk.handler) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua handler key");
                }
                if let Some(sk) = tk.header
                    && let Err(e) = self.lua.remove_registry_value(sk)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua header key");
                }
                if let Some(sk) = tk.permission_scopes
                    && let Err(e) = self.lua.remove_registry_value(sk)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop lua permission_scopes key");
                }
            }
        }
        if let Some(mut cmd_map) = self.lua.app_data_mut::<CommandHandlerMap>()
            && let Some(cmds) = cmd_map.remove(name)
        {
            for (_, entry) in cmds {
                if let Err(e) = self.lua.remove_registry_value(entry.handler) {
                    tracing::warn!(plugin = name, error = %e, "failed to drop command handler key");
                }
            }
            drop(cmd_map);
            if let (Some(map), Some(writer)) = (
                self.lua.app_data_ref::<CommandHandlerMap>(),
                self.lua.app_data_ref::<LuaCommandWriter>(),
            ) {
                publish_command_snapshot(&map, &writer);
            }
        }
        if let Some(mut hints) = self.lua.app_data_mut::<PromptHintCallbacks>()
            && let Some(regs) = hints.remove(name)
        {
            for reg in regs {
                if let HintContent::Callback(key) = reg.content
                    && let Err(e) = self.lua.remove_registry_value(key)
                {
                    tracing::warn!(plugin = name, error = %e, "failed to drop prompt hint callback key");
                }
            }
        }
        if let Some(mut store) = self.lua.app_data_mut::<HintStore>() {
            store.clear_plugin(name);
            let entries = store.snapshot_entries();
            drop(store);
            if let Some(writer) = self.lua.app_data_ref::<HintWriter>() {
                writer.publish(entries);
            }
        }
    }

    async fn collect_prompt_slots(&self) -> craft_agent::prompt::ResolvedSlots {
        use craft_agent::prompt::{PromptId, Slot, SlotEntry};

        enum ResolvedContent {
            Static(String),
            Callback(Function),
        }

        struct ResolvedItem {
            plugin: Arc<str>,
            prompts: Option<Vec<PromptId>>,
            slot: Slot,
            content: ResolvedContent,
        }

        let items: Vec<ResolvedItem> = {
            let Some(map) = self.lua.app_data_ref::<PromptHintCallbacks>() else {
                return craft_agent::prompt::ResolvedSlots::default();
            };
            map.iter()
                .flat_map(|(plugin, regs)| {
                    regs.iter().filter_map(|reg| {
                        let content = match &reg.content {
                            HintContent::Static(s) => ResolvedContent::Static(s.clone()),
                            HintContent::Callback(key) => {
                                let func = self.lua.registry_value::<Function>(key).ok()?;
                                ResolvedContent::Callback(func)
                            }
                        };
                        Some(ResolvedItem {
                            plugin: Arc::clone(plugin),
                            prompts: reg.prompts.clone(),
                            slot: reg.slot,
                            content,
                        })
                    })
                })
                .collect()
        };

        let mut slots = craft_agent::prompt::ResolvedSlots::default();
        for item in items {
            let content = match item.content {
                ResolvedContent::Static(s) => s,
                ResolvedContent::Callback(func) => {
                    let result: mlua::Result<LuaValue> = run_detached(&self.lua, async {
                        let thread = self.lua.create_thread(func)?;
                        thread.into_async::<LuaValue>(())?.await
                    })
                    .await;
                    match result {
                        Ok(LuaValue::String(s)) => s.to_string_lossy().to_string(),
                        Ok(LuaValue::Nil) => continue,
                        Ok(_) => {
                            tracing::warn!(plugin = %item.plugin, "prompt hint callback returned non-string");
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(plugin = %item.plugin, error = %e, "prompt hint callback failed");
                            continue;
                        }
                    }
                }
            };

            if content.is_empty() {
                continue;
            }

            let target_prompts: &[PromptId] = match &item.prompts {
                Some(ids) => ids,
                None => PromptId::ALL,
            };

            for &pid in target_prompts {
                if !pid.has_slot(item.slot) {
                    if item.prompts.is_some() {
                        tracing::warn!(
                            plugin = %item.plugin,
                            prompt = ?pid,
                            slot = ?item.slot,
                            "hint targets prompt that lacks this slot"
                        );
                    }
                    continue;
                }
                slots.insert(
                    pid,
                    item.slot,
                    SlotEntry {
                        plugin: Arc::clone(&item.plugin),
                        content: content.clone(),
                    },
                );
            }
        }

        slots
    }

    fn drain_pending(&self) -> Vec<PendingTool> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }

    fn discard_pending(&mut self, tools: Vec<PendingTool>) {
        for t in tools {
            if let Err(e) = self.lua.remove_registry_value(t.handler_key) {
                tracing::warn!(error = %e, "failed to drop lua handler key on rollback");
            }
            if let Some(sk) = t.header_key
                && let Err(e) = self.lua.remove_registry_value(sk)
            {
                tracing::warn!(error = %e, "failed to drop lua header key on rollback");
            }
            if let Some(PermissionScopeSpec::Callback(sk)) = t.permission_scopes
                && let Err(e) = self.lua.remove_registry_value(sk)
            {
                tracing::warn!(error = %e, "failed to drop lua permission_scopes key on rollback");
            }
        }
    }

    fn build_env(
        &self,
        craft: mlua::Table,
        require_root: Option<PathBuf>,
    ) -> Result<mlua::Table, mlua::Error> {
        let env = self.lua.create_table()?;
        env.set("craft", craft)?;

        if require_root.is_some() || !self.bundled.dirs.is_empty() {
            let require_fn = self.create_require_fn(&env, require_root)?;
            env.set("require", require_fn)?;
        }

        let meta = self.lua.create_table()?;
        meta.set("__index", self.lua.globals())?;
        env.set_metatable(Some(meta))?;
        Ok(env)
    }

    /// Bundled dirs go first so plugins can `require()` shared modules
    /// (like `craft.truncate`) without touching the filesystem.
    fn create_require_fn(
        &self,
        env: &mlua::Table,
        require_root: Option<PathBuf>,
    ) -> Result<Function, mlua::Error> {
        let loader = ModuleLoader {
            bundled: self.bundled.clone(),
            lua_dir: require_root.map(|r| r.canonicalize().unwrap_or(r)),
            env: env.clone(),
            codegen: self.codegen_queue.clone(),
            loaded: self.lua.create_table()?,
            loading: self.lua.create_table()?,
        };

        self.lua
            .create_function(move |lua, modname: String| loader.require(lua, &modname))
    }

    /// `plugins.<name>` options only reach a plugin through
    /// `craft.api.register_options`; if the plugin never declared any, every
    /// key the user set is a typo or unsupported, so fail the load loudly.
    fn check_opts_consumed(&self, name: &str, opts: &PluginOpts) -> Result<(), mlua::Error> {
        if opts.is_empty()
            || self
                .lua
                .app_data_ref::<PluginOptionSpecs>()
                .is_some_and(|store| store.contains_key(name))
        {
            return Ok(());
        }
        let keys: Vec<&str> = opts.keys().map(String::as_str).collect();
        Err(mlua::Error::runtime(format!(
            "unknown options in plugins.{name}: {} (this plugin declares no options via craft.api.register_options)",
            keys.join(", ")
        )))
    }

    async fn load_source(
        &mut self,
        name: Arc<str>,
        source: &str,
        plugin_dir: Option<PathBuf>,
        permissions: &PluginPermissions,
        opts: PluginOpts,
        config_store: Option<&ConfigStore>,
    ) -> LoadResult {
        let stale = self.drain_pending();
        debug_assert!(
            stale.is_empty(),
            "leftover pending tools from previous load"
        );
        self.discard_pending(stale);

        let map_err = |e: mlua::Error| PluginError::Lua {
            plugin: name.to_string(),
            source: e,
        };

        let require_root = plugin_dir.as_ref().map(|d| d.join("lua"));
        let craft = create_craft_global(
            &self.lua,
            Arc::clone(&self.pending),
            Arc::clone(&name),
            self.ui_action_tx.clone(),
            permissions,
            Arc::clone(&opts),
            self.embed_tx.clone(),
        )
        .map_err(&map_err)?;

        if let Some(cs) = config_store {
            let setup_fn = crate::api::util::setup::create_setup_fn(&self.lua, Arc::clone(cs))
                .map_err(&map_err)?;
            craft.set("setup", setup_fn).map_err(&map_err)?;
        }

        let env = self.build_env(craft, require_root).map_err(&map_err)?;

        self.drop_plugin_keys(&name);

        let main_fn = self
            .lua
            .load(source)
            .set_name(name.as_ref())
            .set_environment(env)
            .into_function();
        let exec_result = match main_fn {
            Ok(func) => {
                queue_codegen(&self.codegen_queue, &func);
                func.call_async::<()>(()).await
            }
            Err(e) => Err(e),
        };

        let exec_result = exec_result.and_then(|()| self.check_opts_consumed(&name, &opts));
        if let Err(e) = exec_result {
            let stale = self.drain_pending();
            self.discard_pending(stale);
            self.drop_plugin_keys(&name);
            return Err(map_err(e));
        }

        let pending = self.drain_pending();

        let registry_entries: Vec<(Arc<dyn Tool>, ToolSource)> = pending
            .iter()
            .map(|t| {
                let tool: Arc<dyn Tool> = Arc::new(LuaTool {
                    name: Arc::clone(&t.name),
                    description: t.description.clone(),
                    schema: t.schema,
                    audience: t.audience,
                    tx: self.tx.clone(),
                    plugin: Arc::clone(&name),
                    has_header_fn: t.header_key.is_some(),
                    permission_scope_kind: t
                        .permission_scopes
                        .as_ref()
                        .map(PermissionScopeSpec::kind),
                    mutable_path_field: t.mutable_path_field.clone(),
                    timeout: t.timeout,
                    kind: t.kind.clone(),
                });
                (
                    tool,
                    ToolSource::Lua {
                        plugin: Arc::clone(&name),
                    },
                )
            })
            .collect();

        if let Err(e) = self.registry.replace_plugin(&name, registry_entries) {
            self.discard_pending(pending);
            return Err(match e {
                RegistryError::NameConflict { name: n, .. } => PluginError::NameConflict {
                    plugin: name.to_string(),
                    tool: n,
                },
            });
        }

        let keys: HashMap<Arc<str>, ToolKeys> = pending
            .into_iter()
            .map(|t| {
                (
                    t.name,
                    ToolKeys {
                        handler: t.handler_key,
                        header: t.header_key,
                        restore: t.restore_key,
                        permission_scopes: match t.permission_scopes {
                            Some(PermissionScopeSpec::Callback(k)) => Some(k),
                            _ => None,
                        },
                    },
                )
            })
            .collect();
        self.plugins.borrow_mut().insert(name, keys);

        Ok(())
    }

    fn clear_plugin(&mut self, plugin: &str) {
        self.registry.clear_plugin(plugin);
        self.drop_plugin_keys(plugin);
        if let Some(mut store) = self.lua.app_data_mut::<KeymapStore>() {
            let keys = store.clear_plugin(plugin);
            let entries = store.snapshot_entries();
            drop(store);
            for key in keys {
                let _ = self.lua.remove_registry_value(key);
            }
            if let Some(writer) = self.lua.app_data_ref::<KeymapWriter>() {
                writer.publish(entries);
            }
        }
    }

    /// Resolves a plugin callback and converts its json input, warning on
    /// failure. `None` when the tool has no such callback registered.
    fn plugin_fn(
        &self,
        plugin: &str,
        tool: &str,
        callback: &'static str,
        key: impl FnOnce(&ToolKeys) -> Option<&RegistryKey>,
        input: &Value,
    ) -> Option<(Function, LuaValue)> {
        let func = {
            let plugins = self.plugins.borrow();
            let key = key(plugins.get(plugin)?.get(tool)?)?;
            match self.lua.registry_value::<Function>(key) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(plugin, tool, callback, error = %e, "callback registry lookup failed");
                    return None;
                }
            }
        };
        match json_to_lua(&self.lua, input) {
            Ok(v) => Some((func, v)),
            Err(e) => {
                tracing::warn!(plugin, tool, callback, error = %e, "callback input conversion failed");
                None
            }
        }
    }

    /// Async so header fns can yield (highlight, markdown). A sync call would
    /// hit the C-call boundary and silently fall back to the plain name.
    async fn compute_header(&self, plugin: &str, tool: &str, input: Value) -> HeaderResult {
        let Some((func, input_lua)) =
            self.plugin_fn(plugin, tool, "header", |tk| tk.header.as_ref(), &input)
        else {
            return HeaderResult::plain(tool.to_string());
        };

        let result = run_detached(&self.lua, func.call_async::<LuaValue>(input_lua)).await;

        match result {
            Ok(LuaValue::String(s)) => match s.to_str() {
                Ok(s) => HeaderResult::plain(s.to_owned()),
                Err(_) => HeaderResult::plain(tool.to_string()),
            },
            Ok(LuaValue::UserData(ud)) => match ud.borrow::<BufHandle>() {
                Ok(h) => HeaderResult::Styled(h.buf.take()),
                Err(_) => HeaderResult::plain(tool.to_string()),
            },
            Ok(_) => HeaderResult::plain(tool.to_string()),
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "header fn call failed");
                HeaderResult::plain(tool.to_string())
            }
        }
    }

    async fn restore_item(&self, item: RestoreItem) -> Option<RestoreReply> {
        let (func, plugin_name) = {
            let plugins = self.plugins.borrow();
            let (pname, tk) = plugins
                .iter()
                .find_map(|(pname, tools)| tools.get(&*item.tool).map(|tk| (pname.clone(), tk)))?;
            let key = tk.restore.as_ref()?;
            (self.lua.registry_value::<Function>(key).ok()?, pname)
        };
        let input_lua = json_to_lua(&self.lua, &item.input).ok()?;
        let thread = self.lua.create_thread(func).ok()?;

        let (dummy_tx, _) = flume::unbounded();
        let cell = TaskCell::new(
            CancelToken::none(),
            None,
            Some(LiveCtx {
                event_tx: craft_agent::EventSender::new(dummy_tx, 0),
                tool_use_id: item.tool_use_id.clone(),
            }),
            Arc::clone(&self.terminal_backend),
        );

        let ctx_ud = self
            .lua
            .create_userdata(crate::api::util::ctx::RestoreCtx {
                tool_output_lines: item.tool_output_lines,
            })
            .ok()?;
        let inner = thread
            .into_async::<LuaValue>((input_lua, &*item.output, item.is_error, ctx_ud))
            .ok()?;
        let scope = TaskScope::new(&self.lua, cell);
        lock_cell(scope.handle()).inline_spawn = Some(Vec::new());
        let ret = scope
            .scope_future(inner)
            .await
            .inspect_err(
                |e| tracing::warn!(tool = &*item.tool, error = %e, "restore callback failed"),
            )
            .ok()?;
        self.run_inline_tasks(&scope).await;

        if item.expanded {
            let click_key = lock_cell(scope.handle()).click.take();
            if let Some(key) = click_key
                && let Ok(func) = self.lua.registry_value::<Function>(&key)
                && let Ok(data) = self.lua.create_table()
            {
                let _ = data.set("row", 0);
                if let Err(e) = scope.scope_future(func.call_async::<()>(data)).await {
                    tracing::warn!(tool = &*item.tool, error = %e, "click expand failed");
                }
                let _ = self.lua.remove_registry_value(key);
            }
            self.run_inline_tasks(&scope).await;
        }

        drop(scope);

        let mut reply = extract_restore_reply(&ret)?;
        if reply.header.is_none() {
            reply.header = Some(
                self.compute_header(&plugin_name, &item.tool, item.input)
                    .await
                    .into_snapshot(),
            );
        }
        Some(reply)
    }

    async fn run_inline_tasks(&self, scope: &TaskScope) {
        for _ in 0..RESTORE_SPAWN_ROUNDS {
            let tasks = {
                let mut cell = lock_cell(scope.handle());
                match cell.inline_spawn.as_mut() {
                    Some(queue) if !queue.is_empty() => std::mem::take(queue),
                    _ => return,
                }
            };
            for task in tasks {
                if !task.cancel.is_cancelled() {
                    let deadline = Some(Instant::now() + RESTORE_ASYNC_DEADLINE);
                    if let Err(e) = scope
                        .scope_future(run_work_fn(&self.lua, &task.work_fn, deadline))
                        .await
                    {
                        tracing::debug!(error = %e, "restore inline async task failed");
                    }
                }
                self.lua.remove_registry_value(task.work_fn).ok();
            }
        }
    }

    async fn compute_permission_scopes(
        &self,
        plugin: &str,
        tool: &str,
        input: Value,
    ) -> Option<PermissionScopes> {
        let (func, lua_input) = self.plugin_fn(
            plugin,
            tool,
            "permission_scopes",
            |tk| tk.permission_scopes.as_ref(),
            &input,
        )?;
        let result: LuaValue = match run_detached(&self.lua, func.call_async(lua_input)).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(plugin, tool, error = %e, "permission_scopes callback failed");
                return None;
            }
        };
        let table = match result {
            LuaValue::Table(t) => t,
            _ => return None,
        };
        let scopes_table: mlua::Table = table.get("scopes").ok()?;
        let mut scopes = Vec::new();
        for (_, s) in scopes_table.pairs::<usize, String>().flatten() {
            scopes.push(s);
        }
        if scopes.is_empty() {
            return None;
        }
        let force_prompt: bool = table.get("force_prompt").unwrap_or(false);
        Some(PermissionScopes {
            scopes,
            force_prompt,
            context: craft_agent::types::PermissionContext::default(),
        })
    }

    async fn run_init_lua(
        &mut self,
        source: &str,
        source_name: &str,
        plugin_dir: Option<PathBuf>,
    ) -> Result<Option<RawConfig>, PluginError> {
        let config_store: ConfigStore = Arc::new(Mutex::new(None));
        self.load_source(
            Arc::from(source_name),
            source,
            plugin_dir,
            &PluginPermissions::trusted(),
            PluginOpts::default(),
            Some(&config_store),
        )
        .await?;
        Ok(config_store.lock().unwrap().take())
    }
}

fn extract_restore_reply(ret: &LuaValue) -> Option<RestoreReply> {
    let (body, header) = match ret {
        LuaValue::UserData(ud) => {
            let h = ud.borrow::<BufHandle>().ok()?;
            (Some(h.buf.take()), None)
        }
        LuaValue::Table(t) => {
            let body = t.get::<LuaValue>("body").ok().and_then(|v| {
                let ud = v.as_userdata()?;
                let h = ud.borrow::<BufHandle>().ok()?;
                Some(h.buf.take())
            });
            let header = t.get::<LuaValue>("header").ok().and_then(|v| {
                let ud = v.as_userdata()?;
                let h = ud.borrow::<BufHandle>().ok()?;
                Some(h.buf.take())
            });
            (body, header)
        }
        _ => return None,
    };
    Some(RestoreReply { body, header })
}

/// Drains pending events from background jobs and fires their Lua
/// callbacks synchronously. Called before each tool handler starts so
/// that `output_parts` / `bg_jobs` are up-to-date.
fn drain_bg_job_events(lua: &Lua, handle: &TaskHandle) {
    let mut event_buf = Vec::new();
    lock_cell(handle).jobs.drain_events(&mut event_buf);
    for (job_id, event) in event_buf {
        if let Err(e) = deliver_job_event(lua, job_id, &event) {
            tracing::warn!(job_id, error = %strip_traceback(&e), "bg job callback error");
        }
    }
}

/// Nil from the handler means "I went async". Polls job events until
/// `ctx:finish()`, all jobs die, or the deadline (possibly set
/// mid-flight via `ctx:set_deadline`) expires.
async fn dispatch_async(
    lua: &Lua,
    handle: TaskHandle,
    plugin: &str,
    tool: &str,
    finish_rx: flume::Receiver<ToolCallReply>,
) -> ToolCallReply {
    let (cancel, has_jobs) = {
        let cell = lock_cell(&handle);
        (cell.cancel.clone(), !cell.jobs.is_empty())
    };

    if !has_jobs {
        lua.gc_collect().ok();
        tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
        return match finish_rx.try_recv() {
            Ok(reply) => reply,
            _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
        };
    }

    let timed_out = || {
        lock_cell(&handle)
            .deadline
            .get()
            .is_some_and(|d| Instant::now() > d)
    };
    let mut event_buf = Vec::new();

    loop {
        if cancel.is_cancelled() {
            return ToolCallReply::err(CANCELLED_MSG);
        }
        if timed_out() {
            return timeout_reply(&handle, plugin, tool);
        }

        match finish_rx.try_recv() {
            Ok(reply) => return reply,
            Err(flume::TryRecvError::Disconnected) => {
                return ToolCallReply::err(NIL_WITHOUT_FINISH_MSG);
            }
            Err(flume::TryRecvError::Empty) => {}
        }

        lock_cell(&handle).jobs.drain_events(&mut event_buf);

        if event_buf.is_empty() {
            let has_alive = lock_cell(&handle).jobs.has_alive_jobs();
            if !has_alive {
                tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
                return match finish_rx.try_recv() {
                    Ok(reply) => reply,
                    _ => ToolCallReply::err(NIL_WITHOUT_FINISH_MSG),
                };
            }
            tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
            continue;
        }

        for (job_id, event) in event_buf.drain(..) {
            if let Err(e) = deliver_job_event(lua, job_id, &event) {
                return ToolCallReply::err(format!("job callback error: {}", strip_traceback(&e)));
            }
        }
    }
}

/// The error message format is load-bearing: the bash plugin's `restore`
/// parses it to re-render the timeout sentinel on session reload.
fn timeout_reply(handle: &TaskHandle, plugin: &str, tool: &str) -> ToolCallReply {
    let secs = lock_cell(handle).deadline_secs.get().unwrap_or(0);
    let live_buf = resolve_root_buf(handle);
    let qualified = if plugin == tool || plugin.is_empty() {
        tool.to_owned()
    } else {
        format!("{plugin}.{tool}")
    };

    if let Some(ref buf) = live_buf {
        buf.append(SnapshotLine {
            spans: vec![SnapshotSpan {
                text: format!("Timed out after {secs}s"),
                style: SpanStyle::Named("dim".into()),
            }],
        });
    }

    ToolCallReply {
        result: Err(format!("tool {qualified} timed out after {secs}s")),
        snapshot: None,
        header: None,
        live_buf,
        format: LuaOutputFormat::default(),
        annotation: None,
        written_path: None,
        image: None,
    }
}

/// Deadlines work in two layers: the watchdog interrupt catches tight CPU
/// loops, and the dispatch loop catches I/O waits between job events.
#[allow(clippy::too_many_arguments)]
async fn run_tool_call(
    lua: Lua,
    plugin: Arc<str>,
    tool: Arc<str>,
    input: Value,
    mut ctx: Box<LuaCtx>,
    deadline: Option<Instant>,
    live: Option<LiveCtx>,
    plugins: PluginMap,
    shutdown: Arc<AtomicBool>,
) -> ToolCallReply {
    let handler: Function = {
        let plugins_ref = plugins.borrow();
        let Some(keys) = plugins_ref.get(&*plugin) else {
            return ToolCallReply::err(format!("plugin not loaded: {plugin}"));
        };
        let Some(tool_keys) = keys.get(&*tool) else {
            return ToolCallReply::err(format!("tool not found: {tool}"));
        };
        match lua.registry_value(&tool_keys.handler) {
            Ok(f) => f,
            Err(e) => return ToolCallReply::err(strip_traceback(&e)),
        }
    };
    if shutdown.load(Ordering::Acquire) {
        return ToolCallReply::err("plugin host shutting down");
    }

    let (finish_tx, finish_rx) = flume::bounded::<ToolCallReply>(1);
    ctx.finish_tx = Some(finish_tx);
    let cancel = ctx.cancel.clone();

    let input_lua = match json_to_lua(&lua, &input) {
        Ok(v) => v,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };
    let ctx_ud = match lua.create_userdata(*ctx) {
        Ok(u) => u,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };

    let thread = match lua.create_thread(handler) {
        Ok(t) => t,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };
    let scope = TaskScope::new(
        &lua,
        TaskCell::new(cancel, deadline, live, active_terminal_backend(&lua)),
    );
    let handle = Arc::clone(scope.handle());

    drain_bg_job_events(&lua, &handle);

    let async_thread = match thread.into_async::<LuaValue>((input_lua, ctx_ud)) {
        Ok(at) => at,
        Err(e) => return ToolCallReply::err(strip_traceback(&e)),
    };

    let call_future = scope.scope_future(async {
        let handler_result = {
            let deadline = lock_cell(&handle).deadline.get();
            match deadline {
                Some(dl) => {
                    tokio::select! {
                        result = async_thread => result,
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(dl)) => {
                            Err(mlua::Error::runtime("timeout"))
                        }
                    }
                }
                None => async_thread.await,
            }
        };
        match handler_result {
            Ok(LuaValue::Nil) => {
                let live_shared = {
                    let cell = lock_cell(&handle);
                    cell.live.as_ref().and_then(|live| {
                        let shared = cell.bufs.live_buf()?;
                        Some((
                            live.event_tx.clone(),
                            live.tool_use_id.clone(),
                            Arc::clone(shared),
                        ))
                    })
                };
                if let Some((event_tx, tool_use_id, shared)) = live_shared {
                    let _ = event_tx.send(craft_agent::AgentEvent::LiveToolBuf {
                        id: tool_use_id,
                        body: shared,
                    });
                }
                dispatch_async(&lua, Arc::clone(&handle), &plugin, &tool, finish_rx).await
            }
            Ok(val) => ToolCallReply::from_lua_value(&val),
            Err(e) => ToolCallReply::err(strip_traceback(&e)),
        }
    });

    // Both the dispatch loop and the watchdog interrupt read the live
    // deadline from TaskCell. The outer `tool.rs` timeout is the
    // absolute backstop.
    let reply = call_future.await;
    drop(scope);
    reply
}

pub(crate) struct LuaThread {
    pub tx: flume::Sender<Request>,
    pub join: Option<JoinHandle<()>>,
    pub shutdown: Arc<AtomicBool>,
    pub command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: crate::api::util::command::HintReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
}

/// Lua gets its own OS thread so nothing needs a Mutex. `LocalSet::block_on`
/// drives cooperative async, and load/clear requests wait for in-flight tools.
pub fn spawn(
    registry: Arc<ToolRegistry>,
    bundled_dirs: &'static [&'static Dir<'static>],
    embed_tx: Option<crate::api::embed::EmbedChannel>,
    terminal_backend: Arc<dyn TerminalBackend>,
    jit: bool,
) -> Result<LuaThread, PluginError> {
    let (tx, rx) = flume::unbounded::<Request>();
    let tx_clone = tx.clone();
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let shutdown_thread = Arc::clone(&shutdown);
    let (init_tx, init_rx) = flume::bounded::<Result<(), PluginError>>(1);
    let (ui_action_tx, ui_action_rx) = flume::unbounded::<UiAction>();
    let (command_writer, command_reader) = LuaCommandWriter::new();
    let (keymap_writer, keymap_reader) = KeymapWriter::new();
    let (hint_writer, hint_reader) = HintWriter::new();

    let handle = thread::Builder::new()
        .name("craft-lua".to_owned())
        .spawn(move || {
             let mut rt = match LuaRuntime::new(
                 registry,
                 tx_clone,
                 shutdown_thread,
                 bundled_dirs,
                 Some(ui_action_tx),
                 command_writer,
                 keymap_writer,
                 hint_writer,
                  embed_tx,
                  terminal_backend,
                  jit,
              ) {
                Ok(r) => {
                    let _ = init_tx.send(Ok(()));
                    r
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };

            let local = tokio::task::LocalSet::new();
            let tokio_rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = init_tx.send(Err(PluginError::Lua {
                        plugin: String::new(),
                        source: mlua::Error::external(e),
                    }));
                    return;
                }
            };
            let gate = Rc::new(InflightGate::new(rt.lua.clone()));

            // LocalSet::block_on is the idiomatic tokio pattern for !Send types (Lua uses Rc/RefCell).
            // This dedicated thread exists precisely because Lua state cannot be sent across threads.
            local.block_on(&tokio_rt, async {
                let mut codegen_armed = false;
                loop {
                    // Nothing to serve, so spend the lull on native
                    // codegen. One chunk per pass with a yield in
                    // between, so no request or spawned task ever
                    // waits for more than a single chunk.
                    if codegen_armed && rx.is_empty() && rt.codegen_step() {
                        tokio::task::yield_now().await;
                        continue;
                    }
                    let msg = match rx.recv_async().await {
                        Ok(m) => m,
                        Err(_) => break,
                    };
                    match msg {
                        Request::Shutdown => break,
                        Request::WarmJit => codegen_armed = true,
                        Request::LoadSource {
                            name,
                            source,
                            plugin_dir,
                            permissions,
                            opts,
                            reply,
                        } => {
                            drain_barrier(&rt.lua, &gate).await;
                            let res = rt
                                .load_source(
                                    Arc::clone(&name),
                                    &source,
                                    plugin_dir,
                                    &permissions,
                                    opts,
                                    None,
                                )
                                .await;
                            let _ = reply.send(res);
                        }
                        Request::CallTool {
                            plugin,
                            tool,
                            input,
                            ctx,
                            deadline,
                            reply,
                            live,
                        } => {
                            gate.wait_below(MAX_INFLIGHT_TOOLS).await;
                            let lua = rt.lua.clone();
                            let plugins = Rc::clone(&rt.plugins);
                            let shutdown_ref = Arc::clone(&rt.shutdown);
                            let g = Rc::clone(&gate);

                            tokio::task::spawn_local(async move {
                                let _gate_guard = GateGuard::new(&g);
                                let res = run_tool_call(
                                    lua.clone(),
                                    plugin,
                                    tool,
                                    input,
                                    ctx,
                                    deadline,
                                    live,
                                    plugins,
                                    shutdown_ref,
                                )
                                .await;
                                drain_spawn_queue(&lua, &g);
                                let _ = reply.send(res);
                            });
                        }
                        Request::ClearPlugin { plugin, reply } => {
                            drain_barrier(&rt.lua, &gate).await;
                            rt.clear_plugin(&plugin);
                            let _ = reply.send(());
                        }
                        Request::RunCommand {
                            plugin,
                            command,
                            args,
                        } => {
                            let handler_fn =
                                rt.lua.app_data_ref::<CommandHandlerMap>().and_then(|m| {
                                    let entry = m.get(&plugin)?.get(&command)?;
                                    rt.lua.registry_value::<Function>(&entry.handler).ok()
                                });
                            if let Some(func) = handler_fn {
                                let lua = rt.lua.clone();
                                let g = Rc::clone(&gate);
                                tokio::task::spawn_local(async move {
                                    let run = async {
                                        let opts = lua.create_table()?;
                                        opts.set(
                                            "fargs",
                                            lua.create_sequence_from(args.split_whitespace())?,
                                        )?;
                                        opts.set("args", args)?;
                                        let thread = lua.create_thread(func)?;
                                        thread.into_async::<()>(opts)?.await
                                    };
                                    if let Err(e) = run_detached(&lua, run).await {
                                        tracing::warn!(plugin = %plugin, command = %command, error = %e, "command handler failed");
                                    }
                                    drain_spawn_queue(&lua, &g);
                                });
                            }
                        }
                        Request::ComputeHeader {
                            plugin,
                            tool,
                            input,
                            reply,
                        } => {
                            let res = rt.compute_header(&plugin, &tool, input).await;
                            let _ = reply.send(res);
                        }
                        Request::ComputePermissionScopes {
                            plugin,
                            tool,
                            input,
                            reply,
                        } => {
                            let res = rt.compute_permission_scopes(&plugin, &tool, input).await;
                            let _ = reply.send(res);
                        }
                        Request::RunInitLua {
                            source,
                            source_name,
                            plugin_dir,
                            reply,
                        } => {
                            drain_barrier(&rt.lua, &gate).await;
                            let res = rt.run_init_lua(&source, &source_name, plugin_dir).await;
                            let _ = reply.send(res);
                        }
                        Request::CollectPromptSlots { reply } => {
                            let slots = rt.collect_prompt_slots().await;
                            let _ = reply.send(slots);
                        }
                        Request::CollectPluginOptions { reply } => {
                            let _ = reply.send(collect_plugin_options(&rt.lua));
                        }
                    Request::SetTerminalBackend { backend } => {
                        gate.drain().await;
                        rt.lua.set_app_data::<Arc<dyn TerminalBackend>>(backend);
                    }
                    Request::SetSandboxConfig { config } => {
                        if let Some(shared) = rt.lua.app_data_ref::<SharedSandboxConfig>() {
                            shared.store(Arc::new(config));
                        }
                    }
                    Request::RestoreToolAsync { item, event_tx } => {
                        let id = item.tool_use_id.clone();
                        let theme_gen = item.theme_gen;
                        let res = rt.restore_item(item).await;
                        drain_spawn_queue(&rt.lua, &gate);
                        if let Some(reply) = res {
                            reply.emit(&id, theme_gen, &event_tx);
                        }
                    }
                    Request::RestoreComplete { flag } => {
                        flag.store(false, Ordering::Relaxed);
                    }
                    Request::RunHook {
                        event,
                        tool,
                        input,
                        output,
                        is_error,
                        reply,
                    } => {
                        let result = crate::api::hooks::run_hooks_in_vm(
                            &rt.lua,
                            &event,
                            &tool,
                            &input,
                            &output,
                            is_error,
                        )
                        .await;
                        drain_spawn_queue(&rt.lua, &gate);
                        let _ = reply.send(result);
                    }
                    Request::FireAutocmd { event, data } => {
                        let is_turn_end = event == TURN_END_EVENT;
                        let data = json_to_lua(&rt.lua, &data).unwrap_or(LuaValue::Nil);
                        crate::api::autocmd::dispatch(&rt.lua, &event, None, data);
                        drain_spawn_queue(&rt.lua, &gate);
                        if is_turn_end {
                            rt.lua.gc_collect().ok();
                        }
                    }
                    Request::RunKeybindCallback { id } => {
                        let func = rt.lua.app_data_ref::<KeymapStore>().and_then(|store| {
                            let key = store.callback_for_id(id)?;
                            rt.lua.registry_value::<Function>(key).ok()
                        });
                        if let Some(func) = func {
                            let lua = rt.lua.clone();
                            let g = Rc::clone(&gate);
                            tokio::task::spawn_local(async move {
                                if let Err(e) = run_detached(&lua, func.call_async::<()>(())).await {
                                    tracing::warn!(keybind_id = id, error = %e, "keybind callback failed");
                                }
                                drain_spawn_queue(&lua, &g);
                            });
                        }
                    }
                    }
                }
            });
            // Clones of the host (`EventHandle`, `LuaTool`) can still hold
            // a live sender, so dropping the receiver alone does not free
            // queued requests. Drain them so their reply channels drop and
            // no caller blocks on a dead host.
            for _ in rx.drain() {}
        })
        .map_err(|e| PluginError::Io {
            path: PathBuf::from("lua-thread"),
            source: e,
        })?;

    init_rx.recv().map_err(|_| PluginError::Lua {
        plugin: "<init>".to_owned(),
        source: mlua::Error::runtime("lua thread exited before init completed"),
    })??;

    Ok(LuaThread {
        tx,
        join: Some(handle),
        shutdown,
        command_reader,
        keymap_reader,
        hint_reader,
        ui_action_rx,
    })
}

#[cfg(test)]
pub(crate) fn install_live_ctx(lua: &Lua, tool_use_id: &str) {
    let (tx, _rx) = flume::unbounded();
    let cell = TaskCell::new(
        CancelToken::none(),
        None,
        Some(LiveCtx {
            event_tx: craft_agent::EventSender::new(tx, 0),
            tool_use_id: tool_use_id.to_owned(),
        }),
        Arc::new(crate::terminal_backend::LocalTerminal),
    );
    let handle: TaskHandle = Arc::new(Mutex::new(cell));
    lua.set_app_data::<TaskHandle>(handle);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::tool::ToolCallReply;

    fn make_buf_handle(text: &str) -> BufHandle {
        let buf = Arc::new(craft_agent::SharedBuf::new());
        buf.append(SnapshotLine {
            spans: vec![SnapshotSpan {
                text: text.into(),
                style: SpanStyle::Default,
            }],
        });
        BufHandle { id: 0, buf }
    }

    fn test_lua() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(BufferStore::new());
        lua
    }

    #[test]
    fn from_lua_value_plain_string() {
        let lua = test_lua();
        let val = LuaValue::String(lua.create_string("ok").unwrap());
        let reply = ToolCallReply::from_lua_value(&val);
        assert_eq!(reply.result, Ok("ok".to_string()));
        assert!(reply.snapshot.is_none());
        assert!(reply.header.is_none());
    }

    #[test]
    fn from_lua_value_table_with_body_and_header() {
        let lua = test_lua();
        let body_handle = lua.create_userdata(make_buf_handle("body line")).unwrap();
        let hdr_handle = lua.create_userdata(make_buf_handle("hdr line")).unwrap();
        let t = lua.create_table().unwrap();
        t.set("llm_output", "text").unwrap();
        t.set("body", body_handle).unwrap();
        t.set("header", hdr_handle).unwrap();
        let reply = ToolCallReply::from_lua_value(&LuaValue::Table(t));
        assert_eq!(reply.result, Ok("text".to_string()));
        assert_eq!(reply.snapshot.unwrap().first_line_text(), "body line");
        assert_eq!(reply.header.unwrap().first_line_text(), "hdr line");
    }

    #[test]
    fn from_lua_value_missing_llm_output_still_extracts_body() {
        let lua = test_lua();
        let t = lua.create_table().unwrap();
        t.set("body", lua.create_userdata(make_buf_handle("x")).unwrap())
            .unwrap();
        let reply = ToolCallReply::from_lua_value(&LuaValue::Table(t));
        assert!(reply.result.is_err());
        assert!(reply.snapshot.is_some());
    }

    #[test]
    fn task_scope_clears_jobs_and_bufs_on_drop() {
        let lua = Lua::new();
        let scope = TaskScope::new(&lua, task_cell(None));
        let handle = Arc::clone(scope.handle());
        lock_cell(&handle).bufs.create_live();
        assert!(lock_cell(&handle).bufs.live_buf().is_some());
        drop(scope);
        assert!(lock_cell(&handle).bufs.live_buf().is_none());
    }

    fn watchdog_lua(shutdown: bool) -> (Lua, Watchdog) {
        let lua = Lua::new();
        let watchdog = Watchdog::spawn(&lua, Arc::new(AtomicBool::new(shutdown)));
        (lua, watchdog)
    }

    /// Generous vs the ~10ms expected kill; only a broken watchdog gets here.
    const WATCHDOG_TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// `while true do end` only stops if the watchdog kills it, so run it
    /// on a helper thread: a broken watchdog fails the test fast under any
    /// harness (not just nextest's terminate-after) instead of hanging it.
    /// The leaked thread then spins until the test process exits.
    fn hot_loop_expecting_kill(lua: &Lua) -> mlua::Error {
        let f = lua.load("while true do end").into_function().unwrap();
        let (tx, rx) = flume::bounded(1);
        thread::spawn(move || drop(tx.send(f.call::<bool>(()))));
        rx.recv_timeout(WATCHDOG_TEST_TIMEOUT)
            .expect("watchdog never killed the hot loop")
            .unwrap_err()
    }

    /// Runs long enough (50ms) to guarantee several watchdog pokes.
    fn timed_loop(lua: &Lua) -> Function {
        lua.load("local t = os.clock() while os.clock() - t < 0.05 do end return true")
            .into_function()
            .unwrap()
    }

    fn cancelled_handle() -> TaskHandle {
        let (trigger, token) = CancelToken::new();
        trigger.cancel();
        Arc::new(Mutex::new(TaskCell::new(
            token,
            None,
            None,
            Arc::new(crate::terminal_backend::LocalTerminal),
        )))
    }

    #[test]
    fn stale_cancelled_handle_aborts_callback_without_fresh_scope() {
        let (lua, _watchdog) = watchdog_lua(false);
        lua.set_app_data::<TaskHandle>(cancelled_handle());
        let err = hot_loop_expecting_kill(&lua);
        assert!(err.to_string().contains(INTERRUPT_CANCELLED_MSG));
    }

    #[test]
    fn fresh_task_scope_shields_callback_from_stale_cancelled_handle() {
        let (lua, _watchdog) = watchdog_lua(false);
        lua.set_app_data::<TaskHandle>(cancelled_handle());

        let scope = TaskScope::detached(&lua);
        let result = timed_loop(&lua).call::<bool>(());
        drop(scope);

        assert!(result.unwrap());
    }

    #[test]
    fn shutdown_flag_aborts_callback_even_with_fresh_scope() {
        let (lua, _watchdog) = watchdog_lua(true);

        let scope = TaskScope::detached(&lua);
        let err = hot_loop_expecting_kill(&lua);
        drop(scope);

        assert!(err.to_string().contains(INTERRUPT_SHUTDOWN_MSG));
    }

    #[test]
    fn jit_busy_loop_killed_at_deadline() {
        let (lua, _watchdog) = watchdog_lua(false);
        install_compiler(&lua, true);

        let deadline = Instant::now() + Duration::from_millis(20);
        let cell = TaskCell::new(
            CancelToken::none(),
            Some(deadline),
            None,
            Arc::new(crate::terminal_backend::LocalTerminal),
        );
        lua.set_app_data::<TaskHandle>(Arc::new(Mutex::new(cell)));

        let err = hot_loop_expecting_kill(&lua);
        assert!(err.to_string().contains(INTERRUPT_DEADLINE_MSG));
    }

    fn task_cell(live: Option<LiveCtx>) -> TaskCell {
        TaskCell::new(
            CancelToken::none(),
            None,
            live,
            Arc::new(crate::terminal_backend::LocalTerminal),
        )
    }

    #[test]
    fn with_live_ctx_follows_task_live_field() {
        let lua = Lua::new();

        let (tx, _rx) = flume::unbounded();
        let with_live = task_cell(Some(LiveCtx {
            event_tx: craft_agent::EventSender::new(tx, 0),
            tool_use_id: "tool_abc".into(),
        }));

        let scope = TaskScope::new(&lua, task_cell(None));
        assert!(with_live_ctx(&lua, |_| ()).is_none());
        drop(scope);

        let _scope = TaskScope::new(&lua, with_live);
        assert_eq!(
            with_live_ctx(&lua, |ctx| ctx.tool_use_id.clone()).unwrap(),
            "tool_abc"
        );
    }

    fn gate() -> InflightGate {
        InflightGate::new(Lua::new())
    }

    #[tokio::test]
    async fn inflight_gate_drain_requires_all_decrements() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let g = Rc::new(gate());
                g.increment();
                g.increment();
                let g2 = Rc::clone(&g);
                let waiter = tokio::task::spawn_local(async move { g2.drain().await });
                tokio::task::yield_now().await;
                assert!(!waiter.is_finished());
                g.decrement();
                tokio::task::yield_now().await;
                assert!(!waiter.is_finished());
                g.decrement();
                let _ = waiter.await;
            })
            .await;
    }

    #[tokio::test]
    async fn inflight_gate_blocks_at_max_capacity() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let g = Rc::new(gate());
                for _ in 0..MAX_INFLIGHT_TOOLS {
                    g.increment();
                }
                let g2 = Rc::clone(&g);
                let waiter =
                    tokio::task::spawn_local(
                        async move { g2.wait_below(MAX_INFLIGHT_TOOLS).await },
                    );
                tokio::task::yield_now().await;
                assert!(!waiter.is_finished());
                g.decrement();
                let _ = waiter.await;
            })
            .await;
    }

    #[test]
    fn extract_restore_reply_userdata_returns_body_only() {
        let lua = test_lua();
        let handle = make_buf_handle("restored line");
        let ud = lua.create_userdata(handle).unwrap();
        let val = LuaValue::UserData(ud);
        let reply = extract_restore_reply(&val).expect("should extract from userdata");
        assert_eq!(reply.body.unwrap().first_line_text(), "restored line");
        assert!(reply.header.is_none());
    }

    #[test]
    fn extract_restore_reply_table_with_body_and_header() {
        let lua = test_lua();
        let body = lua.create_userdata(make_buf_handle("body")).unwrap();
        let header = lua.create_userdata(make_buf_handle("header")).unwrap();
        let t = lua.create_table().unwrap();
        t.set("body", body).unwrap();
        t.set("header", header).unwrap();
        let val = LuaValue::Table(t);
        let reply = extract_restore_reply(&val).unwrap();
        assert_eq!(reply.body.unwrap().first_line_text(), "body");
        assert_eq!(reply.header.unwrap().first_line_text(), "header");
    }

    const SPAWN_QUEUE_NOT_INIT: &str = "spawn queue not initialized";

    fn enqueue_test_lua() -> Lua {
        let lua = Lua::new();
        lua.set_app_data(SpawnQueue::new(Vec::new()));
        lua
    }

    fn enqueue_dummy(lua: &Lua) -> RegistryKey {
        let func = lua.create_function(|_, _: ()| Ok(())).unwrap();
        lua.create_registry_value(func).unwrap()
    }

    fn set_active(lua: &Lua, cell: TaskCell) -> TaskScope {
        TaskScope::new(lua, cell)
    }

    #[test]
    fn gate_guard_tracks_count_via_raii() {
        let g = gate();
        let g1 = GateGuard::new(&g);
        let g2 = GateGuard::new(&g);
        assert_eq!(g.count.get(), 2);
        drop(g1);
        assert_eq!(g.count.get(), 1);
        drop(g2);
        assert_eq!(g.count.get(), 0);
    }

    #[test]
    fn enqueue_async_task_missing_spawn_queue_errors() {
        let lua = Lua::new();
        let key = lua
            .create_registry_value(lua.create_function(|_, _: ()| Ok(())).unwrap())
            .unwrap();
        let err = enqueue_async_task(&lua, key).unwrap_err();
        assert!(err.to_string().contains(SPAWN_QUEUE_NOT_INIT));
    }

    #[test]
    fn enqueue_async_task_routes_to_inline_spawn_when_set() {
        let lua = enqueue_test_lua();
        let scope = set_active(
            &lua,
            TaskCell::new(
                CancelToken::none(),
                None,
                None,
                Arc::new(crate::terminal_backend::LocalTerminal),
            ),
        );
        lock_cell(scope.handle()).inline_spawn = Some(Vec::new());

        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        assert!(
            lua.app_data_ref::<SpawnQueue>()
                .unwrap()
                .borrow()
                .is_empty(),
            "task must not reach the global queue"
        );
        let cell = lock_cell(scope.handle());
        assert_eq!(cell.inline_spawn.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn enqueue_async_task_works_without_task_ctx() {
        let lua = enqueue_test_lua();
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = &queue.borrow()[0];
        assert!(queued.live_ctx.is_none());
        assert!(queued.owner.is_none());
    }

    #[test]
    fn enqueue_async_task_inherits_cancel_token() {
        let lua = enqueue_test_lua();
        let (trigger, token) = CancelToken::new();
        let _h = set_active(
            &lua,
            TaskCell::new(
                token,
                None,
                None,
                Arc::new(crate::terminal_backend::LocalTerminal),
            ),
        );
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let queued = &queue.borrow()[0];
        assert!(!queued.cancel.is_cancelled());
        trigger.cancel();
        assert!(
            queued.cancel.is_cancelled(),
            "async task should inherit parent cancel"
        );
    }

    #[test]
    fn enqueue_async_task_uses_fresh_deadline_regardless_of_parent() {
        let lua = enqueue_test_lua();
        let parent_deadline = Instant::now() - Duration::from_secs(10);
        let _h = set_active(
            &lua,
            TaskCell::new(
                CancelToken::none(),
                Some(parent_deadline),
                None,
                Arc::new(crate::terminal_backend::LocalTerminal),
            ),
        );

        let before = Instant::now();
        enqueue_async_task(&lua, enqueue_dummy(&lua)).unwrap();

        let queue = lua.app_data_ref::<SpawnQueue>().unwrap();
        let task_deadline = queue.borrow()[0].deadline.unwrap();
        assert!(
            task_deadline > before,
            "async task should get a fresh deadline, not inherit expired parent"
        );
    }

    fn push_pending_task(lua: &Lua, cancel: CancelToken, deadline: Option<Instant>) {
        let work_fn = enqueue_dummy(lua);
        lua.app_data_ref::<SpawnQueue>()
            .unwrap()
            .borrow_mut()
            .push(PendingAsyncTask {
                work_fn,
                cancel,
                deadline,
                live_ctx: None,
                owner: None,
            });
    }

    #[tokio::test]
    async fn drain_spawn_queue_skips_cancelled_tasks() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = enqueue_test_lua();
                let (trigger, token) = CancelToken::new();
                trigger.cancel();
                push_pending_task(&lua, token, None);

                let g = Rc::new(gate());
                drain_spawn_queue(&lua, &g);
                tokio::task::yield_now().await;
                assert_eq!(g.count.get(), 0);
            })
            .await;
    }

    #[tokio::test]
    async fn drain_spawn_queue_runs_and_decrements_gate() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = enqueue_test_lua();
                push_pending_task(
                    &lua,
                    CancelToken::none(),
                    Some(Instant::now() + Duration::from_secs(5)),
                );

                let g = Rc::new(gate());
                drain_spawn_queue(&lua, &g);

                for _ in 0..10 {
                    tokio::task::yield_now().await;
                    if g.count.get() == 0 {
                        return;
                    }
                }
                panic!("gate count never reached 0 after draining");
            })
            .await;
    }
}
