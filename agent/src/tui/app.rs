//! Application state and input handling. The App renders whatever the
//! provider streams in and translates key presses into provider commands.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

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

pub const SLASH_COMMANDS: [(&str, &str); 6] = [
    ("/clear", "Clear conversation context"),
    ("/compact", "Compact context to save tokens"),
    ("/undo", "Revert the last edit"),
    ("/model", "Switch model"),
    ("/sessions", "List sessions"),
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

pub struct App {
    // --- provider-driven state ---
    pub messages: Vec<Message>,
    pub plan: Vec<PlanItem>,
    pub files: Vec<TouchedFile>,
    pub status: Status,
    /// Animation frame counter for the status indicator (advanced per frame).
    pub status_tick: usize,
    pub token_label: String,
    /// True while a streamed [`Message::Assistant`] is still being appended to.
    assistant_open: bool,
    /// True while a streamed [`Message::Thinking`] is still being appended to.
    thinking_open: bool,

    // --- session chrome ---
    pub models: Vec<ModelChoice>,
    pub model_idx: usize,
    pub effort_idx: usize,
    pub cwd: String,
    pub branch: String,
    pub sidebar_open: bool,

    // --- composer ---
    pub composer: String,
    pub composer_cursor: usize, // char index into composer

    // --- message view ---
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
    pub collapsed: Vec<String>, // tool ids currently collapsed
    pub focused: Option<usize>, // message index of focused tool block

    // --- overlays ---
    pub palette: Option<(String, usize)>, // (query, selected)
    pub model_menu: Option<usize>,        // selected row
    pub slash_selected: usize,            // row in the slash popup
    pub confirm_reject: Option<String>,   // tool id awaiting confirm

    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            messages: Vec::new(),
            plan: Vec::new(),
            files: Vec::new(),
            status: Status::Done,
            status_tick: 0,
            token_label: "…".into(),
            assistant_open: false,
            thinking_open: false,
            models: seed_models(),
            model_idx: 0,
            effort_idx: 2, // "high", the prototype default
            cwd: "~/Projects/craft-web".into(),
            branch: "fix/session-refresh".into(),
            sidebar_open: true,
            composer: String::new(),
            composer_cursor: 0,
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
            collapsed: Vec::new(),
            focused: None,
            palette: None,
            model_menu: None,
            slash_selected: 0,
            confirm_reject: None,
            should_quit: false,
        }
    }

    pub fn model(&self) -> (&str, &str) {
        self.models
            .get(self.model_idx)
            .map(|m| (m.label.as_str(), m.provider_label.as_str()))
            .unwrap_or(("no model", "no provider"))
    }

    /// Open the model picker with the current selection highlighted.
    fn open_model_menu(&mut self) {
        if !self.models.is_empty() {
            self.model_menu = Some(self.model_idx.min(self.models.len().saturating_sub(1)));
        }
    }

    pub fn effort(&self) -> &'static str {
        EFFORTS[self.effort_idx]
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Thinking | Status::Running)
    }

    // ------------------------------------------------------------------
    // Provider events
    // ------------------------------------------------------------------

    pub fn handle_event(&mut self, ev: AgentEvent) {
        let was_following = self.follow;
        match ev {
            AgentEvent::StatusChanged(s) => self.status = s,
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
            AgentEvent::PlanSet(plan) => self.plan = plan,
            AgentEvent::FilesSet(files) => self.files = files,
            AgentEvent::TokenUsage(label) => self.token_label = label,
            AgentEvent::CatalogSet { models, current } => {
                if !models.is_empty() {
                    self.models = models;
                    self.model_idx = current.min(self.models.len() - 1);
                }
            }
            AgentEvent::SessionInfo { cwd, branch } => {
                self.cwd = cwd;
                self.branch = branch;
            }
        }
        if was_following {
            self.follow = true;
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Indices of tool blocks that can be focused: collapsible blocks and
    /// pending diffs, in display order.
    fn focus_targets(&self) -> Vec<usize> {
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_collapsible_tool() || m.is_pending_diff())
            .map(|(i, _)| i)
            .collect()
    }

    fn focused_pending_diff(&self) -> Option<usize> {
        self.focused.filter(|&i| {
            self.messages
                .get(i)
                .map(|m| m.is_pending_diff())
                .unwrap_or(false)
        })
    }

    fn last_pending_diff(&self) -> Option<usize> {
        self.messages.iter().rposition(|m| m.is_pending_diff())
    }

    pub fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        let q = self.composer.as_str();
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
        self.palette.is_none() && !self.slash_matches().is_empty()
    }

    pub fn palette_items(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let (query, _) = self.palette.clone().unwrap_or_default();
        let q = query.to_lowercase();
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
        let text = self.composer.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Enter on an open slash menu executes the highlighted command.
        let slash = self.slash_matches();
        if text.starts_with('/') && !slash.is_empty() {
            let (cmd, _) = slash[self.slash_selected.min(slash.len() - 1)];
            self.composer.clear();
            self.composer_cursor = 0;
            self.run_slash(cmd, tx);
            return;
        }
        self.assistant_open = false;
        self.messages.push(Message::User(text.clone()));
        let _ = tx.send(Command::SendMessage(text));
        self.composer.clear();
        self.composer_cursor = 0;
        self.follow = true;
    }

    fn reset_conversation(&mut self) {
        self.messages.clear();
        self.collapsed.clear();
        self.focused = None;
        self.hover_tool = None;
        self.pending_click = None;
        self.tool_regions.clear();
        self.confirm_reject = None;
        self.scroll = 0;
        self.follow = true;
    }

    fn run_slash(&mut self, cmd: &str, tx: &mpsc::UnboundedSender<Command>) {
        match cmd {
            "/clear" => {
                self.reset_conversation();
                let _ = tx.send(Command::Clear);
            }
            "/model" => self.open_model_menu(),
            // Compact/undo/help/sessions are no-ops for now.
            _ => {}
        }
    }

    fn run_palette(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        match id {
            "new" => {
                self.reset_conversation();
                let _ = tx.send(Command::Reset);
            }
            "toggle-sidebar" => self.sidebar_open = !self.sidebar_open,
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
        if let Some(Message::Tool { id, diff, .. }) = self.messages.get_mut(idx) {
            *diff = Some(DiffState::Approved);
            let _ = tx.send(Command::Approve(id.clone()));
        }
        self.focused = None;
    }

    fn reject_confirmed(&mut self, tx: &mpsc::UnboundedSender<Command>) {
        if let Some(id) = self.confirm_reject.take() {
            if let Some(Message::Tool { diff, .. }) = self
                .messages
                .iter_mut()
                .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id))
            {
                *diff = Some(DiffState::Rejected);
            }
            let _ = tx.send(Command::Reject(id));
        }
        self.focused = None;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let new = self.scroll as i32 + delta;
        self.scroll = new.clamp(0, self.max_scroll as i32) as u16;
        self.follow = self.scroll >= self.max_scroll;
        // Card rects move with the scroll; stale hover/click state is dropped.
        self.hover_tool = None;
        self.pending_click = None;
    }

    /// Collapsible tool card (message index) at a screen position, if any.
    pub fn tool_at(&self, row: u16, col: u16) -> Option<usize> {
        self.tool_regions
            .iter()
            .find(|(_, r)| rect_contains(*r, row, col))
            .map(|(i, _)| *i)
    }

    fn toggle_tool(&mut self, idx: usize) {
        if let Some(Message::Tool { id, kind, .. }) = self.messages.get(idx) {
            if kind.collapsible() {
                if let Some(pos) = self.collapsed.iter().position(|c| c == id) {
                    self.collapsed.remove(pos);
                } else {
                    self.collapsed.push(id.clone());
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
                self.hover_tool = self.tool_at(mouse.row, mouse.column);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A selection starts only inside a selectable region (chat
                // messages or the composer input) and is confined to it; a
                // click elsewhere (sidebar, chrome) just clears the highlight.
                let pos = (mouse.row, mouse.column);
                self.selection = [self.msg_area, self.composer_area]
                    .iter()
                    .copied()
                    .find(|r| rect_contains(*r, mouse.row, mouse.column))
                    .map(|region| Selection {
                        anchor: pos,
                        head: pos,
                        region,
                    });
                self.pending_click = self.tool_at(mouse.row, mouse.column);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // A drag is a text selection, not a card press.
                self.pending_click = None;
                if let Some(sel) = &mut self.selection {
                    sel.head = clamp_to(sel.region, mouse.row, mouse.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let no_drag = self.selection.map(|s| s.is_empty()).unwrap_or(true);
                match (self.pending_click.take(), no_drag) {
                    // Press without drag on a card: toggle it.
                    (Some(idx), true) => {
                        self.selection = None;
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
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            self.selection = None;
            return;
        }
        let text = extract_selection_text(&self.frame_text, sel);
        self.selection = None;
        if !text.is_empty() {
            copy_to_clipboard(&text);
        }
    }

    /// After focus changes, make sure the focused block is in view.
    fn ensure_focus_visible(&mut self) {
        let Some(i) = self.focused else { return };
        let Some(&start) = self.msg_starts.get(i) else {
            return;
        };
        let start = start as i32;
        let top = self.scroll as i32;
        let bottom = top + self.view_height as i32;
        if start < top || start >= bottom {
            self.scroll = (start - 2).max(0).min(self.max_scroll as i32) as u16;
            self.follow = false;
        }
    }

    // ------------------------------------------------------------------
    // Key handling
    // ------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // 1. Confirm dialog swallows everything.
        if self.confirm_reject.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.reject_confirmed(tx),
                _ => self.confirm_reject = None,
            }
            return;
        }

        // 2. Command palette.
        if let Some((query, selected)) = self.palette.clone() {
            let items = self.palette_items();
            match key.code {
                KeyCode::Esc => self.palette = None,
                KeyCode::Up => {
                    self.palette = Some((query, selected.saturating_sub(1)));
                }
                KeyCode::Down => {
                    let max = items.len().saturating_sub(1);
                    self.palette = Some((query, (selected + 1).min(max)));
                }
                KeyCode::Enter => {
                    if let Some((id, ..)) = items.get(selected) {
                        let id = *id;
                        self.palette = None;
                        self.run_palette(id, tx);
                    } else {
                        self.palette = None;
                    }
                }
                KeyCode::Backspace => {
                    let mut q = query;
                    q.pop();
                    self.palette = Some((q, 0));
                }
                KeyCode::Char(c) => {
                    let mut q = query;
                    q.push(c);
                    self.palette = Some((q, 0));
                }
                _ => {}
            }
            return;
        }

        // 3. Model menu.
        if let Some(sel) = self.model_menu {
            match key.code {
                KeyCode::Esc => self.model_menu = None,
                KeyCode::Up => self.model_menu = Some(sel.saturating_sub(1)),
                KeyCode::Down => {
                    self.model_menu = Some((sel + 1).min(self.models.len().saturating_sub(1)))
                }
                KeyCode::Enter => {
                    self.model_idx = sel;
                    if let Some(choice) = self.models.get(sel) {
                        let _ = tx.send(Command::SelectModel {
                            provider: choice.provider.clone(),
                            model: choice.model.clone(),
                        });
                    }
                    self.model_menu = None;
                }
                _ => self.model_menu = None,
            }
            return;
        }

        // 4. Global chords.
        if ctrl {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('p') => {
                    self.palette = Some((String::new(), 0));
                    return;
                }
                KeyCode::Char('b') => {
                    self.sidebar_open = !self.sidebar_open;
                    return;
                }
                KeyCode::Char('l') => {
                    self.open_model_menu();
                    return;
                }
                KeyCode::Char('e') => {
                    self.effort_idx = (self.effort_idx + 1) % EFFORTS.len();
                    return;
                }
                KeyCode::Char('u') => {
                    self.scroll_by(-(self.view_height as i32 / 2).max(1));
                    return;
                }
                KeyCode::Char('d') => {
                    self.scroll_by((self.view_height as i32 / 2).max(1));
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
                        if let Message::Tool { id, .. } = &self.messages[i] {
                            self.confirm_reject = Some(id.clone());
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
                if self.composer.starts_with('/') {
                    self.composer.clear();
                    self.composer_cursor = 0;
                } else if self.focused.is_some() {
                    self.focused = None;
                } else if self.busy() {
                    let _ = tx.send(Command::Interrupt);
                }
            }
            KeyCode::Tab => {
                let targets = self.focus_targets();
                if !targets.is_empty() {
                    self.focused = Some(match self.focused {
                        None => targets[0],
                        Some(cur) => {
                            let pos = targets.iter().position(|&t| t == cur).unwrap_or(0);
                            targets[(pos + 1) % targets.len()]
                        }
                    });
                    self.ensure_focus_visible();
                }
            }
            KeyCode::BackTab => {
                let targets = self.focus_targets();
                if !targets.is_empty() {
                    self.focused = Some(match self.focused {
                        None => targets[targets.len() - 1],
                        Some(cur) => {
                            let pos = targets.iter().position(|&t| t == cur).unwrap_or(0);
                            targets[(pos + targets.len() - 1) % targets.len()]
                        }
                    });
                    self.ensure_focus_visible();
                }
            }
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
            KeyCode::PageUp => self.scroll_by(-(self.view_height as i32).max(1)),
            KeyCode::PageDown => self.scroll_by(self.view_height as i32),
            KeyCode::Enter => {
                // Enter on a focused collapsible block (empty composer) toggles it.
                if self.composer.is_empty()
                    && self
                        .focused
                        .map(|i| {
                            self.messages
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
                let idx = self.byte_index(self.composer_cursor);
                self.composer.insert(idx, c);
                self.composer_cursor += 1;
                self.slash_selected = 0;
            }
            KeyCode::Backspace => {
                if self.composer_cursor > 0 {
                    let idx = self.byte_index(self.composer_cursor - 1);
                    self.composer.remove(idx);
                    self.composer_cursor -= 1;
                    self.slash_selected = 0;
                }
            }
            KeyCode::Left => self.composer_cursor = self.composer_cursor.saturating_sub(1),
            KeyCode::Right => {
                self.composer_cursor = (self.composer_cursor + 1).min(self.composer.chars().count())
            }
            _ => {}
        }
    }

    fn toggle_focused(&mut self) {
        if let Some(i) = self.focused {
            self.toggle_tool(i);
        }
    }

    /// Insert bracketed-paste content into the composer as one unit —
    /// crucially it never triggers submit (raw newlines would have arrived as
    /// Enter keypresses and sent the message line by line). Newlines are kept:
    /// the composer wraps and renders multi-line input.
    pub fn insert_paste(&mut self, text: &str) {
        // Ignore pastes while a modal text input owns the keyboard.
        if self.palette.is_some() || self.confirm_reject.is_some() || self.model_menu.is_some() {
            return;
        }
        // Bracketed paste delivers line breaks as \r or \r\n depending on the
        // terminal; normalize both to \n.
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if clean.trim().is_empty() {
            return;
        }
        let chars_added = clean.chars().count();
        let idx = self.byte_index(self.composer_cursor);
        self.composer.insert_str(idx, &clean);
        self.composer_cursor += chars_added;
        self.slash_selected = 0;
    }

    /// Byte index of the `char_idx`-th char in the composer.
    fn byte_index(&self, char_idx: usize) -> usize {
        self.composer
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.composer.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::LineKind;

    /// Streamed deltas append to one bubble until explicitly closed.
    #[test]
    fn assistant_deltas_append_then_close() {
        let mut app = App::new();
        app.handle_event(AgentEvent::AssistantDelta("Hello".into()));
        app.handle_event(AgentEvent::AssistantDelta(", world".into()));
        app.handle_event(AgentEvent::AssistantEnd);
        app.handle_event(AgentEvent::AssistantDelta("Again".into()));
        let texts: Vec<&str> = app
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
        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            Message::Tool { kind, lines, .. } => {
                assert!(matches!(kind, ToolKind::Read { summary, .. } if summary == "1 lines"));
                assert_eq!(lines.len(), 1);
            }
            _ => panic!("expected a tool card"),
        }
    }

    #[test]
    fn paste_preserves_line_breaks_and_moves_cursor() {
        let mut app = App::new();
        // Multi-line paste with both \r\n and \r line endings (how bracketed
        // paste can deliver breaks) lands as \n-separated text in one unit.
        app.insert_paste("one\r\ntwo\rthree");
        assert_eq!(app.composer, "one\ntwo\nthree");
        assert_eq!(app.composer_cursor, app.composer.chars().count());
        // Nothing was submitted.
        assert!(app.messages.is_empty());
    }

    #[test]
    fn paste_ignored_when_modal_open() {
        let mut app = App::new();
        app.palette = Some(("x".into(), 0));
        app.insert_paste("nope");
        assert!(app.composer.is_empty());
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
        app.tool_regions = vec![(
            0,
            Rect {
                x: 2,
                y: 2,
                width: 38,
                height: 4,
            },
        )];
        assert!(app.collapsed.contains(&"r1".to_string()));
        app
    }

    #[test]
    fn hover_tracks_tool_card() {
        let mut app = app_with_collapsible_tool();
        app.handle_mouse(mouse(MouseEventKind::Moved, 3, 10));
        assert_eq!(app.hover_tool, Some(0));
        app.handle_mouse(mouse(MouseEventKind::Moved, 3, 80));
        assert_eq!(app.hover_tool, None);
    }

    #[test]
    fn click_toggles_collapsible_card() {
        let mut app = app_with_collapsible_tool();
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 3, 10);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 3, 10);
        app.handle_mouse(down);
        app.handle_mouse(up);
        assert!(!app.collapsed.contains(&"r1".to_string()), "press expands");
        app.handle_mouse(down);
        app.handle_mouse(up);
        assert!(
            app.collapsed.contains(&"r1".to_string()),
            "press again collapses"
        );
    }

    #[test]
    fn drag_selects_instead_of_toggling() {
        let mut app = app_with_collapsible_tool();
        app.msg_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 10));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 20));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 20));
        // Still collapsed: the drag became a selection, not a card press.
        assert!(app.collapsed.contains(&"r1".to_string()));
    }
}
