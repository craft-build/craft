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
use serde_json::Value;

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
        match render_goal_doc(&self.goal_doc, &theme) {
            Some(parsed) => lines.extend(parsed),
            None => {
                for raw in self.goal_doc.lines() {
                    lines.push(Line::from(Span::styled(raw.to_string(), theme.foreground)));
                }
            }
        }
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

const LABEL_GOAL: &str = "Goal";
const LABEL_SCOPE: &str = "Scope";
const LABEL_OUT_OF_SCOPE: &str = "Out of scope";
const LABEL_ACCEPTANCE: &str = "Acceptance criteria";

#[derive(Debug)]
enum GoalSection {
    Text {
        label: &'static str,
        value: String,
    },
    List {
        label: &'static str,
        numbered: bool,
        items: Vec<String>,
    },
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse the goal doc JSON into ordered sections. Returns `None` if the input
/// is not valid JSON, is not an object, or yields no sections, so the caller
/// can fall back to showing the raw text.
fn parse_goal_doc(raw: &str) -> Option<Vec<GoalSection>> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let obj = value.as_object()?;

    let mut sections: Vec<GoalSection> = Vec::new();
    if let Some(goal) = obj.get("goal").and_then(Value::as_str) {
        sections.push(GoalSection::Text {
            label: LABEL_GOAL,
            value: goal.to_string(),
        });
    }
    if let Some(scope) = obj.get("scope").and_then(Value::as_str) {
        sections.push(GoalSection::Text {
            label: LABEL_SCOPE,
            value: scope.to_string(),
        });
    }
    let out_of_scope = string_array(obj.get("out_of_scope").unwrap_or(&Value::Null));
    if !out_of_scope.is_empty() {
        sections.push(GoalSection::List {
            label: LABEL_OUT_OF_SCOPE,
            numbered: false,
            items: out_of_scope,
        });
    }
    let criteria = string_array(obj.get("acceptance_criteria").unwrap_or(&Value::Null));
    if !criteria.is_empty() {
        sections.push(GoalSection::List {
            label: LABEL_ACCEPTANCE,
            numbered: true,
            items: criteria,
        });
    }

    if sections.is_empty() {
        None
    } else {
        Some(sections)
    }
}

fn labeled_text(label: &str, value: &str, theme: &theme::Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme.bold),
        Span::styled(value.to_string(), theme.foreground),
    ])
}

fn labeled_heading(label: &str, theme: &theme::Theme) -> Line<'static> {
    Line::from(Span::styled(label.to_string(), theme.bold))
}

fn bulleted(value: &str, theme: &theme::Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("  • ", theme.list_marker),
        Span::styled(value.to_string(), theme.foreground),
    ])
}

fn numbered_item(idx: usize, value: &str, theme: &theme::Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {idx}. "), theme.list_marker),
        Span::styled(value.to_string(), theme.foreground),
    ])
}

fn blank() -> Line<'static> {
    Line::default()
}

/// Render parsed goal sections into styled lines, with blank separators between
/// sections.
fn render_goal_sections(sections: &[GoalSection], theme: &theme::Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, section) in sections.iter().enumerate() {
        if i > 0 {
            lines.push(blank());
        }
        match section {
            GoalSection::Text { label, value } => {
                lines.push(labeled_text(label, value, theme));
            }
            GoalSection::List {
                label,
                numbered,
                items,
            } => {
                lines.push(labeled_heading(label, theme));
                for (j, item) in items.iter().enumerate() {
                    if *numbered {
                        lines.push(numbered_item(j + 1, item, theme));
                    } else {
                        lines.push(bulleted(item, theme));
                    }
                }
            }
        }
    }
    lines
}

/// Parse and render the goal doc JSON into labeled, styled lines. Returns
/// `None` when the input is not parseable so the caller falls back to raw text.
fn render_goal_doc(raw: &str, theme: &theme::Theme) -> Option<Vec<Line<'static>>> {
    Some(render_goal_sections(&parse_goal_doc(raw)?, theme))
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
    fn render_goal_doc_parses_full_object() {
        let raw = serde_json::json!({
            "goal": "ship feature x",
            "scope": "backend only",
            "out_of_scope": ["frontend", "docs"],
            "acceptance_criteria": ["tests pass", "qa pass"],
        })
        .to_string();
        let sections = parse_goal_doc(&raw).expect("should parse");
        assert_eq!(sections.len(), 4);

        match &sections[0] {
            GoalSection::Text { label, value } => {
                assert_eq!(*label, "Goal");
                assert_eq!(value, "ship feature x");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &sections[1] {
            GoalSection::Text { label, value } => {
                assert_eq!(*label, "Scope");
                assert_eq!(value, "backend only");
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &sections[2] {
            GoalSection::List {
                label,
                numbered,
                items,
            } => {
                assert_eq!(*label, "Out of scope");
                assert!(!*numbered);
                assert_eq!(items, &["frontend".to_string(), "docs".to_string()]);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match &sections[3] {
            GoalSection::List {
                label,
                numbered,
                items,
            } => {
                assert_eq!(*label, "Acceptance criteria");
                assert!(*numbered);
                assert_eq!(items, &["tests pass".to_string(), "qa pass".to_string()]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn render_goal_doc_none_for_invalid_json() {
        assert!(parse_goal_doc("not json").is_none());
    }

    #[test]
    fn render_goal_doc_none_for_empty_object() {
        assert!(parse_goal_doc("{}").is_none());
    }

    #[test]
    fn render_goal_doc_skips_empty_arrays() {
        let raw = serde_json::json!({
            "goal": "g",
            "out_of_scope": [],
            "acceptance_criteria": [],
        })
        .to_string();
        let sections = parse_goal_doc(&raw).expect("should parse");
        assert_eq!(sections.len(), 1);
    }
}
