use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::hint_line;
use crate::components::is_ctrl;
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::text_buffer::TextBuffer;
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

const TITLE: &str = " Flow goal ";
const WIDTH_PERCENT: u16 = 80;
const MAX_HEIGHT_PERCENT: u16 = 80;
const H_PAD: u16 = 2;

const HINT_VIEW: &[(&str, &str)] = &[("y/Enter", "Approve"), ("e", "Edit"), ("c/Esc", "Cancel")];
const HINT_EDIT: &[(&str, &str)] = &[("Enter", "Submit"), ("Esc", "Back")];
const EMPTY_REVISION_HINT: &str = "type a revised goal";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PromptState {
    #[default]
    Viewing,
    Editing,
}

/// The user's response to the goal-approval gate. The app maps this to the
/// magic strings the flow loop resumes on (`FLOW_APPROVE_ANSWER`,
/// `FLOW_CANCEL_ANSWER`, or the revised goal text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowGoalAnswer {
    Approve,
    Cancel,
    Revise(String),
}

/// Modal overlay shown when the Flow pipeline pauses at the goal-approval
/// gate. Renders the proposed goal doc (scrollable) and collects an approve /
/// revise / cancel decision without going through the input box. Replaces the
/// earlier input-box routing so the gate has a dedicated surface.
pub struct FlowGoalPrompt {
    open: bool,
    goal_doc: String,
    state: PromptState,
    buffer: TextBuffer,
    scroll: ModalScroll,
}

impl FlowGoalPrompt {
    pub fn new() -> Self {
        Self {
            open: false,
            goal_doc: String::new(),
            state: PromptState::Viewing,
            buffer: TextBuffer::new(String::new()),
            scroll: ModalScroll::new(),
        }
    }

    pub fn open(&mut self, goal_doc: String) {
        self.close();
        self.open = true;
        self.goal_doc = goal_doc;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.goal_doc.clear();
        self.state = PromptState::Viewing;
        self.buffer = TextBuffer::new(String::new());
        self.scroll.reset();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Paste into the revision buffer when in editing mode. Returns whether
    /// the paste was consumed, so `route_text_paste` can fall through.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        if !self.open || self.state != PromptState::Editing {
            return false;
        }
        self.buffer.insert_text(text);
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<FlowGoalAnswer> {
        if !self.open {
            return None;
        }
        if is_ctrl(&key) && key.code == KeyCode::Char('c') {
            return Some(FlowGoalAnswer::Cancel);
        }
        if self.state == PromptState::Editing {
            return match key.code {
                KeyCode::Enter => {
                    let text = self.buffer.value().trim().to_string();
                    Some(FlowGoalAnswer::Revise(text))
                }
                KeyCode::Esc => {
                    self.buffer = TextBuffer::new(String::new());
                    self.state = PromptState::Viewing;
                    None
                }
                _ => {
                    self.buffer.handle_key(key);
                    None
                }
            };
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => Some(FlowGoalAnswer::Approve),
            KeyCode::Char('c') | KeyCode::Esc => Some(FlowGoalAnswer::Cancel),
            KeyCode::Char('e') => {
                self.state = PromptState::Editing;
                None
            }
            _ => {
                self.scroll.handle_key(key);
                None
            }
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        let theme = theme::current();
        let modal = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let border_chrome: u16 = 2;
        let padded_width = (area.width as u32 * WIDTH_PERCENT as u32 / 100)
            .saturating_sub((border_chrome + H_PAD * 2) as u32) as u16;

        let mut lines: Vec<Line> = Vec::new();
        lines.extend(render_goal_doc(&self.goal_doc, &theme));
        if self.state == PromptState::Editing {
            lines.push(Line::default());
            lines.push(self.revision_line(&theme));
        }
        lines.push(Line::default());
        let hints = if self.state == PromptState::Editing {
            HINT_EDIT
        } else {
            HINT_VIEW
        };
        lines.push(hint_line(hints));

        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(padded_width) as u16;
        let (popup, inner) = modal.render(frame, area, total);
        let padded = Rect {
            x: inner.x + H_PAD,
            width: inner.width.saturating_sub(H_PAD * 2),
            ..inner
        };
        let viewport_h = padded.height;
        self.scroll.update_dimensions(total, viewport_h);
        let scroll = self.scroll.offset();

        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, padded);

        if total > viewport_h {
            render_vertical_scrollbar(frame, inner, total, scroll);
        }
        popup
    }

    fn revision_line(&self, theme: &theme::Theme) -> Line<'static> {
        let text = self.buffer.value();
        let (display, cursor_pos) = if text.is_empty() {
            (EMPTY_REVISION_HINT, 0)
        } else {
            (
                text.as_str(),
                TextBuffer::char_to_byte(&text, self.buffer.x()),
            )
        };
        let (before, after) = display.split_at(cursor_pos);
        let mut chars = after.chars();
        let cursor_ch = chars.next().unwrap_or(' ');
        let rest: String = chars.collect();
        let mut spans = vec![Span::styled("revision: ", theme.tool_dim)];
        if text.is_empty() {
            spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
            spans.push(Span::styled(rest, theme.tool_dim));
        } else {
            spans.push(Span::raw(before.to_string()));
            spans.push(Span::styled(cursor_ch.to_string(), Style::new().reversed()));
            if !rest.is_empty() {
                spans.push(Span::raw(rest));
            }
        }
        Line::from(spans)
    }
}

/// Render the goal doc prose into styled lines. The goal doc is markdown
/// (typically `## Goal`, `## Scope`, `## Acceptance criteria` sections);
/// headings are bolded and everything else is shown as plain foreground text.
fn render_goal_doc(raw: &str, theme: &theme::Theme) -> Vec<Line<'static>> {
    raw.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("# ") {
                Line::from(Span::styled(rest.to_string(), theme.bold))
            } else if trimmed.starts_with("##") {
                Line::from(Span::styled(trimmed.to_string(), theme.bold))
            } else {
                Line::from(Span::styled(line.to_string(), theme.foreground))
            }
        })
        .collect()
}

impl Overlay for FlowGoalPrompt {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn open_sets_goal_doc_and_state() {
        let mut p = FlowGoalPrompt::new();
        p.open("do the thing".into());
        assert!(p.is_open());
        assert_eq!(p.state, PromptState::Viewing);
    }

    #[test]
    fn close_resets_state() {
        let mut p = FlowGoalPrompt::new();
        p.open("goal".into());
        p.handle_key(key(KeyCode::Char('e')));
        p.scroll.update_dimensions(100, 10);
        p.scroll.scroll(-5);
        p.close();
        assert!(!p.is_open());
        assert!(p.goal_doc.is_empty());
        assert_eq!(p.state, PromptState::Viewing);
        assert_eq!(p.scroll.offset(), 0);
    }

    #[test]
    fn double_open_resets_first() {
        let mut p = FlowGoalPrompt::new();
        p.open("first".into());
        p.scroll.update_dimensions(100, 10);
        p.scroll.scroll(-5);
        p.open("second".into());
        assert_eq!(p.goal_doc, "second");
        assert_eq!(p.scroll.offset(), 0);
    }

    #[test]
    fn y_approves() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert_eq!(
            p.handle_key(key(KeyCode::Char('y'))),
            Some(FlowGoalAnswer::Approve)
        );
    }

    #[test]
    fn enter_approves_in_viewing() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(FlowGoalAnswer::Approve)
        );
    }

    #[test]
    fn c_cancels() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert_eq!(
            p.handle_key(key(KeyCode::Char('c'))),
            Some(FlowGoalAnswer::Cancel)
        );
    }

    #[test]
    fn ctrl_c_cancels() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert_eq!(p.handle_key(ctrl_c()), Some(FlowGoalAnswer::Cancel));
    }

    #[test]
    fn esc_cancels_in_viewing() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert_eq!(
            p.handle_key(key(KeyCode::Esc)),
            Some(FlowGoalAnswer::Cancel)
        );
    }

    #[test]
    fn e_enters_editing() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert_eq!(p.handle_key(key(KeyCode::Char('e'))), None);
        assert_eq!(p.state, PromptState::Editing);
    }

    #[test]
    fn editing_enter_submits_revision() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        p.handle_key(key(KeyCode::Char('e')));
        p.handle_paste("ship it");
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(FlowGoalAnswer::Revise("ship it".into()))
        );
    }

    #[test]
    fn editing_esc_returns_to_viewing() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        p.handle_key(key(KeyCode::Char('e')));
        p.handle_paste("draft");
        assert_eq!(p.handle_key(key(KeyCode::Esc)), None);
        assert_eq!(p.state, PromptState::Viewing);
        assert!(p.buffer.value().is_empty());
    }

    #[test]
    fn editing_empty_enter_submits_empty_revision() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        p.handle_key(key(KeyCode::Char('e')));
        assert_eq!(
            p.handle_key(key(KeyCode::Enter)),
            Some(FlowGoalAnswer::Revise(String::new()))
        );
    }

    #[test]
    fn handle_paste_only_in_editing() {
        let mut p = FlowGoalPrompt::new();
        p.open("g".into());
        assert!(!p.handle_paste("ignored"));
        p.handle_key(key(KeyCode::Char('e')));
        assert!(p.handle_paste("accepted"));
        assert_eq!(p.buffer.value(), "accepted");
    }

    #[test]
    fn render_goal_doc_returns_one_line_per_input_line() {
        let t = theme::current();
        let lines = render_goal_doc("## Goal\nship it\n## Scope\nbackend", &t);
        assert_eq!(lines.len(), 4);
    }
}
