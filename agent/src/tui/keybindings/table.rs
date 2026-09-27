//! The compile-time defaults table: [`Bind`], the `key` constants, and the
//! [`KEYBINDS`] help-table rows.

use super::help::KeyLabel;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One concrete chord: code + exact modifiers + a display label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bind {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
    pub label: &'static str,
}

impl Bind {
    pub fn matches(&self, key: KeyEvent) -> bool {
        key.code == self.code && key.modifiers == self.modifiers
    }

    #[cfg(test)]
    pub const fn to_key_event(self) -> KeyEvent {
        KeyEvent {
            code: self.code,
            modifiers: self.modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }
}

const fn ctrl(c: char, upper: &'static str) -> Bind {
    Bind {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::CONTROL,
        label: upper,
    }
}

/// Compile-time default chords, one const per action. Dispatch and the help
/// modal both read these through [`super::ActionId::default_binds`] so the
/// overlay can replace them wholesale.
pub mod key {
    use super::{Bind, ctrl};
    use crossterm::event::{KeyCode, KeyModifiers};

    pub const QUIT: Bind = ctrl('c', "Ctrl+C");
    pub const QUIT_ALT: Bind = ctrl('q', "Ctrl+Q");
    pub const HELP: Bind = ctrl('h', "Ctrl+H");
    pub const PALETTE: Bind = ctrl('p', "Ctrl+P");
    pub const SIDEBAR: Bind = ctrl('b', "Ctrl+B");
    pub const MODEL_MENU: Bind = ctrl('l', "Ctrl+L");
    pub const SEARCH: Bind = ctrl('f', "Ctrl+F");
    pub const FILE_PICKER: Bind = ctrl('s', "Ctrl+S");
    pub const PLAN_TOGGLE: Bind = ctrl('t', "Ctrl+T");
    pub const OPEN_EDITOR: Bind = ctrl('o', "Ctrl+O");
    pub const SCROLL_HALF_UP: Bind = ctrl('u', "Ctrl+U");
    pub const SCROLL_HALF_DOWN: Bind = ctrl('d', "Ctrl+D");
    pub const APPROVE_DIFF: Bind = ctrl('y', "Ctrl+Y");
    pub const APPROVE_DIFF_ALWAYS: Bind = Bind {
        code: KeyCode::Char('y'),
        modifiers: KeyModifiers::CONTROL.union(KeyModifiers::SHIFT),
        label: "Ctrl+Shift+Y",
    };
    /// Terminals that report the shifted letter without the SHIFT bit.
    pub const APPROVE_DIFF_ALWAYS_LEGACY: Bind = Bind {
        code: KeyCode::Char('Y'),
        modifiers: KeyModifiers::CONTROL,
        label: "Ctrl+Shift+Y",
    };
    pub const REJECT_DIFF: Bind = ctrl('n', "Ctrl+N");
    pub const DELETE_WORD: Bind = ctrl('w', "Ctrl+W");
    pub const KILL_LINE: Bind = ctrl('k', "Ctrl+K");
    pub const LINE_START: Bind = ctrl('a', "Ctrl+A");
    pub const LINE_END: Bind = ctrl('e', "Ctrl+E");
    pub const EDIT_INPUT: Bind = Bind {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+O",
    };
    pub const EFFORT_CYCLE: Bind = Bind {
        code: KeyCode::Char('e'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+E",
    };
    /// Clipboard image paste (F.6). Terminals without bracketed-paste
    /// image support deliver screenshots via the OSC 52-style clipboard,
    /// so the chord reads the clipboard directly.
    pub const PASTE_IMAGE: Bind = ctrl('v', "Ctrl+V");
}

/// A help-sheet section. Children render nested under their parent, sharing
/// its generic picker bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeybindContext {
    General,
    Editing,
    Streaming,
    Picker,
    CommandPalette,
    Search,
    FilePicker,
    ModelPicker,
    ThemePicker,
    Sessions,
}

const ALL_CONTEXTS: &[KeybindContext] = &[
    KeybindContext::General,
    KeybindContext::Editing,
    KeybindContext::Streaming,
    KeybindContext::Picker,
    KeybindContext::CommandPalette,
    KeybindContext::Search,
    KeybindContext::FilePicker,
    KeybindContext::ModelPicker,
    KeybindContext::ThemePicker,
    KeybindContext::Sessions,
];

pub fn all_contexts() -> impl Iterator<Item = KeybindContext> {
    ALL_CONTEXTS.iter().copied()
}

impl KeybindContext {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Streaming => "While Streaming",
            Self::Picker => "Pickers",
            Self::CommandPalette => "Command Palette",
            Self::Search => "Search",
            Self::FilePicker => "File Picker",
            Self::ModelPicker => "Model Picker",
            Self::ThemePicker => "Theme Picker",
            Self::Sessions => "Sessions",
        }
    }

    pub const fn parent(self) -> Option<KeybindContext> {
        match self {
            Self::CommandPalette
            | Self::Search
            | Self::FilePicker
            | Self::ModelPicker
            | Self::ThemePicker
            | Self::Sessions => Some(Self::Picker),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    All,
    #[allow(dead_code)] // kept for parity: platform-gated rows (e.g. Alt+Del)
    MacOnly,
    UnixOnly,
}

impl Platform {
    pub const fn is_visible(self) -> bool {
        match self {
            Self::All => true,
            Self::MacOnly => cfg!(target_os = "macos"),
            Self::UnixOnly => cfg!(unix),
        }
    }
}

/// One row of the help table. `action_id: None` marks a fixed (non-remappable)
/// key the table still documents (Enter, Tab, arrows, ...).
pub struct Keybind {
    pub action_id: Option<super::ActionId>,
    pub label: KeyLabel,
    pub description: &'static str,
    pub context: KeybindContext,
    pub platform: Platform,
}

pub const KEYBINDS: &[Keybind] = &[
    Keybind {
        action_id: Some(super::ActionId::Quit),
        label: KeyLabel::Single(key::QUIT.label),
        description: "Quit / clear input / interrupt",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::Palette),
        label: KeyLabel::Single(key::PALETTE.label),
        description: "Command palette",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::Help),
        label: KeyLabel::Single(key::HELP.label),
        description: "Show keybindings",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::Sidebar),
        label: KeyLabel::Single(key::SIDEBAR.label),
        description: "Toggle context panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::ModelMenu),
        label: KeyLabel::Single(key::MODEL_MENU.label),
        description: "Model menu",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::Search),
        label: KeyLabel::Single(key::SEARCH.label),
        description: "Search the transcript",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::FilePicker),
        label: KeyLabel::Single(key::FILE_PICKER.label),
        description: "File picker",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::PlanToggle),
        label: KeyLabel::Single(key::PLAN_TOGGLE.label),
        description: "Toggle todo / plan panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::OpenEditor),
        label: KeyLabel::Single(key::OPEN_EDITOR.label),
        description: "Open plan in editor",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Ctrl+Z"),
        description: "Suspend process",
        context: KeybindContext::General,
        platform: Platform::UnixOnly,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Submit prompt",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::MacMulti(
            &["Shift+Enter", "Ctrl+Enter", "Ctrl+J", "Alt+Enter"],
            &["⇧↵", "⌃↵", "⌃J", "⌥↵"],
        ),
        description: "Newline",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Tab"),
        description: "Cycle mode / focus next tool card",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("/command"),
        description: "Slash-command popup",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::DeleteWord),
        label: KeyLabel::MacAlt(key::DELETE_WORD.label, "⌥⌫"),
        description: "Delete word backward",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::MacAlt("Ctrl+Left / Ctrl+Right", "⌥← / ⌥→"),
        description: "Move word left / right",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::KillLine),
        label: KeyLabel::Single(key::KILL_LINE.label),
        description: "Delete to end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::LineStart),
        label: KeyLabel::Single(key::LINE_START.label),
        description: "Jump to start of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::LineEnd),
        label: KeyLabel::Single(key::LINE_END.label),
        description: "Jump to end of line / bottom",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::CycleEffort),
        label: KeyLabel::Single(key::EFFORT_CYCLE.label),
        description: "Cycle reasoning effort",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::EditInput),
        label: KeyLabel::Single(key::EDIT_INPUT.label),
        description: "Edit input in external editor",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::PasteImage),
        label: KeyLabel::Single(key::PASTE_IMAGE.label),
        description: "Attach clipboard image",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::ScrollHalfUp),
        label: KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        description: "Scroll half page up / down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::ApproveDiff),
        label: KeyLabel::Alt(key::APPROVE_DIFF.label, key::REJECT_DIFF.label),
        description: "Approve / reject pending edit",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(super::ActionId::ApproveDiffAlways),
        label: KeyLabel::Single(key::APPROVE_DIFF_ALWAYS.label),
        description: "Approve pending edit, always",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt("↑", "↓"),
        description: "Recall input history",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt("g", "G"),
        description: "Jump to top / bottom (empty composer)",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("PageUp / PageDown"),
        description: "Scroll the transcript",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Esc"),
        description: "Close menu / interrupt the turn",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Esc Esc"),
        description: "Interrupt a running turn",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt("PageUp", "PageDown"),
        description: "Scroll page up / down",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        description: "Scroll half page up / down",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Select",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Type"),
        description: "Filter",
        context: KeybindContext::Picker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Tab"),
        description: "Complete command",
        context: KeybindContext::CommandPalette,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Jump to match",
        context: KeybindContext::Search,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Insert path into composer",
        context: KeybindContext::FilePicker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("!/@/#/$"),
        description: "Set tier (strong/medium/weak/compaction)",
        context: KeybindContext::ModelPicker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Apply theme, Esc restores",
        context: KeybindContext::ThemePicker,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Load session",
        context: KeybindContext::Sessions,
        platform: Platform::All,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn bind_requires_exact_modifiers() {
        let bind = key::OPEN_EDITOR; // Ctrl+O
        let exact = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        let extra = KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let wrong = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT);

        assert!(bind.matches(exact));
        assert!(!bind.matches(extra), "extra modifiers should not match");
        assert!(!bind.matches(wrong), "wrong modifier should not match");
    }

    #[test]
    fn every_context_has_at_least_one_keybind() {
        for ctx in all_contexts() {
            let has_own = KEYBINDS.iter().any(|kb| kb.context == ctx);
            let has_parent = ctx
                .parent()
                .is_some_and(|p| KEYBINDS.iter().any(|kb| kb.context == p));
            assert!(
                has_own || has_parent,
                "context {:?} has no keybinds and no parent with keybinds",
                ctx,
            );
        }
    }

    #[test]
    fn no_duplicate_entries() {
        for (i, a) in KEYBINDS.iter().enumerate() {
            for (j, b) in KEYBINDS.iter().enumerate() {
                if i != j && a.context == b.context {
                    assert!(
                        a.label.flat_str() != b.label.flat_str() || a.description != b.description,
                        "duplicate keybind: {} - {} in {:?}",
                        a.label.flat_str(),
                        a.description,
                        a.context,
                    );
                }
            }
        }
    }
}
