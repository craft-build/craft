//! Question form (A.5): a bottom, non-modal overlay the `question` tool
//! parks on, ported from the reference `plugins/question/question_form.lua`.
//! One tab per question; per tab a cursor over the predefined options plus
//! the always-present "Type your own answer" row. Single-select submits on
//! pick (a confirm stage appears when there is more than one question or
//! the question is multi-select, matching the reference); multi-select
//! toggles; Esc/Ctrl-C dismisses.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::tui::ui::theme;

use super::permission_prompt::PromptBuffer;
use crate::tools::{QuestionAnswer, QuestionSpec};

const CUSTOM_OPTION: &str = "Type your own answer";
const DESC_SEP: &str = " — ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Selecting,
    EditingCustom,
    Confirming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    None,
    Dismiss,
}

pub struct FormState {
    mode: Mode,
    /// Zero-based active question.
    tab: usize,
    /// Zero-based cursor: options, then the custom row.
    cursor: usize,
    /// One list of picked labels per question; empty slot = unanswered.
    answers: Vec<Vec<String>>,
    buffer: PromptBuffer,
}

impl FormState {
    fn has_confirm(&self, questions: &[QuestionSpec]) -> bool {
        questions.len() > 1 || questions.first().is_some_and(|q| q.multi_select)
    }

    fn active<'a>(&self, questions: &'a [QuestionSpec]) -> &'a QuestionSpec {
        &questions[self.tab]
    }

    fn is_selected(&self, label: &str) -> bool {
        self.answers
            .get(self.tab)
            .is_some_and(|ans| ans.iter().any(|v| v == label))
    }

    fn is_predefined(&self, question: &QuestionSpec, label: &str) -> bool {
        question.options.iter().any(|o| o.label == label)
    }

    fn find_custom(&self, question: &QuestionSpec) -> Option<usize> {
        self.answers
            .get(self.tab)?
            .iter()
            .position(|v| !self.is_predefined(question, v))
    }

    fn toggle(&mut self, label: &str) {
        let ans = &mut self.answers[self.tab];
        if let Some(i) = ans.iter().position(|v| v == label) {
            ans.remove(i);
        } else {
            ans.push(label.to_string());
        }
    }

    fn goto_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.cursor = 0;
        self.mode = Mode::Selecting;
    }

    fn next_tab(&mut self, questions: &[QuestionSpec]) {
        if self.tab + 1 < questions.len() {
            self.goto_tab(self.tab + 1);
        } else {
            self.mode = Mode::Confirming;
        }
    }

    /// The reference's `advance`: single-select pick (or custom submit)
    /// either jumps to the next tab or submits the whole form, depending
    /// on whether a confirm stage exists.
    fn advance(&mut self, questions: &[QuestionSpec]) -> Outcome {
        if self.has_confirm(questions) {
            self.next_tab(questions);
            Outcome::None
        } else {
            Outcome::Dismiss // caller maps this to Submit; see `handle_key`
        }
    }
}

pub enum QuestionForm {
    Closed,
    Open {
        /// Question-request id the answer is routed to.
        id: String,
        questions: Vec<QuestionSpec>,
        state: FormState,
    },
}

impl QuestionForm {
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

    pub fn open(&mut self, id: String, questions: Vec<QuestionSpec>) {
        if questions.is_empty() {
            return;
        }
        let state = FormState {
            mode: Mode::Selecting,
            tab: 0,
            cursor: 0,
            answers: vec![Vec::new(); questions.len()],
            buffer: PromptBuffer::default(),
        };
        *self = Self::Open {
            id,
            questions,
            state,
        };
    }

    pub fn close(&mut self) {
        *self = Self::Closed;
    }

    /// Handle a key. `Some(answer)` closes the form: either a submitted
    /// set of picks or a dismissal. Ctrl-C dismisses from any state.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<QuestionAnswer> {
        let Self::Open {
            questions, state, ..
        } = self
        else {
            return None;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(dismissed());
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None; // global chords keep working
        }
        match state.mode {
            Mode::EditingCustom => match key.code {
                KeyCode::Enter => state.submit_custom(questions),
                KeyCode::Esc => {
                    state.mode = Mode::Selecting;
                    None
                }
                _ => {
                    state.buffer.handle_key(key);
                    None
                }
            },
            Mode::Confirming => match key.code {
                KeyCode::Enter => Some(submit(state)),
                KeyCode::Left => {
                    state.goto_tab(questions.len() - 1);
                    None
                }
                KeyCode::Esc => Some(dismissed()),
                _ => None,
            },
            Mode::Selecting => {
                let rows = questions[state.tab].options.len() + 1;
                match key.code {
                    KeyCode::Up => {
                        state.cursor = state.cursor.saturating_sub(1);
                        None
                    }
                    KeyCode::Down => {
                        if state.cursor + 1 < rows {
                            state.cursor += 1;
                        }
                        None
                    }
                    KeyCode::Tab | KeyCode::Right if state.has_confirm(questions) => {
                        state.next_tab(questions);
                        None
                    }
                    KeyCode::BackTab if state.has_confirm(questions) => {
                        if state.tab > 0 {
                            state.goto_tab(state.tab - 1);
                        }
                        None
                    }
                    KeyCode::Left if state.has_confirm(questions) => {
                        if state.tab > 0 {
                            state.goto_tab(state.tab - 1);
                        }
                        None
                    }
                    KeyCode::Esc => Some(dismissed()),
                    KeyCode::Enter => {
                        let q = state.active(questions);
                        if state.cursor == q.options.len() {
                            // Custom row: seed the editor with any existing
                            // custom answer.
                            state.mode = Mode::EditingCustom;
                            state.buffer = PromptBuffer::default();
                            if let Some(existing) = state
                                .find_custom(q)
                                .map(|i| state.answers[state.tab][i].clone())
                            {
                                state.buffer.insert_text(&existing);
                            }
                            None
                        } else {
                            let label = q.options[state.cursor].label.clone();
                            if q.multi_select {
                                state.toggle(&label);
                                None
                            } else {
                                state.answers[state.tab] = vec![label];
                                match state.advance(questions) {
                                    // With no confirm stage the form submits
                                    // immediately; `Dismiss` is reused as the
                                    // "done" signal.
                                    Outcome::Dismiss => Some(submit(state)),
                                    Outcome::None => None,
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
        }
    }

    /// Paste lands in the custom-answer buffer; only consumed while editing.
    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Self::Open { state, .. } = self else {
            return false;
        };
        if state.mode == Mode::EditingCustom {
            state.buffer.insert_text(text);
            return true;
        }
        false
    }

    fn build_lines(&self) -> Vec<Line<'static>> {
        let Self::Open {
            questions, state, ..
        } = self
        else {
            return vec![];
        };
        let t = theme::current();
        let dim = Style::default().fg(t.text_tertiary);
        let primary = Style::default().fg(t.text_primary);
        let success = Style::default().fg(t.success);

        let mut lines = vec![Line::raw("")];
        // Tab bar (only meaningful with a confirm stage; still shows
        // answered state at a glance for multi-question forms).
        if questions.len() > 1 {
            let mut spans = Vec::new();
            for (i, q) in questions.iter().enumerate() {
                let label = q
                    .header
                    .clone()
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| format!("Q{}", i + 1));
                let answered = state.answers.get(i).is_some_and(|a| !a.is_empty());
                let style = if i == state.tab && state.mode != Mode::Confirming {
                    Style::default()
                        .fg(t.text_primary)
                        .add_modifier(Modifier::BOLD)
                } else if answered {
                    success
                } else {
                    dim
                };
                spans.push(Span::styled(format!(" {label} "), style));
            }
            lines.push(Line::from(spans));
            lines.push(Line::raw(""));
        }

        if state.mode == Mode::Confirming {
            lines.push(Line::from(Span::styled(
                " Submit answers?".to_string(),
                primary.add_modifier(Modifier::BOLD),
            )));
            for (i, _q) in questions.iter().enumerate() {
                let picks = state
                    .answers
                    .get(i)
                    .filter(|a| !a.is_empty())
                    .map(|a| a.join(", "))
                    .unwrap_or_else(|| "(no answer)".into());
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("Q{}: ", i + 1), dim),
                    Span::styled(picks, primary),
                ]));
            }
            lines.push(hint_line(&[
                ("Enter", "Submit"),
                ("←", "Back"),
                ("Esc", "Dismiss"),
            ]));
            lines.push(Line::raw(""));
            return lines;
        }

        let q = state.active(questions);
        lines.push(Line::from(Span::styled(
            format!(" Q{}. {}", state.tab + 1, q.question),
            primary.add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::raw(""));

        for (i, option) in q.options.iter().enumerate() {
            let cursor_row = i == state.cursor && state.mode == Mode::Selecting;
            let picked = state.is_selected(&option.label);
            let mut spans = vec![Span::raw("  ")];
            spans.push(Span::styled(
                if cursor_row { "❯ " } else { "  " },
                Style::default().fg(t.cyan),
            ));
            spans.push(Span::styled(if picked { "✓ " } else { "  " }, success));
            spans.push(Span::styled(
                option.label.clone(),
                if picked { success } else { primary },
            ));
            if let Some(description) = &option.description {
                spans.push(Span::styled(format!("{DESC_SEP}{description}"), dim));
            }
            lines.push(Line::from(spans));
        }

        let custom_row = q.options.len();
        if state.mode == Mode::EditingCustom {
            let text = state.buffer.value();
            let cursor = state.buffer.cursor_char();
            let chars: Vec<char> = text.chars().collect();
            let mut spans = vec![
                Span::raw("  "),
                Span::styled("❯ ", Style::default().fg(t.cyan)),
                Span::styled("✓ ", success),
            ];
            spans.push(Span::raw(chars.iter().take(cursor).collect::<String>()));
            spans.push(Span::styled(
                chars
                    .get(cursor)
                    .map(|c| c.to_string())
                    .unwrap_or(" ".into()),
                Style::default().add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::raw(chars.iter().skip(cursor + 1).collect::<String>()));
            lines.push(Line::from(spans));
            lines.push(hint_line(&[("Enter", "Save answer"), ("Esc", "Cancel")]));
        } else {
            let existing = state
                .find_custom(q)
                .map(|i| state.answers[state.tab][i].clone());
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    if custom_row == state.cursor {
                        "❯ "
                    } else {
                        "  "
                    },
                    Style::default().fg(t.cyan),
                ),
                Span::styled("✓ ", success),
            ];
            match existing {
                Some(text) => spans.push(Span::styled(text, success)),
                None => spans.push(Span::styled(CUSTOM_OPTION.to_string(), dim)),
            }
            lines.push(Line::from(spans));
            lines.push(hint_line(&[
                ("↑↓", "Move"),
                ("Enter", if q.multi_select { "Toggle" } else { "Select" }),
                ("Tab", "Next"),
                ("Esc", "Dismiss"),
            ]));
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
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(t.bg_raised)),
            area,
        );
    }

    pub fn height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let rows: usize = self
            .build_lines()
            .iter()
            .map(|line| line.width().div_ceil(inner_width).max(1))
            .sum();
        rows as u16 + 1
    }
}

impl Default for QuestionForm {
    fn default() -> Self {
        Self::new()
    }
}

fn dismissed() -> QuestionAnswer {
    QuestionAnswer {
        dismissed: true,
        answers: vec![],
    }
}

fn submit(state: &FormState) -> QuestionAnswer {
    QuestionAnswer {
        dismissed: false,
        answers: state.answers.clone(),
    }
}

impl FormState {
    /// Enter in the custom editor: empty text removes any existing custom
    /// answer; otherwise the text replaces/creates it. Single-select then
    /// advances like a pick. `None` keeps the form open.
    fn submit_custom(&mut self, questions: &[QuestionSpec]) -> Option<QuestionAnswer> {
        let text = self.buffer.value().trim().to_string();
        let q = &questions[self.tab];
        if text.is_empty() {
            if let Some(i) = self.find_custom(q) {
                self.answers[self.tab].remove(i);
            }
            self.mode = Mode::Selecting;
            return None;
        }
        match self.find_custom(q) {
            Some(i) => self.answers[self.tab][i] = text,
            None => self.answers[self.tab].push(text),
        }
        self.mode = Mode::Selecting;
        if q.multi_select {
            None
        } else {
            match self.advance(questions) {
                Outcome::Dismiss => Some(submit(self)),
                Outcome::None => None,
            }
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::QuestionOption;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn spec(multi: bool, options: &[&str]) -> QuestionSpec {
        QuestionSpec {
            question: "Which?".into(),
            header: None,
            options: options
                .iter()
                .map(|l| QuestionOption {
                    label: (*l).into(),
                    description: None,
                })
                .collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn single_select_submits_immediately() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(false, &["A", "B"])]);
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["A".to_string()]]);
    }

    #[test]
    fn multi_select_toggles_and_confirms() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(true, &["A", "B"])]);
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        form.handle_key(key(KeyCode::Down));
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        // Toggles do not advance; Tab reaches the confirm stage, and Enter
        // there submits both picks (reference semantics).
        form.handle_key(key(KeyCode::Tab));
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert_eq!(answer.answers, vec![vec!["A".to_string(), "B".to_string()]]);
    }

    #[test]
    fn multi_select_deselects_on_second_toggle() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(true, &["A", "B"])]);
        form.handle_key(key(KeyCode::Enter));
        form.handle_key(key(KeyCode::Enter));
        form.handle_key(key(KeyCode::Tab));
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert_eq!(answer.answers, vec![Vec::<String>::new()]);
    }

    #[test]
    fn multiple_questions_walk_tabs_then_confirm() {
        let mut form = QuestionForm::new();
        form.open(
            "q1".into(),
            vec![spec(false, &["A", "B"]), spec(false, &["C", "D"])],
        );
        // Q1 pick advances to Q2 instead of submitting.
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        form.handle_key(key(KeyCode::Down));
        // Q2 pick lands on confirming.
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert_eq!(
            answer.answers,
            vec![vec!["A".to_string()], vec!["D".to_string()]]
        );
    }

    #[test]
    fn back_tab_returns_to_the_previous_question() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(false, &["A"]), spec(false, &["C"])]);
        form.handle_key(key(KeyCode::Enter)); // Q1 done, on Q2
        form.handle_key(key(KeyCode::Left));
        // Back on Q1: the pick re-answers A and advances to Q2 again,
        // whose pick lands on confirming.
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert_eq!(
            answer.answers,
            vec![vec!["A".to_string()], vec!["C".to_string()]]
        );
    }

    #[test]
    fn custom_answer_round_trips_and_edits() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(false, &["A"])]);
        form.handle_key(key(KeyCode::Down)); // custom row
        form.handle_key(key(KeyCode::Enter)); // start editing
        form.handle_paste("my own");
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert_eq!(answer.answers, vec![vec!["my own".to_string()]]);

        // Multi-select custom stays in the form until confirmed.
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(true, &["A"])]);
        form.handle_key(key(KeyCode::Down));
        form.handle_key(key(KeyCode::Enter));
        form.handle_key(key(KeyCode::Char('x')));
        assert!(form.handle_key(key(KeyCode::Enter)).is_none(), "stays open");
        // Reopen the editor: seeded with the saved custom text.
        form.handle_key(key(KeyCode::Down));
        form.handle_key(key(KeyCode::Enter));
        form.handle_paste(" and more");
        assert!(
            form.handle_key(key(KeyCode::Enter)).is_none(),
            "still editing"
        );
        form.handle_key(key(KeyCode::Tab));
        let answer = form
            .handle_key(key(KeyCode::Enter))
            .expect("confirm submits");
        assert_eq!(answer.answers, vec![vec!["x and more".to_string()]]);
    }

    #[test]
    fn empty_custom_clears_the_custom_answer() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(true, &["A"])]);
        form.handle_key(key(KeyCode::Down));
        form.handle_key(key(KeyCode::Enter));
        form.handle_key(key(KeyCode::Char('z')));
        form.handle_key(key(KeyCode::Enter)); // saved, back to selecting (custom row)
        form.handle_key(key(KeyCode::Up));
        form.handle_key(key(KeyCode::Enter)); // toggle A on
        // Re-edit custom (seeded with "z"), clear it: custom removed, A stays.
        form.handle_key(key(KeyCode::Down));
        form.handle_key(key(KeyCode::Enter));
        form.handle_key(key(KeyCode::Backspace));
        assert!(
            form.handle_key(key(KeyCode::Enter)).is_none(),
            "back to selecting"
        );
        form.handle_key(key(KeyCode::Tab));
        let answer = form
            .handle_key(key(KeyCode::Enter))
            .expect("confirm submits");
        assert_eq!(answer.answers, vec![vec!["A".to_string()]]);
    }

    #[test]
    fn esc_dismisses_and_ctrl_c_dismisses_from_editing() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(false, &["A"])]);
        let answer = form.handle_key(key(KeyCode::Esc)).expect("dismisses");
        assert!(answer.dismissed);

        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(false, &["A"])]);
        form.handle_key(key(KeyCode::Down));
        form.handle_key(key(KeyCode::Enter));
        assert!(form.handle_key(ctrl_c()).unwrap().dismissed);
    }

    #[test]
    fn esc_from_custom_editing_returns_to_selecting() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![spec(false, &["A"])]);
        form.handle_key(key(KeyCode::Down));
        form.handle_key(key(KeyCode::Enter));
        form.handle_key(key(KeyCode::Char('q')));
        assert!(form.handle_key(key(KeyCode::Esc)).is_none());
        // Still open, selecting: cursor was on the custom row; move up to A
        // and submit.
        form.handle_key(key(KeyCode::Up));
        let answer = form.handle_key(key(KeyCode::Enter)).expect("submits");
        assert_eq!(answer.answers, vec![vec!["A".to_string()]]);
    }

    #[test]
    fn closed_form_ignores_everything() {
        let mut form = QuestionForm::new();
        assert!(form.handle_key(key(KeyCode::Enter)).is_none());
        assert!(!form.handle_paste("x"));
        form.close();
    }

    #[test]
    fn open_with_no_questions_stays_closed() {
        let mut form = QuestionForm::new();
        form.open("q1".into(), vec![]);
        assert!(!form.is_open());
    }
}
