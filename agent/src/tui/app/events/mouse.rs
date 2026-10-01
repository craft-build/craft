use super::*;

impl App {
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

    pub(super) fn toggle_tool(&mut self, idx: usize) {
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
            if copy_to_clipboard(&text) {
                self.flash("Copied");
            } else {
                self.flash("Copy failed: no clipboard tool");
            }
        }
    }
}
