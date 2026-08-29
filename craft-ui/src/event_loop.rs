//! Multi-session supervisor: every session owns an `App` + `AgentHandles`
//! and keeps draining agent events while backgrounded; only the focused
//! session renders and receives input. `SpawnCx` carries the shared resources
//! needed to spawn session runtimes at any point.
//!
//! Terminal input arrives on a channel (see [`crate::input::InputReader`]), so
//! the loop waits on every event source at once via a `flume::Selector` and
//! wakes the moment a plugin action, agent event, or keypress arrives instead
//! of sleeping in `event::poll`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;
use color_eyre::eyre::eyre;

use craft_agent::command::CustomCommand;
use craft_agent::permissions::PermissionManager;
use craft_agent::{
    AgentConfig, AgentEvent, CancelToken, Envelope, McpCommand, McpConfigErrors, McpHandle,
};
use craft_config::UiConfig;
use craft_lua::{
    EventHandle, HintReader, KeymapReader, LuaCommandReader, ModelRequest, SessionRequest,
    TaskRequest, UiAction, UiReply,
};
use craft_providers::Timeouts;
use craft_providers::provider::{Provider, fetch_all_models, from_model};
use craft_providers::{Message, Model};
use craft_storage::StateDir;
use craft_storage::id::{CraftId, CraftIdParseError};
use crossterm::event::{
    Event, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use serde_json::json;
use tracing::warn;

use crate::AppSession;
use crate::agent::{AgentCommand, AgentHandles, ModelSlot, shared_queue::QueueItem};
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::tasks::{TaskStatus, diff_task_states};
use crate::app::{App, Msg, Notification, QueuedMessage, SubmitOutcome, turn_response};
use crate::color_compat;
use crate::components::input::Submission;
use crate::components::usage_modal::UsageFetchState;
use crate::components::{Action, ExitRequest, Status};
use crate::input::InputReader;
use crate::repaint::{Dirty, IDLE_POLL};

use crate::storage_writer::StorageWriter;
use crate::terminal;
use crate::watch;

/// Max events handled per frame so a flood cannot starve rendering.
const DRAIN_BUDGET: usize = 256;
const AGENT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_FOCUSED_ERR: &str = "cannot delete the focused session";
const MODEL_POLICY_ERR: &str = "Model is not allowed by policy";
const INVALID_MODEL_ERR: &str = "Invalid model";
const PROVIDER_INIT_ERR: &str = "Failed to create provider";
const NOT_LIVE_ERR: &str = "session not live";

/// Tabs carry their in-memory sessions so `/reload` reopens them without a
/// disk round-trip; `session_has_content` tells which ones were saved.
pub(crate) struct ShutdownReport {
    pub exit: ExitRequest,
    pub tabs: Vec<AppSession>,
    pub focused: usize,
}

impl ShutdownReport {
    /// Focused tab's saved id, for the exit/resume hint. `None` if the focused
    /// tab was empty (no content) and so was never persisted.
    pub fn session_id(&self) -> Option<CraftId> {
        self.tabs
            .get(self.focused)
            .filter(|s| crate::app::session_has_content(s))
            .map(|s| s.id.id())
    }

    pub fn exit_request(&self) -> ExitRequest {
        self.exit
    }

    pub fn tabs(&self) -> &[AppSession] {
        &self.tabs
    }

    pub fn focused(&self) -> usize {
        self.focused
    }
}

type RunResult = Result<ShutdownReport>;

pub struct EventLoopParams {
    pub model: Model,
    pub needs_login: bool,
    pub commands: Vec<CustomCommand>,
    pub sessions: Vec<AppSession>,
    pub focused: usize,
    pub startup_warnings: Vec<String>,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub compression: craft_config::CompressionConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    pub lua_command_reader: LuaCommandReader,
    pub keymap_reader: KeymapReader,
    pub hint_reader: HintReader,
    pub ui_action_rx: flume::Receiver<UiAction>,
    pub lua_event_handle: EventHandle,
    pub provider: Arc<dyn Provider>,
    pub mcp_handle: Option<McpHandle>,
    pub mcp_config_errors: McpConfigErrors,
    pub embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>,
    pub watch_enabled: bool,
    pub model_policy: Arc<craft_config::ModelPolicy>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Working,
    NeedsInput,
    Idle,
}

impl SessionStatus {
    fn of(app: &App) -> Self {
        if app.awaiting_input() {
            Self::NeedsInput
        } else if app.status == Status::Streaming {
            Self::Working
        } else {
            Self::Idle
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsInput => "needs_input",
            Self::Idle => "idle",
        }
    }
}

fn prepend_preamble(preamble: &mut Vec<Message>, mut leading: Vec<Message>) {
    leading.append(preamble);
    *preamble = leading;
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingCompletion {
    WaitingForQueueDrain(Notification),
    Due(Notification),
}

#[derive(Default)]
struct RunNotificationState {
    response_candidate: Option<String>,
    pending_completion: Option<PendingCompletion>,
    last_attention: Option<Notification>,
}

impl RunNotificationState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn on_queue_item_consumed(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
    }

    fn on_turn_complete(&mut self, message: &Message) {
        self.response_candidate = turn_response(message);
    }

    fn on_done(&mut self, event: &AgentEvent) {
        let notification = match event {
            AgentEvent::Done { .. } => Notification::TurnComplete {
                response: self.response_candidate.take(),
            },
            AgentEvent::Error { .. } => {
                self.response_candidate = None;
                Notification::error_completion()
            }
            _ => return,
        };
        self.pending_completion = Some(PendingCompletion::WaitingForQueueDrain(notification));
    }

    fn on_drain(&mut self) {
        self.pending_completion = match self.pending_completion.take() {
            Some(PendingCompletion::WaitingForQueueDrain(notification)) => {
                Some(PendingCompletion::Due(notification))
            }
            pending => pending,
        };
    }

    fn on_manual_exit(&mut self) {
        self.pending_completion = None;
    }

    /// True between `Done`/`Error` and the run's `QueueDrained`. An exit must
    /// not fire in that window: a queued follow-up may still start a new run.
    fn waiting_for_drain(&self) -> bool {
        matches!(
            self.pending_completion,
            Some(PendingCompletion::WaitingForQueueDrain(_))
        )
    }

    fn reconcile(
        &mut self,
        attention: Option<Notification>,
        status: SessionStatus,
        queue_empty: bool,
        terminal_focused: bool,
    ) -> Option<Notification> {
        let settled = attention.is_none() && status == SessionStatus::Idle && queue_empty;
        let prompt = (attention != self.last_attention)
            .then(|| attention.clone())
            .flatten();
        self.last_attention = attention;

        // A due completion is decided on its first reconcile: fire if the
        // session settled, otherwise drop it for good.
        let completion = match self.pending_completion.take() {
            Some(PendingCompletion::Due(notification)) => settled.then_some(notification),
            waiting => {
                self.pending_completion = waiting;
                None
            }
        };
        (!terminal_focused).then(|| prompt.or(completion)).flatten()
    }
}

fn is_current_top_level(current_run_id: u64, envelope: &Envelope) -> bool {
    envelope.run_id == current_run_id && envelope.subagent.is_none()
}

fn select_notification(
    selected: Option<Notification>,
    candidate: Option<Notification>,
) -> Option<Notification> {
    match (selected, candidate) {
        (Some(current), Some(candidate)) if candidate.is_urgent() && !current.is_urgent() => {
            Some(candidate)
        }
        (selected @ Some(_), _) => selected,
        (None, candidate) => candidate,
    }
}

#[cfg(not(windows))]
fn terminal_focus_event(event: &Event) -> Option<bool> {
    match event {
        Event::FocusGained => Some(true),
        Event::FocusLost => Some(false),
        _ => None,
    }
}

#[cfg(windows)]
fn terminal_focus_event(_event: &Event) -> Option<bool> {
    None
}

#[cfg(not(windows))]
fn terminal_input_proves_focus(event: &Event) -> bool {
    match event {
        Event::Key(key) => key.kind == KeyEventKind::Press,
        Event::Paste(_) | Event::Mouse(_) => true,
        _ => false,
    }
}

#[cfg(windows)]
fn terminal_input_proves_focus(_event: &Event) -> bool {
    false
}

fn parse_session_id(id: &str) -> Result<CraftId, String> {
    id.parse().map_err(|e: CraftIdParseError| e.to_string())
}

struct SessionRuntime {
    app: App,
    handles: AgentHandles,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    context_window_overrides: Arc<ArcSwap<HashMap<String, u32>>>,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    warn_rx: flume::Receiver<String>,
    /// Per-session action channel. The watch watcher posts `Action::WatchPrompt`
    /// here so it routes to the owning session, not the focused one. Background
    /// model/provider tasks also target this.
    action_tx: flume::Sender<Action>,
    action_rx: flume::Receiver<Action>,
    watch_handle: Option<watch::WatcherHandle>,
    last_status: SessionStatus,
    /// Keyed by task id, never by position: a session reset reuses positions,
    /// so a new task would inherit the old one's status.
    last_tasks: Vec<(Arc<str>, TaskStatus)>,
    notifications: RunNotificationState,
}

impl SessionRuntime {
    fn id(&self) -> CraftId {
        self.app.state.session.id.id()
    }

    /// New work cancels an `exit_on_done` exit still waiting on its drain.
    fn reset_run_notifications(&mut self) {
        if self.notifications.waiting_for_drain() {
            self.app.clear_exit_request();
        }
        self.notifications.reset();
    }

    /// A wake may only start a background run when the session is fully
    /// quiescent. Idle status alone is not enough: restored queue items start
    /// runs without `start_run` (the app only learns of them via
    /// `QueueItemConsumed`), and `start_run` destroys text held for recovery
    /// after an agent error.
    fn quiescent(&self) -> bool {
        SessionStatus::of(&self.app) == SessionStatus::Idle
            && self.handles.queue.is_empty()
            && !self.app.holds_recovery_text()
    }

    fn spawn_watch_watcher(&mut self, action_tx: &flume::Sender<Action>) {
        if self.watch_handle.is_some() {
            return;
        }
        let cwd = PathBuf::from(self.app.state.session.cwd.clone());
        if let Some(handle) = watch::spawn_watcher(cwd, action_tx.clone()) {
            self.watch_handle = Some(handle);
        }
    }

    fn stop_watch_watcher(&mut self) {
        if let Some(handle) = self.watch_handle.take() {
            handle.stop();
        }
    }
}

/// Everything needed to bring up a new session runtime after startup.
struct SpawnCx {
    storage: StateDir,
    config: AgentConfig,
    compression: craft_config::CompressionConfig,
    ui_config: UiConfig,
    input_history_size: usize,
    permissions: Arc<PermissionManager>,
    timeouts: Timeouts,
    custom_commands: Arc<[CustomCommand]>,
    lua_command_reader: LuaCommandReader,
    keymap_reader: KeymapReader,
    hint_reader: HintReader,
    lua_event_handle: EventHandle,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    /// Shared across runtimes: model resolution is a global concept, and the
    /// background model-fetch task keeps it warm.
    model_slot: Arc<ArcSwap<ModelSlot>>,
    available_models: Arc<ArcSwapOption<Vec<String>>>,
    storage_writer: Arc<StorageWriter>,
    embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>,
    watch_enabled: bool,
    flow_store: Arc<craft_storage::flow::FlowStore>,
    model_policy: Arc<craft_config::ModelPolicy>,
}

impl SpawnCx {
    fn spawn_runtime(&self, session: AppSession) -> SessionRuntime {
        let resumed = !session.messages().is_empty();
        let permissions = Arc::new(self.permissions.fork());
        let context_window_overrides = Arc::new(ArcSwap::from_pointee(
            session.meta.context_window_overrides.clone(),
        ));
        let model_slot = Arc::clone(&self.model_slot);
        let handles = AgentHandles::spawn(
            &model_slot,
            session.messages().to_vec(),
            self.config.clone(),
            self.ui_config.tool_output_lines,
            &permissions,
            Some(session.id.clone()),
            self.timeouts,
            self.lua_event_handle.clone(),
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            self.compression.clone(),
            Arc::clone(&self.model_policy),
            Arc::clone(&self.flow_store),
            // Embeds are focus-scoped and stateless; one shared consumer task
            // drains the single embed_rx (see `EventLoop::new`). Respawns and
            // new runtimes therefore pass `None` so only the first consumer
            // owns the receiver.
            None,
        );
        let mut app = App::new(
            &model_slot.load().model,
            session,
            self.storage.clone(),
            Arc::clone(&self.available_models),
            handles.mcp_reader(),
            handles.mcp_config_errors.clone(),
            self.lua_command_reader.clone(),
            self.keymap_reader.clone(),
            self.hint_reader.clone(),
            Arc::clone(&self.storage_writer),
            self.ui_config.clone(),
            self.input_history_size,
            Arc::clone(&permissions),
            Arc::clone(&self.custom_commands),
            self.lua_event_handle.clone(),
            Arc::clone(&self.model_policy),
            self.config.repomap.enabled,
            self.watch_enabled,
        );
        handles.apply_to_app(&mut app);
        app.propagate_lua_handles();
        if resumed {
            app.restore_resumed_session();
        }
        let (warn_tx, warn_rx) = flume::unbounded::<String>();
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        let (action_tx, action_rx) = flume::unbounded::<Action>();
        let mut rt = SessionRuntime {
            app,
            handles,
            model_slot,
            context_window_overrides,
            shell_tx,
            shell_rx,
            warn_rx,
            action_tx,
            action_rx,
            watch_handle: None,
            last_status: SessionStatus::Idle,
            last_tasks: Vec::new(),
            notifications: RunNotificationState::default(),
        };
        rt.app.warn_tx = Some(warn_tx);
        let action_tx = rt.action_tx.clone();
        if self.watch_enabled {
            rt.spawn_watch_watcher(&action_tx);
        }
        rt
    }
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    sessions: Vec<SessionRuntime>,
    focused: usize,
    last_focused: Option<CraftId>,
    terminal_focused: bool,
    notifier: Option<terminal::TerminalNotifier>,
    ctx: SpawnCx,
    input: InputReader,
    /// One consumer task for the shared embed channel; embeds are stateless
    /// (`EmbeddingService::embed(&text)`) so focus does not change the result.
    _embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    ui_action_rx: flume::Receiver<UiAction>,
    _model_fetch_task: tokio::task::JoinHandle<()>,
}

/// One item from any of the event loop's sources; `None` from `next_wake`
/// means the wait timed out (animation/idle tick).
enum Wake {
    Input(Event),
    InputGone,
    Ui(UiAction),
    Agent(usize, Box<Envelope>),
    Shell(usize, ShellEvent),
    /// `None` = global channel (model-fetch discovery), routed to the focused
    /// session. `Some(i)` = per-runtime `warn_rx`, routed to the owning session.
    Warn(Option<usize>, String),
    Flow(usize, craft_agent::FlowProgress),
    Action(usize, Action),
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    warn_tx: flume::Sender<String>,
    task: tokio::task::JoinHandle<()>,
}

const WIKI_MAX_SUMMARY_BYTES: usize = 24 * 1024;

async fn run_wiki_ingest(
    cwd: &std::path::Path,
    source_path: &std::path::Path,
    provider: &dyn Provider,
    model: &Model,
) -> String {
    let content = match std::fs::read_to_string(source_path) {
        Ok(c) => c,
        Err(e) => return format!("wiki ingest failed: read {source_path:?}: {e}"),
    };
    let title = craft_storage::wiki::first_h1_title(&content).unwrap_or_else(|| {
        source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("untitled")
            .to_string()
    });
    let slug = craft_storage::wiki::slugify(&title);
    let excerpt = craft_storage::wiki::extract_excerpt(&content);

    let truncated: String = if content.len() > WIKI_MAX_SUMMARY_BYTES {
        let cut = content[..WIKI_MAX_SUMMARY_BYTES]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(WIKI_MAX_SUMMARY_BYTES);
        content[..cut].to_string()
    } else {
        content.clone()
    };
    let summary = match craft_agent::wiki::summarize(provider, model, &truncated, None).await {
        Ok(s) => s,
        Err(e) => return format!("wiki ingest failed: summarize: {e}"),
    };

    let store = match craft_storage::wiki::WikiStore::open(cwd) {
        Ok(s) => s,
        Err(e) => return format!("wiki ingest failed: open store: {e}"),
    };
    let ingested_at = jiff::Zoned::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let note = craft_storage::wiki::SourceNote {
        slug: slug.clone(),
        title: title.clone(),
        source_path: source_path.display().to_string(),
        ingested_at,
        summary,
        excerpt,
        body: content,
        linked_pages: Vec::new(),
    };
    if let Err(e) = store.write_source_note(&note) {
        return format!("wiki ingest failed: write note: {e}");
    }
    let log_message = format!("Ingested `{}` as `{slug}`.", source_path.display());
    if let Err(e) = store.append_log(&note.ingested_at, "Creation", &log_message) {
        return format!("wiki ingest failed: append log: {e}");
    }
    if let Err(e) = store.rebuild_index() {
        return format!("wiki ingest failed: rebuild index: {e}");
    }
    format!("wiki: ingested `{}` as `{slug}`", source_path.display())
}

fn merge_batch(
    available: &Arc<ArcSwapOption<Vec<String>>>,
    batch: craft_providers::provider::ModelBatch,
    warn_tx: &flume::Sender<String>,
) {
    for w in batch.warnings {
        let _ = warn_tx.try_send(w);
    }
    if batch.models.is_empty() {
        return;
    }
    let mut merged = available.load().as_deref().cloned().unwrap_or_default();
    for spec in &batch.models {
        if !merged.contains(spec) {
            merged.push(spec.clone());
        }
    }
    available.store(Some(Arc::new(merged)));
}

fn spawn_model_fetch(
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    timeouts: Timeouts,
    policy: Arc<craft_config::ModelPolicy>,
) -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let warn_tx_bg = warn_tx.clone();
    let model_slot = Arc::clone(model_slot);
    let task = tokio::spawn(async move {
        let warn_tx = warn_tx_bg;
        let done = Box::new(move || {
            let spec = model_slot.load().model.spec();
            let model_slot = Arc::clone(&model_slot);
            tokio::spawn(async move {
                let mut resolved = match Model::from_spec(&spec) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(spec = %spec, error = %e, "failed to resolve model after discovery");
                        return;
                    }
                };
                let provider = match from_model(&mut resolved, timeouts)
                    .await
                    .map(|p| Arc::from(p) as Arc<dyn Provider>)
                {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(spec = %spec, error = %e, "failed to create provider after discovery");
                        return;
                    }
                };
                // Store the BASE resolved model; per-session overrides are
                // applied by drain_channels / ProviderReady when each runtime
                // consumes the shared slot, so one session's override never
                // leaks into another.
                model_slot.store(Arc::new(ModelSlot {
                    model: resolved,
                    provider,
                }));
            });
        });
        fetch_all_models(
            &policy,
            |batch| merge_batch(&bg, batch, &warn_tx),
            Some(done),
        )
        .await;
    });
    BackgroundModels {
        available,
        warn_rx,
        warn_tx,
        task,
    }
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            model,
            needs_login,
            commands,
            sessions,
            focused,
            mut startup_warnings,
            storage,
            config,
            compression,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            exit_on_done,
            lua_command_reader,
            keymap_reader,
            hint_reader,
            ui_action_rx,
            lua_event_handle,
            provider,
            mcp_handle,
            mcp_config_errors,
            embed_rx,
            watch_enabled,
            model_policy,
        } = params;

        if let Some(ref name) = ui_config.theme {
            match crate::theme::load_by_name(name) {
                Ok(theme) => {
                    crate::theme::set_current_name(name);
                    crate::theme::set(theme);
                }
                Err(e) => startup_warnings.push(format!("config ui.theme: {e}")),
            }
        }

        static PROCESS_WARMUP: std::sync::Once = std::sync::Once::new();
        PROCESS_WARMUP.call_once(|| {
            std::thread::spawn(crate::highlight::warmup);
            crate::update::spawn_check();
        });

        let storage_writer = Arc::new(StorageWriter::new(storage.clone(), None)?);
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let bg = spawn_model_fetch(&model_slot, timeouts, Arc::clone(&model_policy));
        let flow_store = Arc::new(
            craft_storage::flow::FlowStore::new(&storage).unwrap_or_else(|_| {
                craft_storage::flow::FlowStore::from_root(storage.path().join("projects"))
            }),
        );

        let notifier = terminal::TerminalNotifier::new(ui_config.notifications);
        let mut ctx = SpawnCx {
            storage,
            config,
            compression,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            custom_commands: Arc::from(commands),
            lua_command_reader,
            keymap_reader,
            hint_reader,
            lua_event_handle,
            mcp_handle,
            mcp_config_errors,
            model_slot: Arc::clone(&model_slot),
            available_models: Arc::clone(&bg.available),
            storage_writer,
            embed_rx,
            watch_enabled,
            flow_store,
            model_policy,
        };

        let mut runtimes: Vec<SessionRuntime> =
            sessions.into_iter().map(|s| ctx.spawn_runtime(s)).collect();
        if runtimes.is_empty() {
            return Err(eyre!("event loop needs at least one session"));
        }
        let focused = focused.min(runtimes.len() - 1);
        let focused_app = &mut runtimes[focused].app;
        focused_app.exit_on_done = exit_on_done;
        if needs_login {
            focused_app.login_picker.open(focused_app.storage.clone());
        }
        if !ctx.mcp_config_errors.is_empty() {
            focused_app.flash(format!("MCP config error: {}", ctx.mcp_config_errors));
        }
        for w in startup_warnings {
            focused_app.flash(w);
        }

        // Single embed consumer: the EmbeddingService is stateless, so one task
        // drains the shared receiver no matter which session's plugin asked.
        let embed_rx_owned = ctx.embed_rx.take();
        if let Some(rx) = embed_rx_owned.as_ref() {
            let service = craft_agent::EmbeddingService::new();
            let rx = rx.clone();
            tokio::spawn(async move {
                while let Ok((text, reply_tx)) = rx.recv_async().await {
                    let result = service.embed(&text).await.map_err(|e| e.to_string());
                    let _ = reply_tx.send(result);
                }
            });
        }

        Ok(Self {
            terminal,
            sessions: runtimes,
            focused,
            last_focused: None,
            terminal_focused: false,
            notifier,
            ctx,
            input: InputReader::spawn(),
            _embed_rx: embed_rx_owned,
            warn_rx: bg.warn_rx,
            warn_tx: bg.warn_tx,
            ui_action_rx,
            _model_fetch_task: bg.task,
        })
    }

    fn focused_app(&mut self) -> &mut App {
        &mut self.sessions[self.focused].app
    }

    pub(crate) fn run(mut self, initial_prompt: Option<String>) -> RunResult {
        if let Some(prompt) = initial_prompt {
            let sub = Submission {
                text: prompt,
                images: Vec::new(),
            };
            let actions = self.focused_app().handle_submit(sub);
            self.dispatch(self.focused, actions);
        }
        // The first frame always paints. After that only a poller, an event or
        // an animation tick owes another.
        let mut dirty = Dirty::YES;
        let result: Result<()> = loop {
            dirty |= self.tick();
            match self.drain_channels() {
                Ok(d) => dirty |= d,
                Err(e) => break Err(e),
            }
            self.checkpoint_all();
            if dirty.take() {
                crate::terminal::begin_synchronized_output();
                if let Err(e) = self.terminal.draw(|f| {
                    self.sessions[self.focused].app.view(f);
                    color_compat::downgrade_if_needed(f.buffer_mut());
                }) {
                    crate::terminal::end_synchronized_output();
                    break Err(e.into());
                }
                crate::terminal::end_synchronized_output();
            }
            self.focused_app().dispatch_pending_restores();

            if let Some(i) = self.sessions.iter().position(|rt| {
                rt.app.exit_request != ExitRequest::None && !rt.notifications.waiting_for_drain()
            }) {
                // A backgrounded session can finish an `exit_on_done` turn;
                // focus it so shutdown reports its exit code and id.
                self.focused = i;
                self.emit_notifications();
                break Ok(());
            }

            // Sleeping a whole frame instead of a fraction of one is what
            // makes a spinner cost 12 paints a second instead of 62.
            let cadence = self.sessions[self.focused].app.cadence();
            match self.next_wake(cadence.frame().unwrap_or(IDLE_POLL)) {
                // Any event can change the screen, so paint after handling it
                // rather than asking every handler to prove it did.
                Some(wake) => {
                    dirty = Dirty::YES;
                    if let Err(e) = self.handle_wake(wake) {
                        break Err(e);
                    }
                }
                // Only the clock moved, so motion alone owes the frame. The
                // cadence is the one from before the sleep, so motion that
                // just stopped still gets a last paint to clear itself off
                // the screen.
                None => dirty |= Dirty::from(cadence.moves()),
            }
        };
        // Fatal errors still save every session, kill MCP process groups, and
        // drain the storage writer before the process exits.
        let shutdown = self.shutdown();
        result.map(|()| shutdown)
    }

    /// Wait for the next event from any source, or time out so animations and
    /// periodic polls keep running. `Duration::ZERO` drains whatever is
    /// already pending.
    fn next_wake(&self, timeout: Duration) -> Option<Wake> {
        let mut sel = flume::Selector::new().recv(self.input.receiver(), |res| match res {
            Ok(ev) => Some(Wake::Input(ev)),
            Err(_) => Some(Wake::InputGone),
        });
        if !self.ui_action_rx.is_disconnected() {
            sel = sel.recv(&self.ui_action_rx, |res| res.ok().map(Wake::Ui));
        }
        sel = sel.recv(&self.warn_rx, |res| res.ok().map(|w| Wake::Warn(None, w)));
        for (i, rt) in self.sessions.iter().enumerate() {
            if !rt.handles.agent_rx.is_disconnected() {
                sel = sel.recv(&rt.handles.agent_rx, move |res| {
                    res.ok().map(|env| Wake::Agent(i, Box::new(env)))
                });
            }
            sel = sel.recv(&rt.shell_rx, move |res| {
                res.ok().map(|ev| Wake::Shell(i, ev))
            });
            sel = sel.recv(&rt.handles.flow_progress_rx, move |res| {
                res.ok().map(|p| Wake::Flow(i, p))
            });
            sel = sel.recv(&rt.warn_rx, move |res| {
                res.ok().map(|w| Wake::Warn(Some(i), w))
            });
            sel = sel.recv(&rt.action_rx, move |res| {
                res.ok().map(|action| Wake::Action(i, action))
            });
        }
        sel.wait_timeout(timeout).ok().flatten()
    }

    fn handle_wake(&mut self, wake: Wake) -> Result<()> {
        match wake {
            Wake::Input(ev) => self.handle_input(ev),
            Wake::InputGone => return Err(eyre!("terminal input reader stopped")),
            Wake::Ui(action) => self.handle_ui_action(action),
            Wake::Agent(i, envelope) => self.handle_agent(i, envelope),
            Wake::Shell(i, event) => self.sessions[i].app.handle_shell_event(event),
            Wake::Warn(Some(i), warning) => self.sessions[i].app.flash(warning),
            Wake::Warn(None, warning) => self.focused_app().flash(warning),
            Wake::Flow(i, p) => {
                let actions = self.sessions[i].app.update(Msg::FlowProgress(p));
                self.dispatch(i, actions);
            }
            Wake::Action(idx, action) => self.handle_action(idx, action),
        }
        Ok(())
    }

    /// Only the focused session is drawn, so only it can owe a frame; focusing
    /// another is an event, and events always repaint. Background sessions
    /// still drain their floats, or a plugin writing to a window nobody is
    /// looking at would lose the output.
    fn tick(&mut self) -> Dirty {
        let mut dirty = Dirty::NO;
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            if i == self.focused {
                dirty |= rt.app.tick();
            } else {
                let _ = rt.app.float_mgr.tick();
            }
        }
        dirty
    }

    fn handle_agent(&mut self, idx: usize, envelope: Box<Envelope>) {
        let rt = &mut self.sessions[idx];
        let current = is_current_top_level(rt.app.run_id, &envelope);
        match &envelope.event {
            AgentEvent::QueueDrained => {
                if current {
                    rt.notifications.on_drain();
                }
                return;
            }
            AgentEvent::QueueItemConsumed { .. } if current => {
                rt.notifications.on_queue_item_consumed();
                if rt.app.exit_on_done {
                    rt.app.clear_exit_request();
                }
            }
            AgentEvent::TurnComplete(turn) if current => {
                rt.notifications.on_turn_complete(&turn.message);
            }
            event if current => rt.notifications.on_done(event),
            _ => {}
        }
        let actions = self.sessions[idx].app.update(Msg::Agent(envelope));
        self.dispatch(idx, actions);
    }

    /// The one save trigger. A checkpoint writes only on a real change, so
    /// every tool result reaches disk within a frame while an idle session
    /// writes nothing.
    fn checkpoint_all(&mut self) {
        for rt in &mut self.sessions {
            rt.app.checkpoint();
        }
    }

    fn drain_channels(&mut self) -> Result<Dirty> {
        let mut dirty = Dirty::NO;
        // Leftovers beyond the budget are picked up right after the next draw.
        for _ in 0..DRAIN_BUDGET {
            match self.next_wake(Duration::ZERO) {
                Some(wake) => {
                    self.handle_wake(wake)?;
                    dirty = Dirty::YES;
                }
                None => break,
            }
        }

        for rt in &mut self.sessions {
            if rt.app.status == Status::Streaming && rt.handles.task_finished() {
                rt.app.status = Status::error("agent stopped unexpectedly".into());
            }
        }

        let slot_model = self.ctx.model_slot.load();
        let spec = slot_model.model.spec();
        let reserve = reserve_tokens_for(&self.ctx.config, &slot_model.model);
        let buffer = self
            .ctx
            .config
            .resolve_compaction_buffer(slot_model.model.context_window);
        for rt in &mut self.sessions {
            // Compute the context_window this session should have after its own
            // override is applied to the slot's base, so we also resync when
            // model discovery updates the base context_window without changing
            // the spec (e.g. LlamaCpp populating a previously default 128k).
            let mut expected = slot_model.model.clone();
            crate::app::session_state::apply_context_window_override(
                &mut expected,
                &rt.app.state.context_window_overrides,
                reserve,
                buffer,
            );
            if rt.app.state.session.model != spec
                || rt.app.state.model.context_window != expected.context_window
            {
                // The shared slot holds the BASE model; apply this session's
                // own context-window overrides before installing it so one
                // session's override doesn't leak into another.
                rt.app.update_model(&expected);
                dirty = Dirty::YES;
            }
        }
        drop(slot_model);

        // These two only fire Lua autocmds. Anything a handler does comes back
        // as a `UiAction` on the next wake, which repaints then.
        self.emit_focus_change();
        dirty |= self.start_mailbox_runs();
        self.emit_status_changes();
        self.emit_task_changes();
        self.emit_notifications();
        // An `exit_on_done` exit waits on `QueueDrained`; a dead agent loop
        // can never send it, so fail instead of hanging forever.
        if let Some(runtime) = self.sessions.iter().find(|rt| {
            rt.app.exit_request != ExitRequest::None
                && rt.notifications.waiting_for_drain()
                && rt.handles.task_finished()
                && rt.handles.agent_rx.is_empty()
        }) {
            return Err(eyre!(
                "agent for session {} stopped before queue drain",
                runtime.id()
            ));
        }
        Ok(dirty)
    }

    fn handle_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::Flash(msg) => {
                self.focused_app().flash(msg);
            }
            UiAction::SetWindowTitle(title) => {
                if let Err(error) = crate::terminal::set_window_title(&title) {
                    tracing::warn!(%error, "failed to set window title");
                }
            }
            UiAction::OpenEditor { path, reply_tx } => {
                let code = self.open_editor(self.focused, &path);
                let _ = reply_tx.send(code);
            }
            UiAction::OpenWin {
                buf,
                config,
                focus,
                event_tx,
                cmd_rx,
            } => {
                let app = self.focused_app();
                app.float_mgr.open(buf, config, focus, event_tx, cmd_rx);
                if focus {
                    app.transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                }
            }
            UiAction::Session { req, reply_tx } => {
                self.handle_session_request(req, reply_tx);
            }
            UiAction::Model { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_model_request(req));
            }
            UiAction::Task { req, reply_tx } => {
                let _ = reply_tx.send(self.handle_task_request(req));
            }
            UiAction::WinSaveView { reply_tx } => {
                let _ = reply_tx.send(self.focused_app().win_view());
            }
            UiAction::WinRestView { scroll_top } => {
                self.focused_app().scroll_to_row(scroll_top);
            }
            UiAction::Builtin(action) => {
                let actions = self.focused_app().run_builtin(action);
                self.dispatch(self.focused, actions);
            }
            UiAction::RunCommand {
                cmdline,
                depth,
                reply_tx,
            } => {
                // Answer before dispatching: the caller only waits on the name
                // resolving, and dispatch may take a while (or exit the app).
                match self.focused_app().run_cmdline(&cmdline, depth) {
                    Ok(actions) => {
                        let _ = reply_tx.send(Ok(()));
                        self.dispatch(self.focused, actions);
                    }
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                    }
                }
            }
        }
    }

    /// Exits with the editor's status code; `-1` (flashed on the session's
    /// app) when the editor could not be launched.
    fn open_editor(&mut self, idx: usize, path: &std::path::Path) -> i32 {
        let result = {
            let _pause = self.input.pause();
            terminal::open_in_editor(path, self.terminal)
        };
        self.terminal_focused = false;
        match result {
            Ok(code) => code,
            Err(e) => {
                self.sessions[idx].app.flash(e);
                -1
            }
        }
    }

    fn emit_notifications(&mut self) {
        let Some(notifier) = &self.notifier else {
            return;
        };
        let mut selected = None;
        for rt in &mut self.sessions {
            let candidate = rt.notifications.reconcile(
                rt.app.attention(),
                rt.last_status,
                rt.handles.queue.is_empty(),
                self.terminal_focused,
            );
            selected = select_notification(selected, candidate);
        }
        if let Some(notification) = selected
            && let Err(error) = notifier.notify(&notification.message())
        {
            warn!(notifier = ?notifier.notifier(), %error, "terminal notifications disabled after write failure");
            self.notifier = None;
        }
    }

    fn emit_focus_change(&mut self) {
        let id = self.sessions[self.focused].id();
        if self.last_focused == Some(id) {
            return;
        }
        let mut data = json!({ "session_id": id });
        if let Some(previous) = self.last_focused {
            data["previous_session_id"] = json!(previous);
        }
        self.last_focused = Some(id);
        self.ctx
            .lua_event_handle
            .fire_autocmd("SessionFocusChanged", data);
    }

    fn emit_status_changes(&mut self) {
        let handle = &self.ctx.lua_event_handle;
        for (i, rt) in self.sessions.iter_mut().enumerate() {
            let status = SessionStatus::of(&rt.app);
            if status == rt.last_status {
                continue;
            }
            rt.last_status = status;
            handle.fire_autocmd(
                "SessionStatusChanged",
                json!({
                    "session_id": rt.id(),
                    "title": rt.app.state.session.title,
                    "status": status.as_str(),
                    "focused": i == self.focused,
                }),
            );
        }
    }

    /// One diff per frame covers every path that finishes, cancels or errors a
    /// chat, so none of them has to remember to fire an event.
    fn emit_task_changes(&mut self) {
        let handle = &self.ctx.lua_event_handle;
        for rt in &mut self.sessions {
            let session_id = rt.id();
            diff_task_states(&mut rt.last_tasks, rt.app.task_states(), |task| {
                handle.fire_autocmd(
                    "TaskStatusChanged",
                    json!({
                        "session_id": session_id,
                        "id": task.id,
                        "name": task.name,
                        "status": task.status,
                    }),
                );
            });
        }
    }

    fn start_mailbox_runs(&mut self) -> Dirty {
        let ready: Vec<_> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, runtime)| {
                if !runtime.quiescent() {
                    return None;
                }
                let preamble = runtime.handles.claim_mailbox_wake();
                (!preamble.is_empty()).then_some((index, preamble))
            })
            .collect();

        let dirty = Dirty::from(!ready.is_empty());
        for (index, preamble) in ready {
            let actions = self.sessions[index].app.start_mailbox_run(preamble);
            self.dispatch(index, actions);
        }
        dirty
    }

    /// `List` replies from a background task (the scan can be slow); every
    /// other request is answered synchronously by the event loop, which owns
    /// the live runtimes.
    fn handle_session_request(&mut self, req: SessionRequest, reply_tx: flume::Sender<UiReply>) {
        match req {
            SessionRequest::List => {
                let storage = self.ctx.storage.clone();
                let cwd = self.focused_app().state.session.cwd.clone();
                std::thread::spawn(move || {
                    let reply = AppSession::list(Some(&cwd), &storage)
                        .map_err(|e| e.to_string())
                        .and_then(|list| serde_json::to_value(list).map_err(|e| e.to_string()));
                    let _ = reply_tx.send(reply);
                });
            }
            SessionRequest::Delete { id } => {
                let id = match parse_session_id(&id) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = reply_tx.send(Err(e));
                        return;
                    }
                };
                if let Some(i) = self.position(id) {
                    if i == self.focused {
                        let _ = reply_tx.send(Err(DELETE_FOCUSED_ERR.into()));
                        return;
                    }
                    let rt = self.remove_runtime(i);
                    // Tear down on a background task so deleting a mid-flight
                    // agent never wedges the UI thread: cancel the run, save
                    // whatever is on disk, drop the App, then await the agent
                    // task with the standard timeout. MCP is shared and shut
                    // down separately by `EventLoop::shutdown`, so use
                    // `shutdown_no_mcp` to avoid racing the shared handle.
                    tokio::spawn(async move {
                        let mut rt = rt;
                        rt.handles.send_cancel_all();
                        rt.app.checkpoint_now();
                        drop(rt.app);
                        rt.handles.shutdown_no_mcp(AGENT_SHUTDOWN_TIMEOUT).await;
                    });
                }
                self.ctx.storage_writer.delete(id, move |res| {
                    let reply = match res {
                        Ok(())
                        | Err(craft_storage::sessions::SessionError::Storage(
                            craft_storage::StorageError::NotFound(_),
                        )) => Ok(json!(true)),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = reply_tx.send(reply);
                });
            }
            SessionRequest::Live => {
                let list: Vec<_> = self
                    .sessions
                    .iter()
                    .enumerate()
                    .map(|(i, rt)| {
                        json!({
                            "id": rt.id(),
                            "title": rt.app.state.session.title,
                            "status": SessionStatus::of(&rt.app).as_str(),
                            "updated_at": rt.app.state.session.updated_at,
                            "cwd": rt.app.state.session.cwd,
                            "focused": i == self.focused,
                        })
                    })
                    .collect();
                let _ = reply_tx.send(Ok(json!(list)));
            }
            SessionRequest::Current => {
                let _ = reply_tx.send(Ok(json!(self.sessions[self.focused].id())));
            }
            SessionRequest::New { prompt, focus } => {
                let session = self.focused_app().blank_session();
                let idx = self.push_runtime(self.ctx.spawn_runtime(session));
                let id = self.sessions[idx].id();
                if let Some(prompt) = prompt {
                    let _ = self.submit_text(idx, prompt);
                }
                if focus {
                    self.focused = idx;
                }
                let _ = reply_tx.send(Ok(json!(id)));
            }
            SessionRequest::Prompt { id, text } => {
                let idx = match id {
                    None => Ok(self.focused),
                    Some(id) => parse_session_id(&id).and_then(|id| {
                        self.position(id)
                            .ok_or_else(|| format!("{NOT_LIVE_ERR}: {id}"))
                    }),
                };
                let _ = reply_tx.send(idx.and_then(|idx| self.submit_text(idx, text)));
            }
            SessionRequest::Focus { id } => {
                let reply = parse_session_id(&id)
                    .and_then(|id| self.focus_session(id))
                    .map(|()| json!(true));
                let _ = reply_tx.send(reply);
            }
            SessionRequest::SetTitle { id, title } => {
                let title = craft_storage::sessions::normalize_title(&title);
                let reply = (|| {
                    let id = parse_session_id(&id)?;
                    if let Some(i) = self.position(id) {
                        self.sessions[i].app.state.session_mut().set_title(title);
                    } else {
                        let mut session =
                            AppSession::load(id, &self.ctx.storage).map_err(|e| e.to_string())?;
                        session.set_title(title);
                        self.ctx.storage_writer.send(Arc::new(session));
                    }
                    Ok(json!(true))
                })();
                let _ = reply_tx.send(reply);
            }
        }
    }

    /// Lua acts on the focused session, the same target the model picker and
    /// `/thinking` write to.
    fn handle_model_request(&mut self, req: ModelRequest) -> UiReply {
        match req {
            ModelRequest::Get => Ok(self.focused_app().model_state()),
            ModelRequest::Available => {
                let available = self.ctx.available_models.load();
                Ok(json!(
                    available.as_deref().map(Vec::as_slice).unwrap_or(&[])
                ))
            }
            ModelRequest::Set {
                spec,
                thinking,
                fast,
            } => {
                if let Some(spec) = spec {
                    self.change_model(&spec)?;
                }
                let app = self.focused_app();
                if let Some(thinking) = thinking {
                    app.set_thinking(&thinking)?;
                }
                if let Some(fast) = fast {
                    app.set_fast(fast)?;
                }
                Ok(app.model_state())
            }
        }
    }

    fn handle_task_request(&mut self, req: TaskRequest) -> UiReply {
        match req {
            TaskRequest::List => Ok(json!(self.focused_app().tasks())),
            TaskRequest::Focus { id } => self.focused_app().focus_task(&id).map(|()| json!(true)),
        }
    }

    fn submit_text(&mut self, idx: usize, text: String) -> UiReply {
        let msg = QueuedMessage {
            text,
            images: Vec::new(),
        };
        match self.sessions[idx].app.submit_prompt(msg) {
            SubmitOutcome::Started(actions) => {
                self.dispatch(idx, actions);
                Ok(json!("started"))
            }
            SubmitOutcome::Queued => Ok(json!("queued")),
            SubmitOutcome::Rejected(e) => Err(e.into()),
        }
    }

    fn position(&self, id: CraftId) -> Option<usize> {
        self.sessions.iter().position(|rt| rt.id() == id)
    }

    /// The single place that removes a runtime: keeps `focused` pointing at
    /// the same session afterwards. The focused runtime itself is never
    /// removable, so `sessions` stays non-empty.
    fn remove_runtime(&mut self, idx: usize) -> SessionRuntime {
        debug_assert_ne!(idx, self.focused);
        let rt = self.sessions.remove(idx);
        if idx < self.focused {
            self.focused -= 1;
        }
        rt
    }

    fn push_runtime(&mut self, rt: SessionRuntime) -> usize {
        self.sessions.push(rt);
        self.sessions.len() - 1
    }

    /// Focus a live session, or bring a stored one up: in place when the
    /// focused session is a blank idle one (nothing worth keeping), otherwise
    /// as a new runtime so the session you came from stays live.
    fn focus_session(&mut self, id: CraftId) -> Result<(), String> {
        if let Some(i) = self.position(id) {
            self.focused = i;
            return Ok(());
        }
        let focused = &mut self.sessions[self.focused];
        if SessionStatus::of(&focused.app) == SessionStatus::Idle && !focused.app.has_content() {
            let actions = focused.app.load_session(id);
            self.dispatch(self.focused, actions);
            return Ok(());
        }
        let session = AppSession::load(id, &self.ctx.storage)
            .map_err(|e| format!("Failed to load session: {e}"))?;
        let idx = self.push_runtime(self.ctx.spawn_runtime(session));
        self.focused = idx;
        Ok(())
    }

    /// Handles one input event plus any leftover produced while coalescing
    /// bursts of scroll/drag events.
    fn handle_input(&mut self, raw: Event) {
        let mut pending = Some(raw);
        while let Some(ev) = pending.take() {
            let (msg, leftover) = self.translate(ev);
            if let Some(msg) = msg {
                let actions = self.sessions[self.focused].app.update(msg);
                self.dispatch(self.focused, actions);
            }
            pending = leftover;
        }
    }

    fn translate(&mut self, raw: Event) -> (Option<Msg>, Option<Event>) {
        let supports_focus_reporting = self
            .notifier
            .as_ref()
            .is_some_and(terminal::TerminalNotifier::supports_focus_reporting);
        if let Some(focused) = terminal_focus_event(&raw) {
            if supports_focus_reporting {
                self.terminal_focused = focused;
            }
            return (None, None);
        }
        if supports_focus_reporting && terminal_input_proves_focus(&raw) {
            self.terminal_focused = true;
        }
        match raw {
            Event::Key(key) if key.kind == KeyEventKind::Press => (Some(Msg::Key(key)), None),
            Event::Key(_) => (None, None),
            Event::Paste(text) => (Some(Msg::Paste(text)), None),
            Event::Mouse(mouse) => self.translate_mouse(mouse),
            _ => (None, None),
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> (Option<Msg>, Option<Event>) {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let scroll_lines = self.focused_app().ui_config.mouse_scroll_lines;
                let (msg, leftover) = self.aggregate_scroll(mouse, scroll_lines);
                (Some(msg), leftover)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, leftover) = self.coalesce_drag(mouse);
                (Some(Msg::Mouse(drag)), leftover)
            }
            _ => (Some(Msg::Mouse(mouse)), None),
        }
    }

    /// Sums queued scroll events into one delta; the first non-scroll event
    /// drained along the way is returned so it isn't lost.
    fn aggregate_scroll(&self, first: CtMouseEvent, scroll_lines: u32) -> (Msg, Option<Event>) {
        let mut delta = scroll_delta(first.kind, scroll_lines);
        let mut leftover = None;
        while let Ok(next) = self.input.receiver().try_recv() {
            match next {
                Event::Mouse(m)
                    if matches!(
                        m.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    delta += scroll_delta(m.kind, scroll_lines);
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (
            Msg::Scroll {
                column: first.column,
                row: first.row,
                delta,
            },
            leftover,
        )
    }

    /// Keeps only the newest queued drag position; the first non-drag event
    /// drained along the way is returned so it isn't lost.
    fn coalesce_drag(&self, mut latest: CtMouseEvent) -> (CtMouseEvent, Option<Event>) {
        let mut leftover = None;
        while let Ok(next) = self.input.receiver().try_recv() {
            match next {
                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) => {
                    latest = m;
                }
                other => {
                    leftover = Some(other);
                    break;
                }
            }
        }
        (latest, leftover)
    }

    fn dispatch(&mut self, idx: usize, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(idx, action);
        }
    }

    fn respawn_agent(&mut self, idx: usize, history: Vec<Message>) {
        let rt = &mut self.sessions[idx];
        rt.reset_run_notifications();
        let lua_handle = rt.app.lua_event_handle.clone();
        let model_slot = Arc::clone(&rt.model_slot);
        let permissions = Arc::clone(&rt.app.permissions);
        rt.handles.respawn(
            history,
            &model_slot,
            self.ctx.config.clone(),
            self.ctx.compression.clone(),
            self.ctx.ui_config.tool_output_lines,
            &permissions,
            &mut rt.app,
            lua_handle,
        );
    }

    fn handle_action(&mut self, idx: usize, action: Action) {
        match action {
            Action::SendMessage(input) => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let mut input = *input;
                prepend_preamble(&mut input.preamble, rt.app.shell.drain_results());
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Message {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                    input,
                    run_id,
                    displayed: true,
                });
            }
            Action::CancelAgent { run_id } => {
                let rt = &mut self.sessions[idx];
                rt.notifications.reset();
                let _ = rt.handles.cmd_tx.try_send(AgentCommand::Cancel { run_id });
            }
            Action::CancelSubagent { tool_use_id } => {
                let _ = self.sessions[idx]
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::CancelSubagent { tool_use_id });
            }
            Action::NewSession => {
                self.respawn_agent(idx, Vec::new());
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                let model_spec = loaded.model_spec.clone();
                if model_spec != self.ctx.model_slot.load().model.spec()
                    && self.ctx.model_policy.allows(&model_spec)
                {
                    let timeouts = self.ctx.timeouts;
                    let tx = self.sessions[idx].action_tx.clone();
                    tokio::spawn(async move {
                        let result = match Model::from_spec(&model_spec) {
                            Ok(mut model) => from_model(&mut model, timeouts)
                                .await
                                .map(|p| Arc::from(p) as Arc<dyn Provider>)
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(e.to_string()),
                        };
                        let _ = tx.send(Action::ProviderReady {
                            model_spec,
                            provider: result,
                            pending_load_session: Some(Box::new(loaded)),
                        });
                    });
                } else {
                    self.respawn_agent(idx, loaded.messages);
                }
            }
            Action::ChangeModel(spec) => {
                if let Err(e) = self.change_model(&spec) {
                    self.focused_app().flash(e);
                }
            }
            Action::RefreshProvider { slug } => self.refresh_provider(slug),
            Action::AssignTier(spec, tier) => {
                craft_providers::model_registry::set_and_persist(
                    spec,
                    tier,
                    &self.sessions[idx].app.storage,
                );
            }
            Action::UnassignTier(spec, tier) => {
                craft_providers::model_registry::unset_and_persist(
                    &spec,
                    tier,
                    &self.sessions[idx].app.storage,
                );
            }
            Action::Compact => {
                let rt = &mut self.sessions[idx];
                rt.reset_run_notifications();
                let run_id = rt.app.run_id;
                rt.handles.queue.push(QueueItem::Compact { run_id });
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.sessions[idx].handles.send_mcp(McpCommand::Toggle {
                    server: server_name,
                    enabled,
                });
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let rt = &mut self.sessions[idx];
                let (trigger, cancel) = CancelToken::new();
                rt.app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    rt.shell_tx.clone(),
                    cancel,
                    self.ctx.config.clone(),
                );
            }
            Action::OpenEditor(path) => {
                self.open_editor(idx, &path);
            }
            Action::EditInputInEditor => {
                let current_text = self.sessions[idx].app.input_box.buffer.value();
                let result = {
                    let _pause = self.input.pause();
                    terminal::edit_temp_content(&current_text, self.terminal)
                };
                self.terminal_focused = false;
                match result {
                    Ok(edited) => self.sessions[idx].app.input_box.set_input(edited),
                    Err(e) => self.sessions[idx].app.flash(e),
                }
            }
            Action::Btw(question) => {
                let rt = &mut self.sessions[idx];
                let slot = rt.model_slot.load();
                rt.app
                    .start_btw(question, Arc::clone(&slot.provider), slot.model.clone());
            }
            Action::WikiIngest { source_path } => {
                self.wiki_ingest(idx, source_path);
            }
            Action::ToggleWatch { enabled } => {
                let action_tx = self.sessions[idx].action_tx.clone();
                let rt = &mut self.sessions[idx];
                if enabled {
                    rt.spawn_watch_watcher(&action_tx);
                } else {
                    rt.stop_watch_watcher();
                }
            }
            Action::WatchPrompt { text, files } => {
                let label = format!(
                    "watch ({} file{})",
                    files.len(),
                    if files.len() == 1 { "" } else { "s" }
                );
                let actions = self.sessions[idx].app.submit_watch_prompt(label, text);
                self.dispatch(idx, actions);
            }
            Action::Suspend => {
                let _pause = self.input.pause();
                terminal::suspend(self.terminal);
                self.terminal_focused = false;
            }
            Action::RefreshModels => self.refresh_models(),
            Action::RefreshUsage => self.refresh_usage(idx),
            Action::ManualExit => self.sessions[idx].notifications.on_manual_exit(),
            Action::ProviderReady {
                model_spec,
                provider,
                pending_load_session,
            } => {
                let rt = &mut self.sessions[idx];
                match provider {
                    Ok(new_provider) => {
                        if let Ok(base_model) = Model::from_spec(&model_spec) {
                            // Apply the triggering runtime's own overrides to a
                            // private copy for its App state; the shared slot
                            // holds the BASE model so other sessions apply their
                            // own overrides when drain_channels propagates.
                            let mut owned = base_model.clone();
                            let reserve = reserve_tokens_for(&self.ctx.config, &owned);
                            let buffer = self
                                .ctx
                                .config
                                .resolve_compaction_buffer(owned.context_window);
                            crate::app::session_state::apply_context_window_override(
                                &mut owned,
                                &rt.app.state.context_window_overrides,
                                reserve,
                                buffer,
                            );
                            rt.model_slot.store(Arc::new(ModelSlot {
                                model: base_model,
                                provider: new_provider,
                            }));
                            rt.app.update_model(&owned);
                            rt.app.record_recent_model(&model_spec);
                            rt.app.usage_slot.store(None);
                        }
                    }
                    Err(e) => rt.app.flash(format!("{PROVIDER_INIT_ERR}: {e}")),
                }
                if let Some(loaded) = pending_load_session {
                    self.respawn_agent(idx, loaded.messages);
                }
            }
            Action::ApplyContextWindowOverride => self.apply_active_context_window_override(idx),
        }
    }

    /// Apply the active model's stored context-window override to both the
    /// `model_slot` and `app.state.model`, keeping them consistent. Also syncs
    /// the runtime's override map consumed by the background model fetch.
    fn apply_active_context_window_override(&mut self, idx: usize) {
        let rt = &mut self.sessions[idx];
        rt.context_window_overrides
            .store(Arc::new(rt.app.state.context_window_overrides.clone()));
        // The shared slot holds the BASE model; apply this runtime's own
        // overrides to a private copy for its App state only. Storing the
        // overridden model back into the shared slot would leak this
        // session's override into every other session.
        let slot = rt.model_slot.load();
        let reserve = reserve_tokens_for(&self.ctx.config, &slot.model);
        let buffer = self
            .ctx
            .config
            .resolve_compaction_buffer(slot.model.context_window);
        let mut model = slot.model.clone();
        let changed = crate::app::session_state::apply_context_window_override(
            &mut model,
            &rt.app.state.context_window_overrides,
            reserve,
            buffer,
        );
        if changed {
            rt.app.update_model(&model);
        }
    }

    fn change_model(&mut self, spec: &str) -> Result<(), String> {
        let idx = self.focused;
        if !self.ctx.model_policy.allows(spec) {
            return Err(format!("{MODEL_POLICY_ERR}: {spec}"));
        }
        let mut new_model =
            Model::from_spec(spec).map_err(|e| format!("{INVALID_MODEL_ERR}: {e}"))?;
        let model_spec = new_model.spec();
        if model_spec == self.sessions[idx].model_slot.load().model.spec() {
            return Ok(());
        }
        let timeouts = self.ctx.timeouts;
        let tx = self.sessions[idx].action_tx.clone();
        tokio::spawn(async move {
            let result = from_model(&mut new_model, timeouts)
                .await
                .map(|p| Arc::from(p) as Arc<dyn Provider>)
                .map_err(|e| e.to_string());
            let _ = tx.send(Action::ProviderReady {
                model_spec,
                provider: result,
                pending_load_session: None,
            });
        });
        Ok(())
    }

    fn refresh_models(&self) {
        let available = Arc::clone(&self.ctx.available_models);
        let warn_tx = self.warn_tx.clone();
        let policy = Arc::clone(&self.ctx.model_policy);
        available.store(None);
        tokio::spawn(async move {
            fetch_all_models(
                &policy,
                |batch| merge_batch(&available, batch, &warn_tx),
                None,
            )
            .await;
        });
    }

    fn refresh_usage(&mut self, idx: usize) {
        let rt = &self.sessions[idx];
        let provider = Arc::clone(&rt.model_slot.load().provider);
        let slot = Arc::clone(&rt.app.usage_slot);
        slot.store(Some(Arc::new(UsageFetchState::Loading)));
        tokio::spawn(async move {
            let state = match provider.fetch_usage().await {
                Ok(Some(usage)) => UsageFetchState::Ready(usage),
                Ok(None) => UsageFetchState::Unsupported,
                Err(e) => UsageFetchState::Error(e.user_message()),
            };
            slot.store(Some(Arc::new(state)));
        });
    }

    fn wiki_ingest(&self, idx: usize, source_path: PathBuf) {
        let rt = &self.sessions[idx];
        let slot = rt.model_slot.load();
        let provider = Arc::clone(&slot.provider);
        let model = slot.model.clone();
        let cwd = PathBuf::from(rt.app.state.session.cwd.clone());
        let warn_tx = self.sessions[idx]
            .app
            .warn_tx
            .clone()
            .unwrap_or_else(|| self.warn_tx.clone());
        tokio::spawn(async move {
            let result = run_wiki_ingest(&cwd, &source_path, provider.as_ref(), &model).await;
            let _ = warn_tx.send(result);
        });
    }

    fn refresh_provider(&mut self, slug: String) {
        let idx = self.focused;
        let current = self.sessions[idx].model_slot.load();
        let current_model = current.model.clone();

        if current_model.provider.to_string() == slug {
            let mut m = current_model.clone();
            let timeouts = self.ctx.timeouts;
            let tx = self.sessions[idx].action_tx.clone();
            tokio::spawn(async move {
                let result = craft_providers::provider::from_model(&mut m, timeouts)
                    .await
                    .map(|p| Arc::from(p) as Arc<dyn Provider>)
                    .map_err(|e| e.to_string());
                let _ = tx.send(Action::ProviderReady {
                    model_spec: m.spec(),
                    provider: result,
                    pending_load_session: None,
                });
            });
        } else if let Some(builtin) = craft_config::providers::builtin_provider(&slug)
            && let Err(e) = self.change_model(builtin.default_model)
        {
            self.focused_app().flash(e);
        }
    }

    fn shutdown(mut self) -> ShutdownReport {
        let exit = self.sessions[self.focused].app.exit_request;
        let mcp_handle = self.ctx.mcp_handle.clone();
        if let Some(ref h) = mcp_handle {
            craft_agent::mcp::kill_process_groups(&h.reader().load().pids);
        }
        for rt in &mut self.sessions {
            let _ = rt.handles.cmd_tx.try_send(AgentCommand::CancelAll);
            rt.app.checkpoint_now();
            rt.app.cmd_tx = None;
            rt.app.answer_tx = None;
        }
        let mut tabs = Vec::with_capacity(self.sessions.len());
        let mut agent_tasks = Vec::with_capacity(self.sessions.len());
        for rt in self.sessions.drain(..) {
            let SessionRuntime {
                mut app, handles, ..
            } = rt;
            app.checkpoint_now();
            // `app` drops at the end of this iteration, closing the channels
            // the agent loop waits on, so `join_all` can finish.
            tabs.push(Arc::unwrap_or_clone(app.state.session));
            agent_tasks.push(handles.into_task());
        }
        crate::agent::join_all(agent_tasks, AGENT_SHUTDOWN_TIMEOUT);
        if let Some(ref h) = mcp_handle {
            tokio::runtime::Handle::current().block_on(h.shutdown());
        }
        match Arc::try_unwrap(self.ctx.storage_writer) {
            Ok(writer) => writer.shutdown(AGENT_SHUTDOWN_TIMEOUT),
            Err(_) => {
                warn!("storage writer has outstanding references, skipping graceful shutdown")
            }
        }
        ShutdownReport {
            exit,
            tabs,
            focused: self.focused,
        }
    }
}

fn scroll_delta(kind: MouseEventKind, lines: u32) -> i32 {
    if kind == MouseEventKind::ScrollUp {
        lines as i32
    } else {
        -(lines as i32)
    }
}

/// Resolve the effective reserve tokens for a model, mirroring
/// `Agent::effective_compaction_buffer`: per-model `reserve_tokens` overrides
/// `compaction_buffer` when configured.
fn reserve_tokens_for(config: &AgentConfig, model: &Model) -> u32 {
    let provider_model_id = format!("{}/{}", model.provider, model.id);
    if let Some(t) =
        craft_config::resolve_threshold(&config.compaction, Some(&provider_model_id), &model.id)
        && let Some(reserve) = craft_config::resolve_reserve_tokens(t, model.context_window)
    {
        return reserve;
    }
    config.resolve_compaction_buffer(model.context_window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_agent::DoneReason;
    use craft_providers::TokenUsage;
    use test_case::test_case;

    fn done_event() -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 1,
            reason: DoneReason::EndTurn,
        }
    }

    fn due_completion() -> RunNotificationState {
        let mut state = RunNotificationState::default();
        state.on_done(&done_event());
        state.on_drain();
        state
    }

    #[test]
    fn completion_waits_for_queue_drain() {
        let mut state = RunNotificationState {
            response_candidate: Some("done".into()),
            ..RunNotificationState::default()
        };

        state.on_done(&done_event());
        assert!(state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            None
        );

        state.on_drain();
        assert!(!state.waiting_for_drain());
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            Some(Notification::TurnComplete {
                response: Some("done".into())
            })
        );
    }

    #[test_case(SessionStatus::Idle, true, false, true ; "fires_when_settled_and_unfocused")]
    #[test_case(SessionStatus::Idle, true, true, false ; "focused_terminal_swallows")]
    #[test_case(SessionStatus::Idle, false, false, false ; "queued_message_swallows")]
    #[test_case(SessionStatus::Working, true, false, false ; "busy_session_swallows")]
    fn due_completion_is_decided_on_first_reconcile(
        status: SessionStatus,
        queue_empty: bool,
        terminal_focused: bool,
        fires: bool,
    ) {
        let mut state = due_completion();

        let first = state.reconcile(None, status, queue_empty, terminal_focused);
        assert_eq!(first.is_some(), fires);
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            None
        );
    }

    #[test]
    fn prompt_wins_over_completion_and_is_not_repeated_unchanged() {
        let prompt = Notification::QuestionRequested;
        let mut state = due_completion();

        assert_eq!(
            state.reconcile(Some(prompt.clone()), SessionStatus::Idle, true, false),
            Some(prompt.clone())
        );
        assert_eq!(
            state.reconcile(Some(prompt), SessionStatus::NeedsInput, true, false),
            None
        );
        assert_eq!(
            state.reconcile(None, SessionStatus::Idle, true, false),
            None
        );
    }

    #[test]
    fn notification_selection_prefers_urgent_then_session_order() {
        let completion = Notification::TurnComplete { response: None };
        let first_prompt = Notification::QuestionRequested;
        let second_prompt = Notification::AuthenticationRequired;

        let selected = select_notification(None, Some(completion));
        let selected = select_notification(selected, Some(first_prompt.clone()));
        let selected = select_notification(selected, Some(second_prompt));

        assert_eq!(selected, Some(first_prompt));
    }

    #[cfg(not(windows))]
    #[test]
    fn focus_events_map_to_terminal_focus_state() {
        assert_eq!(terminal_focus_event(&Event::FocusGained), Some(true));
        assert_eq!(terminal_focus_event(&Event::FocusLost), Some(false));
        assert_eq!(terminal_focus_event(&Event::Resize(80, 24)), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn interactive_input_proves_terminal_focus() {
        let press = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        );
        let release = crossterm::event::KeyEvent {
            code: crossterm::event::KeyCode::Enter,
            modifiers: crossterm::event::KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };

        assert!(terminal_input_proves_focus(&Event::Key(press)));
        assert!(terminal_input_proves_focus(&Event::Paste("text".into())));
        assert!(!terminal_input_proves_focus(&Event::Key(release)));
        assert!(!terminal_input_proves_focus(&Event::Resize(80, 24)));
    }
}
