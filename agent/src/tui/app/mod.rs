//! Application state and input handling. The App renders whatever the
//! provider streams in and translates key presses into provider commands.

mod commands;
mod events;
mod mode;
mod models;
mod scroll;
mod sidebar;

// Re-exported so `crate::tui::app::*` keeps working for every item that
// lived in the old flat `app.rs`.
#[allow(unused_imports)]
pub use self::commands::{COMMANDS, CommandSpec};
#[allow(unused_imports)]
pub use self::models::{
    AutoReviewLine, Conversation, DiffState, Message, PendingClick, Session, ViewModel,
};

use tokio::sync::mpsc;

use crate::tui::composer::Composer;
use crate::tui::modals::Modal;
use crate::tui::provider::{
    AgentEvent, Command, LoadedMessage, PlanItem, Status, TouchedFile, UsageRow,
};
use crate::tui::repaint;
use crate::tui::shell;
use crate::tui::ui::scrollback::ScrollPos;

pub(crate) use self::mode::Mode;

/// How long a status-row flash toast stays up (reference default,
/// `DEFAULT_FLASH_DURATION_MS`).
pub(crate) const FLASH_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

pub const EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// Plan-mode state: the allocated plan file, the "Plan complete" form,
/// and the Ctrl-O editor request.
pub struct PlanMode {
    /// Session's allocated plan file; set on first entry into Plan mode.
    pub plan_path: Option<std::path::PathBuf>,
    /// The plan form (F.3, Ctrl-T): "Plan complete" menu over the composer
    /// while a finished plan draft is parked.
    pub plan_form: crate::tui::plan_form::PlanForm,
    /// Whether the allocated plan file holds a finished draft; drives the
    /// plan form's shown/drafting lifecycle.
    pub plan_ready: bool,
    /// Plan-file content at the start of the current Plan-mode turn; the
    /// form only surfaces when a turn actually rewrote the plan.
    pub plan_turn_snapshot: Option<String>,
    /// Plan file the run loop should hand to $VISUAL/$EDITOR (Ctrl-O);
    /// taken by the loop, which owns the terminal around the child.
    pub editor_request: Option<std::path::PathBuf>,
}

/// Input-history recall state (↑/↓ navigation over `input_history`).
pub struct HistoryRecall {
    /// Position in `input_history` while recalling; `None` = live editing.
    pub history_index: Option<usize>,
    /// The in-progress text saved when history recall starts, restored on
    /// ↓ past the newest entry.
    pub history_draft: String,
}

/// Every overlay surface: the exclusive modal plus the composer-riding
/// prompts, sheets, and pickers.
pub struct Overlays {
    /// The one modal currently open (palette / model menu / confirm),
    /// exclusive by construction. The slash popup is not a modal: it rides
    /// on the composer (shown when the composer starts with '/').
    pub modal: Modal,
    pub slash_selected: usize, // row in the slash popup
    /// Permission-prompt overlay (F.5): open while a gated tool call is
    /// parked on the user's decision. Not a modal — it rides above the
    /// composer and owns plain keys while open.
    pub permission_prompt: crate::tui::permission_prompt::PermissionPrompt,
    /// Question form (A.5): open while the `question` tool is parked on
    /// the user's answers. Like the permission prompt, not a modal — it
    /// rides above the composer and owns plain keys while open.
    pub question_form: crate::tui::question_form::QuestionForm,
    /// Latest per-model usage snapshot from the provider (the `/usage`
    /// overlay's data; refreshed after every completed run).
    pub usage: Vec<UsageRow>,
    /// Latest provider quota answer for `/usage` (F.5); kept across modal
    /// close/reopen so the last successful fetch survives.
    pub usage_quota: crate::tui::provider::UsageFetchState,
    /// Scroll offset of the `/usage` overlay's line list, reset on open.
    pub usage_scroll: usize,
    /// Max scroll of the `/usage`/`/stats` sheet, written back by its
    /// renderer so handlers clamp against the real bound (not
    /// `usize::MAX`, which pins the sheet at the bottom).
    pub usage_scroll_max: usize,
    /// Scroll offset of the keybindings help modal (F.1), reset on open.
    pub help_scroll: usize,
    /// Max scroll of the help sheet, written back by its renderer.
    pub help_scroll_max: usize,
    /// Data-driven keybinding resolution (F.1): compile-time defaults plus
    /// the user's config overlay. All chord dispatch goes through this.
    pub keybinds: crate::tui::keybindings::KeybindingResolver,
    /// Fuzzy transcript search (F.3): owns the keyboard while open, above
    /// the base surface like a modal but outside the exclusive `modal` slot.
    pub search: crate::tui::search_modal::SearchModal,
    /// File picker (F.3, Ctrl-S): fuzzy path matcher over an async walkdir
    /// of the session cwd; owns the keyboard while open, above the base
    /// surface like the search modal.
    pub file_picker: crate::tui::file_picker::FilePicker,
}

/// Inline-image state (F.6): picker, decoded render-state cache, staged
/// attachments, and pending loads.
pub struct Images {
    /// Terminal-graphics picker (kitty/sixel/halfblocks), resolved once.
    pub picker: crate::tui::ui::image::ImagePicker,
    /// Decoded image render states by tool id, with the width they were
    /// built for; rebuilt on resize, reused across frames otherwise.
    pub states: std::collections::HashMap<
        String,
        (u16, std::sync::Arc<crate::tui::ui::image::ImageRenderState>),
    >,
    /// LRU order of `states` keys; the cache is capped so decoded
    /// render states can't grow unbounded across a long session.
    pub states_order: std::collections::VecDeque<String>,
    /// Image attachments staged in the composer (path picks and clipboard
    /// pastes); sent with the next message and cleared on submit.
    pub attached: Vec<crate::history::ImageBlock>,
    /// Finished clipboard/file loads awaiting pickup by the render loop.
    pub loads: Vec<std::sync::mpsc::Receiver<Result<crate::history::ImageBlock, String>>>,
}

pub struct App {
    // --- mode (F.2 Tab cycling, Build/Plan) ---
    pub mode: Mode,
    pub plan_mode: PlanMode,

    // --- provider-driven state ---
    pub plan: Vec<PlanItem>,
    pub files: Vec<TouchedFile>,
    pub status: Status,
    /// Set when the user interrupted a running turn; cleared by the next
    /// provider status change. Keeps `busy()` false so a repeated Ctrl-C
    /// quits without misreporting the turn as failed.
    pub interrupt_requested: bool,
    /// Animation frame counter for the status indicator (advanced per frame).
    pub status_tick: usize,
    /// When the current turn started; drives the elapsed-seconds counter
    /// next to the thinking/running spinner.
    pub turn_started: Option<std::time::Instant>,
    /// Timed flash toast for the status row: text plus its expiry.
    pub flash: Option<(String, std::time::Instant)>,

    // --- state groups (fields stay pub for the renderer for now;
    // accessor encapsulation is a follow-up) ---
    pub conversation: Conversation,
    pub view: ViewModel,
    pub session: Session,

    pub composer: Composer,
    /// Rolling user-input history (↑/↓ recall; persisted per state dir).
    pub input_history: crate::storage::input_history::InputHistory,
    pub history_recall: HistoryRecall,
    /// Composer text last handed to the provider for draft checkpointing;
    /// `sync_draft` sends only the changes.
    pub sent_draft: String,

    pub overlays: Overlays,
    pub images: Images,

    pub should_quit: bool,
}

/// Upper bound on cached decoded image render states (LRU-evicted).
const IMAGE_STATE_CAP: usize = 32;

impl App {
    pub fn new() -> Self {
        App {
            mode: Mode::Build,
            plan_mode: PlanMode {
                plan_path: None,
                plan_form: crate::tui::plan_form::PlanForm::new(),
                plan_ready: false,
                plan_turn_snapshot: None,
                editor_request: None,
            },
            plan: Vec::new(),
            files: Vec::new(),
            status: Status::Done,
            interrupt_requested: false,
            status_tick: 0,
            turn_started: None,
            flash: None,
            conversation: Conversation::new(),
            view: ViewModel::new(),
            session: Session::new(),
            composer: Composer::new(),
            input_history: crate::storage::input_history::InputHistory::default(),
            history_recall: HistoryRecall {
                history_index: None,
                history_draft: String::new(),
            },
            sent_draft: String::new(),
            overlays: Overlays {
                modal: Modal::None,
                slash_selected: 0,
                permission_prompt: crate::tui::permission_prompt::PermissionPrompt::new(),
                question_form: crate::tui::question_form::QuestionForm::new(),
                usage: Vec::new(),
                usage_quota: crate::tui::provider::UsageFetchState::Idle,
                usage_scroll: 0,
                usage_scroll_max: usize::MAX,
                help_scroll: 0,
                help_scroll_max: usize::MAX,
                keybinds: crate::tui::keybindings::KeybindingResolver::new(),
                search: crate::tui::search_modal::SearchModal::new(),
                file_picker: crate::tui::file_picker::FilePicker::new(),
            },
            images: Images {
                picker: crate::tui::ui::image::ImagePicker::new(),
                states: std::collections::HashMap::new(),
                states_order: std::collections::VecDeque::new(),
                attached: Vec::new(),
                loads: Vec::new(),
            },
            should_quit: false,
        }
    }

    pub fn model(&self) -> (&str, &str) {
        self.session
            .models
            .get(self.session.model_idx)
            .map(|m| (m.label.as_str(), m.provider_label.as_str()))
            .unwrap_or(("no model", "no provider"))
    }

    pub fn effort(&self) -> &'static str {
        EFFORTS[self.session.effort_idx]
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Thinking | Status::Running) && !self.interrupt_requested
    }

    /// Show `msg` in the status row until [`FLASH_SECS`] elapses or the
    /// next keypress (e.g. "Copied" after a selection copy).
    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), std::time::Instant::now()));
    }

    /// Whether a live flash still holds the status row's right side.
    pub fn flash_text(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < FLASH_TTL)
            .map(|(s, _)| s.as_str())
    }

    /// Decode `data` into a cached render state for tool `id`, reusing the
    /// cached one while the width is unchanged (F.6). The cache is a
    /// capped LRU (see [`Self::touch_image_state`]).
    pub(crate) fn image_state(
        &mut self,
        id: &str,
        data: &str,
        width: u16,
    ) -> Option<std::sync::Arc<crate::tui::ui::image::ImageRenderState>> {
        if let Some((at_width, state)) = self.images.states.get(id)
            && *at_width == width
        {
            let state = state.clone();
            self.touch_image_state(id);
            return Some(state);
        }
        let state = self.images.picker.render_state(data, width)?;
        let state = std::sync::Arc::new(state);
        self.images
            .states
            .insert(id.to_string(), (width, state.clone()));
        self.images.states_order.push_back(id.to_string());
        // Evict least-recently-used entries beyond the cap.
        while self.images.states.len() > IMAGE_STATE_CAP {
            let Some(oldest) = self.images.states_order.pop_front() else {
                break;
            };
            self.images.states.remove(&oldest);
        }
        Some(state)
    }

    /// Mark `id` most-recently-used in the image-state LRU.
    fn touch_image_state(&mut self, id: &str) {
        if let Some(pos) = self.images.states_order.iter().position(|k| k == id) {
            if let Some(k) = self.images.states_order.remove(pos) {
                self.images.states_order.push_back(k);
            }
        }
    }

    /// Pick up finished image loads (path picks and clipboard pastes);
    /// true when the caller owes a repaint.
    pub(crate) fn poll_image_loads(&mut self) -> bool {
        let mut dirty = false;
        let mut i = 0;
        while i < self.images.loads.len() {
            match self.images.loads[i].try_recv() {
                Ok(Ok(block)) => {
                    self.images.loads.swap_remove(i);
                    self.images.attached.push(block);
                    self.flash("Image attached");
                    dirty = true;
                }
                Ok(Err(e)) => {
                    self.images.loads.swap_remove(i);
                    self.flash(format!("Image paste failed: {e}"));
                    dirty = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.images.loads.swap_remove(i);
                }
            }
        }
        dirty
    }

    /// Whether the selected model takes image input (unknown models are
    /// allowed: the catalog is best-effort).
    fn model_supports_vision(&self) -> bool {
        self.session
            .models
            .get(self.session.model_idx)
            .and_then(|m| crate::models_dev::metadata_for(&m.provider, &m.model))
            .is_none_or(|meta| meta.supports_vision)
    }

    /// Attach an image file by path (paste of an image path, F.6).
    pub fn start_file_image_paste(
        &mut self,
        path: std::path::PathBuf,
        media: crate::history::ImageMedia,
    ) {
        if !self.model_supports_vision() {
            self.flash("Model does not support image input");
            return;
        }
        let msg = format!("Reading {}...", path.display());
        self.spawn_image_load(msg, move || {
            crate::tui::ui::image::load_file_image(&path, media)
        });
    }

    /// Attach the clipboard image (Ctrl+V, F.6).
    pub fn start_clipboard_image_paste(&mut self) {
        if !self.model_supports_vision() {
            self.flash("Model does not support image input");
            return;
        }
        self.spawn_image_load(
            "Reading clipboard...".into(),
            crate::tui::ui::image::load_clipboard_image,
        );
    }

    fn spawn_image_load(
        &mut self,
        flash: String,
        f: impl FnOnce() -> Result<crate::history::ImageBlock, String> + Send + 'static,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        self.images.loads.push(rx);
        self.flash(flash);
    }

    /// Drop an expired flash; true when the caller owes a repaint.
    pub fn clear_expired_flash(&mut self) -> bool {
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH_TTL)
        {
            self.flash = None;
            true
        } else {
            false
        }
    }

    // ------------------------------------------------------------------
    // Provider events
    // ------------------------------------------------------------------

    /// How soon the loop must wake for this app's screen, and whether the
    /// clock alone owes a frame. Only the spinner statuses animate; every
    /// other status paints pixels that a timeout cannot change.
    pub fn cadence(&self) -> repaint::Cadence {
        repaint::Cadence::any([
            repaint::Cadence::when(
                matches!(
                    self.status,
                    Status::Thinking | Status::Running | Status::WaitingApproval
                ),
                repaint::Cadence::SPINNER,
            ),
            // A live flash expires on the clock: keep waking up so the
            // status row drops it without waiting for the next event.
            repaint::Cadence::when(self.flash.is_some(), repaint::Cadence::SPINNER),
            // The file picker's walk runs on its own thread: keep coming
            // back so the streaming paths land on screen (and animate the
            // scanning spinner once the list is visible).
            repaint::Cadence::when(
                self.overlays.file_picker.walking(),
                repaint::Cadence::PENDING,
            ),
            repaint::Cadence::when(
                self.overlays.file_picker.walking() && self.overlays.file_picker.visible(),
                repaint::Cadence::SPINNER,
            ),
        ])
    }

    pub fn handle_event(&mut self, ev: AgentEvent) {
        let was_following = self.view.follow;
        match ev {
            AgentEvent::StatusChanged(s) => {
                let was_busy = matches!(self.status, Status::Thinking | Status::Running);
                // A fresh busy status starts the elapsed-seconds clock; a
                // settled status stops it.
                if matches!(s, Status::Thinking | Status::Running) && !was_busy {
                    self.turn_started = Some(std::time::Instant::now());
                } else if !matches!(s, Status::Thinking | Status::Running) {
                    self.turn_started = None;
                }
                self.status = s;
                self.interrupt_requested = false;
                self.update_plan_lifecycle(was_busy);
            }
            AgentEvent::PlanSet(plan) => self.plan = plan,
            AgentEvent::FilesSet(files) => self.files = files,
            AgentEvent::TokenUsage(label) => self.session.token_label = label,
            AgentEvent::CatalogSet { models, current } => {
                if !models.is_empty() {
                    self.session.models = models;
                    self.session.model_idx = current.min(self.session.models.len() - 1);
                }
            }
            AgentEvent::SessionInfo { cwd, branch } => {
                self.session.cwd = cwd;
                self.session.branch = branch;
            }
            AgentEvent::UsageSnapshot(rows) => {
                self.overlays.usage = rows;
                // Keep an open /usage overlay fresh when a run completes.
                if matches!(self.overlays.modal, Modal::Usage(_)) {
                    self.overlays.modal = Modal::Usage(self.overlays.usage.clone());
                }
            }
            AgentEvent::UsageQuota(state) => self.overlays.usage_quota = state,
            AgentEvent::SessionLoaded { messages, draft } => {
                self.reset_conversation();
                for msg in messages {
                    let msg = match msg {
                        LoadedMessage::User(text) => Message::User(text),
                        LoadedMessage::Assistant(text) => Message::Assistant(text),
                    };
                    self.conversation.messages.push(msg);
                }
                // F.3 draft preservation: a draft saved across checkpoints
                // comes back into the composer, cursor at the end.
                if !draft.is_empty() {
                    self.composer.set_text(draft);
                    self.sent_draft = self.composer.text.clone();
                }
            }
            AgentEvent::PermissionRequest {
                id,
                tool,
                scopes,
                files,
                commands,
            } => self
                .overlays
                .permission_prompt
                .open(id, tool, scopes, files, commands),
            AgentEvent::PermissionResolved { id } => {
                if self.overlays.permission_prompt.id() == Some(id.as_str()) {
                    self.overlays.permission_prompt.close();
                }
                // Deny and cancel paths never produce a completed card, so
                // the decision itself must retire the "needs approval" badge.
                if let Some(Message::Tool { diff, .. }) =
                    self.conversation.messages.iter_mut().find(|m| {
                        matches!(
                            m,
                            Message::Tool {
                                id: mid,
                                diff: Some(DiffState::Pending),
                                ..
                            } if *mid == id
                        )
                    })
                {
                    *diff = None;
                }
            }
            AgentEvent::QuestionRequest { id, questions } => {
                self.overlays.question_form.open(id, questions)
            }
            AgentEvent::QuestionResolved { id } => {
                if self.overlays.question_form.id() == Some(id.as_str()) {
                    self.overlays.question_form.close();
                }
            }
            // Message-bearing events merge into the conversation.
            ev => self.conversation.apply(ev),
        }
        if was_following {
            self.view.follow = true;
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Indices of tool blocks that can be focused: collapsible blocks and
    /// pending diffs, in display order.
    pub(crate) fn focus_targets(&self) -> Vec<usize> {
        self.conversation
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_collapsible_tool() || m.is_pending_diff())
            .map(|(i, _)| i)
            .collect()
    }

    pub fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        let q = self.composer.text.as_str();
        if !commands::opens_slash_menu(q) {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .flat_map(|spec| {
                [spec.slash, spec.alias]
                    .into_iter()
                    .flatten()
                    .map(|slash| (slash, spec.desc))
            })
            .filter(|(cmd, _)| q == "/" || cmd.starts_with(q))
            .collect()
    }

    /// Plain text of every rendered segment, one entry per segment index
    /// (the search corpus). Derived on demand like the reference's
    /// `segment_search_texts`: output can land behind the modal.
    pub(crate) fn search_texts(&self) -> Vec<String> {
        (0..self.view.segments.len())
            .map(|i| {
                self.view
                    .segments
                    .get(i)
                    .map(|seg| {
                        seg.lines()
                            .iter()
                            .map(|l| {
                                l.spans
                                    .iter()
                                    .map(|s| s.content.as_ref())
                                    .collect::<String>()
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Number of live search matches (test accessor).
    #[cfg(test)]
    pub(crate) fn search_matches(&self) -> usize {
        self.overlays.search.match_count()
    }

    /// Re-run the fuzzy match over the current corpus, then keep the
    /// transcript parked on the selected match.
    pub(crate) fn refresh_search_matches(&mut self) {
        let corpus = self.search_texts();
        self.overlays.search.update_matches(|| corpus);
        self.sync_search_highlight();
    }

    /// Scroll the transcript to the currently selected match and highlight
    /// its segment while the search modal stays open.
    pub(crate) fn sync_search_highlight(&mut self) {
        let at = self.overlays.search.current_segment_index();
        if let Some((seg, row)) = at {
            self.scroll_to_segment(seg, row);
        }
        self.view.highlight_segment = at.map(|(seg, _)| seg);
    }

    /// Poll the file picker's walker (paths streaming in, walk endings,
    /// self-close flashes); true when the screen changed. Called by the
    /// event loop every turn, since nothing else announces the walker.
    pub(crate) fn tick_file_picker(&mut self) -> bool {
        let (mut dirty, flash) = self.overlays.file_picker.tick();
        if let Some(msg) = flash {
            self.flash(msg);
        }
        dirty.take()
    }

    /// Hand the current composer draft to the provider when it changed
    /// (F.3 draft preservation). Called once per loop turn, like the
    /// reference's per-frame checkpoint; unchanged drafts cost nothing.
    /// Submitting also lands here: the emptied draft clears the stored
    /// copy a frame before the turn mirrors the prompt back.
    pub(crate) fn sync_draft(&mut self, tx: &mpsc::UnboundedSender<Command>) {
        if self.composer.text != self.sent_draft {
            self.sent_draft = self.composer.text.clone();
            let _ = tx.send(Command::SetDraft(self.composer.text.clone()));
        }
    }

    pub fn slash_open(&self) -> bool {
        matches!(self.overlays.modal, Modal::None) && !self.slash_matches().is_empty()
    }

    pub fn palette_items(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let q = match &self.overlays.modal {
            Modal::Palette { query, .. } => query.to_lowercase(),
            _ => String::new(),
        };
        COMMANDS
            .iter()
            .map(|spec| (spec.id, spec.label, spec.hint))
            .filter(|(_, label, _)| label.to_lowercase().contains(&q))
            .collect()
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    pub(crate) fn submit(&mut self, tx: &mpsc::UnboundedSender<Command>) {
        let text = self.composer.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Enter on an open slash menu executes the highlighted command.
        let slash = self.slash_matches();
        if text.starts_with('/') && !slash.is_empty() {
            let (cmd, _) = slash[self.overlays.slash_selected.min(slash.len() - 1)];
            self.composer.clear();
            self.input_history.push(text);
            self.history_recall.history_index = None;
            self.history_recall.history_draft.clear();
            self.run_slash(cmd, tx);
            return;
        }
        // Bang-mode: run the line as a shell command, bypassing the model.
        if let Some(prefix) = shell::parse_shell_prefix(&text) {
            let cmd = prefix.command.trim();
            if cmd == "cd" || cmd.starts_with("cd ") {
                // The subshell cannot change the session's cwd; only /cd can.
                self.flash("Only /cd can change the working directory");
            }
            let sigil = if prefix.visible { "!" } else { "!!" };
            self.conversation.assistant_open = false;
            self.conversation
                .messages
                .push(Message::User(format!("{sigil} {}", prefix.command)));
            let _ = tx.send(Command::Shell {
                command: prefix.command,
                visible: prefix.visible,
            });
            self.input_history.push(text);
            self.history_recall.history_index = None;
            self.history_recall.history_draft.clear();
            self.composer.clear();
            self.view.follow = true;
            return;
        }
        self.conversation.assistant_open = false;
        self.conversation.messages.push(Message::User(text.clone()));
        let images = std::mem::take(&mut self.images.attached);
        let _ = tx.send(Command::SendMessage(
            text.clone(),
            self.agent_mode(),
            images,
        ));
        self.input_history.push(text);
        self.history_recall.history_index = None;
        self.history_recall.history_draft.clear();
        self.composer.clear();
        self.view.follow = true;
    }

    /// Whether the history entry at `index` is normal text that ↑/↓ may
    /// recall. Slash commands are skipped so recalling one cannot reopen the
    /// slash menu and hijack the history keys. Ported from the reference
    /// `InputBox::history_up`.
    fn recallable(&self, index: usize) -> bool {
        self.input_history
            .get(index)
            .is_some_and(|entry| !commands::opens_slash_menu(entry))
    }

    /// Recall the previous (older) normal-text history entry, saving the
    /// in-progress text as the draft restored by [`Self::history_down`].
    pub fn history_up(&mut self) {
        let from = match self.history_recall.history_index {
            None => self.input_history.len(),
            Some(0) => return,
            Some(i) => i,
        };
        let Some(new_index) = (0..from).rev().find(|&i| self.recallable(i)) else {
            return; // no older normal-text entry to recall
        };
        if self.history_recall.history_index.is_none() {
            self.history_recall.history_draft = self.composer.text.clone();
        }
        self.history_recall.history_index = Some(new_index);
        let entry = self
            .input_history
            .get(new_index)
            .expect("index found by the recallable scan")
            .to_string();
        self.composer.set_text(entry);
    }

    /// Recall the next (newer) normal-text history entry; ↓ past the newest
    /// restores the draft saved on entry.
    pub fn history_down(&mut self) {
        let Some(i) = self.history_recall.history_index else {
            return;
        };
        match (i + 1..self.input_history.len()).find(|&j| self.recallable(j)) {
            Some(new_index) => {
                self.history_recall.history_index = Some(new_index);
                let entry = self
                    .input_history
                    .get(new_index)
                    .expect("index found by the recallable scan")
                    .to_string();
                self.composer.set_text(entry);
            }
            None => {
                self.history_recall.history_index = None;
                let draft = std::mem::take(&mut self.history_recall.history_draft);
                self.composer.set_text(draft);
            }
        }
    }

    pub(crate) fn reset_conversation(&mut self) {
        self.conversation.messages.clear();
        self.conversation.collapsed.clear();
        self.conversation.expanded_bodies.clear();
        self.conversation.focused = None;
        self.view.hover_tool = None;
        self.view.pending_click = None;
        self.view.highlight_segment = None;
        self.view.tool_regions.clear();
        self.view.notice_regions.clear();
        self.overlays.modal = Modal::None;
        self.view.scroll = ScrollPos::default();
        self.view.segments.clear();
        self.view.follow = true;
        self.images.states.clear();
        self.images.states_order.clear();
    }
}

/// Shared fixtures for the submodule test modules.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::tui::provider::AgentEvent;
    use crate::tui::ui;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Paint the app once so the renderer builds the segment document and
    /// resolves the viewport, as the real loop does before key handling.
    pub(crate) fn draw_app(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
    }

    pub(crate) fn scrolled_app() -> App {
        let mut app = App::new();
        for i in 0..30 {
            app.handle_event(AgentEvent::AssistantText(format!(
                "message number {i} with some wrapping text to fill rows"
            )));
        }
        app
    }

    pub(crate) fn screen_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|r| {
                (0..buf.area.width)
                    .map(|c| buf[(c, r)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn app_with_collapsible_tool() -> App {
        use crate::tui::provider::{ToolCallData, ToolKind};

        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            awaiting_approval: false,
            id: "r1".into(),
            kind: ToolKind::Read {
                path: "src/x.ts".into(),
                summary: "10 lines".into(),
            },
            lines: vec![],
            image: None,
        }));
        // Card spans rows 2..6, columns 2..40 (as the renderer would report).
        app.view.tool_regions = vec![(
            0,
            ratatui::layout::Rect {
                x: 2,
                y: 2,
                width: 38,
                height: 4,
            },
        )];
        assert!(app.conversation.collapsed.contains(&"r1".to_string()));
        app
    }

    pub(crate) fn usage_rows() -> Vec<crate::tui::provider::UsageRow> {
        vec![
            crate::tui::provider::UsageRow {
                model: "anthropic/claude-sonnet-5".into(),
                tokens: 12_345,
                cost: Some(0.0123),
            },
            crate::tui::provider::UsageRow {
                model: "mock/free-model".into(),
                tokens: 500,
                cost: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::screen_text;
    use super::*;
    use crate::tui::repaint::Cadence;

    /// The composer info line carries the active mode label (F.2).
    #[test]
    fn composer_info_line_shows_mode_label() {
        let mut app = App::new();
        app.session.sidebar_open = false;
        let build = screen_text(&mut app, 120, 24);
        assert!(build.contains("[BUILD]"), "build label missing:\n{build}");
        assert!(!build.contains("[PLAN]"));
        app.toggle_mode();
        let plan = screen_text(&mut app, 120, 24);
        assert!(plan.contains("[PLAN]"), "plan label missing:\n{plan}");
    }

    /// Only the statuses that render the spinner glyph owe cadence frames;
    /// settled statuses sleep until a real event wakes the loop.
    #[test]
    fn cadence_tracks_animating_statuses() {
        let mut app = App::new();
        for status in [Status::Done, Status::Failed] {
            app.handle_event(AgentEvent::StatusChanged(status));
            assert_eq!(app.cadence(), Cadence::IDLE, "{status:?} is static");
        }
        for status in [Status::Thinking, Status::Running, Status::WaitingApproval] {
            app.handle_event(AgentEvent::StatusChanged(status));
            assert_eq!(app.cadence(), Cadence::SPINNER, "{status:?} animates");
        }
    }

    /// The status row gains the cwd (abbreviated to its last component) and
    /// the turn's elapsed-seconds prompt progress.
    #[test]
    fn status_row_shows_cwd_and_elapsed_seconds() {
        let mut app = App::new();
        app.session.sidebar_open = false;
        app.handle_event(AgentEvent::SessionInfo {
            cwd: "/Users/x/Projects/craft-code".into(),
            branch: "main".into(),
        });
        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        let text = screen_text(&mut app, 120, 24);
        assert!(
            text.contains("…/craft-code"),
            "cwd segment missing:\n{text}"
        );
        assert!(text.contains("running 0s"), "elapsed missing:\n{text}");
    }

    /// A loaded session's preserved draft lands back in the composer (F.3),
    /// and `sync_draft` hands the provider only the changes.
    #[test]
    fn loaded_draft_restores_and_sync_draft_sends_only_changes() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.handle_event(AgentEvent::SessionLoaded {
            messages: vec![crate::tui::provider::LoadedMessage::User("earlier".into())],
            draft: "unsent words".into(),
        });
        assert_eq!(app.composer.text, "unsent words");

        // The restored draft is the new baseline: no SetDraft echo for it.
        app.sync_draft(&tx);
        assert!(rx.try_recv().is_err(), "unchanged draft sends nothing");

        app.composer.set_text("typed more".to_string());
        app.sync_draft(&tx);
        assert!(
            matches!(rx.try_recv(), Ok(Command::SetDraft(t)) if t == "typed more"),
            "a changed draft reaches the provider"
        );
        app.sync_draft(&tx);
        assert!(rx.try_recv().is_err(), "each change is sent exactly once");
    }

    /// A flash toast takes over the status row's right side until it
    /// expires; a keypress clears it early.
    #[test]
    fn flash_toast_shows_then_expires() {
        let mut app = App::new();
        app.flash("Copied");
        let text = screen_text(&mut app, 120, 24);
        assert!(text.contains("Copied"), "flash missing:\n{text}");

        // Deadline already past: the draw path drops it.
        app.flash = Some((
            "Copied".into(),
            std::time::Instant::now() - FLASH_TTL - std::time::Duration::from_secs(1),
        ));
        let text = screen_text(&mut app, 120, 24);
        assert!(!text.contains("Copied"), "expired flash lingers:\n{text}");

        // A live flash is also dismissed by any keypress.
        app.flash("Copied");
        let (tx, _rx) = mpsc::unbounded_channel();
        app.handle_key(
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('a')),
            &tx,
        );
        let text = screen_text(&mut app, 120, 24);
        assert!(
            !text.contains("Copied"),
            "keypress must clear the flash:\n{text}"
        );
    }

    /// The image-state cache is capped (LRU) and emptied on reset.
    #[test]
    fn image_states_are_lru_capped_and_reset() {
        let mut app = App::new();
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        for i in 0..40 {
            let id = format!("img-{i}");
            // A 1x1 PNG decodes at any width; ids differ per call.
            app.image_state(&id, png, 10);
        }
        assert!(
            app.images.states.len() <= IMAGE_STATE_CAP,
            "cache grew past the cap: {}",
            app.images.states.len()
        );
        assert!(
            !app.images.states.contains_key("img-0"),
            "the oldest entry is evicted"
        );
        assert!(app.images.states.contains_key("img-39"));

        app.reset_conversation();
        assert!(app.images.states.is_empty());
        assert!(app.images.states_order.is_empty());
    }
}
