use super::*;

impl App {
    pub fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // Any keypress dismisses a live flash toast.
        self.flash = None;
        // A pending task-chat Esc-Esc is armed by Esc alone; any other
        // key disarms it (Esc itself is handled in the navigation path).
        if key.code != KeyCode::Esc {
            self.esc_pending = None;
        }
        // A modal owns the keyboard while open; its keys never fall
        // through to base chords (so ctrl+q does not quit under an open
        // palette). Permission/question may decline a chord; declined
        // keys keep walking down the stack, and the plan form passes
        // Tab through so mode cycling keeps working (the form is
        // non-modal).
        let mut owner = self.keyboard_owner();
        loop {
            match owner {
                KeyboardOwner::Modal => {
                    self.handle_modal_key(key, tx);
                    return;
                }
                KeyboardOwner::PermissionPrompt => {
                    if self.handle_permission_key(key, tx) {
                        return;
                    }
                    owner = if self.overlays.question_form.is_open() {
                        KeyboardOwner::QuestionForm
                    } else {
                        self.owner_below()
                    };
                }
                KeyboardOwner::QuestionForm => {
                    if self.handle_question_key(key, tx) {
                        return;
                    }
                    owner = self.owner_below();
                }
                KeyboardOwner::Search => {
                    self.handle_search_key(key);
                    return;
                }
                KeyboardOwner::FilePicker => {
                    use crate::tui::file_picker::FilePickerAction;
                    match self.overlays.file_picker.handle_key(key) {
                        FilePickerAction::Consumed => {}
                        FilePickerAction::Select(path) => {
                            self.overlays.file_picker.close();
                            self.insert_path_into_composer(&path);
                        }
                        FilePickerAction::Close => self.overlays.file_picker.close(),
                    }
                    return;
                }
                KeyboardOwner::PlanForm => {
                    use crate::tui::plan_form::PlanFormAction;
                    let action = self
                        .plan_mode
                        .plan_form
                        .handle_key(key, &self.overlays.keybinds);
                    if action != PlanFormAction::Passthrough {
                        self.handle_plan_form_action(action, tx);
                        return;
                    }
                    owner = KeyboardOwner::Base;
                }
                KeyboardOwner::Base => {
                    // Chords the permission/question handlers did not
                    // consume still fall through to the base surface.
                    self.handle_base_key(key, tx);
                    return;
                }
            }
        }
    }

    /// Route a key through the open search modal: typing refreshes the
    /// matches (derived fresh, since output can land behind the modal),
    /// navigation scrolls the transcript to the current match, Enter jumps
    /// and closes, Esc restores the scroll position saved on open.
    fn handle_search_key(&mut self, key: KeyEvent) {
        use crate::tui::search_modal::SearchAction;
        match self.overlays.search.handle_key(key) {
            SearchAction::Consumed => self.refresh_search_matches(),
            SearchAction::Navigate => self.sync_search_highlight(),
            SearchAction::Select(seg, row) => {
                self.scroll_to_segment(seg, row);
                self.view.highlight_segment = None;
                self.overlays.search.close();
            }
            SearchAction::Close(saved) => {
                self.view.highlight_segment = None;
                if let Some((pos, follow)) = saved {
                    self.set_scroll_pos(pos);
                    self.view.follow = follow;
                }
                self.overlays.search.close();
            }
        }
    }

    /// key was consumed (answer produced, state transition, or a plain key
    /// swallowed while the prompt owns the keyboard).
    fn handle_permission_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<Command>,
    ) -> bool {
        let Some(id) = self.overlays.permission_prompt.id().map(str::to_owned) else {
            return false;
        };
        match self.overlays.permission_prompt.handle_key(key) {
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

    /// Route a key through the open question form (A.5): an answer (picked
    /// labels or dismissal) goes to the provider by question-request id;
    /// unhandled ctrl chords keep working, plain keys stay in the form.
    fn handle_question_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) -> bool {
        let Some(id) = self.overlays.question_form.id().map(str::to_owned) else {
            return false;
        };
        match self.overlays.question_form.handle_key(key) {
            Some(answer) => {
                let _ = tx.send(Command::AnswerQuestion { id, answer });
                true
            }
            None => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
        }
    }

    /// Keys for the normal (modal-free) surface: global chords, then
    /// navigation and composer editing.
    fn handle_base_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        if self.handle_chord_key(key, tx) {
            return;
        }
        if self.handle_editing_chord_key(key, tx) {
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

    /// Global chords, dispatched through the data-driven keybinding table
    /// (F.1): quit/interrupt, palette, sidebar, model, effort, half-page
    /// scroll, plan panel/editor, help, and diff approval/rejection.
    fn handle_chord_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) -> bool {
        let key = crate::tui::keybindings::normalize_key(key);
        let keybinds = &self.overlays.keybinds;
        let m = |id: ActionId| keybinds.matches(id, key);
        if m(ActionId::Quit) {
            if !self.composer.text.is_empty() {
                self.composer.clear();
            } else if self.busy() {
                let _ = tx.send(Command::Interrupt);
                self.interrupt_requested = true;
            } else {
                self.should_quit = true;
            }
            return true;
        }
        if m(ActionId::Help) {
            self.overlays.help_scroll = 0;
            self.overlays.modal = Modal::Help;
            return true;
        }
        // Task-chat picker (task 96): Ctrl-N opens the modal selector,
        // claimed only while task chats exist; Ctrl-P keeps opening the
        // palette unconditionally (no more chord clash).
        if !self.task_chats.is_empty() && m(ActionId::TaskChatPicker) {
            self.open_task_picker();
            return true;
        }
        if m(ActionId::Palette) {
            self.overlays.modal = Modal::Palette {
                query: String::new(),
                selected: 0,
            };
            return true;
        }
        if m(ActionId::Sidebar) {
            self.session.sidebar_open = !self.session.sidebar_open;
            return true;
        }
        if m(ActionId::ModelMenu) {
            self.open_model_menu();
            return true;
        }
        if m(ActionId::Search) {
            self.overlays
                .search
                .open(self.view.scroll, self.view.follow);
            return true;
        }
        if m(ActionId::PasteImage) {
            // Clipboard image attach (F.6); a no-op flash when the
            // clipboard holds no image.
            self.start_clipboard_image_paste();
            return true;
        }
        if m(ActionId::FilePicker) {
            let cwd = if self.session.cwd.is_empty() {
                ".".to_string()
            } else {
                self.session.cwd.clone()
            };
            self.overlays.file_picker.open(&cwd);
            return true;
        }
        if m(ActionId::PlanToggle) {
            self.toggle_plan_panel();
            return true;
        }
        if m(ActionId::OpenEditor) {
            // Plan editor handoff (F.3, task 79): Ctrl-O opens the
            // session's plan file in $VISUAL/$EDITOR.
            self.open_plan_editor();
            return true;
        }
        if m(ActionId::ScrollHalfUp) {
            self.scroll_by(-(self.view.view_height as i32 / 2).max(1));
            return true;
        }
        if m(ActionId::ScrollHalfDown) {
            self.scroll_by((self.view.view_height as i32 / 2).max(1));
            return true;
        }
        false
    }

    /// Composer editing chords (reference `TextBuffer::handle_key`):
    /// ctrl motions and deletes plus Alt word motions. Effort cycling moved
    /// to Ctrl-F so Ctrl-E can be line-end; with an empty composer it jumps
    /// the scrollback to bottom.
    fn handle_editing_chord_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<Command>,
    ) -> bool {
        let key = crate::tui::keybindings::normalize_key(key);
        let keybinds = &self.overlays.keybinds;
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if keybinds.matches(ActionId::LineStart, key) {
                self.composer.move_home();
                return true;
            }
            if keybinds.matches(ActionId::LineEnd, key) {
                if self.composer.text.is_empty() {
                    self.scroll_to_bottom();
                } else {
                    self.composer.move_end();
                }
                return true;
            }
            if keybinds.matches(ActionId::DeleteWord, key)
                || (key.code == KeyCode::Backspace && key.modifiers == KeyModifiers::CONTROL)
            {
                self.composer.delete_word_back();
                return true;
            }
            if keybinds.matches(ActionId::KillLine, key) {
                self.composer.kill_to_end_of_line();
                return true;
            }
            return match key.code {
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
            if keybinds.matches(ActionId::CycleEffort, key) {
                let choices = self.thinking_choices();
                let current = crate::thinking::selected_choice(self.session.thinking, &choices);
                let next = choices
                    .iter()
                    .position(|choice| *choice == current)
                    .map_or(0, |index| (index + 1) % choices.len());
                self.set_thinking(choices[next], tx);
                return true;
            }
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
                // Inside a task chat, Esc stays local (the reference
                // swallows it there too): Esc-Esc cancels the subagent
                // while it works, and does nothing once finished — never
                // the main chat's interrupt chain.
                if let Some(idx) = self.active_task {
                    if self.task_chats[idx].outcome.is_none() {
                        let armed = self
                            .esc_pending
                            .is_some_and(|at| at.elapsed() <= super::FLASH_TTL);
                        self.esc_pending = None;
                        if armed {
                            let id = self.task_chats[idx].tool_use_id.clone();
                            let _ = tx.send(Command::CancelSubagent { tool_use_id: id });
                            // The cancelled chat closes as an error and
                            // says so in its own transcript.
                            self.conversation.apply(AgentEvent::Notice {
                                tone: crate::tui::provider::Tone::Warning,
                                text: "cancelled".into(),
                            });
                            self.task_chats[idx].finish(super::TaskOutcome::Error);
                            self.flash("task cancelled");
                        } else {
                            self.esc_pending = Some(std::time::Instant::now());
                            self.flash("press Esc again to cancel this task");
                        }
                    }
                    return true;
                }
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
            // BackTab so block navigation remains reachable. The change is
            // user-initiated, so it persists into the session meta.
            KeyCode::Tab => {
                self.toggle_mode();
                let _ = tx.send(Command::SetMode {
                    plan: self.mode == Mode::Plan,
                });
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
                    self.overlays.slash_selected = self.overlays.slash_selected.saturating_sub(1);
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
                    self.overlays.slash_selected = (self.overlays.slash_selected + 1).min(max);
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
                self.overlays.slash_selected = 0;
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                self.overlays.slash_selected = 0;
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
}
