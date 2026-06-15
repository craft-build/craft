use std::sync::Arc;
use std::time::Duration;

use arc_swap::{ArcSwap, ArcSwapOption};
use color_eyre::Result;

use craft_agent::command::CustomCommand;
use craft_agent::permissions::PermissionManager;
use craft_agent::{AgentConfig, CancelToken, McpCommand, McpConfigErrors, McpHandle};
use craft_config::UiConfig;
use craft_lua::{EventHandle, LuaCommandReader, UiAction};
use craft_providers::Timeouts;
use craft_providers::provider::{Provider, fetch_all_models, from_model};
use craft_providers::{Message, Model};
use craft_storage::StateDir;
use crossterm::event::{
    self, Event, KeyEventKind, MouseButton, MouseEvent as CtMouseEvent, MouseEventKind,
};
use tracing::warn;

use crate::AppSession;
use crate::agent::{
    AgentCommand, AgentHandles, ModelSlot,
    shared_queue::{QueueItem, lock},
};
use crate::app::shell::{ShellEvent, spawn_shell};
use crate::app::{App, Msg};
#[cfg(feature = "demo")]
use crate::components;
use crate::components::input::Submission;
use crate::components::{Action, ExitRequest, LoadedSession, Status};

#[cfg(feature = "demo")]
use crate::mock;
use crate::storage_writer::StorageWriter;
use crate::terminal;

const ANIMATION_INTERVAL_MS: u64 = 16;
const IDLE_POLL_INTERVAL_MS: u64 = 100;

pub type BufClickHandler = Arc<dyn Fn(&str, u32) -> Option<craft_lua::ClickReply> + Send + Sync>;

pub struct ShutdownResult {
    session_id: Option<String>,
    exit_code: i32,
    handles: AgentHandles,
    storage_writer: Arc<StorageWriter>,
}

impl ShutdownResult {
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub async fn cleanup(self) {
        self.handles.shutdown(Duration::from_secs(3)).await;
        match Arc::try_unwrap(self.storage_writer) {
            Ok(writer) => writer.shutdown(Duration::from_secs(3)),
            Err(_) => {
                warn!("storage writer has outstanding references, skipping graceful shutdown")
            }
        }
    }
}

type RunResult = Result<ShutdownResult>;

pub struct EventLoopParams {
    pub model: Model,
    pub commands: Vec<CustomCommand>,
    pub session: AppSession,
    pub storage: StateDir,
    pub config: AgentConfig,
    pub compression: craft_config::CompressionConfig,
    pub ui_config: UiConfig,
    pub input_history_size: usize,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub exit_on_done: bool,
    pub lua_command_reader: LuaCommandReader,
    pub ui_action_rx: Option<flume::Receiver<UiAction>>,
    pub lua_event_handle: Option<EventHandle>,
    pub buf_click: Option<BufClickHandler>,
    pub provider: Arc<dyn Provider>,
    pub mcp_handle: Option<McpHandle>,
    pub mcp_config_errors: McpConfigErrors,
    #[cfg(feature = "demo")]
    pub demo: bool,
    #[cfg(feature = "onnx")]
    pub embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>,
}

pub(crate) struct EventLoop<'t> {
    terminal: &'t mut ratatui::DefaultTerminal,
    app: App,
    handles: AgentHandles,
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    compression: craft_config::CompressionConfig,
    permissions: Arc<PermissionManager>,
    shell_tx: flume::Sender<ShellEvent>,
    shell_rx: flume::Receiver<ShellEvent>,
    warn_rx: flume::Receiver<String>,
    storage_writer: Arc<StorageWriter>,
    timeouts: Timeouts,
    ui_action_rx: Option<flume::Receiver<UiAction>>,
    action_rx: flume::Receiver<Action>,
    action_tx: flume::Sender<Action>,
    _model_fetch_task: tokio::task::JoinHandle<()>,
}

struct BackgroundModels {
    available: Arc<ArcSwapOption<Vec<String>>>,
    warn_rx: flume::Receiver<String>,
    task: tokio::task::JoinHandle<()>,
}

fn spawn_model_fetch() -> BackgroundModels {
    let available: Arc<ArcSwapOption<Vec<String>>> = Arc::new(ArcSwapOption::empty());
    let bg = Arc::clone(&available);
    let (warn_tx, warn_rx) = flume::unbounded::<String>();
    let task = tokio::spawn(async move {
        let warn_tx = warn_tx;
        fetch_all_models(|batch| {
            for w in batch.warnings {
                let _ = warn_tx.try_send(w);
            }
            if batch.models.is_empty() {
                return;
            }
            let mut merged = bg.load().as_deref().cloned().unwrap_or_default();
            merged.extend(batch.models);
            bg.store(Some(Arc::new(merged)));
        })
        .await;
    });
    BackgroundModels {
        available,
        warn_rx,
        task,
    }
}

fn restore_session(app: &mut App, handles: &AgentHandles) {
    app.permissions
        .load_session_rules(crate::app::session_state::stored_to_rules(
            &app.state.session.meta.session_rules,
        ));
    *handles
        .tool_outputs
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = app.state.session.tool_outputs.clone();
    app.restore_display();
    for w in app.state.warnings.drain(..) {
        app.status_bar.flash(w);
    }
}

#[cfg(feature = "demo")]
fn apply_demo(app: &mut App) {
    app.status = components::Status::Streaming;
    app.run_id = 1;
    for event in mock::mock_events() {
        match event {
            mock::MockEvent::User(text) => app.main_chat().push_user_message(&text),
            mock::MockEvent::Error(text) => {
                app.main_chat().push(components::DisplayMessage::new(
                    components::DisplayRole::Error,
                    text,
                ));
            }
            mock::MockEvent::Flush => app.flush_all_chats(),
            mock::MockEvent::Agent(envelope) => {
                app.update(Msg::Agent(envelope));
            }
        }
    }
    app.flush_all_chats();
    app.status = components::Status::Idle;
}

impl<'t> EventLoop<'t> {
    pub(crate) fn new(
        terminal: &'t mut ratatui::DefaultTerminal,
        params: EventLoopParams,
    ) -> Result<Self> {
        let EventLoopParams {
            model,
            commands,
            session,
            storage,
            config,
            compression,
            ui_config,
            input_history_size,
            permissions,
            timeouts,
            exit_on_done,
            lua_command_reader,
            ui_action_rx,
            lua_event_handle,
            buf_click,
            provider,
            mcp_handle,
            mcp_config_errors,
            #[cfg(feature = "demo")]
            demo,
            #[cfg(feature = "onnx")]
            embed_rx,
        } = params;

        std::thread::spawn(crate::highlight::warmup);
        crate::update::spawn_check();

        let bg = spawn_model_fetch();
        let storage_writer = Arc::new(StorageWriter::new(storage.clone())?);
        let (shell_tx, shell_rx) = flume::unbounded::<ShellEvent>();
        let (action_tx, action_rx) = flume::unbounded::<Action>();

        let resumed = !session.messages.is_empty();
        let initial_history = session.messages.clone();

        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: model.clone(),
            provider,
        }));
        let handles = AgentHandles::spawn(
            &model_slot,
            initial_history,
            config.clone(),
            ui_config.tool_output_lines,
            &permissions,
            Some(session.id.clone()),
            timeouts,
            lua_event_handle.clone(),
            mcp_handle,
            mcp_config_errors.clone(),
            compression.clone(),
            #[cfg(feature = "onnx")]
            embed_rx,
        );

        let custom_commands: Arc<[CustomCommand]> = Arc::from(commands);
        let mut app = App::new(
            &model,
            session,
            storage,
            bg.available,
            handles.mcp_reader(),
            handles.mcp_config_errors.clone(),
            lua_command_reader,
            Arc::clone(&storage_writer),
            ui_config,
            input_history_size,
            Arc::clone(&permissions),
            custom_commands,
        );
        app.exit_on_done = exit_on_done;
        app.buf_click = buf_click;
        app.lua_event_handle = lua_event_handle;

        #[cfg(feature = "demo")]
        if demo {
            apply_demo(&mut app);
        }

        handles.apply_to_app(&mut app);

        if !handles.mcp_config_errors.is_empty() {
            app.flash(format!("MCP config error: {}", handles.mcp_config_errors));
        }

        if resumed {
            restore_session(&mut app, &handles);
        }

        Ok(Self {
            terminal,
            app,
            handles,
            model_slot,
            config,
            compression,
            permissions,
            shell_tx,
            shell_rx,
            warn_rx: bg.warn_rx,
            storage_writer,
            timeouts,
            ui_action_rx,
            action_rx,
            action_tx,
            _model_fetch_task: bg.task,
        })
    }

    pub(crate) fn run(mut self, initial_prompt: Option<String>) -> RunResult {
        if let Some(prompt) = initial_prompt {
            let sub = Submission {
                text: prompt,
                images: Vec::new(),
            };
            let actions = self.app.handle_submit(sub);
            self.dispatch(actions);
        }
        loop {
            self.tick();
            let had_agent_msg = self.drain_channels();
            self.terminal.draw(|f| self.app.view(f))?;
            self.app.dispatch_pending_restores();

            if self.app.exit_request != ExitRequest::None {
                return Ok(self.shutdown());
            }

            self.poll_and_handle_input(had_agent_msg)?;
        }
    }

    fn tick(&mut self) {
        self.app.tick_edge_scroll();
        self.app.tick_error_expiry();
        self.app.poll_image_paste();
        self.app.btw_modal.poll();
        self.app.status_bar.poll_branch_update();
        self.app.mcp_picker.refresh();
        self.app.float_mgr.tick();
    }

    fn drain_channels(&mut self) -> bool {
        while let Ok(event) = self.shell_rx.try_recv() {
            self.app.handle_shell_event(event);
        }

        let mut had_agent_msg = false;
        loop {
            match self.handles.agent_rx.try_recv() {
                Ok(envelope) => {
                    had_agent_msg = true;
                    let actions = self.app.update(Msg::Agent(Box::new(envelope)));
                    self.dispatch(actions);
                }
                Err(flume::TryRecvError::Disconnected) if self.app.status == Status::Streaming => {
                    self.app.status = Status::error("agent stopped unexpectedly".into());
                    break;
                }
                Err(_) => break,
            }
        }

        while let Ok(warning) = self.warn_rx.try_recv() {
            self.app.flash(warning);
        }

        if let Some(rx) = &self.ui_action_rx {
            while let Ok(action) = rx.try_recv() {
                match action {
                    UiAction::Flash(msg) => {
                        self.app.flash(msg);
                    }
                    UiAction::OpenEditor { path, reply_tx } => {
                        let code = match crate::terminal::open_in_editor(&path, self.terminal) {
                            Ok(code) => code,
                            Err(e) => {
                                self.app.flash(e);
                                -1
                            }
                        };
                        let _ = reply_tx.send(code);
                    }
                    UiAction::OpenWin {
                        buf,
                        config,
                        focus,
                        event_tx,
                        cmd_rx,
                    } => {
                        self.app
                            .float_mgr
                            .open(buf, config, focus, event_tx, cmd_rx);
                        if focus {
                            self.app
                                .transition_plan(crate::app::mode::PlanTrigger::InteractivePrompt);
                        }
                    }
                }
            }
        }

        while let Ok(action) = self.action_rx.try_recv() {
            self.handle_action(action);
        }

        had_agent_msg
    }

    fn poll_and_handle_input(&mut self, had_agent_msg: bool) -> Result<()> {
        let has_pending_ui_action = self.ui_action_rx.as_ref().is_some_and(|rx| !rx.is_empty());
        let poll_duration = if had_agent_msg || has_pending_ui_action {
            Duration::ZERO
        } else if self.app.is_animating() {
            Duration::from_millis(ANIMATION_INTERVAL_MS)
        } else {
            Duration::from_millis(IDLE_POLL_INTERVAL_MS)
        };

        if !event::poll(poll_duration)? {
            return Ok(());
        }

        if let Some(msg) = self.translate_input()? {
            let actions = self.app.update(msg);
            self.dispatch(actions);
        }
        Ok(())
    }

    fn translate_input(&mut self) -> Result<Option<Msg>> {
        let raw = event::read()?;
        match raw {
            Event::Key(key) if key.kind == KeyEventKind::Press => Ok(Some(Msg::Key(key))),
            Event::Key(_) => Ok(None),
            Event::Paste(text) => Ok(Some(Msg::Paste(text))),
            Event::Mouse(mouse) => Ok(self.translate_mouse(mouse)),
            _ => Ok(None),
        }
    }

    fn translate_mouse(&mut self, mouse: CtMouseEvent) -> Option<Msg> {
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let (scroll, extra) = aggregate_scroll(
                    mouse.column,
                    mouse.row,
                    scroll_delta(mouse.kind, self.app.ui_config.mouse_scroll_lines),
                    self.app.ui_config.mouse_scroll_lines,
                );
                if let Some(extra) = extra {
                    let actions = self.app.update(scroll);
                    self.dispatch(actions);
                    Some(extra)
                } else {
                    Some(scroll)
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let (drag, extra) = coalesce_drag(mouse);
                let actions = self.app.update(Msg::Mouse(drag));
                self.dispatch(actions);
                extra
            }
            _ => Some(Msg::Mouse(mouse)),
        }
    }

    fn dispatch(&mut self, actions: Vec<Action>) {
        for action in actions {
            self.handle_action(action);
        }
    }

    fn respawn_with_tool_outputs(&mut self, loaded: LoadedSession) {
        self.respawn_agent(loaded.messages);
        *lock(&self.handles.tool_outputs) = loaded.tool_outputs;
    }

    fn respawn_agent(&mut self, history: Vec<Message>) {
        let lua_handle = self.app.lua_event_handle.clone();
        self.handles.respawn(
            history,
            &self.model_slot,
            self.config.clone(),
            self.compression.clone(),
            self.app.ui_config.tool_output_lines,
            &self.permissions,
            &mut self.app,
            lua_handle,
        );
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::SendMessage(input) => {
                let mut input = *input;
                input.preamble = self.app.shell.drain_results();
                let run_id = self.app.run_id;
                self.handles.queue.push(QueueItem::Message {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                    input,
                    run_id,
                    displayed: true,
                });
            }
            Action::CancelAgent { run_id } => {
                let _ = self
                    .handles
                    .cmd_tx
                    .try_send(AgentCommand::Cancel { run_id });
            }
            Action::NewSession => {
                self.respawn_agent(Vec::new());
            }
            Action::LoadSession(loaded) => {
                let loaded = *loaded;
                let model_spec = loaded.model_spec.clone();
                if model_spec != self.model_slot.load().model.spec() {
                    let timeouts = self.timeouts;
                    let tx = self.action_tx.clone();
                    tokio::spawn(async move {
                        let result = match Model::from_spec(&model_spec) {
                            Ok(model) => from_model(&model, timeouts)
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
                    self.respawn_with_tool_outputs(loaded);
                }
            }
            Action::ChangeModel(spec) => self.change_model(spec),
            Action::AssignTier(spec, tier) => {
                craft_providers::tier_map::set_and_persist(spec, tier, &self.app.storage);
            }
            Action::Compact => {
                self.handles.queue.push(QueueItem::Compact {
                    run_id: self.app.run_id,
                });
            }
            Action::ToggleMcp(server_name, enabled) => {
                self.handles.send_mcp(McpCommand::Toggle {
                    server: server_name,
                    enabled,
                });
            }
            Action::ShellCommand {
                id,
                command,
                visible,
            } => {
                let (trigger, cancel) = CancelToken::new();
                self.app.shell.add_trigger(trigger);
                spawn_shell(
                    command,
                    id,
                    visible,
                    self.shell_tx.clone(),
                    cancel,
                    self.config.clone(),
                );
            }
            Action::OpenEditor(path) => {
                if let Err(e) = terminal::open_in_editor(&path, self.terminal) {
                    self.app.flash(e);
                }
            }
            Action::EditInputInEditor => {
                let current_text = self.app.input_box.buffer.value();
                match terminal::edit_temp_content(&current_text, self.terminal) {
                    Ok(edited) => self.app.input_box.set_input(edited),
                    Err(e) => self.app.flash(e),
                }
            }
            Action::Btw(question) => {
                let slot = self.model_slot.load();
                self.app
                    .start_btw(question, Arc::clone(&slot.provider), slot.model.clone());
            }
            Action::Suspend => terminal::suspend(self.terminal),
            Action::Quit => {}
            Action::ProviderReady {
                model_spec,
                provider,
                pending_load_session,
            } => {
                match provider {
                    Ok(new_provider) => {
                        if let Ok(new_model) = Model::from_spec(&model_spec) {
                            self.app.update_model(&new_model);
                            self.model_slot.store(Arc::new(ModelSlot {
                                model: new_model,
                                provider: new_provider,
                            }));
                        }
                    }
                    Err(e) => self.app.flash(format!("Failed to create provider: {e}")),
                }
                if let Some(loaded) = pending_load_session {
                    self.respawn_with_tool_outputs(*loaded);
                }
            }
        }
    }

    fn change_model(&mut self, spec: String) {
        match Model::from_spec(&spec) {
            Ok(new_model) => {
                let model_spec = new_model.spec();
                if model_spec == self.model_slot.load().model.spec() {
                    return;
                }
                let timeouts = self.timeouts;
                let tx = self.action_tx.clone();
                tokio::spawn(async move {
                    let result = from_model(&new_model, timeouts)
                        .await
                        .map(|p| Arc::from(p) as Arc<dyn Provider>)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Action::ProviderReady {
                        model_spec,
                        provider: result,
                        pending_load_session: None,
                    });
                });
            }
            Err(e) => self.app.flash(format!("Invalid model: {e}")),
        }
    }

    fn shutdown(mut self) -> ShutdownResult {
        let exit_code = self.app.exit_request.code();
        let session_id = self
            .app
            .has_content()
            .then(|| self.app.state.session.id.clone());
        craft_agent::mcp::kill_process_groups(&self.handles.mcp_reader().load().pids);
        self.app.cmd_tx = None;
        self.app.answer_tx = None;
        drop(self.app);
        self.handles.send_cancel_all();
        ShutdownResult {
            session_id,
            exit_code,
            handles: self.handles,
            storage_writer: self.storage_writer,
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

fn aggregate_scroll(
    column: u16,
    row: u16,
    mut delta: i32,
    scroll_lines: u32,
) -> (Msg, Option<Msg>) {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        if let Ok(Event::Mouse(next)) = event::read() {
            match next.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    delta += scroll_delta(next.kind, scroll_lines);
                }
                _ => return (Msg::Scroll { column, row, delta }, Some(Msg::Mouse(next))),
            }
        } else {
            break;
        }
    }
    (Msg::Scroll { column, row, delta }, None)
}

fn coalesce_drag(mut latest: CtMouseEvent) -> (CtMouseEvent, Option<Msg>) {
    while event::poll(Duration::ZERO).unwrap_or(false) {
        if let Ok(Event::Mouse(next)) = event::read() {
            if matches!(next.kind, MouseEventKind::Drag(MouseButton::Left)) {
                latest = next;
            } else {
                return (latest, Some(Msg::Mouse(next)));
            }
        } else {
            break;
        }
    }
    (latest, None)
}
