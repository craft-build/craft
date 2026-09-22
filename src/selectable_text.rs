//! Selectable rich text for rendered conversation and diff content.
//!
//! GPUI's built-in `StyledText` supports links and rich runs but does not
//! provide browser-style selection. This wrapper keeps the same text layout,
//! maps pointer drags back to UTF-8 offsets, paints the selected range, and
//! exposes the active selection to the application-level copy shortcut.

use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

use gpui::{
    App, Bounds, ClipboardItem, CursorStyle, DispatchPhase, Element, ElementId, GlobalElementId,
    Hitbox, HitboxBehavior, InspectorElementId, IntoElement, Keystroke, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, SharedString, StyledText,
    Window, fill, point, rgba,
};

thread_local! {
    static ACTIVE_SELECTION: RefCell<Option<ActiveSelection>> = const { RefCell::new(None) };
}

#[derive(Clone)]
pub struct CommentTarget {
    pub key: String,
    pub label: String,
    pub scroll_handle: gpui::ScrollHandle,
    pub focus_handle: gpui::FocusHandle,
}

struct ActiveSelection {
    key: String,
    text: String,
    comment_target: Option<CommentTarget>,
}

/// Only ordinary text input starts a comment, never shortcuts or navigation.
pub fn comment_text_for_keystroke(keystroke: &Keystroke) -> Option<&str> {
    if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.function {
        return None;
    }
    keystroke
        .key_char
        .as_deref()
        .filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
}

/// Consume the selection once the first typed character is routed into a draft.
pub fn take_comment_selection() -> Option<(CommentTarget, String)> {
    ACTIVE_SELECTION.with_borrow_mut(|active| {
        let selection = active.as_ref()?;
        if selection.text.is_empty() {
            return None;
        }
        let target = selection.comment_target.clone()?;
        let text = selection.text.clone();
        *active = None;
        Some((target, text))
    })
}

/// Copies the most recently dragged text selection, if one exists.
pub fn copy_active_selection(cx: &mut App) -> bool {
    ACTIVE_SELECTION.with_borrow(|active| {
        let Some(selection) = active.as_ref() else {
            return false;
        };
        if selection.text.is_empty() {
            return false;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(selection.text.clone()));
        true
    })
}

#[derive(Default)]
struct SelectionState {
    anchor: Rc<Cell<usize>>,
    head: Rc<Cell<usize>>,
    dragging: Rc<Cell<bool>>,
}

type ClickListener = dyn Fn(usize, &mut Window, &mut App);

pub struct SelectableText {
    element_id: ElementId,
    selection_key: String,
    text: StyledText,
    source: SharedString,
    comment_target: Option<CommentTarget>,
    clickable_ranges: Vec<Range<usize>>,
    click_listener: Option<Box<ClickListener>>,
}

impl SelectableText {
    pub fn new(
        id: impl Into<SharedString>,
        text: StyledText,
        source: impl Into<SharedString>,
    ) -> Self {
        let id = id.into();
        Self {
            element_id: id.clone().into(),
            selection_key: id.to_string(),
            text,
            source: source.into(),
            comment_target: None,
            clickable_ranges: Vec::new(),
            click_listener: None,
        }
    }

    pub fn comment_target(mut self, target: CommentTarget) -> Self {
        self.comment_target = Some(target);
        self
    }

    pub fn on_click(
        mut self,
        ranges: Vec<Range<usize>>,
        listener: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.clickable_ranges = ranges;
        self.click_listener = Some(Box::new(listener));
        self
    }

    fn selection(anchor: usize, head: usize) -> Range<usize> {
        anchor.min(head)..anchor.max(head)
    }

    fn index_at(layout: &gpui::TextLayout, position: gpui::Point<Pixels>) -> usize {
        layout
            .index_for_position(position)
            .unwrap_or_else(|closest| closest)
            .min(layout.len())
    }

    fn remember_selection(
        key: &str,
        source: &str,
        range: Range<usize>,
        comment_target: Option<&CommentTarget>,
    ) {
        let text = source.get(range).unwrap_or_default().to_string();
        ACTIVE_SELECTION.with_borrow_mut(|active| {
            *active = Some(ActiveSelection {
                key: key.to_string(),
                text,
                comment_target: comment_target.cloned(),
            });
        });
    }

    fn is_active(key: &str) -> bool {
        ACTIVE_SELECTION.with_borrow(|active| {
            active
                .as_ref()
                .is_some_and(|selection| selection.key == key)
        })
    }

    fn word_range(source: &str, index: usize) -> Range<usize> {
        let is_word = |character: char| character.is_alphanumeric() || character == '_';
        let start = source[..index]
            .char_indices()
            .rev()
            .take_while(|(_, character)| is_word(*character))
            .last()
            .map(|(offset, _)| offset)
            .unwrap_or(index);
        let end = source[index..]
            .char_indices()
            .find(|(_, character)| !is_word(*character))
            .map(|(offset, _)| index + offset)
            .unwrap_or(source.len());
        start..end
    }

    fn selection_quads(layout: &gpui::TextLayout, range: Range<usize>) -> Vec<PaintQuad> {
        if range.is_empty() {
            return Vec::new();
        }
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let Some(start) = layout.position_for_index(range.start) else {
            return Vec::new();
        };
        let end = layout
            .position_for_index(range.end)
            .unwrap_or_else(|| point(bounds.right(), bounds.bottom() - line_height));
        let color = rgba(0x75a7df40);

        if start.y == end.y {
            return vec![fill(
                Bounds::from_corners(start, point(end.x, start.y + line_height)),
                color,
            )];
        }

        let mut quads = vec![fill(
            Bounds::from_corners(start, point(bounds.right(), start.y + line_height)),
            color,
        )];
        let middle_top = start.y + line_height;
        if middle_top < end.y {
            quads.push(fill(
                Bounds::from_corners(
                    point(bounds.left(), middle_top),
                    point(bounds.right(), end.y),
                ),
                color,
            ));
        }
        quads.push(fill(
            Bounds::from_corners(
                point(bounds.left(), end.y),
                point(end.x, end.y + line_height),
            ),
            color,
        ));
        quads
    }
}

impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = (Hitbox, Vec<PaintQuad>);

    fn id(&self) -> Option<ElementId> {
        Some(self.element_id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.text.request_layout(None, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.text
            .prepaint(None, inspector_id, bounds, state, window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let quads = window.with_element_state::<SelectionState, _>(
            global_id.expect("selectable text requires an element id"),
            |selection, _| {
                let selection = selection.unwrap_or_default();
                let range = if Self::is_active(&self.selection_key) {
                    Self::selection(selection.anchor.get(), selection.head.get())
                } else {
                    0..0
                };
                let quads = Self::selection_quads(self.text.layout(), range);
                (quads, selection)
            },
        );
        (hitbox, quads)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        (hitbox, quads): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        // Rich runs (notably Markdown code) paint their own opaque backgrounds.
        // Paint the translucent selection above those runs so it remains visible.
        self.text
            .paint(None, inspector_id, bounds, &mut (), &mut (), window, cx);
        for quad in quads.drain(..) {
            window.paint_quad(quad);
        }

        let current_view = window.current_view();
        let layout = self.text.layout().clone();
        let source = self.source.clone();
        let selection_key = self.selection_key.clone();
        let comment_target = self.comment_target.clone();
        let clickable_ranges = std::mem::take(&mut self.clickable_ranges);
        let click_listener = self.click_listener.take();
        let hitbox = hitbox.clone();

        window.with_element_state::<SelectionState, _>(
            global_id.expect("selectable text requires an element id"),
            move |selection, window| {
                let selection = selection.unwrap_or_default();

                {
                    let layout = layout.clone();
                    let hitbox = hitbox.clone();
                    let anchor = selection.anchor.clone();
                    let head = selection.head.clone();
                    let dragging = selection.dragging.clone();
                    let selection_key = selection_key.clone();
                    let source = source.clone();
                    let comment_target = comment_target.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                        // Clear before a new target handles the click, including clicks
                        // on inputs, blank space, and controls that change the view.
                        if phase == DispatchPhase::Capture
                            && Self::is_active(&selection_key)
                            && !hitbox.is_hovered(window)
                        {
                            ACTIVE_SELECTION.with_borrow_mut(|active| *active = None);
                            dragging.set(false);
                            window.refresh();
                        }
                        if phase == DispatchPhase::Bubble
                            && event.button == MouseButton::Left
                            && hitbox.is_hovered(window)
                        {
                            if let Some(target) = &comment_target {
                                window.focus(&target.focus_handle);
                            } else {
                                window.blur();
                            }
                            let index = Self::index_at(&layout, event.position);
                            let range = if event.click_count >= 3 {
                                0..source.len()
                            } else if event.click_count == 2 {
                                Self::word_range(&source, index)
                            } else {
                                index..index
                            };
                            anchor.set(range.start);
                            head.set(range.end);
                            dragging.set(event.click_count == 1);
                            Self::remember_selection(
                                &selection_key,
                                &source,
                                range,
                                comment_target.as_ref(),
                            );
                            cx.notify(current_view);
                            window.refresh();
                        }
                    });
                }

                {
                    let layout = layout.clone();
                    let anchor = selection.anchor.clone();
                    let head = selection.head.clone();
                    let dragging = selection.dragging.clone();
                    let selection_key = selection_key.clone();
                    let source = source.clone();
                    let comment_target = comment_target.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && dragging.get() {
                            let index = Self::index_at(&layout, event.position);
                            head.set(index);
                            Self::remember_selection(
                                &selection_key,
                                &source,
                                Self::selection(anchor.get(), index),
                                comment_target.as_ref(),
                            );
                            cx.notify(current_view);
                            window.refresh();
                        }
                    });
                }

                {
                    let layout = layout.clone();
                    let anchor = selection.anchor.clone();
                    let head = selection.head.clone();
                    let dragging = selection.dragging.clone();
                    let selection_key = selection_key.clone();
                    let source = source.clone();
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && dragging.replace(false) {
                            let index = Self::index_at(&layout, event.position);
                            head.set(index);
                            let range = Self::selection(anchor.get(), index);
                            Self::remember_selection(
                                &selection_key,
                                &source,
                                range.clone(),
                                comment_target.as_ref(),
                            );
                            if range.is_empty()
                                && let Some((range_index, _)) = clickable_ranges
                                    .iter()
                                    .enumerate()
                                    .find(|(_, link_range)| link_range.contains(&index))
                                && let Some(listener) = click_listener.as_ref()
                            {
                                listener(range_index, window, cx);
                            }
                            cx.notify(current_view);
                            window.refresh();
                        }
                    });
                }

                ((), selection)
            },
        );
    }
}

impl IntoElement for SelectableText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_starts_comments_but_shortcuts_and_navigation_do_not() {
        let mut keystroke = Keystroke::default();
        for text in ["a", "A", " ", "é", "🦀"] {
            keystroke.key_char = Some(text.to_string());
            assert_eq!(comment_text_for_keystroke(&keystroke), Some(text));
        }
        keystroke.modifiers.shift = true;
        assert_eq!(comment_text_for_keystroke(&keystroke), Some("🦀"));
        for modifiers in [
            gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            gpui::Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            keystroke.modifiers = modifiers;
            assert_eq!(comment_text_for_keystroke(&keystroke), None);
        }
        keystroke.modifiers = Default::default();
        for text in [None, Some(""), Some("\n"), Some("\t"), Some("\u{1b}")] {
            keystroke.key_char = text.map(str::to_string);
            assert_eq!(comment_text_for_keystroke(&keystroke), None);
        }
    }

    #[gpui::test]
    fn comment_selection_keeps_the_target_and_exact_quote_and_is_consumed_once(
        cx: &mut gpui::TestAppContext,
    ) {
        let target = CommentTarget {
            key: "session:msg_1".into(),
            label: "assistant reply".into(),
            scroll_handle: gpui::ScrollHandle::new(),
            focus_handle: cx.update(|cx| cx.focus_handle()),
        };
        SelectableText::remember_selection("markdown", "é\ncode", 0..7, Some(&target));
        let (selected_target, quote) = take_comment_selection().unwrap();
        assert_eq!(selected_target.key, target.key);
        assert_eq!(selected_target.label, target.label);
        assert_eq!(quote, "é\ncode");
        assert!(take_comment_selection().is_none());
        assert!(!SelectableText::is_active("markdown"));
    }

    #[gpui::test]
    fn empty_or_unanchored_selection_does_not_start_a_comment(cx: &mut gpui::TestAppContext) {
        SelectableText::remember_selection("plain", "text", 0..4, None);
        assert!(take_comment_selection().is_none());
        let target = CommentTarget {
            key: "session:msg_1".into(),
            label: "assistant reply".into(),
            scroll_handle: gpui::ScrollHandle::new(),
            focus_handle: cx.update(|cx| cx.focus_handle()),
        };
        SelectableText::remember_selection("markdown", "text", 1..1, Some(&target));
        assert!(take_comment_selection().is_none());
    }

    #[test]
    fn reverse_selection_copies_rendered_unicode_and_newlines() {
        let source = "é `code`\nnext";
        let range = SelectableText::selection(source.len(), "é ".len());
        SelectableText::remember_selection("code", source, range, None);
        ACTIVE_SELECTION.with_borrow(|active| {
            assert_eq!(active.as_ref().unwrap().text, "`code`\nnext");
        });
    }

    #[test]
    fn only_the_latest_selection_is_active() {
        SelectableText::remember_selection("markdown", "code", 0..4, None);
        assert!(SelectableText::is_active("markdown"));

        SelectableText::remember_selection("diff", "let value = 1;", 4..9, None);
        assert!(!SelectableText::is_active("markdown"));
        assert!(SelectableText::is_active("diff"));
        ACTIVE_SELECTION.with_borrow(|active| {
            assert_eq!(active.as_ref().unwrap().text, "value");
        });

        SelectableText::remember_selection("diff", "let value = 1;", 4..4, None);
        ACTIVE_SELECTION.with_borrow(|active| {
            assert!(active.as_ref().unwrap().text.is_empty());
        });
    }

    #[test]
    fn double_click_word_range_uses_utf8_boundaries() {
        let source = "let café_value = 1;";
        let range = SelectableText::word_range(source, "let café".len());
        assert_eq!(&source[range], "café_value");
    }
}
