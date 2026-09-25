//! Scroll math: offsets into the segment document, clamping, and keeping
//! the focused tool block in view.

use super::App;
use crate::tui::ui::scrollback::{Layout, ScrollPos};

impl App {
    pub fn scroll_by(&mut self, delta: i32) {
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        let pos = if delta >= 0 {
            layout.advance(self.view.scroll, delta as u32)
        } else {
            layout.retreat(self.view.scroll, delta.unsigned_abs())
        };
        self.set_scroll_pos(pos);
    }

    pub fn scroll_to_top(&mut self) {
        self.set_scroll_pos(ScrollPos::default());
    }

    pub fn scroll_to_bottom(&mut self) {
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        // `bottom` (not `end`) so the viewport is exactly filled; `end`
        // would address rows past the last one and paint a blank screen.
        let target = if self.view.view_height > 0 {
            layout.bottom(self.view.view_height)
        } else {
            layout.end()
        };
        self.set_scroll_pos(target);
    }

    /// Applies a scroll position: clamped into the document, and re-pins
    /// follow once the viewport sits at the document bottom.
    fn set_scroll_pos(&mut self, pos: ScrollPos) {
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        let mut clamped = layout.clamp(pos);
        // Before the first frame (no viewport height) keep the follow flag
        // as-is: there is no bottom to be at yet. Otherwise, like the
        // flat-offset model before it, the viewport never scrolls past the
        // point where the document stops filling it.
        if self.view.view_height > 0 {
            let bottom = layout.bottom(self.view.view_height);
            clamped = clamped.min(bottom);
            self.view.follow = clamped >= bottom;
        }
        self.view.scroll = clamped;
        // Card rects move with the scroll; stale hover/click state is dropped.
        self.view.hover_tool = None;
        self.view.pending_click = None;
    }

    /// After focus changes, make sure the focused block is in view.
    pub(crate) fn ensure_focus_visible(&mut self) {
        let Some(i) = self.conversation.focused else {
            return;
        };
        let Some(&doc) = self.view.msg_starts.get(i) else {
            return;
        };
        let doc = doc as u32;
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        let top = layout.doc_row(self.view.scroll);
        if doc < top || doc >= top + u32::from(self.view.view_height) {
            self.set_scroll_pos(layout.at_row(doc.saturating_sub(2)));
            // Like the flat-offset code before it: an explicit jump away
            // from the bottom must not be re-pinned by the next frame.
            self.view.follow = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{draw_app, scrolled_app};
    use super::*;
    use crate::tui::provider::AgentEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::sync::mpsc;

    /// While following, the viewport sits at the document bottom every
    /// frame, and streaming more content keeps it pinned there.
    #[test]
    fn follow_pins_the_viewport_to_the_bottom() {
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        assert!(app.view.follow);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            layout
                .total_rows()
                .saturating_sub(u32::from(app.view.view_height)),
            "follow shows the last viewport-height rows"
        );
        app.handle_event(AgentEvent::AssistantText("one more".into()));
        draw_app(&mut app, 80, 24);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            layout
                .total_rows()
                .saturating_sub(u32::from(app.view.view_height)),
            "new content keeps the bottom pinned"
        );
    }

    /// Scrolling up breaks follow; appending messages afterwards leaves
    /// the viewport where it was (append-stable addressable rows).
    #[test]
    fn scrolling_up_breaks_follow_and_appends_do_not_jump() {
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        app.scroll_by(-3);
        assert!(!app.view.follow, "scrolling up breaks follow");
        let before = {
            let layout = Layout::new(&app.view.segments, app.view.view_width);
            (app.view.scroll, layout.doc_row(app.view.scroll))
        };
        app.handle_event(AgentEvent::AssistantText("tail message".into()));
        draw_app(&mut app, 80, 24);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(app.view.scroll, before.0, "anchor position survives append");
        assert_eq!(
            layout.doc_row(app.view.scroll),
            before.1,
            "appended segments do not shift the viewport"
        );
        assert!(!app.view.follow);
    }

    /// The scroll position is width-independent: a resize keeps the anchor
    /// segment instead of jumping by a row delta.
    #[test]
    fn resize_keeps_the_anchor_segment() {
        let mut app = scrolled_app();
        draw_app(&mut app, 100, 24);
        app.scroll_by(-5);
        draw_app(&mut app, 100, 24);
        let anchored = app.view.scroll.seg;
        assert!(anchored > 0, "scrolled off the top of the document");
        draw_app(&mut app, 50, 24);
        assert_eq!(
            app.view.scroll.seg, anchored,
            "a resize is not a scroll: the anchor segment survives re-wrapping"
        );
    }

    /// The full keyboard scroll set: line, half page, page, top, bottom.
    #[test]
    fn keyboard_scroll_set_walks_and_clamps() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);

        // Top: Home and g.
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &tx);
        assert_eq!(app.view.scroll, ScrollPos::default());
        app.scroll_by(2);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
        assert_eq!(app.view.scroll, ScrollPos::default());

        // Bottom: End and G re-pin follow.
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &tx);
        assert!(app.view.follow);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll) + u32::from(app.view.view_height),
            layout.total_rows()
        );
        app.scroll_by(-2);
        assert!(!app.view.follow);
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &tx);
        assert!(app.view.follow, "G lands at the bottom and re-pins");

        // Line, half page, page: positions move by the expected row counts.
        // The arrows are history keys now, so the one-row step goes through
        // the public API instead of a keypress.
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &tx);
        app.scroll_by(1);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(layout.doc_row(app.view.scroll), 1, "one row down");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &tx,
        );
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            1 + u32::from(app.view.view_height) / 2,
            "Ctrl-D scrolls half a page"
        );
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &tx);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            1 + u32::from(app.view.view_height) / 2 + u32::from(app.view.view_height),
            "PageDown scrolls a full page"
        );

        // Scrolling past the top clamps at the document start.
        app.scroll_by(-9999);
        assert_eq!(app.view.scroll, ScrollPos::default());
    }
}
