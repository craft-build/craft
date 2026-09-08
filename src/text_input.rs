//! A minimal single-focus-handle text field.
//!
//! GPUI has no built-in text input widget (Zed's own text editing lives in
//! the much heavier `editor` crate), so this hand-rolls just enough: a
//! blinking-less caret rendered by splitting the content around the cursor
//! byte offset, backspace/delete/arrow handling, and character insertion via
//! `Keystroke::key_char`. That last part means IME composition (dead keys,
//! CJK input) isn't supported — acceptable for this app's plain-ASCII chat
//! composer and comment fields, but worth knowing if this is reused
//! elsewhere. Click-to-position (measuring text to find the clicked byte
//! offset) is likewise skipped: clicking a field focuses it and moves the
//! caret to the end.

use std::{ops::Range, rc::Rc};

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, FocusHandle, Focusable, HighlightStyle, KeyDownEvent, MouseButton,
    SharedString, StyledText, Window, div, px, rgb,
};

use crate::theme;

type SubmitCallback = dyn Fn(&str, &mut Window, &mut App);
type ChangeCallback = dyn Fn(&mut Window, &mut App);

pub struct TextInput {
    pub focus_handle: FocusHandle,
    pub content: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    placeholder: SharedString,
    /// Enter submits when `false`; when `true` plain Enter inserts a
    /// newline and only Cmd/Ctrl+Enter submits (matches the composer's
    /// "Cmd+Enter to send" placeholder).
    pub multiline: bool,
    soft_wrap: bool,
    on_submit: Option<Rc<SubmitCallback>>,
    on_change: Option<Rc<ChangeCallback>>,
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            cursor: 0,
            selection_anchor: None,
            placeholder: placeholder.into(),
            multiline: false,
            soft_wrap: false,
            on_submit: None,
            on_change: None,
        }
    }

    pub fn on_submit(mut self, f: impl Fn(&str, &mut Window, &mut App) + 'static) -> Self {
        self.on_submit = Some(Rc::new(f));
        self
    }

    #[allow(dead_code)]
    pub fn on_change(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }

    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self.soft_wrap = true;
        self
    }

    pub fn soft_wrap(mut self) -> Self {
        self.soft_wrap = true;
        self
    }

    pub fn set_content(&mut self, content: impl Into<String>) {
        self.content = content.into();
        self.cursor = self.content.len();
        self.selection_anchor = None;
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.content.clear();
        self.cursor = 0;
        self.selection_anchor = None;
    }

    pub fn take_content(&mut self) -> String {
        self.cursor = 0;
        self.selection_anchor = None;
        std::mem::take(&mut self.content)
    }

    fn selected_range(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then(|| anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selected_range() else {
            self.selection_anchor = None;
            return false;
        };
        self.cursor = range.start;
        self.content.replace_range(range, "");
        self.selection_anchor = None;
        true
    }

    fn replace_selection(&mut self, text: &str) {
        self.delete_selection();
        self.content.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn move_cursor(&mut self, cursor: usize, selecting: bool) {
        if selecting {
            self.selection_anchor.get_or_insert(self.cursor);
        } else {
            self.selection_anchor = None;
        }
        self.cursor = cursor;
    }

    fn prev_boundary(&self) -> usize {
        self.content[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.content[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.content.len())
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = event.keystroke.clone();
        let submit_mod = ks.modifiers.platform || ks.modifiers.control;
        if submit_mod {
            match ks.key.as_str() {
                "a" => {
                    self.selection_anchor = Some(0);
                    self.cursor = self.content.len();
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                "c" => {
                    if let Some(range) = self.selected_range() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            self.content[range].to_string(),
                        ));
                        cx.stop_propagation();
                        return;
                    }
                }
                "x" => {
                    if let Some(range) = self.selected_range() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            self.content[range].to_string(),
                        ));
                        self.delete_selection();
                        if let Some(cb) = self.on_change.clone() {
                            cb(window, cx);
                        }
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                "v" => {
                    if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                        let text = if self.multiline {
                            text
                        } else {
                            text.replace(['\r', '\n'], " ")
                        };
                        self.replace_selection(&text);
                        if let Some(cb) = self.on_change.clone() {
                            cb(window, cx);
                        }
                        cx.notify();
                    }
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        let selecting = ks.modifiers.shift;
        match ks.key.as_str() {
            "backspace" => {
                if !self.delete_selection() && self.cursor > 0 {
                    let start = self.prev_boundary();
                    self.content.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
            }
            "delete" => {
                if !self.delete_selection() && self.cursor < self.content.len() {
                    let end = self.next_boundary();
                    self.content.replace_range(self.cursor..end, "");
                }
            }
            "left" => {
                let cursor = if !selecting {
                    self.selected_range()
                        .map(|range| range.start)
                        .unwrap_or_else(|| self.prev_boundary())
                } else {
                    self.prev_boundary()
                };
                self.move_cursor(cursor, selecting);
            }
            "right" => {
                let cursor = if !selecting {
                    self.selected_range()
                        .map(|range| range.end)
                        .unwrap_or_else(|| self.next_boundary())
                } else {
                    self.next_boundary()
                };
                self.move_cursor(cursor, selecting);
            }
            "home" => self.move_cursor(0, selecting),
            "end" => self.move_cursor(self.content.len(), selecting),
            "enter" => {
                if submit_mod || !self.multiline {
                    if let Some(cb) = self.on_submit.clone() {
                        let content = self.take_content();
                        cb(&content, window, cx);
                    }
                    cx.notify();
                    return;
                }
                self.replace_selection("\n");
            }
            "escape" => {
                window.blur();
            }
            _ => {
                if !ks.modifiers.platform
                    && !ks.modifiers.control
                    && let Some(ch) = ks.key_char.as_ref()
                {
                    self.replace_selection(ch);
                }
            }
        }
        if let Some(cb) = self.on_change.clone() {
            cb(window, cx);
        }
        cx.notify();
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        let empty = self.content.is_empty();
        let rendered_text = (!empty).then(|| {
            let mut text = self.content.clone();
            let highlights = if let Some(range) = self.selected_range() {
                vec![(
                    range,
                    HighlightStyle {
                        background_color: Some(rgb(theme::SELECTION).into()),
                        ..Default::default()
                    },
                )]
            } else if focused {
                const CARET: &str = "▏";
                text.insert_str(self.cursor, CARET);
                vec![(
                    self.cursor..self.cursor + CARET.len(),
                    HighlightStyle::color(rgb(theme::ACCENT).into()),
                )]
            } else {
                Vec::new()
            };
            StyledText::new(text).with_highlights(highlights)
        });

        div()
            .id(("text-input", cx.entity_id()))
            .track_focus(&self.focus_handle)
            .cursor_text()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus_handle);
                    this.cursor = this.content.len();
                    this.selection_anchor = None;
                    cx.notify();
                }),
            )
            .w_full()
            .min_w(px(0.))
            .when(!self.soft_wrap || empty, |d| {
                d.flex().flex_row().items_center()
            })
            .when(self.soft_wrap, |d| d.whitespace_normal())
            .when(empty && focused, |d| {
                d.child(
                    div()
                        .w(px(1.5))
                        .h(px(14.))
                        .flex_shrink_0()
                        .bg(rgb(theme::ACCENT)),
                )
            })
            .when(empty, |d| {
                d.child(
                    div()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(self.placeholder.clone()),
                )
            })
            .when_some(rendered_text, |d, text| d.child(text))
    }
}
