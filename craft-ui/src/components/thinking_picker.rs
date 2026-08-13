use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

use craft_providers::ThinkingConfig;
use craft_storage::sessions::Effort;

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

const TITLE: &str = " Thinking level ";
const CURRENT_SUFFIX: &str = "current";
const ADAPTIVE_DETAIL: &str = "let the model decide";
const OFF_DETAIL: &str = "disabled";

pub enum ThinkingPickerAction {
    Consumed,
    Select(ThinkingConfig),
    Close,
}

struct ThinkingChoice {
    config: ThinkingConfig,
    label: String,
    detail: Option<&'static str>,
    is_current: bool,
}

impl PickerItem for ThinkingChoice {
    fn label(&self) -> &str {
        &self.label
    }

    fn suffix(&self) -> Option<&str> {
        self.is_current.then_some(CURRENT_SUFFIX)
    }

    fn detail(&self) -> Option<&str> {
        self.detail
    }
}

pub struct ThinkingPicker {
    picker: ListPicker<ThinkingChoice>,
}

impl ThinkingPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new(),
        }
    }

    pub fn open(&mut self, current: ThinkingConfig) {
        let mut choices = Vec::with_capacity(Effort::ALL.len() + 3);
        choices.push(ThinkingChoice {
            config: ThinkingConfig::Off,
            label: ThinkingConfig::Off.to_string(),
            detail: Some(OFF_DETAIL),
            is_current: current == ThinkingConfig::Off,
        });
        choices.push(ThinkingChoice {
            config: ThinkingConfig::Adaptive,
            label: ThinkingConfig::Adaptive.to_string(),
            detail: Some(ADAPTIVE_DETAIL),
            is_current: current == ThinkingConfig::Adaptive,
        });
        for level in Effort::ALL {
            let config = ThinkingConfig::Effort(level);
            choices.push(ThinkingChoice {
                config,
                label: level.to_string(),
                detail: None,
                is_current: current == config,
            });
        }
        if let ThinkingConfig::Budget(n) = current {
            choices.push(ThinkingChoice {
                config: current,
                label: format!("budget: {n}"),
                detail: None,
                is_current: true,
            });
        }

        let preselect = choices.iter().position(|c| c.is_current).unwrap_or(0);
        self.picker.open(choices, TITLE);
        self.picker.select(preselect);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn set_keybindings(
        &mut self,
        resolver: std::sync::Arc<crate::components::keybindings::KeybindingResolver>,
    ) {
        self.picker.set_keybindings(resolver);
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ThinkingPickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed | PickerAction::Toggle(..) => ThinkingPickerAction::Consumed,
            PickerAction::Select(choice) => ThinkingPickerAction::Select(choice.config),
            PickerAction::Close => ThinkingPickerAction::Close,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }
}

impl Overlay for ThinkingPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crossterm::event::KeyCode;

    #[test]
    fn open_preselects_current_off() {
        let mut p = ThinkingPicker::new();
        p.open(ThinkingConfig::Off);
        assert_eq!(
            p.picker.selected_item().unwrap().config,
            ThinkingConfig::Off
        );
    }

    #[test]
    fn open_preselects_current_effort() {
        let mut p = ThinkingPicker::new();
        p.open(ThinkingConfig::Effort(Effort::High));
        assert_eq!(
            p.picker.selected_item().unwrap().config,
            ThinkingConfig::Effort(Effort::High)
        );
    }

    #[test]
    fn open_preselects_current_budget_entry() {
        let mut p = ThinkingPicker::new();
        p.open(ThinkingConfig::Budget(4096));
        assert_eq!(
            p.picker.selected_item().unwrap().config,
            ThinkingConfig::Budget(4096)
        );
    }

    #[test]
    fn select_emits_chosen_config() {
        let mut p = ThinkingPicker::new();
        p.open(ThinkingConfig::Off);
        p.handle_key(key(KeyCode::Down));
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            action,
            ThinkingPickerAction::Select(ThinkingConfig::Adaptive)
        ));
        assert!(!p.is_open());
    }

    #[test]
    fn cancel_closes_without_select() {
        let mut p = ThinkingPicker::new();
        p.open(ThinkingConfig::Off);
        let action = p.handle_key(key(KeyCode::Esc));
        assert!(matches!(action, ThinkingPickerAction::Close));
        assert!(!p.is_open());
    }
}
