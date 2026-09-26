//! Keyboard and mouse dispatch for the normal (modal-free) surface.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use super::{App, Message, Modal, PendingClick};
use crate::tui::app::EFFORTS;
use crate::tui::provider::Command;
use crate::tui::selection::{clamp_to, copy_to_clipboard, extract_selection_text, rect_contains};

impl App {
    pub fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // Any keypress dismisses a live flash toast.
        self.flash = None;
        // A modal owns the keyboard while open; its keys never fall through
        // to base chords (so ctrl+q does not quit under an open palette).
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_key(key, tx);
            return;
        }
        // The permission prompt (F.5) owns plain keys while open: the tool
        // call is parked on it. Ctrl-modified chords it does not consume
        // (ctrl-c denies inside it) still fall through to the base surface.
        if self.permission_prompt.is_open() && self.handle_permission_key(key, tx) {
            return;
        }
        self.handle_base_key(key, tx);
    }

    /// Route a key through the permission prompt. Returns true when the
    /// key was consumed (answer produced, state transition, or a plain key
    /// swallowed while the prompt owns the keyboard).
    fn handle_permission_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<Command>,
    ) -> bool {
        let Some(id) = self.permission_prompt.id().map(str::to_owned) else {
            return false;
        };
        match self.permission_prompt.handle_key(key) {
            Some(answer) => {
                let _ = tx.send(Command::AnswerPermission { id, answer });
                true
            }
            None => {
                // Unhandled ctrl chords keep working (palette, quit path,
                // ...); plain keys never reach the composer while the
                // prompt is open.
                !key.modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            }
        }
    }

    /// Keys for the normal (modal-free) surface: global chords, then
    /// navigation and composer editing.
    fn handle_base_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        if self.handle_chord_key(key, tx) {
            return;
        }
        if self.handle_editing_chord_key(key) {
            return;
        }
        if self.handle_navigation_key(key, tx) {
            return;
        }
        if self.handle_history_key(key) {
            return;
        }
        self.handle_submit_or_edit_key(key, tx);
    }

    /// Global ctrl chords: quit/interrupt, palette, sidebar, model, effort,
    /// half-page scroll, focus toggle, and diff approval/rejection.
    fn handle_chord_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Char('c') | KeyCode::Char('q') => {
                if !self.composer.text.is_empty() {
                    self.composer.clear();
                } else if self.busy() {
                    let _ = tx.send(Command::Interrupt);
                    self.interrupt_requested = true;
                } else {
                    self.should_quit = true;
                }
                true
            }
            KeyCode::Char('p') => {
                self.modal = Modal::Palette {
                    query: String::new(),
                    selected: 0,
                };
                true
            }
            KeyCode::Char('b') => {
                self.session.sidebar_open = !self.session.sidebar_open;
                true
            }
            KeyCode::Char('l') => {
                self.open_model_menu();
                true
            }
            KeyCode::Char('f') => {
                self.session.effort_idx = (self.session.effort_idx + 1) % EFFORTS.len();
                true
            }
            KeyCode::Char('u') => {
                self.scroll_by(-(self.view.view_height as i32 / 2).max(1));
                true
            }
            KeyCode::Char('d') => {
                self.scroll_by((self.view.view_height as i32 / 2).max(1));
                true
            }
            KeyCode::Char('o') => {
                self.toggle_focused();
                true
            }
            KeyCode::Char('Y') => {
                if let Some(i) = self
                    .focused_pending_diff()
                    .or_else(|| self.last_pending_diff())
                {
                    self.approve(i, tx, true);
                }
                true
            }
            KeyCode::Char('y') => {
                if let Some(i) = self
                    .focused_pending_diff()
                    .or_else(|| self.last_pending_diff())
                {
                    self.approve(i, tx, false);
                }
                true
            }
            KeyCode::Char('n') => {
                if let Some(i) = self
                    .focused_pending_diff()
                    .or_else(|| self.last_pending_diff())
                    && let Message::Tool { id, .. } = &self.conversation.messages[i]
                {
                    self.modal = Modal::ConfirmReject(id.clone());
                }
                true
            }
            _ => false,
        }
    }

    /// Composer editing chords (reference `TextBuffer::handle_key`):
    /// ctrl motions and deletes plus Alt word motions. Effort cycling moved
    /// to Ctrl-F so Ctrl-E can be line-end; with an empty composer it jumps
    /// the scrollback to bottom.
    fn handle_editing_chord_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('a') => {
                    self.composer.move_home();
                    true
                }
                KeyCode::Char('e') => {
                    if self.composer.text.is_empty() {
                        self.scroll_to_bottom();
                    } else {
                        self.composer.move_end();
                    }
                    true
                }
                KeyCode::Char('w') | KeyCode::Backspace => {
                    self.composer.delete_word_back();
                    true
                }
                KeyCode::Char('k') => {
                    self.composer.kill_to_end_of_line();
                    true
                }
                KeyCode::Delete => {
                    self.composer.delete_word_forward();
                    true
                }
                KeyCode::Left => {
                    self.composer.move_word_left();
                    true
                }
                KeyCode::Right => {
                    self.composer.move_word_right();
                    true
                }
                _ => false,
            };
        }
        // Alt chords: word motions (Alt-←/→, Alt-b/f). Alt-O (editor) is
        // intercepted one level up, in the event loop, where the terminal
        // is reachable for the suspend/resume dance.
        if key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return match key.code {
                KeyCode::Left | KeyCode::Char('b') => {
                    self.composer.move_word_left();
                    true
                }
                KeyCode::Right | KeyCode::Char('f') => {
                    self.composer.move_word_right();
                    true
                }
                _ => false,
            };
        }
        false
    }

    /// Navigation keys: Esc dismissal order, focus cycling, page/edge
    /// scrolls, and the vim-style g/G jump keys (empty composer only).
    fn handle_navigation_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<Command>,
    ) -> bool {
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
                true
            }
            // Tab cycles Build/Plan (F.2); focus cycling stays on
            // BackTab so block navigation remains reachable.
            KeyCode::Tab => {
                self.toggle_mode();
                true
            }
            KeyCode::BackTab => {
                self.cycle_focus(-1);
                true
            }
            KeyCode::PageUp => {
                self.scroll_by(-(self.view.view_height as i32).max(1));
                true
            }
            KeyCode::PageDown => {
                self.scroll_by(self.view.view_height as i32);
                true
            }
            KeyCode::Home => {
                self.scroll_to_top();
                true
            }
            KeyCode::End => {
                self.scroll_to_bottom();
                true
            }
            // Vim-style scroll keys, only while the composer is empty so
            // typing a message never eats a character.
            KeyCode::Char('g') if self.composer.text.is_empty() => {
                self.scroll_to_top();
                true
            }
            KeyCode::Char('G') if self.composer.text.is_empty() => {
                self.scroll_to_bottom();
                true
            }
            _ => false,
        }
    }

    /// History keys: on the first/last composer line the arrows recall the
    /// input history; elsewhere they stay cursor motions. The arrows never
    /// scroll the transcript (PageUp/Down, Ctrl-U/D, and the wheel do that).
    fn handle_history_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up => {
                if self.slash_open() {
                    self.slash_selected = self.slash_selected.saturating_sub(1);
                } else if !self.composer.cursor_on_first_line() {
                    self.composer.move_up();
                } else {
                    self.history_up();
                }
                true
            }
            KeyCode::Down => {
                if self.slash_open() {
                    let max = self.slash_matches().len().saturating_sub(1);
                    self.slash_selected = (self.slash_selected + 1).min(max);
                } else if !self.composer.cursor_on_last_line() {
                    self.composer.move_down();
                } else {
                    self.history_down();
                }
                true
            }
            _ => false,
        }
    }

    /// Plain keys: Enter submits (or toggles a focused card), and the rest
    /// edit the composer.
    fn handle_submit_or_edit_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        match key.code {
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
    /// keyboard so pastes are dropped while one is open. An open permission
    /// prompt's guidance buffer takes precedence.
    pub fn insert_paste(&mut self, text: &str) {
        if !matches!(self.modal, Modal::None) {
            return;
        }
        if self.permission_prompt.handle_paste(text) {
            return;
        }
        self.composer.insert_paste(text);
        self.slash_selected = 0;
    }

    // ------------------------------------------------------------------
    // Mouse: wheel scroll + app-side text selection
    // ------------------------------------------------------------------

    /// Collapsible tool card (message index) at a screen position, if any.
    pub fn tool_at(&self, row: u16, col: u16) -> Option<usize> {
        self.view
            .tool_regions
            .iter()
            .find(|(_, r)| rect_contains(*r, row, col))
            .map(|(i, _)| *i)
    }

    /// "Click to expand" notice row (message index) at a screen position.
    fn notice_at(&self, row: u16, col: u16) -> Option<usize> {
        self.view
            .notice_regions
            .iter()
            .find(|(_, r)| rect_contains(*r, row, col))
            .map(|(i, _)| *i)
    }

    /// Toggle a card body between truncated and fully expanded.
    fn toggle_body(&mut self, idx: usize) {
        if let Some(Message::Tool { id, .. }) = self.conversation.messages.get(idx) {
            let id = id.clone();
            if let Some(pos) = self
                .conversation
                .expanded_bodies
                .iter()
                .position(|c| c == &id)
            {
                self.conversation.expanded_bodies.remove(pos);
            } else {
                self.conversation.expanded_bodies.push(id);
            }
        }
    }

    fn toggle_tool(&mut self, idx: usize) {
        if let Some(Message::Tool { id, kind, .. }) = self.conversation.messages.get(idx)
            && kind.collapsible()
        {
            if let Some(pos) = self.conversation.collapsed.iter().position(|c| c == id) {
                self.conversation.collapsed.remove(pos);
            } else {
                self.conversation.collapsed.push(id.clone());
            }
        }
    }

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
                    .map(|region| crate::tui::selection::Selection {
                        anchor: pos,
                        head: pos,
                        region,
                    });
                self.view.pending_click = self
                    .notice_at(mouse.row, mouse.column)
                    .map(PendingClick::Notice)
                    .or_else(|| {
                        self.tool_at(mouse.row, mouse.column)
                            .map(PendingClick::Card)
                    });
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
                    // Press without drag on a card: toggle it. A notice-row
                    // press toggles only the body's truncation.
                    (Some(PendingClick::Card(idx)), true) => {
                        self.view.selection = None;
                        self.toggle_tool(idx);
                    }
                    (Some(PendingClick::Notice(idx)), true) => {
                        self.view.selection = None;
                        self.toggle_body(idx);
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
            self.flash("Copied");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{app_with_collapsible_tool, draw_app, scrolled_app};
    use super::*;
    use crate::tui::app::EFFORTS;
    use crate::tui::provider::{AgentEvent, LineKind, Status, ToolCallData, ToolKind, ToolLine};
    use ratatui::layout::Rect;
    use tokio::sync::mpsc;

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

    fn open_permission(app: &mut App) {
        app.handle_event(AgentEvent::PermissionRequest {
            id: "t9".into(),
            tool: "bash".into(),
            scopes: vec!["execute".into()],
            files: Vec::new(),
            commands: vec!["rm -rf /tmp/x".into()],
        });
    }

    #[test]
    fn permission_request_opens_the_overlay_and_resolution_closes_it() {
        let mut app = App::new();
        open_permission(&mut app);
        assert!(app.permission_prompt.is_open());
        // A stale resolution must not close a newer request.
        app.handle_event(AgentEvent::PermissionResolved { id: "other".into() });
        assert!(app.permission_prompt.is_open());
        app.handle_event(AgentEvent::PermissionResolved { id: "t9".into() });
        assert!(!app.permission_prompt.is_open());
    }

    #[test]
    fn answering_the_prompt_routes_the_answered_command() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        open_permission(&mut app);
        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::AnswerPermission { id, answer: crate::permissions::PermissionAnswer::AllowOnce })
                if id == "t9"
        ));
    }

    #[test]
    fn prompt_owns_plain_keys_while_open() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        open_permission(&mut app);
        // A plain char never reaches the composer.
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE), &tx);
        assert!(app.composer.text.is_empty());
        // Enter would submit the composer; here it does nothing.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.composer.text.is_empty());
        // Ctrl-chords fall through (e.g. the palette still opens).
        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(app.modal, Modal::Palette { .. }));
    }

    #[test]
    fn paste_lands_in_the_guidance_buffer_while_editing() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        open_permission(&mut app);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
        app.insert_paste("use cat");
        assert!(app.composer.text.is_empty());
        // Enter denies with the typed guidance.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        // The overlay stays open until PermissionResolved arrives.
        assert!(app.permission_prompt.is_open());
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

    /// Tab cycles Build -> Plan -> Build (F.2); BackTab keeps focus cycling.
    #[test]
    fn tab_cycles_modes_and_backtab_keeps_focus_cycling() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        assert_eq!(app.mode, crate::tui::app::Mode::Build);
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.mode, crate::tui::app::Mode::Plan);
        assert!(app.plan_path.is_some(), "plan path allocated on entry");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.mode, crate::tui::app::Mode::Build);
    }

    /// Esc dismisses the command palette without running a command.
    #[test]
    fn esc_closes_the_command_palette() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Palette {
            query: "mo".into(),
            selected: 0,
        };
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
    }

    fn mouse(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
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

    /// A long bash body truncates with a notice row; clicking the notice
    /// expands the body in place, and clicking the fold-back notice that
    /// replaces it re-truncates. The card's own collapse is untouched.
    #[test]
    fn click_notice_row_expands_and_retruncates_body() {
        let mut app = App::new();
        let lines: Vec<ToolLine> = (0..60)
            .map(|i| ToolLine {
                kind: LineKind::Context,
                text: format!("out {i}"),
                ..Default::default()
            })
            .collect();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "b1".into(),
            kind: ToolKind::Bash { cmd: "make".into() },
            lines,
            awaiting_approval: false,
        }));
        draw_app(&mut app, 80, 24);
        let press = |app: &mut App, row, col| {
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), row, col));
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), row, col));
        };
        let notice = |app: &App| app.view.notice_regions.first().copied();
        let (idx, rect) = notice(&app).expect("truncated body carries a notice");
        press(&mut app, rect.y, rect.x + 4);
        assert!(
            app.conversation.expanded_bodies.iter().any(|c| c == "b1"),
            "notice press expands the body"
        );
        assert!(
            app.conversation.collapsed.is_empty(),
            "card collapse untouched"
        );
        draw_app(&mut app, 80, 24);
        let (_, rect) = notice(&app).expect("expanded body carries a fold-back notice");
        press(&mut app, rect.y, rect.x + 4);
        assert!(
            app.conversation.expanded_bodies.is_empty(),
            "fold-back press re-truncates"
        );
        let _ = idx;
    }

    /// The ported composer chords: Ctrl-W deletes a word, Ctrl-K kills to the
    /// end of the line, Alt-Left moves back a word.
    #[test]
    fn composer_chords_edit_words_and_lines() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("foo bar".into());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.text, "foo ");

        app.composer.set_text("keep\nkill this".into());
        app.composer.cursor = "keep\n".chars().count();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.text, "keep\n");

        app.composer.set_text("one two".into());
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT), &tx);
        assert_eq!(app.composer.cursor, 4);
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), &tx);
        assert_eq!(app.composer.cursor, 7);
    }

    /// g and G still type into the composer when it holds text.
    #[test]
    fn vim_scroll_keys_type_when_composer_has_text() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.insert_char('a');
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "agG");
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
    }

    /// ↑ recalls older entries, clamps at the oldest, ↓ walks back toward the
    /// newest and restores the in-progress draft past it.
    #[test]
    fn history_recall_navigates_and_restores_draft() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.input_history.push("first".into());
        app.input_history.push("second".into());
        app.composer.set_text("draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "second");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "first");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "first", "clamped at the oldest entry");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "second");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "draft", "draft restored past newest");
        assert!(app.history_index.is_none());
    }

    /// Slash commands are recorded in history but ↑/↓ skip over them, so
    /// recalling one can never reopen the slash menu and trap the arrows.
    #[test]
    fn history_recall_skips_slash_commands() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.input_history.push("old text".into());
        app.input_history.push("/help".into());
        app.input_history.push("new text".into());
        app.composer.set_text("draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "new text");
        assert!(!app.slash_open(), "slash menu stays closed while recalling");

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "old text", "skipped over /help");
        assert!(!app.slash_open());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "old text", "clamped at the oldest entry");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "new text", "skipped over /help");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "draft", "draft restored past newest");
        assert!(app.history_index.is_none());
    }

    /// A slash command as the newest entry is never recalled; ↑ stays put and
    /// the menu does not open.
    #[test]
    fn history_up_ignores_a_newest_slash_command() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.input_history.push("/stats".into());
        app.composer.set_text("draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "draft", "nothing but slash commands");
        assert!(app.history_index.is_none());
        assert!(!app.slash_open());
    }

    /// With an empty composer, ↑/↓ drive the input history straight away —
    /// the arrows are not scrollback keys.
    #[test]
    fn arrows_drive_history_from_an_empty_composer() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        app.scroll_by(-3);
        assert!(!app.view.follow);
        app.input_history.push("earlier".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "earlier");
        assert!(!app.view.follow, "Up recalled history instead of scrolling");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "", "draft (empty) restored past newest");
        assert!(app.history_index.is_none());
    }

    /// Ctrl-E: line-end while the composer holds text; with an empty
    /// composer it jumps the scrollback to the bottom and re-pins follow.
    #[test]
    fn ctrl_e_is_both_line_end_and_jump_to_bottom() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("hello".into());
        app.composer.cursor = 0;
        app.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.cursor, 5, "line-end with text");

        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        app.scroll_by(-5);
        assert!(!app.view.follow, "scrolled off the bottom");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.view.follow, "Ctrl-E on empty composer jumps to bottom");
        let layout =
            crate::tui::ui::scrollback::Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll) + u32::from(app.view.view_height),
            layout.total_rows()
        );
    }

    /// Submitting a message records it in the rolling input history.
    #[test]
    fn submit_records_history() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("hello world".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(app.input_history.len(), 1);
        assert_eq!(app.input_history.get(0), Some("hello world"));
        assert!(app.composer.text.is_empty());
    }

    /// Ctrl-C tri-state (reference `handle_ctrl` Quit branch): text first,
    /// then the running turn, then the app.
    #[test]
    fn ctrl_c_tri_state_clears_then_cancels_then_quits() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("draft".into());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.composer.text.is_empty(), "first press clears input");
        assert!(!app.should_quit);
        assert!(rx.try_recv().is_err(), "no command sent while text present");

        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(
            matches!(rx.try_recv(), Ok(Command::Interrupt)),
            "second press cancels the running turn"
        );
        assert!(!app.should_quit);

        app.handle_event(AgentEvent::StatusChanged(Status::Done));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.should_quit, "idle press quits");
    }

    /// Bang-mode submit: `! cmd` sends a visible `Command::Shell` and shows
    /// the echoed command instead of a model turn.
    #[test]
    fn submit_bang_sends_visible_shell_command() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("! echo hi".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::Shell { command, visible }) if command == "echo hi" && visible
        ));
        assert!(rx.try_recv().is_err(), "no SendMessage follows");
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::User(text)) if text == "! echo hi"
        ));
        assert_eq!(app.input_history.get(0), Some("! echo hi"));
        assert!(app.composer.text.is_empty());
    }

    /// `!! cmd` runs hidden from the model: `visible: false`, no history
    /// result will be queued, and the echo uses the double sigil.
    #[test]
    fn submit_double_bang_sends_hidden_shell_command() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("!! make".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(Command::Shell { command, visible }) if command == "make" && !visible
        ));
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::User(text)) if text == "!! make"
        ));
    }

    /// A lone sigil or interior bang stays normal input.
    #[test]
    fn submit_lone_bang_is_a_normal_message() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("!".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(rx.try_recv(), Ok(Command::SendMessage(text, _)) if text == "!"));
    }

    /// `cd` through bang-mode flashes the hint but still runs (reference
    /// behavior); the echo uses the single sigil.
    #[test]
    fn submit_bang_cd_flashes() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("! cd /tmp".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(
            app.flash_text(),
            Some("Only /cd can change the working directory")
        );
    }

    /// Effort lives on Ctrl-F; Ctrl-E moves to the end of the line.
    #[test]
    fn ctrl_f_cycles_effort_and_ctrl_e_moves_to_line_end() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("hello".into());
        app.composer.cursor = 0;
        let before = app.session.effort_idx;
        app.handle_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.session.effort_idx, (before + 1) % EFFORTS.len());
        assert_eq!(app.composer.text, "hello", "ctrl-f does not type");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.cursor, 5);
    }
}
