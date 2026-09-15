//! Application state and input handling. The App renders whatever the
//! provider streams in and translates key presses into provider commands.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::tui::composer::Composer;
use crate::tui::modals::Modal;
use crate::tui::provider::{
    AgentEvent, Command, ModelChoice, PlanItem, Status, ToolCallData, ToolKind, ToolLine,
    TouchedFile,
};
use crate::tui::selection::{
    Selection, clamp_to, copy_to_clipboard, extract_selection_text, rect_contains,
};

/// Models shown before the provider's catalog arrives (or under the test mock).
const SEED_MODELS: [(&str, &str); 4] = [
    ("GLM-5.3", "Zhipu AI Coding Plan"),
    ("Claude Sonnet 4.5", "Anthropic"),
    ("Claude Opus 4.1", "Anthropic"),
    ("DeepSeek V3.2", "DeepSeek"),
];

fn seed_models() -> Vec<ModelChoice> {
    SEED_MODELS
        .iter()
        .map(|(label, provider)| ModelChoice {
            provider: provider.to_string(),
            model: label.to_string(),
            label: label.to_string(),
            provider_label: provider.to_string(),
        })
        .collect()
}

pub const EFFORTS: [&str; 3] = ["low", "medium", "high"];

pub const SLASH_COMMANDS: [(&str, &str); 7] = [
    ("/clear", "Clear conversation context"),
    ("/compact", "Compact context to save tokens"),
    ("/undo", "Revert the last edit"),
    ("/model", "Switch model"),
    ("/sessions", "List sessions"),
    ("/auto-review", "Toggle LLM auto-review of permissions"),
    ("/help", "Show keybindings"),
];

/// Palette entries: (id, label, hint).
pub const PALETTE_COMMANDS: [(&str, &str, &str); 6] = [
    ("new", "New session", ""),
    ("sessions", "Switch session", ""),
    ("toggle-sidebar", "Toggle context panel", "ctrl+b"),
    ("model", "Change model", "ctrl+l"),
    ("clear", "Clear context", "/clear"),
    ("copy", "Copy last message", ""),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffState {
    Pending,
    Approved,
    Rejected,
}

pub enum Message {
    User(String),
    Assistant(String),
    /// Model reasoning (thinking) text, rendered dimmer than replies.
    Thinking(String),
    Tool {
        id: String,
        kind: ToolKind,
        lines: Vec<ToolLine>,
        diff: Option<DiffState>,
    },
}

impl Message {
    fn is_collapsible_tool(&self) -> bool {
        matches!(self, Message::Tool { kind, .. } if kind.collapsible())
    }

    fn is_pending_diff(&self) -> bool {
        matches!(
            self,
            Message::Tool {
                diff: Some(DiffState::Pending),
                ..
            }
        )
    }
}

/// Message-list state: what the provider streams in, plus focus/collapse
/// chrome that rides on it.
pub struct Conversation {
    pub messages: Vec<Message>,
    pub collapsed: Vec<String>, // tool ids currently collapsed
    pub focused: Option<usize>, // message index of focused tool block
    /// True while a streamed [`Message::Assistant`] is still being appended to.
    assistant_open: bool,
    /// True while a streamed [`Message::Thinking`] is still being appended to.
    thinking_open: bool,
}

impl Conversation {
    fn new() -> Self {
        Conversation {
            messages: Vec::new(),
            collapsed: Vec::new(),
            focused: None,
            assistant_open: false,
            thinking_open: false,
        }
    }

    /// Merge a provider event into the message list. Non-message events
    /// (status, plan, catalog, ...) are ignored; App routes those itself.
    pub(crate) fn apply(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AssistantText(text) => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.messages.push(Message::Assistant(text));
            }
            AgentEvent::AssistantDelta(text) => {
                self.thinking_open = false;
                match self.messages.last_mut() {
                    Some(Message::Assistant(buf)) if self.assistant_open => buf.push_str(&text),
                    _ => self.messages.push(Message::Assistant(text)),
                }
                self.assistant_open = true;
            }
            AgentEvent::ReasoningDelta(text) => {
                self.assistant_open = false;
                match self.messages.last_mut() {
                    Some(Message::Thinking(buf)) if self.thinking_open => buf.push_str(&text),
                    _ => self.messages.push(Message::Thinking(text)),
                }
                self.thinking_open = true;
            }
            AgentEvent::AssistantEnd => {
                self.assistant_open = false;
                self.thinking_open = false;
            }
            AgentEvent::ToolCall(ToolCallData {
                id,
                kind,
                lines,
                awaiting_approval,
            }) => {
                // Tool boundaries close any open streamed paragraph.
                self.assistant_open = false;
                self.thinking_open = false;
                // Cards merge by id: a start event shows the running card, the
                // completion event fills in its body.
                let existing = self
                    .messages
                    .iter_mut()
                    .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id));
                match existing {
                    Some(Message::Tool {
                        kind: existing_kind,
                        lines: body,
                        diff,
                        ..
                    }) => {
                        // Completion events carry the authoritative kind
                        // (summaries arrive with the result).
                        *existing_kind = kind;
                        *body = lines;
                        if awaiting_approval && matches!(diff, None | Some(DiffState::Pending)) {
                            *diff = Some(DiffState::Pending);
                        }
                    }
                    _ => {
                        let diff = if matches!(kind, ToolKind::Edit { .. }) && awaiting_approval {
                            Some(DiffState::Pending)
                        } else {
                            None
                        };
                        let collapsible = kind.collapsible();
                        self.messages.push(Message::Tool {
                            id: id.clone(),
                            kind,
                            lines,
                            diff,
                        });
                        if collapsible {
                            self.collapsed.push(id);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Viewport state: scroll/follow plus the renderer-written frame snapshot
/// (message starts, per-row text, hit regions) and pointer state.
pub struct ViewModel {
    pub scroll: u16,
    pub follow: bool,
    pub max_scroll: u16,
    pub view_height: u16,
    /// Line offset where each message starts (filled by the renderer).
    pub msg_starts: Vec<usize>,
    /// Text of the last rendered frame, one entry per terminal row (filled by
    /// the renderer; used to extract selection text on copy).
    pub frame_text: Vec<String>,
    /// Selectable regions of the last frame: chat messages and composer input.
    pub msg_area: Rect,
    pub composer_area: Rect,
    pub selection: Option<Selection>,
    /// Screen rects of collapsible tool cards in the last frame: (message
    /// index, rect). Used for hover highlight and click-to-toggle.
    pub tool_regions: Vec<(usize, Rect)>,
    /// Tool card under the pointer (hover state).
    pub hover_tool: Option<usize>,
    /// Card the current click started on; a press without drag toggles it.
    pub pending_click: Option<usize>,
}

impl ViewModel {
    fn new() -> Self {
        ViewModel {
            scroll: 0,
            follow: true,
            max_scroll: 0,
            view_height: 0,
            msg_starts: Vec::new(),
            frame_text: Vec::new(),
            msg_area: Rect::default(),
            composer_area: Rect::default(),
            selection: None,
            tool_regions: Vec::new(),
            hover_tool: None,
            pending_click: None,
        }
    }
}

/// Session chrome: model catalog + selection, cwd/branch, tokens, sidebar.
pub struct Session {
    pub models: Vec<ModelChoice>,
    pub model_idx: usize,
    pub effort_idx: usize,
    pub cwd: String,
    pub branch: String,
    pub token_label: String,
    pub sidebar_open: bool,
}

impl Session {
    fn new() -> Self {
        Session {
            models: seed_models(),
            model_idx: 0,
            effort_idx: 2, // "high", the prototype default
            cwd: "~/Projects/craft-web".into(),
            branch: "fix/session-refresh".into(),
            token_label: "…".into(),
            sidebar_open: true,
        }
    }
}

pub struct App {
    // --- provider-driven state ---
    pub plan: Vec<PlanItem>,
    pub files: Vec<TouchedFile>,
    pub status: Status,
    /// Animation frame counter for the status indicator (advanced per frame).
    pub status_tick: usize,

    // --- state groups (fields stay pub for the renderer for now;
    // accessor encapsulation is a follow-up) ---
    pub conversation: Conversation,
    pub view: ViewModel,
    pub session: Session,

    // --- composer ---
    pub composer: Composer,

    // --- overlays ---
    /// The one modal currently open (palette / model menu / confirm),
    /// exclusive by construction. The slash popup is not a modal: it rides
    /// on the composer (shown when the composer starts with '/').
    pub modal: Modal,
    pub slash_selected: usize, // row in the slash popup

    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            plan: Vec::new(),
            files: Vec::new(),
            status: Status::Done,
            status_tick: 0,
            conversation: Conversation::new(),
            view: ViewModel::new(),
            session: Session::new(),
            composer: Composer::new(),
            modal: Modal::None,
            slash_selected: 0,
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

    /// Open the model picker with the current selection highlighted,
    /// replacing any modal already open.
    fn open_model_menu(&mut self) {
        if !self.session.models.is_empty() {
            self.modal = Modal::ModelMenu(
                self.session
                    .model_idx
                    .min(self.session.models.len().saturating_sub(1)),
            );
        }
    }

    pub fn effort(&self) -> &'static str {
        EFFORTS[self.session.effort_idx]
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Thinking | Status::Running)
    }

    // ------------------------------------------------------------------
    // Provider events
    // ------------------------------------------------------------------

    pub fn handle_event(&mut self, ev: AgentEvent) {
        let was_following = self.view.follow;
        match ev {
            AgentEvent::StatusChanged(s) => self.status = s,
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
    fn focus_targets(&self) -> Vec<usize> {
        self.conversation
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_collapsible_tool() || m.is_pending_diff())
            .map(|(i, _)| i)
            .collect()
    }

    fn focused_pending_diff(&self) -> Option<usize> {
        self.conversation.focused.filter(|&i| {
            self.conversation
                .messages
                .get(i)
                .map(|m| m.is_pending_diff())
                .unwrap_or(false)
        })
    }

    fn last_pending_diff(&self) -> Option<usize> {
        self.conversation
            .messages
            .iter()
            .rposition(|m| m.is_pending_diff())
    }

    pub fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        let q = self.composer.text.as_str();
        if !q.starts_with('/') {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .copied()
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
        PALETTE_COMMANDS
            .iter()
            .copied()
            .filter(|(_, label, _)| label.to_lowercase().contains(&q))
            .collect()
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    fn submit(&mut self, tx: &mpsc::UnboundedSender<Command>) {
        let text = self.composer.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Enter on an open slash menu executes the highlighted command.
        let slash = self.slash_matches();
        if text.starts_with('/') && !slash.is_empty() {
            let (cmd, _) = slash[self.slash_selected.min(slash.len() - 1)];
            self.composer.clear();
            self.run_slash(cmd, tx);
            return;
        }
        self.conversation.assistant_open = false;
        self.conversation.messages.push(Message::User(text.clone()));
        let _ = tx.send(Command::SendMessage(text));
        self.composer.clear();
        self.view.follow = true;
    }

    fn reset_conversation(&mut self) {
        self.conversation.messages.clear();
        self.conversation.collapsed.clear();
        self.conversation.focused = None;
        self.view.hover_tool = None;
        self.view.pending_click = None;
        self.view.tool_regions.clear();
        self.modal = Modal::None;
        self.view.scroll = 0;
        self.view.follow = true;
    }

    fn run_slash(&mut self, cmd: &str, tx: &mpsc::UnboundedSender<Command>) {
        match cmd {
            "/clear" => {
                self.reset_conversation();
                let _ = tx.send(Command::Clear);
            }
            "/undo" => {
                let _ = tx.send(Command::Undo);
            }
            "/model" => self.open_model_menu(),
            "/auto-review" => {
                let _ = tx.send(Command::ToggleAutoReview);
            }
            // Compact/help/sessions are no-ops for now.
            _ => {}
        }
    }

    pub(crate) fn run_palette(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        match id {
            "new" => {
                self.reset_conversation();
                let _ = tx.send(Command::Reset);
            }
            "toggle-sidebar" => self.session.sidebar_open = !self.session.sidebar_open,
            "model" => self.open_model_menu(),
            "clear" => {
                self.reset_conversation();
                let _ = tx.send(Command::Clear);
            }
            // Copy/sessions are no-ops under the mock provider.
            _ => {}
        }
    }

    fn approve(&mut self, idx: usize, tx: &mpsc::UnboundedSender<Command>) {
        if let Some(Message::Tool { id, diff, .. }) = self.conversation.messages.get_mut(idx) {
            *diff = Some(DiffState::Approved);
            let _ = tx.send(Command::Approve(id.clone()));
        }
        self.conversation.focused = None;
    }

    pub(crate) fn reject_confirmed(&mut self, tx: &mpsc::UnboundedSender<Command>) {
        if let Modal::ConfirmReject(id) = std::mem::replace(&mut self.modal, Modal::None) {
            if let Some(Message::Tool { diff, .. }) = self
                .conversation
                .messages
                .iter_mut()
                .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id))
            {
                *diff = Some(DiffState::Rejected);
            }
            let _ = tx.send(Command::Reject(id));
        }
        self.conversation.focused = None;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let new = self.view.scroll as i32 + delta;
        self.view.scroll = new.clamp(0, self.view.max_scroll as i32) as u16;
        self.view.follow = self.view.scroll >= self.view.max_scroll;
        // Card rects move with the scroll; stale hover/click state is dropped.
        self.view.hover_tool = None;
        self.view.pending_click = None;
    }

    /// Collapsible tool card (message index) at a screen position, if any.
    pub fn tool_at(&self, row: u16, col: u16) -> Option<usize> {
        self.view
            .tool_regions
            .iter()
            .find(|(_, r)| rect_contains(*r, row, col))
            .map(|(i, _)| *i)
    }

    fn toggle_tool(&mut self, idx: usize) {
        if let Some(Message::Tool { id, kind, .. }) = self.conversation.messages.get(idx) {
            if kind.collapsible() {
                if let Some(pos) = self.conversation.collapsed.iter().position(|c| c == id) {
                    self.conversation.collapsed.remove(pos);
                } else {
                    self.conversation.collapsed.push(id.clone());
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Mouse: wheel scroll + app-side text selection
    // ------------------------------------------------------------------

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::Moved => {
                self.view.hover_tool = self.tool_at(mouse.row, mouse.column);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A selection starts only inside a selectable region (chat
                // messages or the composer input) and is confined to it; a
                // click elsewhere (sidebar, chrome) just clears the highlight.
                let pos = (mouse.row, mouse.column);
                self.view.selection = [self.view.msg_area, self.view.composer_area]
                    .iter()
                    .copied()
                    .find(|r| rect_contains(*r, mouse.row, mouse.column))
                    .map(|region| Selection {
                        anchor: pos,
                        head: pos,
                        region,
                    });
                self.view.pending_click = self.tool_at(mouse.row, mouse.column);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // A drag is a text selection, not a card press.
                self.view.pending_click = None;
                if let Some(sel) = &mut self.view.selection {
                    sel.head = clamp_to(sel.region, mouse.row, mouse.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let no_drag = self.view.selection.map(|s| s.is_empty()).unwrap_or(true);
                match (self.view.pending_click.take(), no_drag) {
                    // Press without drag on a card: toggle it.
                    (Some(idx), true) => {
                        self.view.selection = None;
                        self.toggle_tool(idx);
                    }
                    _ => self.copy_selection(),
                }
            }
            _ => {}
        }
    }

    /// Extract the selected text from the last rendered frame and copy it to
    /// the system clipboard. Tiny (single-cell) "selections" are treated as
    /// plain clicks and just clear the highlight.
    fn copy_selection(&mut self) {
        let Some(sel) = self.view.selection else {
            return;
        };
        if sel.is_empty() {
            self.view.selection = None;
            return;
        }
        let text = extract_selection_text(&self.view.frame_text, sel);
        self.view.selection = None;
        if !text.is_empty() {
            copy_to_clipboard(&text);
        }
    }

    /// After focus changes, make sure the focused block is in view.
    fn ensure_focus_visible(&mut self) {
        let Some(i) = self.conversation.focused else {
            return;
        };
        let Some(&start) = self.view.msg_starts.get(i) else {
            return;
        };
        let start = start as i32;
        let top = self.view.scroll as i32;
        let bottom = top + self.view.view_height as i32;
        if start < top || start >= bottom {
            self.view.scroll = (start - 2).max(0).min(self.view.max_scroll as i32) as u16;
            self.view.follow = false;
        }
    }

    // ------------------------------------------------------------------
    // Key handling
    // ------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // A modal owns the keyboard while open; its keys never fall through
        // to base chords (so ctrl+q does not quit under an open palette).
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_key(key, tx);
            return;
        }
        self.handle_base_key(key, tx);
    }

    /// Keys for the normal (modal-free) surface: global chords, then
    /// navigation and composer editing.
    fn handle_base_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('p') => {
                    self.modal = Modal::Palette {
                        query: String::new(),
                        selected: 0,
                    };
                    return;
                }
                KeyCode::Char('b') => {
                    self.session.sidebar_open = !self.session.sidebar_open;
                    return;
                }
                KeyCode::Char('l') => {
                    self.open_model_menu();
                    return;
                }
                KeyCode::Char('e') => {
                    self.session.effort_idx = (self.session.effort_idx + 1) % EFFORTS.len();
                    return;
                }
                KeyCode::Char('u') => {
                    self.scroll_by(-(self.view.view_height as i32 / 2).max(1));
                    return;
                }
                KeyCode::Char('d') => {
                    self.scroll_by((self.view.view_height as i32 / 2).max(1));
                    return;
                }
                KeyCode::Char('o') => {
                    self.toggle_focused();
                    return;
                }
                KeyCode::Char('y') => {
                    if let Some(i) = self
                        .focused_pending_diff()
                        .or_else(|| self.last_pending_diff())
                    {
                        self.approve(i, tx);
                    }
                    return;
                }
                KeyCode::Char('n') => {
                    if let Some(i) = self
                        .focused_pending_diff()
                        .or_else(|| self.last_pending_diff())
                    {
                        if let Message::Tool { id, .. } = &self.conversation.messages[i] {
                            self.modal = Modal::ConfirmReject(id.clone());
                        }
                    }
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                // Close slash menu → clear focus → interrupt a running turn.
                if self.composer.text.starts_with('/') {
                    self.composer.clear();
                } else if self.conversation.focused.is_some() {
                    self.conversation.focused = None;
                } else if self.busy() {
                    let _ = tx.send(Command::Interrupt);
                }
            }
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Up => {
                if self.slash_open() {
                    self.slash_selected = self.slash_selected.saturating_sub(1);
                } else {
                    self.scroll_by(-1);
                }
            }
            KeyCode::Down => {
                if self.slash_open() {
                    let max = self.slash_matches().len().saturating_sub(1);
                    self.slash_selected = (self.slash_selected + 1).min(max);
                } else {
                    self.scroll_by(1);
                }
            }
            KeyCode::PageUp => self.scroll_by(-(self.view.view_height as i32).max(1)),
            KeyCode::PageDown => self.scroll_by(self.view.view_height as i32),
            KeyCode::Enter => {
                // Enter on a focused collapsible block (empty composer) toggles it.
                if self.composer.text.is_empty()
                    && self
                        .conversation
                        .focused
                        .map(|i| {
                            self.conversation
                                .messages
                                .get(i)
                                .map(|m| m.is_collapsible_tool())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                {
                    self.toggle_focused();
                } else {
                    self.submit(tx);
                }
            }
            KeyCode::Char(c) => {
                self.composer.insert_char(c);
                self.slash_selected = 0;
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                self.slash_selected = 0;
            }
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            _ => {}
        }
    }

    /// Move focus to the next (dir = 1) / previous (dir = -1) focusable tool
    /// block, wrapping around.
    fn cycle_focus(&mut self, dir: i32) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
        let len = targets.len() as i32;
        let next = match self.conversation.focused {
            None => {
                if dir > 0 {
                    targets[0]
                } else {
                    targets[len as usize - 1]
                }
            }
            Some(cur) => {
                let pos = targets.iter().position(|&t| t == cur).unwrap_or(0) as i32;
                targets[(pos + dir).rem_euclid(len) as usize]
            }
        };
        self.conversation.focused = Some(next);
        self.ensure_focus_visible();
    }

    fn toggle_focused(&mut self) {
        if let Some(i) = self.conversation.focused {
            self.toggle_tool(i);
        }
    }

    /// Paste lands in the composer as one unit; modal inputs own the
    /// keyboard so pastes are dropped while one is open.
    pub fn insert_paste(&mut self, text: &str) {
        if !matches!(self.modal, Modal::None) {
            return;
        }
        self.composer.insert_paste(text);
        self.slash_selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::LineKind;

    /// `/auto-review` routes a toggle command to the provider.
    #[test]
    fn auto_review_slash_sends_toggle_command() {
        let mut app = App::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.run_slash("/auto-review", &tx);
        assert!(matches!(rx.try_recv(), Ok(Command::ToggleAutoReview)));
        // The command stays reachable from the composer's slash popup.
        app.composer.text = "/auto".into();
        assert!(
            app.slash_matches()
                .iter()
                .any(|(cmd, _)| *cmd == "/auto-review")
        );
    }

    /// Streamed deltas append to one bubble until explicitly closed.
    #[test]
    fn assistant_deltas_append_then_close() {
        let mut app = App::new();
        app.handle_event(AgentEvent::AssistantDelta("Hello".into()));
        app.handle_event(AgentEvent::AssistantDelta(", world".into()));
        app.handle_event(AgentEvent::AssistantEnd);
        app.handle_event(AgentEvent::AssistantDelta("Again".into()));
        let texts: Vec<&str> = app
            .conversation
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hello, world", "Again"]);
    }

    /// Reasoning deltas stream into their own dimmed block, which closes when
    /// the reply text starts instead of merging into it.
    #[test]
    fn reasoning_deltas_form_their_own_block() {
        let mut app = App::new();
        app.handle_event(AgentEvent::ReasoningDelta("considering ".into()));
        app.handle_event(AgentEvent::ReasoningDelta("options".into()));
        app.handle_event(AgentEvent::AssistantDelta("Answer".into()));
        let contents: Vec<(&str, &str)> = app
            .conversation
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Thinking(t) => Some(("thinking", t.as_str())),
                Message::Assistant(t) => Some(("assistant", t.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            contents,
            [("thinking", "considering options"), ("assistant", "Answer")]
        );
    }

    /// Start and completion events for the same call render one card.
    #[test]
    fn tool_call_events_merge_by_id() {
        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Read {
                path: "src/a.rs".into(),
                summary: String::new(),
            },
            lines: vec![],
            awaiting_approval: false,
        }));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Read {
                path: "src/a.rs".into(),
                summary: "1 lines".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Context,
                text: "fn main() {}".into(),
            }],
            awaiting_approval: false,
        }));
        assert_eq!(app.conversation.messages.len(), 1);
        match &app.conversation.messages[0] {
            Message::Tool { kind, lines, .. } => {
                assert!(matches!(kind, ToolKind::Read { summary, .. } if summary == "1 lines"));
                assert_eq!(lines.len(), 1);
            }
            _ => panic!("expected a tool card"),
        }
    }

    #[test]
    fn paste_ignored_when_modal_open() {
        let mut app = App::new();
        app.modal = Modal::Palette {
            query: "x".into(),
            selected: 0,
        };
        app.insert_paste("nope");
        assert!(app.composer.text.is_empty());
    }

    /// Modals are exclusive by construction: running the palette's "model"
    /// item replaces the palette with the model menu instead of stacking.
    #[test]
    fn model_menu_replaces_open_palette() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Palette {
            query: "mo".into(),
            selected: 0,
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::ModelMenu(_)));
    }

    fn mouse(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn app_with_collapsible_tool() -> App {
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
            Rect {
                x: 2,
                y: 2,
                width: 38,
                height: 4,
            },
        )];
        assert!(app.conversation.collapsed.contains(&"r1".to_string()));
        app
    }

    #[test]
    fn hover_tracks_tool_card() {
        let mut app = app_with_collapsible_tool();
        app.handle_mouse(mouse(MouseEventKind::Moved, 3, 10));
        assert_eq!(app.view.hover_tool, Some(0));
        app.handle_mouse(mouse(MouseEventKind::Moved, 3, 80));
        assert_eq!(app.view.hover_tool, None);
    }

    #[test]
    fn click_toggles_collapsible_card() {
        let mut app = app_with_collapsible_tool();
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 3, 10);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 3, 10);
        app.handle_mouse(down);
        app.handle_mouse(up);
        assert!(
            !app.conversation.collapsed.contains(&"r1".to_string()),
            "press expands"
        );
        app.handle_mouse(down);
        app.handle_mouse(up);
        assert!(
            app.conversation.collapsed.contains(&"r1".to_string()),
            "press again collapses"
        );
    }

    #[test]
    fn drag_selects_instead_of_toggling() {
        let mut app = app_with_collapsible_tool();
        app.view.msg_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 10));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 20));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 20));
        // Still collapsed: the drag became a selection, not a card press.
        assert!(app.conversation.collapsed.contains(&"r1".to_string()));
    }
}
