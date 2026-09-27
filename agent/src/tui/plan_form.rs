//! Todo/plan panel (F.3, task 79): Ctrl-T toggles the plan form — the
//! "Plan complete" menu shown when the agent finishes drafting its plan in
//! Plan mode — and Ctrl-O hands the plan file to `$VISUAL`/`$EDITOR`.
//! Ported from the reference `craft-ui/src/components/plan_form.rs`; the
//! data-driven keybinding resolver (task 80) is not ported yet, so the
//! dismiss and open-editor chords are matched directly.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

use crate::tui::ui::theme;

const FORM_LABEL: &str = " Plan complete ";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINT_PAIRS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("Space", "toggle parallel"),
    ("Enter", "confirm"),
    ("Ctrl+O", "edit plan"),
    (DISMISS_KEYS, "dismiss"),
];

struct MenuItem {
    label: &'static str,
    desc: &'static str,
    action: fn() -> PlanFormAction,
}

const MENU: &[MenuItem] = &[
    MenuItem {
        label: "Refine plan",
        desc: "  Dismiss and keep editing the plan",
        action: || PlanFormAction::Hide,
    },
    MenuItem {
        label: "Clear context and implement",
        desc: "  Start fresh session, then implement the plan",
        action: || PlanFormAction::ClearAndImplement,
    },
    MenuItem {
        label: "Implement plan",
        desc: "  Keep current context, implement the plan",
        action: || PlanFormAction::Implement,
    },
];

// 2 borders + 1 empty line + 1 hint bar
const CHROME_LINES: u16 = 4;
const FORM_HEIGHT: u16 = MENU.len() as u16 + CHROME_LINES;

#[derive(Debug, PartialEq)]
pub(crate) enum PlanFormAction {
    Consumed,
    Passthrough,
    ClearAndImplement,
    Implement,
    OpenEditor,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

pub(crate) struct PlanForm {
    visibility: Visibility,
    selected: usize,
    parallel: bool,
}

fn is_ctrl(key: &KeyEvent, c: char) -> bool {
    key.code == KeyCode::Char(c)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
}

fn selected_prefix(is_selected: bool) -> (&'static str, Style) {
    if is_selected {
        ("▸ ", Style::default().fg(theme::current().accent))
    } else {
        ("  ", Style::default().fg(theme::current().text_primary))
    }
}

fn hint_line(hints: &[(&str, &str)]) -> Line<'static> {
    let t = theme::current();
    Line::from(
        hints
            .iter()
            .flat_map(|(key, desc)| {
                [
                    Span::raw("  "),
                    Span::styled((*key).to_string(), Style::default().fg(t.cyan)),
                    Span::styled(format!(" {desc}"), Style::default().fg(t.text_tertiary)),
                ]
            })
            .collect::<Vec<_>>(),
    )
}

impl PlanForm {
    pub(crate) fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            selected: 0,
            parallel: false,
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    /// A finished plan draft (re)opens the form unless the user dismissed
    /// it for this plan.
    pub(crate) fn on_plan_ready(&mut self) {
        if self.visibility != Visibility::UserDismissed {
            self.visibility = Visibility::Shown;
            self.selected = 0;
        }
    }

    pub(crate) fn on_plan_drafting(&mut self) {
        self.visibility = Visibility::Hidden;
    }

    pub(crate) fn toggle(&mut self) {
        self.visibility = if self.is_visible() {
            Visibility::UserDismissed
        } else {
            self.selected = 0;
            Visibility::Shown
        };
    }

    pub(crate) fn hide(&mut self) {
        if self.is_visible() {
            self.visibility = Visibility::UserDismissed;
        }
    }

    pub(crate) fn parallel(&self) -> bool {
        self.parallel
    }

    pub(crate) fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.selected = 0;
    }

    /// One-line reminder (" Plan Ctrl+T ") for the row under the status
    /// line while a ready plan's form is dismissed.
    pub(crate) fn hint_line(&self) -> Option<Line<'static>> {
        if self.visibility != Visibility::UserDismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(" Plan ", Style::default().fg(t.text_primary)),
            Span::styled("Ctrl+T", Style::default().fg(t.cyan)),
            Span::raw(" "),
        ]))
    }

    pub(crate) fn height(&self) -> u16 {
        if self.is_visible() { FORM_HEIGHT } else { 0 }
    }

    pub(crate) fn handle_key(&mut self, key_event: KeyEvent) -> PlanFormAction {
        if key_event.code == KeyCode::Esc || is_ctrl(&key_event, 't') {
            return PlanFormAction::Hide;
        }
        if is_ctrl(&key_event, 'o') {
            return PlanFormAction::OpenEditor;
        }
        match key_event.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PlanFormAction::Consumed
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(MENU.len() - 1);
                PlanFormAction::Consumed
            }
            KeyCode::Char(' ') => {
                self.parallel = !self.parallel;
                PlanFormAction::Consumed
            }
            KeyCode::Enter => (MENU[self.selected].action)(),
            KeyCode::Tab => PlanFormAction::Passthrough,
            _ => PlanFormAction::Consumed,
        }
    }

    pub(crate) fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            return;
        }

        let t = theme::current();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(MENU.len() + 1);

        for (i, item) in MENU.iter().enumerate() {
            let (prefix, style) = selected_prefix(i == self.selected);
            let style = style.add_modifier(if i == self.selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
            let mut spans = vec![
                Span::styled(prefix, Style::default().fg(t.text_tertiary)),
                Span::styled(item.label, style),
                Span::styled(item.desc, Style::default().fg(t.text_tertiary)),
            ];
            if self.parallel {
                spans.push(Span::styled(
                    " (parallel)",
                    Style::default()
                        .fg(t.text_tertiary)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        lines.push(hint_line(HINT_PAIRS));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(t.border_strong))
            .title_top(Line::from(FORM_LABEL.to_string()).left_aligned())
            .title_style(
                Style::default()
                    .fg(t.text_primary)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().fg(t.text_primary))
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char(c))
        }
    }

    const LAST: usize = MENU.len() - 1;

    #[test]
    fn on_plan_ready_shows_and_resets_selected() {
        let mut form = PlanForm::new();
        form.selected = 1;
        form.on_plan_ready();
        assert!(form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn on_plan_ready_respects_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_ready();
        assert!(!form.is_visible());
    }

    #[test]
    fn on_plan_drafting_clears_user_dismissed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.hide();
        form.on_plan_drafting();
        form.on_plan_ready();
        assert!(
            form.is_visible(),
            "drafting should clear dismiss so next ready shows"
        );
    }

    #[test]
    fn toggle_cycles_visibility() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert!(form.is_visible());
        form.toggle();
        assert!(!form.is_visible());
        form.toggle();
        assert!(form.is_visible());
    }

    #[test]
    fn reset_clears_state() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = 1;
        form.reset();
        assert!(!form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn hint_line_only_when_dismissed() {
        let mut form = PlanForm::new();
        assert!(form.hint_line().is_none());
        form.on_plan_ready();
        assert!(form.hint_line().is_none());
        form.hide();
        assert!(form.hint_line().is_some());
    }

    #[test]
    fn height_reflects_visibility() {
        let mut form = PlanForm::new();
        assert_eq!(form.height(), 0);
        form.on_plan_ready();
        assert_eq!(form.height(), FORM_HEIGHT);
        form.hide();
        assert_eq!(form.height(), 0);
    }

    #[test]
    fn navigation() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        form.selected = 0;
        assert_eq!(form.handle_key(key(KeyCode::Up)), PlanFormAction::Consumed);
        assert_eq!(form.selected, 0, "up at zero stays");
        assert_eq!(
            form.handle_key(key(KeyCode::Down)),
            PlanFormAction::Consumed
        );
        assert_eq!(form.selected, 1);
        form.selected = LAST;
        assert_eq!(
            form.handle_key(key(KeyCode::Down)),
            PlanFormAction::Consumed
        );
        assert_eq!(form.selected, LAST, "down at max stays");
        assert_eq!(form.handle_key(key(KeyCode::Up)), PlanFormAction::Consumed);
        assert_eq!(form.selected, LAST - 1);
    }

    #[test]
    fn enter_dispatches() {
        for (selected, expected) in [
            (0, PlanFormAction::Hide),
            (1, PlanFormAction::ClearAndImplement),
            (2, PlanFormAction::Implement),
        ] {
            let mut form = PlanForm::new();
            form.on_plan_ready();
            form.selected = selected;
            assert_eq!(form.handle_key(key(KeyCode::Enter)), expected);
        }
    }

    #[test]
    fn space_toggles_parallel() {
        let mut form = PlanForm::new();
        let initial = form.parallel();
        form.on_plan_ready();
        assert_eq!(form.parallel(), initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), !initial);
        assert_eq!(
            form.handle_key(key(KeyCode::Char(' '))),
            PlanFormAction::Consumed
        );
        assert_eq!(form.parallel(), initial);
    }

    #[test]
    fn dismiss() {
        for k in [key(KeyCode::Esc), ctrl('t')] {
            let mut form = PlanForm::new();
            form.on_plan_ready();
            assert_eq!(form.handle_key(k), PlanFormAction::Hide);
        }
    }

    #[test]
    fn ctrl_o_opens_editor() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(form.handle_key(ctrl('o')), PlanFormAction::OpenEditor);
    }

    #[test]
    fn unknown_key_consumed() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Char('x'))),
            PlanFormAction::Consumed
        );
    }

    #[test]
    fn tab_passes_through() {
        let mut form = PlanForm::new();
        form.on_plan_ready();
        assert_eq!(
            form.handle_key(key(KeyCode::Tab)),
            PlanFormAction::Passthrough
        );
    }
}
