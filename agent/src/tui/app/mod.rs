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
pub use self::commands::{COMMANDS, CommandSpec, help_rows};
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

pub struct App {
    // --- mode (F.2 Tab cycling, Build/Plan) ---
    pub mode: Mode,
    /// Session's allocated plan file; set on first entry into Plan mode.
    pub plan_path: Option<std::path::PathBuf>,

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

    // --- composer ---
    pub composer: Composer,
    /// Rolling user-input history (↑/↓ recall; persisted per state dir).
    pub input_history: crate::storage::input_history::InputHistory,
    /// Position in `input_history` while recalling; `None` = live editing.
    pub history_index: Option<usize>,
    /// The in-progress text saved when history recall starts, restored on
    /// ↓ past the newest entry.
    pub history_draft: String,

    // --- overlays ---
    /// The one modal currently open (palette / model menu / confirm),
    /// exclusive by construction. The slash popup is not a modal: it rides
    /// on the composer (shown when the composer starts with '/').
    pub modal: Modal,
    pub slash_selected: usize, // row in the slash popup
    /// Latest per-model usage snapshot from the provider (the `/usage`
    /// overlay's data; refreshed after every completed run).
    pub usage: Vec<UsageRow>,

    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            mode: Mode::Build,
            plan_path: None,
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
            history_index: None,
            history_draft: String::new(),
            modal: Modal::None,
            slash_selected: 0,
            usage: Vec::new(),
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
        ])
    }

    pub fn handle_event(&mut self, ev: AgentEvent) {
        let was_following = self.view.follow;
        match ev {
            AgentEvent::StatusChanged(s) => {
                // A fresh busy status starts the elapsed-seconds clock; a
                // settled status stops it.
                if matches!(s, Status::Thinking | Status::Running)
                    && !matches!(self.status, Status::Thinking | Status::Running)
                {
                    self.turn_started = Some(std::time::Instant::now());
                } else if !matches!(s, Status::Thinking | Status::Running) {
                    self.turn_started = None;
                }
                self.status = s;
                self.interrupt_requested = false;
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
                self.usage = rows;
                // Keep an open /usage overlay fresh when a run completes.
                if matches!(self.modal, Modal::Usage(_)) {
                    self.modal = Modal::Usage(self.usage.clone());
                }
            }
            AgentEvent::SessionLoaded { messages } => {
                self.reset_conversation();
                for msg in messages {
                    let msg = match msg {
                        LoadedMessage::User(text) => Message::User(text),
                        LoadedMessage::Assistant(text) => Message::Assistant(text),
                    };
                    self.conversation.messages.push(msg);
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

    pub(crate) fn focused_pending_diff(&self) -> Option<usize> {
        self.conversation.focused.filter(|&i| {
            self.conversation
                .messages
                .get(i)
                .map(|m| m.is_pending_diff())
                .unwrap_or(false)
        })
    }

    pub(crate) fn last_pending_diff(&self) -> Option<usize> {
        self.conversation
            .messages
            .iter()
            .rposition(|m| m.is_pending_diff())
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

    pub fn slash_open(&self) -> bool {
        matches!(self.modal, Modal::None) && !self.slash_matches().is_empty()
    }

    pub fn palette_items(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let q = match &self.modal {
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
            let (cmd, _) = slash[self.slash_selected.min(slash.len() - 1)];
            self.composer.clear();
            self.input_history.push(text);
            self.history_index = None;
            self.history_draft.clear();
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
            self.history_index = None;
            self.history_draft.clear();
            self.composer.clear();
            self.view.follow = true;
            return;
        }
        self.conversation.assistant_open = false;
        self.conversation.messages.push(Message::User(text.clone()));
        let _ = tx.send(Command::SendMessage(text.clone(), self.agent_mode()));
        self.input_history.push(text);
        self.history_index = None;
        self.history_draft.clear();
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
        let from = match self.history_index {
            None => self.input_history.len(),
            Some(0) => return,
            Some(i) => i,
        };
        let Some(new_index) = (0..from).rev().find(|&i| self.recallable(i)) else {
            return; // no older normal-text entry to recall
        };
        if self.history_index.is_none() {
            self.history_draft = self.composer.text.clone();
        }
        self.history_index = Some(new_index);
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
        let Some(i) = self.history_index else {
            return;
        };
        match (i + 1..self.input_history.len()).find(|&j| self.recallable(j)) {
            Some(new_index) => {
                self.history_index = Some(new_index);
                let entry = self
                    .input_history
                    .get(new_index)
                    .expect("index found by the recallable scan")
                    .to_string();
                self.composer.set_text(entry);
            }
            None => {
                self.history_index = None;
                let draft = std::mem::take(&mut self.history_draft);
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
        self.view.tool_regions.clear();
        self.view.notice_regions.clear();
        self.modal = Modal::None;
        self.view.scroll = ScrollPos::default();
        self.view.segments.clear();
        self.view.follow = true;
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
}
