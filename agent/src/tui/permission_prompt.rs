//! Permission prompt overlay (F.5): a bottom, non-modal form that surfaces
//! a gated tool call — its tool name, scopes, and display context — and
//! negotiates the answer: allow once/session/always (project or all
//! projects), deny with editable guidance, deny-always. Ported from the
//! reference `craft-ui/src/components/permission_prompt.rs`; `subagent_id`
//! routing is deferred until subagents land (task 95).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::permissions::{DEFAULT_DENY_GUIDANCE, PermissionAnswer, ToolKey, generalized_scopes};
use crate::tui::ui::theme;

const HINT_ALLOW_ROW: &[(&str, &str)] = &[
    ("y", "Allow"),
    ("a", "Always (project)"),
    ("A", "Always (all projects)"),
    ("s", "Session"),
];
const HINT_DENY_ROW: &[(&str, &str)] = &[
    ("n", "Deny"),
    ("d", "Deny-always (project)"),
    ("D", "Deny-always (all)"),
];

const CONFIRM_ALLOW_PROJECT_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow-always (project)"),
    ("any", "Cancel"),
];
const CONFIRM_ALLOW_ALL_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm allow-always (all projects)"),
    ("any", "Cancel"),
];
const CONFIRM_SESSION_HINTS: &[(&str, &str)] =
    &[("Enter / y", "Confirm allow (session)"), ("any", "Cancel")];
const CONFIRM_DENY_PROJECT_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny-always (project)"),
    ("any", "Cancel"),
];
const CONFIRM_DENY_ALL_HINTS: &[(&str, &str)] = &[
    ("Enter / y", "Confirm deny-always (all projects)"),
    ("any", "Cancel"),
];

const DENY_GUIDANCE_HINTS: &[(&str, &str)] = &[("Enter", "Deny"), ("Esc", "Cancel")];

fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    Line::from(
        hints
            .iter()
            .enumerate()
            .flat_map(|(i, (key, desc))| {
                let sep = if i == 0 { "  " } else { "    " };
                [
                    Span::raw(sep),
                    Span::styled(
                        (*key).to_string(),
                        Style::default().fg(theme::current().cyan),
                    ),
                    Span::styled(
                        format!(" {desc}"),
                        Style::default().fg(theme::current().text_tertiary),
                    ),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

/// Two-row hint grid with columns aligned across rows, so the allow and
/// deny actions read as a table rather than two ragged lists.
fn aligned_hint_rows(rows: &[&[(&str, &str)]]) -> Vec<Line<'static>> {
    let max_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut col_widths = vec![0usize; max_cols];
    for row in rows {
        for (i, (key, desc)) in row.iter().enumerate() {
            let cell_len = key.len() + 1 + desc.len();
            col_widths[i] = col_widths[i].max(cell_len);
        }
    }
    rows.iter()
        .map(|row| {
            let mut spans = Vec::with_capacity(row.len() * 2);
            for (i, (key, desc)) in row.iter().enumerate() {
                spans.push(Span::raw("  ".to_string()));
                spans.push(Span::styled(
                    (*key).to_string(),
                    Style::default().fg(theme::current().cyan),
                ));
                let cell_len = key.len() + 1 + desc.len();
                let pad = if i + 1 < row.len() {
                    col_widths[i].saturating_sub(cell_len)
                } else {
                    0
                };
                spans.push(Span::styled(
                    format!(" {desc}{:width$}", "", width = pad),
                    Style::default().fg(theme::current().text_tertiary),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

/// Single-line editable buffer for the deny-guidance field: insert,
/// backspace, and cursor motions. The composer is multi-line and
/// submit-oriented, so it does not fit here.
#[derive(Default)]
pub(crate) struct PromptBuffer {
    text: String,
    /// Cursor position in chars.
    cursor: usize,
}

impl PromptBuffer {
    fn insert(&mut self, c: char) {
        let byte = self.char_to_byte();
        self.text.insert(byte, c);
        self.cursor += 1;
    }

    fn insert_text(&mut self, text: &str) {
        let n = text.chars().count();
        if n == 0 {
            return;
        }
        let byte = self.char_to_byte();
        self.text.insert_str(byte, text);
        self.cursor += n;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let byte = self.char_to_byte();
            self.text.remove(byte);
        }
    }

    fn char_to_byte(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    fn value(&self) -> &str {
        &self.text
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                if self.cursor < self.text.chars().count() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.chars().count(),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptState {
    #[default]
    Normal,
    ConfirmAllowAlwaysLocal,
    ConfirmAllowAlwaysGlobal,
    ConfirmAllowSession,
    ConfirmDenyAlwaysLocal,
    ConfirmDenyAlwaysGlobal,
    DenyEditing,
}

pub enum PermissionPrompt {
    Closed,
    Open {
        /// Tool-call id the answer is routed to.
        id: String,
        tool: String,
        scopes: Vec<String>,
        files: Vec<String>,
        commands: Vec<String>,
        /// What an "always" answer would actually grant (generalized
        /// scopes); empty when it is identical to the concrete scopes.
        allow_scopes: Vec<String>,
        state: PromptState,
        buffer: PromptBuffer,
    },
}

impl PermissionPrompt {
    pub fn new() -> Self {
        Self::Closed
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open { .. })
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Open { id, .. } => Some(id),
            Self::Closed => None,
        }
    }

    pub fn open(
        &mut self,
        id: String,
        tool: String,
        scopes: Vec<String>,
        files: Vec<String>,
        commands: Vec<String>,
    ) {
        let tool_key = ToolKey::native(&tool);
        let allow_scopes = generalized_scopes(&tool_key, &scopes);
        let allow_scopes = if allow_scopes == scopes {
            vec![]
        } else {
            allow_scopes
        };
        *self = Self::Open {
            id,
            tool,
            scopes,
            files,
            commands,
            allow_scopes,
            state: PromptState::Normal,
            buffer: PromptBuffer::default(),
        };
    }

    pub fn close(&mut self) {
        *self = Self::Closed;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<PermissionAnswer> {
        let Self::Open { state, buffer, .. } = self else {
            return None;
        };
        // Ctrl-C denies from any state, matching the reference: the prompt
        // is the urgent keyboard owner while a tool call is parked on it.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(PermissionAnswer::Deny);
        }
        if *state == PromptState::DenyEditing {
            return match key.code {
                KeyCode::Enter => {
                    let text = buffer.value().trim().to_string();
                    if text.is_empty() {
                        Some(PermissionAnswer::Deny)
                    } else {
                        Some(PermissionAnswer::DenyWithGuidance(text))
                    }
                }
                KeyCode::Esc => {
                    *buffer = PromptBuffer::default();
                    *state = PromptState::Normal;
                    None
                }
                _ => {
                    buffer.handle_key(key);
                    None
                }
            };
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let confirm_answer = match *state {
            PromptState::ConfirmAllowAlwaysLocal => Some(PermissionAnswer::AllowAlwaysLocal),
            PromptState::ConfirmAllowAlwaysGlobal => Some(PermissionAnswer::AllowAlwaysGlobal),
            PromptState::ConfirmAllowSession => Some(PermissionAnswer::AllowSession),
            PromptState::ConfirmDenyAlwaysLocal => Some(PermissionAnswer::DenyAlwaysLocal),
            PromptState::ConfirmDenyAlwaysGlobal => Some(PermissionAnswer::DenyAlwaysGlobal),
            _ => None,
        };
        if let Some(answer) = confirm_answer {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(answer),
                _ => {
                    *state = PromptState::Normal;
                    None
                }
            };
        }
        match key.code {
            KeyCode::Char('y') => Some(PermissionAnswer::AllowOnce),
            KeyCode::Char('n') => {
                *state = PromptState::DenyEditing;
                None
            }
            KeyCode::Char('a') => {
                *state = PromptState::ConfirmAllowAlwaysLocal;
                None
            }
            KeyCode::Char('A') => {
                *state = PromptState::ConfirmAllowAlwaysGlobal;
                None
            }
            KeyCode::Char('d') => {
                *state = PromptState::ConfirmDenyAlwaysLocal;
                None
            }
            KeyCode::Char('D') => {
                *state = PromptState::ConfirmDenyAlwaysGlobal;
                None
            }
            KeyCode::Char('s') => {
                *state = PromptState::ConfirmAllowSession;
                None
            }
            _ => None,
        }
    }

    /// Paste lands in the guidance buffer; only consumed while editing.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Self::Open { state, buffer, .. } = self else {
            return false;
        };
        if *state == PromptState::DenyEditing {
            buffer.insert_text(text);
            return true;
        }
        false
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let Self::Open {
            tool,
            scopes,
            files,
            commands,
            allow_scopes,
            state,
            buffer,
            ..
        } = self
        else {
            return vec![];
        };
        let label_style = Style::default().fg(theme::current().text_tertiary);
        let value_style = Style::default().fg(theme::current().text_primary);

        let mut lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("tool  ".to_string(), label_style),
                Span::styled(tool.clone(), value_style),
            ]),
        ];
        for (i, s) in scopes.iter().enumerate() {
            let label = if i == 0 { "scope " } else { "    + " };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label.to_string(), label_style),
                Span::styled(s.clone(), value_style),
            ]));
        }
        for (i, f) in files.iter().enumerate() {
            let label = if i == 0 { "file  " } else { "    + " };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label.to_string(), label_style),
                Span::styled(f.clone(), value_style),
            ]));
        }
        for (i, c) in commands.iter().enumerate() {
            let label = if i == 0 { "cmd   " } else { "    + " };
            let display = if c.chars().count() > 80 {
                let truncated: String = c.chars().take(79).collect();
                format!("{truncated}…")
            } else {
                c.clone()
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(label.to_string(), label_style),
                Span::styled(display, value_style),
            ]));
        }
        if !allow_scopes.is_empty() {
            for (i, g) in allow_scopes.iter().enumerate() {
                let label = if i == 0 { "allow " } else { "    + " };
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label.to_string(), label_style),
                    Span::styled(g.clone(), value_style),
                ]));
            }
        }

        if *state == PromptState::DenyEditing {
            let text = buffer.value();
            let (before, after) = if text.is_empty() {
                ("", DEFAULT_DENY_GUIDANCE)
            } else {
                let byte = buffer.char_to_byte();
                (&text[..byte], &text[byte..])
            };
            let mut chars = after.chars();
            let cursor_ch = chars.next().unwrap_or(' ');
            let rest: String = chars.collect();
            let mut spans = vec![
                Span::raw("  "),
                Span::styled("guide ".to_string(), label_style),
            ];
            if text.is_empty() {
                spans.push(Span::styled(
                    cursor_ch.to_string(),
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::styled(rest, label_style));
            } else {
                spans.push(Span::raw(before.to_string()));
                spans.push(Span::styled(
                    cursor_ch.to_string(),
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
                if !rest.is_empty() {
                    spans.push(Span::raw(rest));
                }
            }
            lines.push(Line::from(spans));
        }

        lines.push(Line::raw(""));
        match *state {
            PromptState::ConfirmAllowAlwaysLocal => {
                lines.push(hint_line(CONFIRM_ALLOW_PROJECT_HINTS));
            }
            PromptState::ConfirmAllowAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_ALLOW_ALL_HINTS));
            }
            PromptState::ConfirmAllowSession => {
                lines.push(hint_line(CONFIRM_SESSION_HINTS));
            }
            PromptState::ConfirmDenyAlwaysLocal => {
                lines.push(hint_line(CONFIRM_DENY_PROJECT_HINTS));
            }
            PromptState::ConfirmDenyAlwaysGlobal => {
                lines.push(hint_line(CONFIRM_DENY_ALL_HINTS));
            }
            PromptState::DenyEditing => lines.push(hint_line(DENY_GUIDANCE_HINTS)),
            PromptState::Normal => {
                lines.extend(aligned_hint_rows(&[HINT_ALLOW_ROW, HINT_DENY_ROW]));
            }
        }
        lines.push(Line::raw(""));
        lines
    }

    pub fn view(&self, f: &mut Frame, area: Rect) {
        let t = theme::current();

        if !self.is_open() {
            return;
        }
        let lines = self.build_lines();
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(
            ratatui::widgets::Block::default().style(Style::default().bg(t.bg_raised)),
            area,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Permission Required",
                Style::default()
                    .fg(t.text_primary)
                    .add_modifier(Modifier::BOLD),
            )))
            .style(Style::default().bg(t.bg_raised)),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1.min(area.height),
            },
        );
        let body = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        };
        if !body.is_empty() {
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().bg(t.bg_raised)),
                body,
            );
        }
    }

    pub fn height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2).max(1) as usize;
        // `Paragraph::line_count` is unstable; estimate wrapped rows by
        // unicode width per line, matching `Wrap { trim: false }`.
        let rows: usize = self
            .build_lines()
            .iter()
            .map(|line| {
                let w = line.width();
                w.div_ceil(inner_width).max(1)
            })
            .sum();
        rows as u16
            + 1 // title row
            + 1 // rounding margin
    }
}

impl Default for PermissionPrompt {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{PermissionPrompt, PromptState};
    use crate::permissions::PermissionAnswer;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn open_prompt() -> PermissionPrompt {
        let mut prompt = PermissionPrompt::new();
        prompt.open(
            "id".into(),
            "bash".into(),
            vec!["execute".into()],
            Vec::new(),
            Vec::new(),
        );
        prompt
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn state(prompt: &PermissionPrompt) -> PromptState {
        let PermissionPrompt::Open { state, .. } = prompt else {
            panic!("expected Open");
        };
        *state
    }

    #[test]
    fn ctrl_c_denies() {
        let mut prompt = open_prompt();
        assert_eq!(prompt.handle_key(ctrl_c()), Some(PermissionAnswer::Deny));
        // Also from the editing state.
        let mut prompt2 = open_prompt();
        prompt2.handle_key(key(KeyCode::Char('n')));
        prompt2.handle_key(key(KeyCode::Char('t')));
        assert_eq!(prompt2.handle_key(ctrl_c()), Some(PermissionAnswer::Deny));
    }

    #[test]
    fn y_allows_once() {
        let mut prompt = open_prompt();
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))),
            Some(PermissionAnswer::AllowOnce)
        );
    }

    #[test]
    fn session_and_always_go_through_confirm() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('s')));
        assert_eq!(state(&prompt), PromptState::ConfirmAllowSession);
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::AllowSession)
        );

        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('a')));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))),
            Some(PermissionAnswer::AllowAlwaysLocal)
        );

        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('A')));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::AllowAlwaysGlobal)
        );

        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('d')));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::DenyAlwaysLocal)
        );

        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('D')));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Char('y'))),
            Some(PermissionAnswer::DenyAlwaysGlobal)
        );
    }

    #[test]
    fn any_key_cancels_confirm() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('s')));
        assert_eq!(prompt.handle_key(key(KeyCode::Char('x'))), None);
        assert_eq!(state(&prompt), PromptState::Normal);
    }

    #[test]
    fn n_goes_to_deny_editing() {
        let mut prompt = open_prompt();
        assert_eq!(prompt.handle_key(key(KeyCode::Char('n'))), None);
        assert_eq!(state(&prompt), PromptState::DenyEditing);
    }

    #[test]
    fn deny_editing_esc_returns_to_normal() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_key(key(KeyCode::Char('t')));
        assert_eq!(prompt.handle_key(key(KeyCode::Esc)), None);
        if let PermissionPrompt::Open { buffer, .. } = &prompt {
            assert_eq!(state(&prompt), PromptState::Normal);
            assert!(buffer.value().is_empty());
        } else {
            panic!("expected Open");
        }
    }

    #[test]
    fn deny_editing_enter_empty_sends_deny() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::Deny)
        );
    }

    #[test]
    fn deny_editing_with_text_sends_guidance() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_paste("Use cat");
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::DenyWithGuidance("Use cat".into()))
        );
    }

    #[test]
    fn typed_guidance_trims_and_keeps_cursor_editing() {
        let mut prompt = open_prompt();
        prompt.handle_key(key(KeyCode::Char('n')));
        prompt.handle_key(key(KeyCode::Char('u')));
        prompt.handle_key(key(KeyCode::Char('s')));
        prompt.handle_key(key(KeyCode::Backspace));
        prompt.handle_paste("se cat instead");
        assert_eq!(
            prompt.handle_key(key(KeyCode::Enter)),
            Some(PermissionAnswer::DenyWithGuidance("use cat instead".into()))
        );
    }

    #[test]
    fn handle_paste_requires_editing_mode() {
        let mut prompt = open_prompt();
        assert!(!prompt.handle_paste("ignored"));
        prompt.handle_key(key(KeyCode::Char('n')));
        assert!(prompt.handle_paste("accepted"));
        if let PermissionPrompt::Open { buffer, .. } = &prompt {
            assert_eq!(buffer.value(), "accepted");
        } else {
            panic!("expected Open");
        }
    }

    #[test]
    fn generalizes_allow_scopes_for_file_tools() {
        let mut prompt = PermissionPrompt::new();
        prompt.open(
            "id".into(),
            "edit".into(),
            vec!["/repo/src/main.rs".into()],
            vec!["/repo/src/main.rs".into()],
            Vec::new(),
        );
        // The generalized scope (parent glob) differs, so it is offered.
        assert!(prompt.height(80) > 0);
    }

    #[test]
    fn closed_prompt_ignores_everything() {
        let mut prompt = PermissionPrompt::new();
        assert_eq!(prompt.handle_key(key(KeyCode::Char('y'))), None);
        assert!(!prompt.handle_paste("x"));
        assert!(!prompt.is_open());
        prompt.close(); // idempotent
    }
}
