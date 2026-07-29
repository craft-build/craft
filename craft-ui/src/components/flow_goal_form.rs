use crate::components::form::{render_form, selected_prefix};
use crate::components::hint_line;
use crate::components::keybindings::{ActionId, KeybindingResolver, key};
use crate::theme;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::sync::Arc;

const FORM_LABEL: &str = " Flow goal ";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINT_PAIRS: &[(&str, &str)] = &[
    ("↑↓", "select"),
    ("Enter", "confirm"),
    (DISMISS_KEYS, "dismiss"),
];

struct MenuItem {
    label: &'static str,
    desc: &'static str,
    action: fn() -> FlowGoalFormAction,
}

const MENU: &[MenuItem] = &[
    MenuItem {
        label: "Approve goal",
        desc: "  Resume the pipeline at plan",
        action: || FlowGoalFormAction::Approve,
    },
    MenuItem {
        label: "Refine goal",
        desc: "  Dismiss and type a revised goal",
        action: || FlowGoalFormAction::Hide,
    },
    MenuItem {
        label: "Cancel run",
        desc: "  Cancel the flow run",
        action: || FlowGoalFormAction::Cancel,
    },
];

// 2 borders + 1 empty line + 1 hint bar
const CHROME_LINES: u16 = 4;
const FORM_HEIGHT: u16 = MENU.len() as u16 + CHROME_LINES;

#[derive(Debug, PartialEq)]
pub enum FlowGoalFormAction {
    Consumed,
    Passthrough,
    Approve,
    Cancel,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

/// Bottom-anchored selector shown at the Flow goal-approval gate. The goal
/// doc itself is already in the chat transcript, so the form only collects
/// the approve / refine / cancel decision, mirroring `PlanForm`.
pub struct FlowGoalForm {
    visibility: Visibility,
    selected: usize,
    keybindings: Arc<KeybindingResolver>,
}

impl FlowGoalForm {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            selected: 0,
            keybindings: Arc::new(KeybindingResolver::new()),
        }
    }

    pub fn set_keybindings(&mut self, resolver: Arc<KeybindingResolver>) {
        self.keybindings = resolver;
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    pub fn show(&mut self) {
        self.visibility = Visibility::Shown;
        self.selected = 0;
    }

    pub fn toggle(&mut self) {
        self.visibility = if self.is_visible() {
            Visibility::UserDismissed
        } else {
            self.selected = 0;
            Visibility::Shown
        };
    }

    pub fn hide(&mut self) {
        if self.is_visible() {
            self.visibility = Visibility::UserDismissed;
        }
    }

    pub fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.selected = 0;
    }

    pub fn hint_line(&self) -> Option<Line<'static>> {
        if self.visibility != Visibility::UserDismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(" awaiting goal ", Style::new().fg(t.foreground)),
            Span::styled(key::PLAN_TOGGLE.label, t.keybind_key),
            Span::raw(" "),
        ]))
    }

    pub fn height(&self) -> u16 {
        if self.is_visible() { FORM_HEIGHT } else { 0 }
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> FlowGoalFormAction {
        if self.keybindings.matches(ActionId::Quit, key_event)
            || key_event.code == KeyCode::Esc
            || self.keybindings.matches(ActionId::PlanToggle, key_event)
        {
            return FlowGoalFormAction::Hide;
        }
        match key_event.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                FlowGoalFormAction::Consumed
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(MENU.len() - 1);
                FlowGoalFormAction::Consumed
            }
            KeyCode::Enter => (MENU[self.selected].action)(),
            KeyCode::Tab => FlowGoalFormAction::Passthrough,
            _ => FlowGoalFormAction::Consumed,
        }
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            return;
        }

        let t = theme::current();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(MENU.len() + 1);

        for (i, item) in MENU.iter().enumerate() {
            let (prefix, style) = selected_prefix(&t, i == self.selected);
            lines.push(Line::from(vec![
                Span::styled(prefix, t.tool_dim),
                Span::styled(item.label, style),
                Span::styled(item.desc, t.tool_dim),
            ]));
        }
        lines.push(Line::default());
        lines.push(hint_line(HINT_PAIRS));

        render_form(&t, FORM_LABEL, frame, area, lines, (0, 0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use test_case::test_case;

    const LAST: usize = MENU.len() - 1;

    #[test]
    fn show_makes_visible_and_resets_selected() {
        let mut form = FlowGoalForm::new();
        form.selected = 1;
        form.show();
        assert!(form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn toggle_cycles_visibility() {
        let mut form = FlowGoalForm::new();
        form.show();
        assert!(form.is_visible());
        form.toggle();
        assert!(!form.is_visible());
        form.toggle();
        assert!(form.is_visible());
    }

    #[test]
    fn reset_clears_state() {
        let mut form = FlowGoalForm::new();
        form.show();
        form.selected = 1;
        form.reset();
        assert!(!form.is_visible());
        assert_eq!(form.selected, 0);
    }

    #[test]
    fn hint_line_only_when_dismissed() {
        let mut form = FlowGoalForm::new();
        assert!(form.hint_line().is_none());
        form.show();
        assert!(form.hint_line().is_none());
        form.hide();
        assert!(form.hint_line().is_some());
    }

    #[test]
    fn height_reflects_visibility() {
        let mut form = FlowGoalForm::new();
        assert_eq!(form.height(), 0);
        form.show();
        assert_eq!(form.height(), FORM_HEIGHT);
        form.hide();
        assert_eq!(form.height(), 0);
    }

    #[test_case(0, KeyCode::Up,   0    ; "up_at_zero_stays")]
    #[test_case(0, KeyCode::Down, 1    ; "down_from_zero")]
    #[test_case(LAST, KeyCode::Down, LAST ; "down_at_max_stays")]
    #[test_case(LAST, KeyCode::Up, LAST - 1 ; "up_from_max")]
    fn navigation(start: usize, code: KeyCode, expected: usize) {
        let mut form = FlowGoalForm::new();
        form.show();
        form.selected = start;
        assert_eq!(form.handle_key(key(code)), FlowGoalFormAction::Consumed);
        assert_eq!(form.selected, expected);
    }

    #[test_case(0, FlowGoalFormAction::Approve ; "enter_at_0_approve")]
    #[test_case(1, FlowGoalFormAction::Hide    ; "enter_at_1_refine")]
    #[test_case(2, FlowGoalFormAction::Cancel  ; "enter_at_2_cancel")]
    fn enter_dispatches(selected: usize, expected: FlowGoalFormAction) {
        let mut form = FlowGoalForm::new();
        form.show();
        form.selected = selected;
        assert_eq!(form.handle_key(key(KeyCode::Enter)), expected);
    }

    #[test_case(key(KeyCode::Esc)              ; "esc")]
    #[test_case(key::QUIT.to_key_event()      ; "ctrl_c")]
    #[test_case(key::PLAN_TOGGLE.to_key_event(); "ctrl_t")]
    fn dismiss(k: KeyEvent) {
        let mut form = FlowGoalForm::new();
        form.show();
        assert_eq!(form.handle_key(k), FlowGoalFormAction::Hide);
    }

    #[test]
    fn unknown_key_consumed() {
        let mut form = FlowGoalForm::new();
        form.show();
        assert_eq!(
            form.handle_key(key(KeyCode::Char('x'))),
            FlowGoalFormAction::Consumed
        );
    }

    #[test]
    fn tab_passes_through() {
        let mut form = FlowGoalForm::new();
        form.show();
        assert_eq!(
            form.handle_key(key(KeyCode::Tab)),
            FlowGoalFormAction::Passthrough
        );
    }
}
