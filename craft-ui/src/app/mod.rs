//! Elm-style `update(Msg) -> Vec<Action>`; side effects are dispatched by the caller.
//! Double-esc: first esc flashes a hint, second within `flash_duration` cancels/rewinds.
//! `run_id` increments each run so stale events from previous agent runs are ignored.

mod btw;
mod image_paste;
pub(crate) mod mode;
mod mouse;
mod queue;
mod session;
pub(crate) mod session_state;
pub(crate) mod shell;
#[cfg(test)]
mod tests;
pub(crate) mod view;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::AppSession;
use crate::agent::shared_queue::lock;
use crate::chat::Chat;
use crate::chat::{CANCELLED_TEXT, ChatEventResult, DONE_TEXT, ERROR_TEXT};
use crate::clipboard::ClipboardState;
use crate::components::btw_modal::BtwModal;
use crate::components::command::{CommandAction, CommandPalette, ParsedCommand};
use crate::components::file_picker::{FilePickerModal, FilePickerModalAction};
use crate::components::flow_goal_prompt::{FlowGoalAnswer, FlowGoalPrompt};
use crate::components::flow_graph::FlowGraph;
use crate::components::flow_panel::{FlowPanel, FlowPanelAction, FlowSnapshot, FlowSnapshotChunk};
use crate::components::help_modal::HelpModal;
use crate::components::input::{InputAction, InputBox, Submission};
use crate::components::keybindings::{ActionId, KeybindingResolver, key, normalize_key};
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::login_picker::{LoginPicker, LoginPickerAction};
use crate::components::lua_float::FloatManager;
use crate::components::mcp_picker::{McpPicker, McpPickerAction};
use crate::components::model_picker::{ModelPicker, ModelPickerAction};
use crate::components::permission_prompt::PermissionPrompt;
use crate::components::plan_form::{PlanForm, PlanFormAction};
use crate::components::recipe_picker::{RecipePicker, RecipePickerAction};
use crate::components::rewind_picker::{RewindPicker, RewindPickerAction};
use crate::components::scrollbar;
use crate::components::search_modal::{SearchAction, SearchModal};
use crate::components::stats_modal::StatsModal;
use crate::components::status_bar::StatusBar;
use crate::components::theme_picker::{ThemePicker, ThemePickerAction};
use crate::components::tool_display::format_turn_usage;
use crate::components::usage_modal::{UsageFetchState, UsageModal};
use crate::components::{
    Action, DisplayMessage, DisplayRole, ExitRequest, Overlay, RetryInfo, Status, is_ctrl,
};
use crate::image;
use crate::image_render::ImagePicker;
use crate::selection::{SelectionState, ZoneRegistry};
use arc_swap::{ArcSwap, ArcSwapOption};
use craft_agent::permissions::PermissionManager;
use craft_agent::{
    AgentEvent, Envelope, ImageSource, McpConfigErrors, McpPromptInfo, McpSnapshotReader,
    SubagentInfo, ToolOutput,
};
use craft_config::UiConfig;
use craft_lua::{EventHandle, HintReader, KeymapReader, LuaCommandReader};
use craft_providers::{Message, Model, ThinkingConfig};
use craft_storage::StateDir;
use craft_storage::input_history::InputHistory;
use craft_storage::model::persist_model;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};

use crate::storage_writer::StorageWriter;
use ratatui::layout::Position;

pub(crate) use crate::agent::QueuedMessage;
pub(crate) use mode::{Mode, PlanState, PlanTrigger};
#[cfg(test)]
use mouse::EDGE_SCROLL_LINES;
pub(crate) use queue::{MessageQueue, SubmitOutcome};
pub(crate) use session::session_has_content;
use session_state::SessionState;

pub(crate) const RESTORE_RUN_ID: u64 = u64::MAX;

const CANCEL_MSG: &str = "Cancelled.";
const FLASH_CANCEL: &str = "Press esc again to stop...";
const FLASH_REWIND: &str = "Press esc again to rewind...";
const FAST_UNSUPPORTED_MSG: &str = "Fast mode requires an Anthropic Opus 4.6+ model (API only)";
const FAST_ON_MSG: &str = "Fast mode: on";
const FAST_OFF_MSG: &str = "Fast mode: off";
const SET_CONTEXT_WINDOW_USAGE: &str = "Usage: /set-context-window [model] <tokens> (tokens > 0)";
const AUTH_EXPIRED_MSG: &str =
    "Token expired. Run `craft auth login` in another terminal, then press Enter to retry.";
const FLASH_NO_PLAN: &str = "No plan file";
const IMPLEMENT_MSG_PREFIX: &str = "Implement the plan";
const IMPLEMENT_PARALLEL_HINT: &str = "Use batch+task to parallelize, assign each subagent a separate module and restrict its tests to that module to avoid interference.";

const TASK_DONE_DETAIL: &str = "✓ ";

/// `Option<bool>` lets us distinguish the main chat (None, no status indicator)
/// from subagents (Some, with spinner or checkmark).
#[derive(Clone)]
pub(super) struct TaskEntry {
    name: String,
    finished: Option<bool>,
}

impl PickerItem for TaskEntry {
    fn label(&self) -> &str {
        &self.name
    }
    fn detail(&self) -> Option<&str> {
        matches!(self.finished, Some(true)).then_some(TASK_DONE_DETAIL)
    }
    fn is_spinning(&self) -> bool {
        matches!(self.finished, Some(false))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) enum PendingInput {
    #[default]
    None,
    AuthRetry {
        subagent_id: Option<String>,
    },
}

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Scroll { column: u16, row: u16, delta: i32 },
    Agent(Box<Envelope>),
    FlowProgress(craft_flow::FlowProgress),
}

pub struct App {
    pub(super) chats: Vec<Chat>,
    pub(super) active_chat: usize,
    pub(super) chat_index: HashMap<String, usize>,
    pub(crate) input_box: InputBox,
    pub(super) command_palette: CommandPalette,
    pub(super) task_picker: ListPicker<TaskEntry>,
    pub(super) task_picker_original: Option<usize>,
    pub(super) theme_picker: ThemePicker,
    pub(super) model_picker: ModelPicker,
    pub(super) login_picker: LoginPicker,
    pub(super) mcp_picker: McpPicker,
    pub(super) recipe_picker: RecipePicker,
    pub(super) rewind_picker: RewindPicker,
    pub(super) help_modal: HelpModal,
    pub(super) usage_modal: UsageModal,
    pub(super) stats_modal: StatsModal,
    pub(super) btw_modal: BtwModal,
    pub(super) float_mgr: FloatManager,
    pub(super) search_modal: SearchModal,
    pub(super) file_picker: FilePickerModal,
    pub(super) permission_prompt: PermissionPrompt,
    pub(super) plan_form: PlanForm,
    pub(super) flow_panel: FlowPanel,
    pub(super) flow_graph: FlowGraph,
    pub(super) image_picker: ImagePicker,
    pub(super) flow_goal_prompt: FlowGoalPrompt,
    /// Index of the selected chunk in the flow panel, if any. Drives the right
    /// pane of the flow-graph split to show that chunk's subagent history.
    pub(super) flow_selected_chunk: Option<String>,
    /// True while the flow pipeline is paused at the goal-approval gate. The
    /// next submit is routed to the agent's answer channel (as the approval
    /// payload) instead of starting a new agent turn.
    pub(super) flow_awaiting_approval: bool,
    /// True when the last flow run ended in `FlowOutcome::Failed` and the
    /// workstream has persisted state worth resuming. Drives the in-app retry
    /// affordance: submitting in Flow mode while this is set re-enters the
    /// pipeline with `flow_resume = true` instead of starting fresh.
    pub(super) flow_failed: bool,
    /// Max concurrent chunks from config; drives the graph's parallel layout.
    pub(super) flow_parallel_chunks: u32,
    pub(super) status_bar: StatusBar,
    pub status: Status,
    pub(crate) state: session_state::SessionState,
    pub exit_request: ExitRequest,
    pub(crate) exit_on_done: bool,
    pub(crate) queue: MessageQueue,
    pub answer_tx: Option<flume::Sender<String>>,
    pub(crate) cmd_tx: Option<flume::Sender<super::AgentCommand>>,
    pub(super) pending_input: PendingInput,
    pub(crate) run_id: u64,
    pub(super) retry_info: Option<RetryInfo>,
    pub(super) zones: ZoneRegistry,
    pub(super) selection_state: Option<SelectionState>,
    pub(super) clipboard: ClipboardState,
    pub(super) last_esc: Option<Instant>,

    pub(crate) storage: StateDir,
    pub(crate) usage_slot: Arc<ArcSwapOption<UsageFetchState>>,
    pub(crate) shared_history: Option<Arc<ArcSwap<Vec<Message>>>>,
    pub(crate) shared_tool_outputs: Option<Arc<Mutex<HashMap<String, ToolOutput>>>>,
    pub(crate) btw_system: Option<Arc<ArcSwap<String>>>,
    pub(crate) image_paste_rx: Vec<flume::Receiver<Result<ImageSource, String>>>,
    storage_writer: Arc<StorageWriter>,
    pub(crate) shell: shell::ShellState,
    pub(crate) ui_config: UiConfig,
    pub(crate) keybindings: Arc<KeybindingResolver>,
    pub(crate) permissions: Arc<PermissionManager>,
    pub(crate) lua_event_handle: Option<EventHandle>,
    pub(super) keymap_reader: KeymapReader,
    pub(super) hint_reader: HintReader,
    subagent_answers: HashMap<String, flume::Sender<String>>,
    pub(crate) restore_event_tx: Option<craft_agent::EventSender>,
    pub(super) restoring: Arc<AtomicBool>,
    pub(super) repomap_enabled: Arc<std::sync::atomic::AtomicBool>,
    pub(super) watch_enabled: bool,
    /// Per-session warning channel drained by the event loop's selector.
    /// Background tasks (wiki ingest, model fetch) post human-readable
    /// messages here so they surface as flashes on the owning session.
    pub(crate) warn_tx: Option<flume::Sender<String>>,
}

macro_rules! define_overlays {
    ($($field:ident),+ $(,)?) => {
        fn overlays(&self) -> Vec<&dyn Overlay> {
            vec![$(&self.$field,)+]
        }
        fn overlays_mut(&mut self) -> Vec<&mut dyn Overlay> {
            vec![$(&mut self.$field,)+]
        }
    };
}

/// True for per-chunk pipeline stages (Req/Execute/Review/Qa) that carry a
/// chunk id in their `flow_stage_id`, as opposed to top-level stages.
fn is_chunk_stage(stage: craft_flow::Stage) -> bool {
    matches!(
        stage,
        craft_flow::Stage::Req
            | craft_flow::Stage::Execute
            | craft_flow::Stage::Review
            | craft_flow::Stage::Qa
    )
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &Model,
        session: AppSession,
        storage: StateDir,
        available_models: Arc<ArcSwapOption<Vec<String>>>,
        mcp_reader: McpSnapshotReader,
        mcp_config_errors: McpConfigErrors,
        lua_command_reader: LuaCommandReader,
        keymap_reader: KeymapReader,
        hint_reader: HintReader,
        storage_writer: Arc<StorageWriter>,
        ui_config: UiConfig,
        input_history_size: usize,
        permissions: Arc<PermissionManager>,
        custom_commands: Arc<[craft_agent::command::CustomCommand]>,
        repomap_enabled: bool,
        watch_enabled: bool,
    ) -> Self {
        scrollbar::set_enabled(ui_config.scrollbar);
        let state = SessionState::from_session(session, model, &storage);
        let keybindings = {
            let mut warnings = Vec::new();
            let resolver =
                KeybindingResolver::from_overlay(&ui_config.keybindings.entries, &mut warnings);
            for warning in &warnings {
                tracing::warn!(%warning, "keybinding config");
            }
            Arc::new(resolver)
        };
        let mut app = Self {
            chats: vec![Chat::new("Main".into(), ui_config.clone())],
            active_chat: 0,
            chat_index: HashMap::new(),
            input_box: InputBox::new(InputHistory::load(&storage, input_history_size)),
            command_palette: CommandPalette::new(
                custom_commands,
                mcp_reader.clone(),
                lua_command_reader,
            ),
            task_picker: ListPicker::new(),
            task_picker_original: None,
            theme_picker: ThemePicker::new(),
            model_picker: ModelPicker::new(available_models),
            login_picker: LoginPicker::new(),
            mcp_picker: McpPicker::new(mcp_reader, mcp_config_errors),
            recipe_picker: RecipePicker::new(),
            rewind_picker: RewindPicker::new(),
            help_modal: HelpModal::new(),
            usage_modal: UsageModal::new(),
            stats_modal: StatsModal::new(),
            btw_modal: BtwModal::new(ui_config.typewriter_ms_per_char),
            float_mgr: FloatManager::new(),
            search_modal: SearchModal::new(),
            file_picker: FilePickerModal::new(),
            permission_prompt: PermissionPrompt::new(),
            plan_form: PlanForm::new(),
            flow_panel: FlowPanel::new(),
            flow_graph: FlowGraph::new(),
            image_picker: ImagePicker::new(),
            flow_goal_prompt: FlowGoalPrompt::new(),
            flow_selected_chunk: None,
            flow_awaiting_approval: false,
            flow_failed: false,
            flow_parallel_chunks: 1,
            status_bar: StatusBar::new(ui_config.flash_duration()),
            status: Status::Idle,
            state,
            exit_request: ExitRequest::None,
            exit_on_done: false,
            queue: MessageQueue::default(),
            answer_tx: None,
            cmd_tx: None,
            pending_input: PendingInput::None,
            run_id: 0,
            retry_info: None,
            zones: ZoneRegistry::new(),
            selection_state: None,
            clipboard: ClipboardState::new(),
            last_esc: None,
            storage,
            usage_slot: Arc::new(ArcSwapOption::empty()),
            shared_history: None,
            shared_tool_outputs: None,
            btw_system: None,
            image_paste_rx: vec![],
            storage_writer,
            shell: shell::ShellState::default(),
            ui_config,
            keybindings,
            permissions,
            lua_event_handle: None,
            keymap_reader,
            hint_reader,
            subagent_answers: HashMap::new(),
            restore_event_tx: None,
            restoring: Arc::new(AtomicBool::new(false)),
            repomap_enabled: Arc::new(std::sync::atomic::AtomicBool::new(repomap_enabled)),
            watch_enabled,
            warn_tx: None,
        };
        app.task_picker.set_keybindings(app.keybindings.clone());
        app.theme_picker.set_keybindings(app.keybindings.clone());
        app.model_picker.set_keybindings(app.keybindings.clone());
        app.model_picker
            .set_recents(craft_storage::model::read_recents(&app.storage));
        // `/sessions` is now a Lua plugin (plugins/sessions) driving craft.session.*.
        app.rewind_picker.set_keybindings(app.keybindings.clone());
        app.login_picker.set_keybindings(app.keybindings.clone());
        app.mcp_picker.set_keybindings(app.keybindings.clone());
        app.plan_form.set_keybindings(app.keybindings.clone());
        app.flow_panel.set_keybindings(app.keybindings.clone());
        app
    }

    pub(crate) fn main_chat(&mut self) -> &mut Chat {
        &mut self.chats[0]
    }

    pub(crate) fn propagate_lua_handles(&mut self) {
        let handle = self.lua_event_handle.clone();
        let tx = self.restore_event_tx.clone();
        for chat in &mut self.chats {
            chat.set_lua_event_handle(handle.clone());
            chat.set_restore_event_tx(tx.clone());
        }
    }

    pub(crate) fn dispatch_pending_restores(&mut self) {
        let items: Vec<_> = self
            .chats
            .iter_mut()
            .flat_map(|c| c.drain_pending_restores())
            .collect();
        let Some(handle) = &self.lua_event_handle else {
            return;
        };
        let Some(event_tx) = &self.restore_event_tx else {
            return;
        };
        for item in items {
            handle.request_restore(item, event_tx.clone());
        }
    }

    fn is_main_chat(&self) -> bool {
        self.active_chat == 0
    }

    fn plan_form_active(&self) -> bool {
        self.state.mode == Mode::Plan && self.plan_form.is_visible()
    }

    fn flow_panel_active(&self) -> bool {
        self.state.mode == Mode::Flow && self.flow_panel.is_visible()
    }

    /// True when the flow-graph split should replace the messages area: Flow
    /// mode + panel open + not waiting at the goal-approval overlay.
    fn flow_graph_active(&self) -> bool {
        self.flow_panel_active() && !self.flow_awaiting_approval
    }

    /// Resolve the chat index for the subagent currently driving the flow, so
    /// the right pane of the flow-graph split can render its live transcript.
    ///
    /// Top-level stages (Scout/TPM/Plan/Integrator/Verifier) map to
    /// `flow:{ws}:{stage}`; chunk stages (Req/Execute/Review/Qa) map to
    /// `flow:{ws}:{sub_stage}:{chunk}` using the selected chunk's current
    /// sub-stage (falling back to the first running chunk). Returns `None`
    /// when no matching subagent chat exists yet.
    fn resolve_flow_subagent_chat(&self) -> Option<usize> {
        let workstream = &self.state.flow.workstream_id;
        let candidate_ids: Vec<String> = match self.state.flow.stage {
            Some(stage) if is_chunk_stage(stage) => {
                let chunk_id = self.flow_selected_chunk.clone().or_else(|| {
                    self.state
                        .flow
                        .chunks
                        .iter()
                        .find(|(_, c)| c.status == craft_flow::ChunkStatus::Running)
                        .map(|(id, _)| id.clone())
                })?;
                let sub_stage = self
                    .state
                    .flow
                    .chunks
                    .get(&chunk_id)
                    .and_then(|c| c.stage)
                    .unwrap_or(craft_flow::Stage::Req);
                vec![format!(
                    "flow:{workstream}:{}:{chunk_id}",
                    sub_stage.as_str()
                )]
            }
            Some(stage) => vec![format!("flow:{workstream}:{}", stage.as_str())],
            None => vec![],
        };
        for id in candidate_ids {
            if let Some(&idx) = self.chat_index.get(id.as_str()) {
                return Some(idx);
            }
        }
        None
    }

    /// Whether `key` matches a global shortcut (Tasks, chat cycling, queue
    /// pop, etc) that should fire even while the flow panel is visible.
    fn is_global_shortcut(&self, key: KeyEvent) -> bool {
        use crate::components::keybindings::ActionId;
        self.keybindings.matches(ActionId::Tasks, key)
            || self.keybindings.matches(ActionId::NextChat, key)
            || self.keybindings.matches(ActionId::PrevChat, key)
            || self.keybindings.matches(ActionId::PopQueue, key)
            || self.keybindings.matches(ActionId::Quit, key)
            || self.keybindings.matches(ActionId::Search, key)
            || self.keybindings.matches(ActionId::FilePicker, key)
            || self.keybindings.matches(ActionId::Help, key)
            || self.keybindings.matches(ActionId::OpenEditor, key)
    }

    /// Push the current `FlowState` into the panel's render snapshot. Called
    /// each frame before rendering so the panel always reflects live state
    /// without holding a borrow into `SessionState`.
    fn sync_flow_snapshot(&mut self) {
        let snapshot = FlowSnapshot {
            workstream_id: self.state.flow.workstream_id.clone(),
            stage: self.state.flow.stage,
            parallel_chunks: self.flow_parallel_chunks,
            chunks: self
                .state
                .flow
                .chunks
                .iter()
                .map(|(id, c)| {
                    (
                        id.clone(),
                        FlowSnapshotChunk {
                            title: c.title.clone(),
                            status: c.status,
                            stage: c.stage,
                            order: c.order,
                            depends_on: c.depends_on.clone(),
                        },
                    )
                })
                .collect(),
        };
        self.flow_panel.set_snapshot(snapshot);
    }

    /// Update a single chunk's status in the active workstream, surfaced from
    /// craft-flow stage events. No-op outside Flow mode.
    #[allow(dead_code)]
    pub(crate) fn update_flow_chunk(&mut self, chunk_id: &str, status: craft_flow::ChunkStatus) {
        if self.state.mode != Mode::Flow {
            return;
        }
        self.state.flow.set_chunk_status(chunk_id, status);
    }

    /// Bulk-replace the workstream's stage + chunks from a craft-flow snapshot.
    #[allow(dead_code)]
    pub(crate) fn sync_flow_state(
        &mut self,
        stage: Option<craft_flow::Stage>,
        chunks: impl IntoIterator<Item = (String, String, craft_flow::ChunkStatus)>,
    ) {
        if self.state.mode != Mode::Flow {
            return;
        }
        self.state.flow.sync_from(stage, chunks);
    }

    /// Apply a `FlowProgress` event from the pipeline to the live `FlowState`
    /// (so the FlowPanel reflects stage/chunk transitions) or open the
    /// goal-approval overlay at the gate. No-op outside Flow mode.
    fn handle_flow_progress(&mut self, p: craft_flow::FlowProgress) -> Vec<Action> {
        if self.state.mode != Mode::Flow {
            return vec![];
        }
        match p {
            craft_flow::FlowProgress::Stage(stage) => {
                if stage == craft_flow::Stage::Scout {
                    self.state.flow.clear_chunks();
                }
                self.state.flow.stage = Some(stage);
                self.flow_failed = false;
            }
            craft_flow::FlowProgress::Chunk {
                id,
                title,
                status,
                stage,
                depends_on,
                order,
            } => {
                self.state
                    .flow
                    .set_chunk(&id, &title, status, stage, order, &depends_on);
            }
            craft_flow::FlowProgress::GoalReady { goal_doc } => {
                self.flow_awaiting_approval = true;
                self.flow_goal_prompt.open(goal_doc);
            }
            craft_flow::FlowProgress::Done { .. } => {
                // On success every chunk should already be Done (each chunk
                // emits its own Done after QA passes). Do NOT finalize here:
                // forcing still-running chunks to Blocked on a successful run
                // would mask a real missed-transition bug. Finalize only runs
                // on Failed/Cancelled below.
                self.flow_failed = false;
                self.flash("Flow run complete.".into());
            }
            craft_flow::FlowProgress::NeedsReview { .. } => {
                self.flow_failed = false;
                self.flash("Flow verification needs review.".into());
            }
            craft_flow::FlowProgress::Failed { stage, reason } => {
                self.state.flow.finalize_non_terminal();
                self.flow_failed = true;
                self.status = Status::error(format!(
                    "flow {stage:?} failed: {reason} (press Enter to retry)"
                ));
            }
            craft_flow::FlowProgress::Cancelled => {
                self.state.flow.finalize_non_terminal();
                self.flow_failed = false;
                self.status = Status::error("flow run cancelled".into());
            }
        }
        vec![]
    }

    pub(crate) fn update_model(&mut self, model: &Model) {
        self.state.update_model(model);
        persist_model(&self.storage, &self.state.session.model).ok();
    }

    pub(crate) fn flash(&mut self, msg: String) {
        self.status_bar.flash(msg);
    }

    pub(crate) fn record_recent_model(&mut self, spec: &str) {
        let recents = craft_storage::model::push_recent(&self.storage, spec);
        self.model_picker.set_recents(recents);
    }

    pub fn tick_error_expiry(&mut self) {
        if self.status.is_error_expired() {
            self.status = Status::Idle;
        }
    }

    fn active_chat(&mut self) -> &mut Chat {
        &mut self.chats[self.active_chat]
    }

    fn clear_selection_unless_pending_copy(&mut self) {
        if !self
            .selection_state
            .as_ref()
            .is_some_and(|s| s.is_pending_copy())
        {
            self.selection_state = None;
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Action> {
        match msg {
            Msg::Key(key) => self.handle_key(key),
            Msg::Paste(text) => {
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                if text.is_empty() {
                    if self.is_main_chat() && self.image_paste_rx.is_empty() {
                        self.start_image_paste();
                    }
                } else {
                    let mut any_image = false;
                    if self.is_main_chat() {
                        for line in text.lines() {
                            if let Some((path, mt)) = image::try_parse_image_path(line) {
                                self.start_file_image_paste(path, mt);
                                any_image = true;
                            }
                        }
                    }
                    if !any_image {
                        self.route_text_paste(&text);
                    }
                }
                vec![]
            }
            Msg::Mouse(event) => {
                self.handle_mouse(event);
                vec![]
            }
            Msg::Scroll { column, row, delta } => {
                self.clear_selection_unless_pending_copy();
                self.handle_scroll(column, row, delta);
                vec![]
            }
            Msg::Agent(envelope) => self.handle_agent_event(*envelope),
            Msg::FlowProgress(p) => self.handle_flow_progress(p),
        }
    }

    fn send_answer(&self, answer: String) {
        if let Some(tx) = &self.answer_tx {
            let _ = tx.try_send(answer);
        }
    }

    fn send_to_agent(&self, subagent_id: Option<&str>, answer: String) {
        let routed = subagent_id.and_then(|id| self.subagent_answers.get(id));
        if let Some(tx) = routed {
            let _ = tx.try_send(answer);
        } else {
            self.send_answer(answer);
        }
    }

    fn handle_scroll(&mut self, column: u16, row: u16, delta: i32) {
        if self.btw_modal.is_open() {
            self.btw_modal.scroll(delta);
            return;
        }
        if self.help_modal.is_open() {
            self.help_modal.scroll(delta);
            return;
        }
        if self.usage_modal.is_open() {
            self.usage_modal.scroll(delta);
            return;
        }
        if self.stats_modal.is_open() {
            self.stats_modal.scroll(delta);
            return;
        }
        let pos = Position::new(column, row);
        if self.float_mgr.is_open() && self.float_mgr.contains(pos) {
            self.float_mgr.scroll(delta);
            return;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.is_open() {
                    if $picker.contains(pos) {
                        $picker.scroll(delta);
                    }
                    return;
                }
            };
        }
        try_picker!(self.rewind_picker);
        try_picker!(self.task_picker);
        try_picker!(self.model_picker);
        try_picker!(self.file_picker);
        if let Some(zone) = self.zone_at(row, column) {
            self.scroll_zone(zone.zone, delta);
        }
    }

    fn open_tasks(&mut self) {
        let entries: Vec<TaskEntry> = self
            .chats
            .iter()
            .enumerate()
            .map(|(i, c)| TaskEntry {
                name: c.name.clone(),
                finished: (i > 0).then_some(c.is_finished()),
            })
            .collect();
        self.task_picker_original = Some(self.active_chat);
        self.task_picker.open(entries, " Tasks ");
        self.task_picker.select(self.active_chat);
    }

    /// Ctrl shortcuts that apply when no overlay owns input.
    fn handle_ctrl(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if !is_ctrl(&key) {
            return None;
        }
        if self.keybindings.matches(ActionId::Quit, key) {
            self.command_palette.close();
            return Some(if !self.is_main_chat() || self.input_box.is_empty() {
                if self.status == Status::Streaming {
                    return Some(self.handle_cancel());
                }
                self.quit()
            } else {
                self.input_box.discard();
                vec![]
            });
        }
        if self.keybindings.matches(ActionId::Help, key) {
            self.help_modal.toggle();
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::Tasks, key) {
            self.open_tasks();
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::PrevChat, key) {
            self.active_chat = self.active_chat.saturating_sub(1);
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::NextChat, key) {
            self.active_chat = (self.active_chat + 1).min(self.chats.len() - 1);
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::ScrollHalfUp, key) {
            let half = self.chats[self.active_chat].half_page();
            self.active_chat().scroll(half);
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::ScrollHalfDown, key) {
            let half = self.chats[self.active_chat].half_page();
            self.active_chat().scroll(-half);
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::ScrollTop, key) {
            self.active_chat().scroll_to_top();
            return Some(vec![]);
        }
        if self.keybindings.matches(ActionId::ScrollBottom, key) {
            self.active_chat().enable_auto_scroll();
            return Some(vec![]);
        }
        None
    }

    /// Routes input to whichever overlay currently owns focus.
    /// Returns `Some` when an overlay is open (consuming the key),
    /// `None` when no overlay is active and input should continue.
    fn dispatch_overlay(&mut self, key: KeyEvent) -> Option<Vec<Action>> {
        if self.flow_goal_prompt.is_open() {
            if let Some(answer) = self.flow_goal_prompt.handle_key(key) {
                let encoded = match answer {
                    FlowGoalAnswer::Approve => craft_flow::FLOW_APPROVE_ANSWER.to_owned(),
                    FlowGoalAnswer::Cancel => craft_flow::FLOW_CANCEL_ANSWER.to_owned(),
                    FlowGoalAnswer::Revise(text) => text,
                };
                self.flow_goal_prompt.close();
                self.flow_awaiting_approval = false;
                self.send_answer(encoded);
            }
            return Some(vec![]);
        }

        if self.permission_prompt.is_open() {
            if let Some(answer) = self.permission_prompt.handle_key(key) {
                let subagent_id = self.permission_prompt.subagent_id().map(str::to_owned);
                let encoded = answer.encode();
                self.permission_prompt.close();
                self.send_to_agent(subagent_id.as_deref(), encoded);
            }
            return Some(vec![]);
        }

        if self.recipe_picker.is_open() {
            match self.recipe_picker.handle_key(key) {
                RecipePickerAction::Consumed => {}
                RecipePickerAction::Select(path) => {
                    self.recipe_picker.close();
                    return Some(self.run_recipe(&path));
                }
                RecipePickerAction::Close => {
                    self.recipe_picker.close();
                }
            }
            return Some(vec![]);
        }

        // plan_form is non-modal: Passthrough falls through to the rest of dispatch
        if self.plan_form_active() {
            let action = self.plan_form.handle_key(key);
            if action != PlanFormAction::Passthrough {
                return Some(self.handle_plan_form_action(action));
            }
        }

        // flow_panel is non-modal: it hides on Esc/Ctrl+Q/Ctrl+T but passes
        // global shortcuts (Tasks, NextChat, PrevChat, PopQueue, etc) through
        // so they still work while the panel is visible. Skip entirely while
        // an overlay (task picker, search, file picker, etc) has focus so the
        // overlay's keys (Up/Down/Enter) aren't swallowed.
        let overlay_open = self.task_picker.is_open()
            || self.search_modal.is_open()
            || self.file_picker.is_open()
            || self.help_modal.is_open()
            || self.stats_modal.is_open()
            || self.btw_modal.is_open();
        if self.flow_panel_active() && !overlay_open {
            let action = self.flow_panel.handle_key(key);
            match action {
                FlowPanelAction::Hide => {
                    self.flow_panel.hide();
                    return Some(vec![]);
                }
                FlowPanelAction::Passthrough => {}
                FlowPanelAction::Consumed => {
                    self.flow_selected_chunk = self.flow_panel.selected_chunk().map(str::to_owned);
                    if self.is_global_shortcut(key) {
                        // fall through to normal dispatch
                    } else {
                        return Some(vec![]);
                    }
                }
            }
        }

        if self.help_modal.is_open() {
            self.help_modal.handle_key(key, &self.keybindings);
            return Some(vec![]);
        }

        if self.usage_modal.is_open() {
            if key::REFRESH.matches(key) {
                return Some(vec![Action::RefreshUsage]);
            }
            self.usage_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.stats_modal.is_open() {
            self.stats_modal.handle_key(key, &self.keybindings);
            return Some(vec![]);
        }

        if self.btw_modal.is_open() {
            self.btw_modal.handle_key(key);
            return Some(vec![]);
        }

        if self.float_mgr.handle_key(key) {
            return Some(vec![]);
        }

        if self.search_modal.is_open() {
            match self.search_modal.handle_key(key) {
                SearchAction::Consumed => {
                    let chat = &mut self.chats[self.active_chat];
                    let texts = chat.segment_search_texts();
                    self.search_modal.update_matches(&texts);
                    sync_search_highlight(&self.search_modal, chat);
                }
                SearchAction::Navigate => {
                    sync_search_highlight(&self.search_modal, &mut self.chats[self.active_chat]);
                }
                SearchAction::Select(idx) => {
                    let chat = &mut self.chats[self.active_chat];
                    chat.scroll_to_segment(idx);
                    chat.set_highlight_segment(None);
                    self.search_modal.close();
                }
                SearchAction::Close(saved) => {
                    let chat = &mut self.chats[self.active_chat];
                    chat.set_highlight_segment(None);
                    if let Some((top, auto)) = saved {
                        chat.restore_scroll(top, auto);
                    }
                    self.search_modal.close();
                }
            }
            return Some(vec![]);
        }

        if self.file_picker.is_open() {
            return Some(match self.file_picker.handle_key(key) {
                FilePickerModalAction::Consumed => vec![],
                FilePickerModalAction::Select(path) => {
                    self.file_picker.close();
                    if let InputAction::PaletteSync(val) =
                        self.input_box.handle_paste_with_spaces(&path)
                    {
                        self.command_palette.sync(&val);
                    }
                    vec![]
                }
                FilePickerModalAction::Close => {
                    self.file_picker.close();
                    vec![]
                }
            });
        }

        if self.queue.focus().is_some() {
            match key.code {
                KeyCode::Up => self.queue.move_focus_up(),
                KeyCode::Down => self.queue.move_focus_down(),
                KeyCode::Enter => {
                    self.queue.remove_focused();
                }
                KeyCode::Esc => self.queue.unfocus(),
                _ if self.keybindings.matches(ActionId::Quit, key) => self.queue.unfocus(),
                _ if self.keybindings.matches(ActionId::PopQueue, key) => {
                    self.queue.remove(0);
                }
                _ => {}
            }
            return Some(vec![]);
        }

        if self.task_picker.is_open() {
            if self.keybindings.matches(ActionId::Tasks, key) {
                self.task_picker.close();
                return Some(vec![]);
            }
            return Some(match self.task_picker.handle_key(key) {
                PickerAction::Consumed | PickerAction::Toggle(..) => vec![],
                PickerAction::Select(idx, _) => {
                    self.task_picker_original = None;
                    self.active_chat = idx;
                    vec![]
                }
                PickerAction::Close => {
                    self.active_chat = self.task_picker_original.take().unwrap_or(0);
                    vec![]
                }
            });
        }

        if self.rewind_picker.is_open() {
            return Some(match self.rewind_picker.handle_key(key) {
                RewindPickerAction::Consumed => vec![],
                RewindPickerAction::Select(entry) => self.rewind_to(entry),
                RewindPickerAction::Close => vec![],
            });
        }

        if self.theme_picker.is_open() {
            return Some(match self.theme_picker.handle_key(key) {
                ThemePickerAction::Consumed => vec![],
                ThemePickerAction::Closed => vec![],
            });
        }

        if self.model_picker.is_open() {
            return Some(match self.model_picker.handle_key(key) {
                ModelPickerAction::Consumed => vec![],
                ModelPickerAction::Select(spec) => {
                    vec![Action::ChangeModel(spec)]
                }
                ModelPickerAction::AssignTier(spec, tier) => {
                    vec![Action::AssignTier(spec, tier)]
                }
                ModelPickerAction::UnassignTier(spec, tier) => {
                    vec![Action::UnassignTier(spec, tier)]
                }
                ModelPickerAction::Close => vec![],
            });
        }

        if self.login_picker.is_open() {
            return Some(match self.login_picker.handle_key(key) {
                LoginPickerAction::Consumed => vec![],
                LoginPickerAction::Close => vec![],
                LoginPickerAction::Authenticated { model_spec } => {
                    vec![Action::ChangeModel(model_spec), Action::RefreshModels]
                }
                LoginPickerAction::Configured { slug } => {
                    vec![Action::RefreshProvider { slug }, Action::RefreshModels]
                }
            });
        }

        if self.mcp_picker.is_open() {
            return Some(match self.mcp_picker.handle_key(key) {
                McpPickerAction::Consumed => vec![],
                McpPickerAction::Toggle {
                    server_name,
                    enabled,
                } => {
                    vec![Action::ToggleMcp(server_name, enabled)]
                }
                McpPickerAction::Close => vec![],
            });
        }

        None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<Action> {
        self.clear_selection_unless_pending_copy();

        if self.keybindings.matches(ActionId::Suspend, key) && cfg!(unix) {
            return self.suspend();
        }

        if let Some(actions) = self.dispatch_overlay(key) {
            return actions;
        }

        if !(self.status == Status::Streaming && self.is_streaming_stop_key(key))
            && self.dispatch_override(key)
        {
            return vec![];
        }

        if let Some(actions) = self.handle_ctrl(key) {
            return actions;
        }

        if !self.is_main_chat() {
            return match key.code {
                KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                KeyCode::Esc if !self.chats[self.active_chat].is_finished() => {
                    if let Some(t) = self.last_esc.take()
                        && t.elapsed() < self.status_bar.flash_duration
                    {
                        self.handle_subagent_cancel()
                    } else {
                        self.last_esc = Some(Instant::now());
                        self.status_bar.flash(FLASH_CANCEL.into());
                        vec![]
                    }
                }
                _ => vec![],
            };
        }

        self.handle_main_chat_key(key)
    }

    fn dispatch_override(&self, key: KeyEvent) -> bool {
        let key = normalize_key(key);
        let snap = self.keymap_reader.load();
        for entry in &snap.entries {
            if entry.key == key.code
                && entry.modifiers == key.modifiers
                && let Some(ref handle) = self.lua_event_handle
                && handle.run_keybind_callback(entry.id)
            {
                return true;
            }
        }
        false
    }

    fn is_streaming_stop_key(&self, key: KeyEvent) -> bool {
        self.keybindings.matches(ActionId::Quit, key) || key.code == KeyCode::Esc
    }

    fn handle_main_chat_key(&mut self, key: KeyEvent) -> Vec<Action> {
        if self.keybindings.matches(ActionId::EditInput, key) {
            return vec![Action::EditInputInEditor];
        }
        if is_ctrl(&key) {
            if self.keybindings.matches(ActionId::PopQueue, key) {
                self.queue.remove(0);
            } else if self.keybindings.matches(ActionId::OpenEditor, key) {
                return match self.state.plan.path() {
                    Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                    None => {
                        self.flash(FLASH_NO_PLAN.into());
                        vec![]
                    }
                };
            } else if self.keybindings.matches(ActionId::PlanToggle, key) {
                match self.state.mode {
                    Mode::Plan => self.plan_form.toggle(),
                    Mode::Flow => self.flow_panel.toggle(),
                    _ => self.float_mgr.toggle_panel_visibility(),
                }
            } else if self.keybindings.matches(ActionId::Search, key) {
                let top = self.chats[self.active_chat].scroll_top();
                let auto = self.chats[self.active_chat].auto_scroll();
                self.search_modal.open(top, auto);
            } else if self.keybindings.matches(ActionId::FilePicker, key) {
                self.file_picker.open(&self.state.session.cwd);
            } else if key.code == KeyCode::Char('v') && self.image_paste_rx.is_empty() {
                self.start_image_paste();
            } else if let InputAction::PaletteSync(val) = self.input_box.handle_key(key) {
                self.command_palette.sync(&val);
            }
            return vec![];
        }

        match self
            .command_palette
            .handle_key(key, &self.input_box.buffer.value())
        {
            CommandAction::Consumed => return vec![],
            CommandAction::Execute(cmd) => return self.execute_command(cmd),
            CommandAction::Complete(text) => {
                self.command_palette.sync(&text);
                self.input_box.set_input(text);
                self.input_box.buffer.move_to_end();
                return vec![];
            }
            CommandAction::Passthrough => {}
        }

        let streaming = self.status == Status::Streaming;
        match self.input_box.handle_key(key) {
            InputAction::Submit(sub) => self.handle_submit(sub),
            InputAction::PaletteSync(val) => {
                self.command_palette.sync(&val);
                vec![]
            }
            InputAction::Passthrough(key) => {
                if key.code != KeyCode::Esc {
                    self.last_esc = None;
                }
                match key.code {
                    KeyCode::Up if streaming => {
                        self.active_chat().scroll(1);
                        vec![]
                    }
                    KeyCode::Down if streaming => {
                        self.active_chat().scroll(-1);
                        vec![]
                    }
                    KeyCode::Tab if !self.is_bash_input() => self.toggle_mode(),
                    KeyCode::Esc => {
                        if let Some(t) = self.last_esc.take()
                            && t.elapsed() < self.status_bar.flash_duration
                        {
                            if streaming {
                                self.handle_cancel()
                            } else {
                                self.open_rewind_picker()
                            }
                        } else {
                            self.last_esc = Some(Instant::now());
                            self.status_bar.flash(
                                if streaming {
                                    FLASH_CANCEL
                                } else {
                                    FLASH_REWIND
                                }
                                .into(),
                            );
                            vec![]
                        }
                    }
                    _ => vec![],
                }
            }
            InputAction::ContinueLine | InputAction::None => vec![],
        }
    }

    fn suspend(&mut self) -> Vec<Action> {
        vec![Action::Suspend]
    }

    fn quit(&mut self) -> Vec<Action> {
        self.quit_with(ExitRequest::Success)
    }

    fn quit_with(&mut self, req: ExitRequest) -> Vec<Action> {
        self.save_session();
        self.save_input_history();
        self.exit_request = req;
        vec![Action::Quit]
    }

    pub(crate) fn handle_submit(&mut self, sub: Submission) -> Vec<Action> {
        match std::mem::take(&mut self.pending_input) {
            PendingInput::AuthRetry { subagent_id } => {
                self.send_to_agent(subagent_id.as_deref(), String::new());
                return vec![];
            }
            PendingInput::None => {}
        }
        if self.flow_awaiting_approval {
            if !sub.text.trim().is_empty() {
                let text = sub.text.trim().to_owned();
                self.main_chat().show_user_message(text.clone());
                self.flow_awaiting_approval = false;
                self.flow_goal_prompt.close();
                let answer = match text.as_str() {
                    "cancel" => craft_flow::FLOW_CANCEL_ANSWER.to_owned(),
                    "approved" => craft_flow::FLOW_APPROVE_ANSWER.to_owned(),
                    other => other.to_owned(),
                };
                self.send_answer(answer);
            }
            return vec![];
        }
        // Flow retry: when the last flow run failed and persisted state exists,
        // submitting in Flow mode (Enter, optionally with a note) re-enters the
        // pipeline at the failed stage instead of starting a fresh workstream.
        // A non-empty submission that isn't a bare retry intent falls through
        // to the normal path below (treated as a new request).
        if self.state.mode == Mode::Flow && self.flow_failed {
            let note = sub.text.trim();
            self.flow_failed = false;
            self.run_id += 1;
            let msg = QueuedMessage {
                text: if note.is_empty() {
                    "resume flow from last failure".to_string()
                } else {
                    note.to_string()
                },
                images: sub.images.clone(),
            };
            return self.start_from_queue(&msg, true);
        }
        if sub.is_empty() {
            return vec![];
        }
        if sub.text.trim() == "exit" {
            return self.quit();
        }

        if let Some(prefix) = shell::parse_shell_prefix(&sub.text) {
            let cmd = prefix.command.trim();
            if cmd == "cd" || cmd.starts_with("cd ") {
                self.flash("Only /cd can change the working directory".into());
            }
            let id = self.shell.next_id();
            let sigil = if prefix.visible { "!" } else { "!!" };
            let display = format!("{sigil} {}", prefix.command);
            self.main_chat().show_user_message(display);
            return vec![Action::ShellCommand {
                id,
                command: prefix.command,
                visible: prefix.visible,
            }];
        }
        let msg: QueuedMessage = sub.into();
        self.submit_or_queue(msg)
    }

    fn handle_cancel(&mut self) -> Vec<Action> {
        let cancelled_run = self.run_id;
        self.run_id += 1;
        self.retry_info = None;
        let awaiting_flow = self.flow_awaiting_approval;
        self.close_all_overlays();
        self.pending_input = PendingInput::None;
        if awaiting_flow {
            self.flow_awaiting_approval = false;
            self.send_answer(craft_flow::FLOW_CANCEL_ANSWER.to_owned());
        }
        self.finish_subagents(DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.clear();
        self.shell.cancel_all();
        for chat in &mut self.chats {
            chat.flush();
            chat.cancel_in_progress();
        }
        self.main_chat()
            .push(DisplayMessage::new(DisplayRole::Error, CANCEL_MSG.into()));
        self.queue.clear();
        self.status = Status::Idle;
        vec![Action::CancelAgent {
            run_id: cancelled_run,
        }]
    }

    fn handle_subagent_cancel(&mut self) -> Vec<Action> {
        let tool_use_id = self
            .chat_index
            .iter()
            .find(|&(_, &idx)| idx == self.active_chat)
            .map(|(id, _)| id.clone());

        let Some(tool_use_id) = tool_use_id else {
            return vec![];
        };

        self.chats[self.active_chat].flush();
        self.chats[self.active_chat].cancel_in_progress();
        self.chats[self.active_chat].mark_finished(DisplayRole::Error, CANCELLED_TEXT);
        self.subagent_answers.remove(&tool_use_id);

        vec![Action::CancelSubagent { tool_use_id }]
    }

    fn handle_agent_event(&mut self, envelope: Envelope) -> Vec<Action> {
        if envelope.run_id == RESTORE_RUN_ID {
            let (id, snapshot, theme_gen, is_header) = match envelope.event {
                AgentEvent::ToolSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, false),
                AgentEvent::ToolHeaderSnapshot {
                    id,
                    snapshot,
                    theme_gen,
                } => (id, snapshot, theme_gen, true),
                _ => return vec![],
            };
            for chat in &mut self.chats {
                if is_header {
                    chat.tool_header_snapshot(&id, snapshot.clone(), theme_gen);
                } else {
                    chat.tool_snapshot(&id, snapshot.clone(), theme_gen);
                }
            }
            return vec![];
        }
        if envelope.run_id != self.run_id {
            // Stale run_id after cancel: agent updates shared_history before sending
            // Done/Error, so this is the first moment the full conversation is available.
            if matches!(
                envelope.event,
                AgentEvent::Done { .. } | AgentEvent::Error { .. }
            ) {
                self.save_session();
            }
            return vec![];
        }

        if let AgentEvent::SubagentHistory {
            tool_use_id,
            messages,
        } = envelope.event
        {
            self.state
                .session
                .subagent_messages
                .insert(tool_use_id, messages);
            return vec![];
        }

        let subagent_id = envelope
            .subagent
            .as_ref()
            .map(|s| s.parent_tool_use_id.clone());

        let chat_idx = match envelope.subagent {
            Some(ref subagent) => self.resolve_or_create_chat(subagent),
            None => 0,
        };

        if let AgentEvent::ToolDone(ref e) = envelope.event {
            if self.state.mode == Mode::Plan
                && self.state.plan.path().is_some_and(|pp| e.wrote_to(pp))
            {
                self.transition_plan(PlanTrigger::WriteDone);
            }
            if let Some(ref outputs) = self.shared_tool_outputs {
                lock(outputs).insert(e.id.clone(), e.output.clone());
            }
            if let Some(&sub_idx) = self.chat_index.get(&e.id) {
                let (role, text) = if e.is_error {
                    (DisplayRole::Error, ERROR_TEXT)
                } else {
                    (DisplayRole::Done, DONE_TEXT)
                };
                self.chats[sub_idx].mark_finished(role, text);
            }
        }

        if let AgentEvent::Retry {
            attempt,
            message,
            delay_ms,
        } = envelope.event
        {
            self.chats[chat_idx].stream_reset();
            if chat_idx == 0 {
                self.retry_info = Some(RetryInfo {
                    attempt,
                    message,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                });
            }
            return vec![];
        }

        if let AgentEvent::ModelEscalation { to, .. } = &envelope.event {
            return vec![Action::ChangeModel(to.clone())];
        }

        self.retry_info = None;

        let plan_path = if self.state.mode == Mode::Plan {
            self.state.plan.path()
        } else {
            None
        };

        if let AgentEvent::TurnComplete(ref tc) = envelope.event {
            self.state.token_usage += tc.usage;
            self.chats[chat_idx].token_usage += tc.usage;
            *self
                .state
                .session
                .meta
                .usage_by_model
                .entry(tc.model.clone())
                .or_default() += tc.usage.into();
            let ctx_size = tc.context_size.unwrap_or_else(|| tc.usage.context_tokens());
            self.chats[chat_idx].context_size = ctx_size;
            if chat_idx == 0 {
                self.state.context_size = ctx_size;
            }
            let formatted =
                format_turn_usage(&tc.usage, &self.state.model.pricing, self.state.fast);
            self.chats[chat_idx].set_pending_turn_usage(formatted);
        }

        let result = self.chats[chat_idx].handle_event(envelope.event, plan_path);

        if let ChatEventResult::QueueItemConsumed { text, image_count } = result {
            if chat_idx == 0 {
                self.on_queue_item_consumed(&text, image_count);
            }
            return vec![];
        }

        if let ChatEventResult::PermissionRequest {
            id,
            tool,
            scopes,
            context,
        } = result
        {
            self.permission_prompt
                .open(id, tool, scopes, context, subagent_id);
            return vec![];
        }

        if let ChatEventResult::AuthRequired = result {
            self.chats[chat_idx].push(DisplayMessage::new(
                DisplayRole::Error,
                AUTH_EXPIRED_MSG.into(),
            ));
            if chat_idx != 0 {
                self.main_chat().push(DisplayMessage::new(
                    DisplayRole::Error,
                    AUTH_EXPIRED_MSG.into(),
                ));
            }
            self.pending_input = PendingInput::AuthRetry { subagent_id };
            return vec![];
        }

        if let ChatEventResult::Done { usage } = &result {
            self.record_cost(chat_idx, *usage);
        }

        if chat_idx == 0 {
            match result {
                ChatEventResult::Done { .. } => {
                    self.status_bar.clear_flash();
                    self.save_session();
                    self.chat_index.clear();
                    self.subagent_answers.clear();
                    self.status = Status::Idle;
                    if let Some(ref handle) = self.lua_event_handle {
                        handle.fire_autocmd("TurnEnd", serde_json::json!({}));
                    }
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Success;
                    }
                }
                ChatEventResult::Error(message) => {
                    self.status = Status::error(message.clone());
                    self.status_bar.clear_flash();
                    self.save_session();
                    self.queue.clear();
                    self.subagent_answers.clear();
                    self.finish_subagents(DisplayRole::Error, ERROR_TEXT);
                    for chat in &mut self.chats {
                        chat.fail_in_progress_with_message(message.clone());
                    }
                    if let Some(ref handle) = self.lua_event_handle {
                        handle.fire_autocmd("TurnError", serde_json::json!({ "message": message }));
                    }
                    if self.exit_on_done {
                        self.exit_request = ExitRequest::Error;
                    }
                }
                ChatEventResult::AuthRequired
                | ChatEventResult::PermissionRequest { .. }
                | ChatEventResult::QueueItemConsumed { .. } => unreachable!(),
                ChatEventResult::Continue => {}
            }
        }
        vec![]
    }

    fn resolve_or_create_chat(&mut self, subagent: &SubagentInfo) -> usize {
        let id = &subagent.parent_tool_use_id;
        if let Some(&idx) = self.chat_index.get(id.as_str()) {
            return idx;
        }
        let idx = self.chats.len();
        self.chat_index.insert(id.clone(), idx);
        if let Some(ref tx) = subagent.answer_tx {
            self.subagent_answers.insert(id.clone(), tx.clone());
        }
        self.chats[0].update_tool_summary(id, &subagent.name);
        if let Some(ref model) = subagent.model {
            self.chats[0].update_tool_model(id, model);
        }
        let mut chat = Chat::new(subagent.name.clone(), self.ui_config.clone());
        chat.model_id = subagent.model.clone();
        chat.set_lua_event_handle(self.lua_event_handle.clone());
        chat.set_restore_event_tx(self.restore_event_tx.clone());
        if let Some(ref prompt) = subagent.prompt {
            chat.push_user_message(prompt);
        }
        self.chats.push(chat);
        idx
    }

    fn execute_command(&mut self, cmd: ParsedCommand) -> Vec<Action> {
        self.input_box.discard();
        match cmd.name.as_str() {
            "/tasks" => {
                self.open_tasks();
                vec![]
            }
            "/compact" => {
                if self.status == Status::Streaming {
                    self.queue_compact();
                    return vec![];
                }
                self.status = Status::Streaming;
                vec![Action::Compact]
            }
            "/help" => {
                self.help_modal.toggle();
                vec![]
            }
            "/usage" => {
                self.usage_modal.toggle();
                if self.usage_modal.is_open() {
                    vec![Action::RefreshUsage]
                } else {
                    vec![]
                }
            }
            "/stats" => {
                let ledger = craft_storage::stats::CostLedger::from_state_dir(&self.storage)
                    .map_err(|e| {
                        tracing::warn!(error = %e, "failed to open cost ledger");
                    })
                    .ok();
                if let Some(ledger) = ledger {
                    self.stats_modal.toggle(&ledger);
                }
                vec![]
            }
            "/btw" => {
                let question = cmd.args.trim().to_string();
                if question.is_empty() {
                    self.flash("Usage: /btw <question>".into());
                    vec![]
                } else {
                    vec![Action::Btw(question)]
                }
            }
            "/new" => self.reset_session(),
            "/queue" => {
                self.queue.set_focus();
                vec![]
            }
            "/model" => {
                self.model_picker.open(&self.state.model.spec());
                vec![Action::RefreshModels]
            }
            "/theme" => {
                self.theme_picker.open();
                vec![]
            }
            "/mcp" => {
                self.mcp_picker.open();
                vec![]
            }
            "/login" => {
                self.login_picker.open(self.storage.clone());
                vec![]
            }
            "/cd" => self.cmd_cd(&cmd.args),
            "/set-context-window" => self.cmd_set_context_window(&cmd.args),
            "/clear-context-window" => self.cmd_clear_context_window(&cmd.args),
            "/yolo" => {
                let enabled = self.permissions.toggle_yolo();
                let sandbox_cfg = if enabled {
                    craft_config::SandboxConfig {
                        enabled: false,
                        mode: craft_config::SandboxMode::Off,
                        network: true,
                    }
                } else {
                    craft_config::SandboxConfig::default()
                };
                if let Some(handle) = &self.lua_event_handle {
                    handle.set_sandbox_config(sandbox_cfg);
                }
                let msg = if enabled {
                    "YOLO mode enabled"
                } else {
                    "YOLO mode disabled"
                };
                self.flash(msg.into());
                vec![]
            }
            "/thinking" => {
                if !self.state.model.supports_thinking() {
                    self.flash("Thinking requires a model that supports it".into());
                    return vec![];
                }
                match ThinkingConfig::parse(cmd.args.trim(), self.state.thinking) {
                    Ok(thinking) => {
                        self.state.thinking = thinking;
                        self.flash(format!("Thinking: {thinking}"));
                    }
                    Err(msg) => self.flash(msg.into()),
                }
                vec![]
            }
            "/fast" => {
                if !self.state.model.supports_fast() {
                    self.flash(FAST_UNSUPPORTED_MSG.into());
                    return vec![];
                }
                self.state.fast = !self.state.fast;
                self.flash(
                    if self.state.fast {
                        FAST_ON_MSG
                    } else {
                        FAST_OFF_MSG
                    }
                    .into(),
                );
                vec![]
            }
            "/exit" => self.quit(),
            "/reload" => self.quit_with(ExitRequest::Reload),
            "/goal" => {
                let goal = cmd.args.trim().to_string();
                if goal.is_empty() {
                    self.state.session.meta.goal = None;
                    self.state.session.meta.goal_criteria.clear();
                    self.flash("Goal cleared".into());
                } else {
                    let criteria = parse_goal_criteria(&goal);
                    self.state.session.meta.goal = Some(goal.clone());
                    self.state.session.meta.goal_criteria = criteria;
                    self.flash(format!("Goal set: {goal}"));
                }
                vec![]
            }
            "/recipe" => {
                self.recipe_picker.open();
                vec![]
            }
            "/dream" => self.run_meta_prompt("/dream", craft_agent::prompt::DREAM_PROMPT),
            "/distill" => self.run_meta_prompt("/distill", craft_agent::prompt::DISTILL_PROMPT),
            "/checkpoint" => {
                self.run_meta_prompt("/checkpoint", craft_agent::prompt::CHECKPOINT_PROMPT)
            }
            "/wiki" => self.execute_wiki_command(&cmd.args),
            "/map" => self.execute_map_command(),
            "/map-refresh" => {
                if let Some(rm) = craft_repomap::RepoMap::try_from_cwd() {
                    rm.force_refresh();
                    self.flash("Repo map cache cleared.".into());
                } else {
                    self.flash("Not in a git repo.".into());
                }
                vec![]
            }
            "/map-toggle" => {
                let prev = self
                    .repomap_enabled
                    .load(std::sync::atomic::Ordering::Relaxed);
                self.repomap_enabled
                    .store(!prev, std::sync::atomic::Ordering::Relaxed);
                let state = if !prev { "enabled" } else { "disabled" };
                self.flash(format!("Repo map {state}."));
                vec![]
            }
            "/watch" => {
                self.watch_enabled = !self.watch_enabled;
                let state = if self.watch_enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                self.flash(format!("Watch mode {state}."));
                vec![Action::ToggleWatch {
                    enabled: self.watch_enabled,
                }]
            }
            name if name.starts_with("/project:") || name.starts_with("/user:") => {
                self.execute_custom_command(name, &cmd.args)
            }
            name if self.command_palette.find_mcp_prompt(name).is_some() => {
                self.execute_mcp_prompt(name, &cmd.args)
            }
            name if self.command_palette.find_lua_command(name).is_some() => {
                self.run_lua_command(name, cmd.args);
                vec![]
            }
            _ => vec![],
        }
    }

    fn execute_map_command(&mut self) -> Vec<Action> {
        let map = craft_repomap::RepoMap::try_from_cwd()
            .map(|rm| rm.get_repo_map(&[], &[], ""))
            .unwrap_or_default();
        if map.is_empty() {
            self.flash("Repo map is empty or not in a git repo.".into());
        } else {
            let chat = self.main_chat();
            chat.flush();
            chat.push(DisplayMessage::new(
                DisplayRole::Assistant,
                format!("Repo map (ranked symbols, may be stale):\n\n{map}"),
            ));
            chat.enable_auto_scroll();
        }
        vec![]
    }

    fn execute_wiki_command(&mut self, args: &str) -> Vec<Action> {
        let mut parts = args.split_whitespace();
        let sub = parts.next().unwrap_or("");
        match sub {
            "ingest" => {
                let Some(path_str) = parts.next() else {
                    self.flash("Usage: /wiki ingest <file>".into());
                    return vec![];
                };
                let path = PathBuf::from(path_str);
                let absolute = if path.is_absolute() {
                    path
                } else {
                    PathBuf::from(&self.state.session.cwd).join(&path)
                };
                if !absolute.exists() {
                    self.flash(format!("No such file: {}", absolute.display()));
                    return vec![];
                }
                self.flash(format!("Ingesting {}...", absolute.display()));
                vec![Action::WikiIngest {
                    source_path: absolute,
                }]
            }
            "list" => match self.wiki_list_text() {
                Ok(text) => {
                    self.flash(text);
                    vec![]
                }
                Err(e) => {
                    self.flash(format!("wiki list failed: {e}"));
                    vec![]
                }
            },
            "show" => {
                let Some(slug) = parts.next() else {
                    self.flash("Usage: /wiki show <slug>".into());
                    return vec![];
                };
                match self.wiki_show_text(slug) {
                    Ok(text) => {
                        self.flash(text);
                        vec![]
                    }
                    Err(e) => {
                        self.flash(format!("wiki show failed: {e}"));
                        vec![]
                    }
                }
            }
            "init" => self.run_meta_prompt("/wiki init", craft_agent::prompt::WIKI_INIT_PROMPT),
            "" => {
                self.flash("Usage: /wiki <init | ingest <file> | list | show <slug>>".into());
                vec![]
            }
            other => {
                self.flash(format!("Unknown /wiki subcommand: {other}"));
                vec![]
            }
        }
    }

    fn wiki_list_text(&self) -> Result<String, String> {
        let cwd = PathBuf::from(&self.state.session.cwd);
        let store = craft_storage::wiki::WikiStore::open(&cwd).map_err(|e| e.to_string())?;
        let listings = store.list().map_err(|e| e.to_string())?;
        if listings.is_empty() {
            return Ok("Wiki is empty. Use /wiki ingest <file>.".into());
        }
        let mut out = String::new();
        for entry in listings {
            let kind = if entry.kind == craft_storage::wiki::ListingKind::Source {
                "source"
            } else {
                "page"
            };
            out.push_str(&format!("[{kind}] {} - {}\n", entry.slug, entry.title));
        }
        Ok(out)
    }

    fn wiki_show_text(&self, slug: &str) -> Result<String, String> {
        let cwd = PathBuf::from(&self.state.session.cwd);
        let store = craft_storage::wiki::WikiStore::open(&cwd).map_err(|e| e.to_string())?;
        store.read_page(slug).map_err(|e| e.to_string())
    }

    fn run_lua_command(&self, name: &str, args: String) {
        let Some(lua_cmd) = self.command_palette.find_lua_command(name) else {
            return;
        };
        let Some(handle) = &self.lua_event_handle else {
            return;
        };
        handle.run_command(Arc::clone(&lua_cmd.plugin), Arc::clone(&lua_cmd.name), args);
    }

    fn execute_mcp_prompt(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(prompt) = self.command_palette.find_mcp_prompt(name) else {
            return vec![];
        };
        let prompt = prompt.clone();

        let arguments = Self::parse_prompt_args(&prompt, args);
        let missing: Vec<_> = prompt
            .arguments
            .iter()
            .filter(|a| a.required && !arguments.contains_key(&a.name))
            .map(|a| format!("<{}>", a.name))
            .collect();
        if !missing.is_empty() {
            self.flash(format!("Usage: {} {}", name, missing.join(" ")));
            return vec![];
        }

        let prompt_ref = craft_agent::McpPromptRef {
            qualified_name: prompt.qualified_name.clone(),
            arguments,
        };
        let display_text = if args.trim().is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
        let mut input = self.build_agent_input(&QueuedMessage {
            text: display_text.clone(),
            images: Vec::new(),
        });
        input.prompt = Some(Box::new(prompt_ref));

        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            vec![]
        } else {
            self.run_id += 1;
            self.status = Status::Streaming;
            self.main_chat().show_user_message(display_text);
            vec![Action::SendMessage(Box::new(input))]
        }
    }

    fn run_meta_prompt(&mut self, label: &str, prompt: &'static str) -> Vec<Action> {
        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            return vec![];
        }
        let input = self.build_agent_input(&QueuedMessage {
            text: prompt.to_string(),
            images: Vec::new(),
        });
        self.run_id += 1;
        self.status = Status::Streaming;
        self.main_chat().show_user_message(label.to_string());
        vec![Action::SendMessage(Box::new(input))]
    }

    fn run_recipe(&mut self, path: &std::path::Path) -> Vec<Action> {
        if self.status == Status::Streaming {
            self.flash("Agent is busy, try again later".into());
            return vec![];
        }
        let recipe = match craft_agent::recipe::load(path) {
            Ok(r) => r,
            Err(e) => {
                self.flash(format!("recipe load error: {e}"));
                return vec![];
            }
        };
        let overrides = HashMap::new();
        if !recipe.missing_required(&overrides).is_empty() {
            self.flash(
                "recipe has required parameters without defaults; use CLI: craft recipe run <name> --param key=value"
                    .into(),
            );
            return vec![];
        }
        let params = match recipe.resolve_parameters(&overrides) {
            Ok(p) => p,
            Err(e) => {
                self.flash(format!("recipe param error: {e}"));
                return vec![];
            }
        };
        let prompt = match recipe.render(&params, path) {
            Ok(p) => p,
            Err(e) => {
                self.flash(format!("recipe render error: {e}"));
                return vec![];
            }
        };
        let label = recipe.name.clone().unwrap_or_else(|| "recipe".into());
        let input = self.build_agent_input(&QueuedMessage {
            text: prompt,
            images: Vec::new(),
        });
        self.run_id += 1;
        self.status = Status::Streaming;
        self.main_chat().show_user_message(label);
        vec![Action::SendMessage(Box::new(input))]
    }

    pub(crate) fn submit_watch_prompt(&mut self, label: String, text: String) -> Vec<Action> {
        if self.status == Status::Streaming {
            self.flash("Agent is busy, watch prompt discarded".into());
            return vec![];
        }
        let input = self.build_agent_input(&QueuedMessage {
            text,
            images: Vec::new(),
        });
        self.run_id += 1;
        self.status = Status::Streaming;
        self.main_chat().show_user_message(label);
        vec![Action::SendMessage(Box::new(input))]
    }

    fn parse_prompt_args(prompt: &McpPromptInfo, args: &str) -> HashMap<String, String> {
        let mut result = HashMap::new();
        let mut remaining = args.trim();
        if remaining.is_empty() || prompt.arguments.is_empty() {
            return result;
        }
        let last_idx = prompt.arguments.len() - 1;
        for (i, arg) in prompt.arguments.iter().enumerate() {
            if remaining.is_empty() {
                break;
            }
            if i == last_idx {
                result.insert(arg.name.clone(), remaining.to_string());
            } else if let Some((word, rest)) = remaining.split_once(char::is_whitespace) {
                result.insert(arg.name.clone(), word.to_string());
                remaining = rest.trim_start();
            } else {
                result.insert(arg.name.clone(), remaining.to_string());
                break;
            }
        }
        result
    }

    fn execute_custom_command(&mut self, name: &str, args: &str) -> Vec<Action> {
        let Some(cmd) = self.command_palette.find_custom_command(name) else {
            self.flash(format!("Unknown command: {name}"));
            return vec![];
        };
        let rendered = cmd.render(args);
        self.submit_or_queue(QueuedMessage {
            text: rendered,
            images: Vec::new(),
        })
    }

    fn cmd_cd(&mut self, args: &str) -> Vec<Action> {
        let path = if args.is_empty() {
            craft_storage::paths::home().unwrap_or_default()
        } else {
            match args.strip_prefix('~') {
                Some(rest) => {
                    let home = craft_storage::paths::home().unwrap_or_default();
                    if rest.is_empty() {
                        home
                    } else {
                        home.join(rest.trim_start_matches('/'))
                    }
                }
                None => PathBuf::from(args),
            }
        };
        match std::env::set_current_dir(&path) {
            Ok(()) => {
                if let Ok(canonical) = std::env::current_dir() {
                    self.state.session.cwd = canonical.to_string_lossy().into_owned();
                }
                self.status_bar.refresh_cwd();
                self.flash(format!("cd {}", path.display()))
            }
            Err(e) => self.flash(format!("cd: {e}")),
        }
        vec![]
    }

    fn cmd_set_context_window(&mut self, args: &str) -> Vec<Action> {
        let trimmed = args.trim();
        if trimmed.is_empty() || trimmed == "status" {
            return self.flash_context_window_overrides();
        }
        let (spec, tokens_str) = match trimmed.split_once(char::is_whitespace) {
            Some((a, b)) => (a.trim().to_string(), b.trim()),
            None => (self.state.model.spec(), trimmed),
        };
        let Ok(tokens) = tokens_str.parse::<u32>() else {
            self.flash(SET_CONTEXT_WINDOW_USAGE.into());
            return vec![];
        };
        if tokens == 0 {
            self.flash(SET_CONTEXT_WINDOW_USAGE.into());
            return vec![];
        }
        self.state
            .context_window_overrides
            .insert(spec.clone(), tokens);
        self.flash(format!("context window for {spec} set to {tokens}"));
        if spec == self.state.model.spec() {
            vec![Action::ApplyContextWindowOverride]
        } else {
            vec![]
        }
    }

    fn cmd_clear_context_window(&mut self, args: &str) -> Vec<Action> {
        let trimmed = args.trim();
        if trimmed == "all" {
            if self.state.context_window_overrides.is_empty() {
                self.flash("no context-window overrides set".into());
                return vec![];
            }
            self.state.context_window_overrides.clear();
            self.flash("cleared all context-window overrides".into());
            return vec![Action::ApplyContextWindowOverride];
        }
        let spec = if trimmed.is_empty() {
            self.state.model.spec()
        } else {
            trimmed.to_string()
        };
        if self.state.context_window_overrides.remove(&spec).is_some() {
            self.flash(format!(
                "context window for {spec} restored to catalog value"
            ));
            if spec == self.state.model.spec() {
                vec![Action::ApplyContextWindowOverride]
            } else {
                vec![]
            }
        } else {
            self.flash(format!("no override set for {spec}"));
            vec![]
        }
    }

    fn flash_context_window_overrides(&mut self) -> Vec<Action> {
        if self.state.context_window_overrides.is_empty() {
            self.flash("no context-window overrides set".into());
            return vec![];
        }
        let mut entries: Vec<_> = self.state.context_window_overrides.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let lines: Vec<String> = entries
            .iter()
            .map(|(spec, tokens)| format!("{spec} -> {tokens}"))
            .collect();
        self.flash(format!("active overrides:\n{}", lines.join("\n")));
        vec![]
    }

    define_overlays!(
        help_modal,
        usage_modal,
        stats_modal,
        btw_modal,
        float_mgr,
        search_modal,
        file_picker,
        task_picker,
        recipe_picker,
        rewind_picker,
        theme_picker,
        model_picker,
        login_picker,
        mcp_picker,
        permission_prompt,
        flow_goal_prompt,
    );

    pub fn any_overlay_open(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open())
    }

    pub fn has_modal_overlay(&self) -> bool {
        self.overlays().iter().any(|o| o.is_open() && o.is_modal())
    }

    pub fn close_all_overlays(&mut self) {
        self.overlays_mut().iter_mut().for_each(|o| o.close());
    }

    pub fn is_animating(&self) -> bool {
        !self.image_paste_rx.is_empty()
            || self.btw_modal.is_animating()
            || self.file_picker.is_loading()
            || self.float_mgr.is_open()
            || self
                .selection_state
                .as_ref()
                .is_some_and(|s| s.is_edge_scrolling())
            || self.restoring.load(Ordering::Relaxed)
            || self.chats.iter().any(|c| c.is_animating())
    }

    fn finish_subagents(&mut self, role: DisplayRole, text: &str) {
        for &sub_idx in self.chat_index.values() {
            self.chats[sub_idx].mark_finished(role.clone(), text);
        }
        self.chat_index.clear();
    }

    pub fn flush_all_chats(&mut self) {
        for chat in &mut self.chats {
            chat.flush();
        }
    }

    fn route_text_paste(&mut self, text: &str) {
        if self.plan_form_active() {
            return;
        }
        if self.flow_goal_prompt.handle_paste(text) {
            return;
        }
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        if self.float_mgr.handle_paste(text) {
            return;
        }
        if self.search_modal.is_open() {
            self.search_modal.handle_paste(text);
            let chat = &mut self.chats[self.active_chat];
            let texts = chat.segment_search_texts();
            self.search_modal.update_matches(&texts);
            sync_search_highlight(&self.search_modal, chat);
            return;
        }
        macro_rules! try_picker {
            ($picker:expr) => {
                if $picker.handle_paste(text) {
                    return;
                }
            };
        }
        try_picker!(self.file_picker);
        try_picker!(self.task_picker);
        try_picker!(self.rewind_picker);
        try_picker!(self.theme_picker);
        try_picker!(self.model_picker);
        try_picker!(self.login_picker);
        try_picker!(self.mcp_picker);
        try_picker!(self.recipe_picker);
        if !self.is_main_chat() {
            return;
        }
        if let InputAction::PaletteSync(val) = self.input_box.handle_paste(text) {
            self.command_palette.sync(&val);
        }
    }

    fn handle_plan_form_action(&mut self, action: PlanFormAction) -> Vec<Action> {
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => vec![],
            PlanFormAction::Hide => {
                self.plan_form.hide();
                vec![]
            }
            PlanFormAction::OpenEditor => match self.state.plan.path() {
                Some(p) => vec![Action::OpenEditor(p.to_path_buf())],
                None => {
                    self.flash(FLASH_NO_PLAN.into());
                    vec![]
                }
            },
            PlanFormAction::Implement => self.implement_plan(false),
            PlanFormAction::ClearAndImplement => self.implement_plan(true),
        }
    }

    fn implement_plan(&mut self, clear_context: bool) -> Vec<Action> {
        let parallel = self.plan_form.parallel();
        self.plan_form.reset();
        let plan_snapshot = match std::mem::take(&mut self.state.plan) {
            PlanState::Ready(p) => Some((
                std::fs::read_to_string(&p).unwrap_or_default(),
                p.display().to_string(),
            )),
            _ => None,
        };

        self.state.mode = Mode::Build;

        let mut actions = if clear_context {
            self.reset_session()
        } else {
            vec![]
        };

        let text = if let Some((content, path_str)) = plan_snapshot {
            let text = if parallel {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`. {IMPLEMENT_PARALLEL_HINT}")
            } else {
                format!("{IMPLEMENT_MSG_PREFIX} at `{path_str}`.")
            };
            self.main_chat()
                .push(DisplayMessage::plan(content, path_str));
            text
        } else {
            format!("{}.", IMPLEMENT_MSG_PREFIX)
        };
        self.run_id += 1;
        let msg = QueuedMessage {
            text,
            images: vec![],
        };
        actions.extend(self.start_from_queue(&msg, false));
        actions
    }
}

fn sync_search_highlight(modal: &SearchModal, chat: &mut Chat) {
    let idx = modal.current_segment_index();
    if let Some(i) = idx {
        chat.scroll_to_segment(i);
    }
    chat.set_highlight_segment(idx);
}

fn format_with_images(text: &str, image_count: usize) -> String {
    match image_count {
        0 => text.to_string(),
        1 => format!("{text} [1 image]"),
        n => format!("{text} [{n} images]"),
    }
}

fn parse_goal_criteria(goal: &str) -> Vec<String> {
    let marker = "## acceptance criteria";
    let lines: Vec<&str> = goal.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().to_ascii_lowercase() == marker {
            return lines[i + 1..]
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .map(|l| {
                    l.strip_prefix("- ")
                        .or_else(|| l.strip_prefix("* "))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_else(|| l.to_string())
                })
                .collect();
        }
    }
    Vec::new()
}
