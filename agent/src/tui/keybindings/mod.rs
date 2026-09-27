//! Data-driven keybindings (F.1): a compile-time `KEYBINDS` table plus a
//! user-rebindable overlay resolved by [`KeybindingResolver`]. Ported from
//! the reference `craft-ui/src/components/keybindings.rs`, adapted to this
//! repo's action set (the reference's chat-queue / rewind / task-chat keys
//! are not ported because those subsystems do not exist here yet).
//!
//! Layout: [`table`] holds [`Bind`] and the defaults/`KEYBINDS` table,
//! [`parse`] parses user chord strings, [`help`] renders the help modal,
//! and this file holds [`ActionId`], [`KeybindingResolver`], and key
//! normalization.
//!
//! Note on memory: `parse_chord` intentionally `Box::leak`s each rendered
//! label. That is fine — chord strings are parsed only at config-load
//! volume (a handful per action), and the leaked `&'static str` labels must
//! live for the process lifetime since [`Bind`] is `Copy` and holds no
//! lifetime parameter.

mod help;
mod parse;
mod table;

// The full pre-split public surface is re-exported for path stability;
// some items are currently unused outside the module tree.
#[allow(unused_imports)]
pub use help::{ALT_SEP, KeyLabel, ResolvedLabel, effective_label, help_lines};
#[allow(unused_imports)]
pub use parse::parse_chord;
#[allow(unused_imports)]
pub use table::{Bind, KEYBINDS, Keybind, KeybindContext, Platform, key};

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Every remappable action in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    Quit,
    Help,
    Palette,
    Sidebar,
    ModelMenu,
    Search,
    FilePicker,
    PlanToggle,
    OpenEditor,
    ScrollHalfUp,
    ScrollHalfDown,
    ApproveDiff,
    ApproveDiffAlways,
    RejectDiff,
    DeleteWord,
    KillLine,
    LineStart,
    LineEnd,
    EditInput,
    CycleEffort,
    PasteImage,
}

const ALL_ACTION_IDS: &[ActionId] = &[
    ActionId::Quit,
    ActionId::Help,
    ActionId::Palette,
    ActionId::Sidebar,
    ActionId::ModelMenu,
    ActionId::Search,
    ActionId::FilePicker,
    ActionId::PlanToggle,
    ActionId::OpenEditor,
    ActionId::ScrollHalfUp,
    ActionId::ScrollHalfDown,
    ActionId::ApproveDiff,
    ActionId::ApproveDiffAlways,
    ActionId::RejectDiff,
    ActionId::DeleteWord,
    ActionId::KillLine,
    ActionId::LineStart,
    ActionId::LineEnd,
    ActionId::EditInput,
    ActionId::CycleEffort,
    ActionId::PasteImage,
];

pub fn all_action_ids() -> impl Iterator<Item = ActionId> {
    ALL_ACTION_IDS.iter().copied()
}

impl ActionId {
    pub const fn snake(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Help => "help",
            Self::Palette => "palette",
            Self::Sidebar => "sidebar",
            Self::ModelMenu => "model_menu",
            Self::Search => "search",
            Self::FilePicker => "file_picker",
            Self::PlanToggle => "plan_toggle",
            Self::OpenEditor => "open_editor",
            Self::ScrollHalfUp => "scroll_half_up",
            Self::ScrollHalfDown => "scroll_half_down",
            Self::ApproveDiff => "approve_diff",
            Self::ApproveDiffAlways => "approve_diff_always",
            Self::RejectDiff => "reject_diff",
            Self::DeleteWord => "delete_word",
            Self::KillLine => "kill_line",
            Self::LineStart => "line_start",
            Self::LineEnd => "line_end",
            Self::EditInput => "edit_input",
            Self::CycleEffort => "cycle_effort",
            Self::PasteImage => "paste_image",
        }
    }

    pub fn from_snake(s: &str) -> Option<Self> {
        all_action_ids().find(|a| a.snake() == s)
    }

    pub const fn default_binds(self) -> &'static [Bind] {
        match self {
            Self::Quit => &[key::QUIT, key::QUIT_ALT],
            Self::Help => &[key::HELP],
            Self::Palette => &[key::PALETTE],
            Self::Sidebar => &[key::SIDEBAR],
            Self::ModelMenu => &[key::MODEL_MENU],
            Self::Search => &[key::SEARCH],
            Self::FilePicker => &[key::FILE_PICKER],
            Self::PlanToggle => &[key::PLAN_TOGGLE],
            Self::OpenEditor => &[key::OPEN_EDITOR],
            Self::ScrollHalfUp => &[key::SCROLL_HALF_UP],
            Self::ScrollHalfDown => &[key::SCROLL_HALF_DOWN],
            Self::ApproveDiff => &[key::APPROVE_DIFF],
            Self::ApproveDiffAlways => &[key::APPROVE_DIFF_ALWAYS, key::APPROVE_DIFF_ALWAYS_LEGACY],
            Self::RejectDiff => &[key::REJECT_DIFF],
            Self::DeleteWord => &[key::DELETE_WORD],
            Self::KillLine => &[key::KILL_LINE],
            Self::LineStart => &[key::LINE_START],
            Self::LineEnd => &[key::LINE_END],
            Self::EditInput => &[key::EDIT_INPUT],
            Self::CycleEffort => &[key::EFFORT_CYCLE],
            Self::PasteImage => &[key::PASTE_IMAGE],
        }
    }
}

/// Normalize a raw terminal key event so user-defined chords match
/// regardless of how the terminal reports it: BackTab becomes Tab+Shift,
/// and shift-modified letters are lowercased. Ported from the reference;
/// applied before chord matching only (never before composer insertion).
pub fn normalize_key(key: KeyEvent) -> KeyEvent {
    match key.code {
        KeyCode::BackTab => KeyEvent::new(KeyCode::Tab, key.modifiers | KeyModifiers::SHIFT),
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            KeyEvent::new(KeyCode::Char(c.to_ascii_lowercase()), key.modifiers)
        }
        _ => key,
    }
}

/// Resolves effective [`Bind`]s per [`ActionId`], applying a user overlay on
/// top of the compile-time defaults. An overlay entry of `[]` disables the
/// action.
#[derive(Debug, Clone, Default)]
pub struct KeybindingResolver {
    overlay: HashMap<ActionId, Vec<Bind>>,
}

impl KeybindingResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a resolver from a user overlay keyed by snake_case action id.
    /// Unknown ids and unparseable chords are reported via `warnings` and
    /// dropped.
    pub fn from_overlay(entries: &[(String, Vec<String>)], warnings: &mut Vec<String>) -> Self {
        let mut overlay = HashMap::new();
        for (id_str, chords) in entries {
            let Some(id) = ActionId::from_snake(id_str) else {
                warnings.push(format!("unknown keybinding action `{id_str}`"));
                continue;
            };
            if chords.is_empty() {
                overlay.insert(id, Vec::new());
                continue;
            }
            let mut binds = Vec::new();
            for chord in chords {
                match parse_chord(chord) {
                    Some(b) => binds.push(b),
                    None => {
                        warnings.push(format!("unparseable chord `{chord}` for action `{id_str}`"))
                    }
                }
            }
            if !binds.is_empty() {
                overlay.insert(id, binds);
            }
        }
        Self { overlay }
    }

    /// Effective binds for an action: the overlay if set, else the defaults.
    pub fn binds(&self, id: ActionId) -> &[Bind] {
        self.overlay
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_else(|| id.default_binds())
    }

    /// A user overlay is present for this action (even if disabling it).
    pub fn is_overridden(&self, id: ActionId) -> bool {
        self.overlay.contains_key(&id)
    }

    pub fn matches(&self, id: ActionId, key: KeyEvent) -> bool {
        self.binds(id).iter().any(|b| b.matches(key))
    }

    /// True when no overlay entries are present (pure defaults).
    #[allow(dead_code)] // exercised by the unit tests
    pub fn overlay_is_empty(&self) -> bool {
        self.overlay.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    #[test_case(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT), KeyCode::Char('a'), KeyModifiers::SHIFT ; "shift_letter_lowercased")]
    #[test_case(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), KeyCode::Tab, KeyModifiers::SHIFT ; "backtab_with_shift")]
    #[test_case(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE), KeyCode::Tab, KeyModifiers::SHIFT ; "backtab_without_shift")]
    #[test_case(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), KeyCode::Char('a'), KeyModifiers::CONTROL ; "ctrl_letter_unchanged")]
    #[test_case(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), KeyCode::Char('a'), KeyModifiers::NONE ; "plain_letter_unchanged")]
    fn normalize_key_cases(input: KeyEvent, expected_code: KeyCode, expected_mods: KeyModifiers) {
        let normalized = normalize_key(input);
        assert_eq!(normalized.code, expected_code);
        assert_eq!(normalized.modifiers, expected_mods);
    }

    #[test]
    fn normalized_events_match_user_chords() {
        let entries = vec![("palette".to_string(), vec!["Shift+Tab".to_string()])];
        let mut warnings = Vec::new();
        let resolver = KeybindingResolver::from_overlay(&entries, &mut warnings);
        assert!(warnings.is_empty());
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        assert!(resolver.matches(ActionId::Palette, normalize_key(backtab)));
    }

    #[test]
    fn approve_diff_always_matches_both_terminal_reports() {
        let resolver = KeybindingResolver::new();
        let shifted = KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        let legacy = KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::CONTROL);
        assert!(resolver.matches(ActionId::ApproveDiffAlways, normalize_key(shifted)));
        assert!(resolver.matches(ActionId::ApproveDiffAlways, legacy));
        assert!(!resolver.matches(ActionId::ApproveDiff, legacy));
    }

    #[test]
    fn every_action_id_has_default_binds() {
        for id in all_action_ids() {
            assert!(
                !id.default_binds().is_empty(),
                "action {:?} has no default binds",
                id,
            );
        }
    }

    #[test]
    fn every_action_id_snake_roundtrips() {
        for id in all_action_ids() {
            let s = id.snake();
            assert_eq!(ActionId::from_snake(s), Some(id), "roundtrip for {id:?}");
        }
    }

    #[test]
    fn every_action_id_appears_in_the_table() {
        // Rows may document a pair of actions under one Alt label (e.g.
        // Ctrl+U / Ctrl+D), so only the anchor action of each row is
        // checked; the paired ones are still dispatchable.
        for id in all_action_ids() {
            let anchored = KEYBINDS.iter().any(|kb| kb.action_id == Some(id));
            let paired = matches!(
                id,
                ActionId::ScrollHalfDown
                    | ActionId::Quit
                    | ActionId::ApproveDiff
                    | ActionId::RejectDiff
            );
            assert!(anchored || paired, "action {:?} missing from KEYBINDS", id,);
        }
    }

    #[test]
    fn resolver_overlay_replaces_chord() {
        let entries = vec![("search".to_string(), vec!["Alt+M".to_string()])];
        let mut warnings = Vec::new();
        let resolver = KeybindingResolver::from_overlay(&entries, &mut warnings);
        assert!(warnings.is_empty());
        let alt_m = KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT);
        let ctrl_f = key::SEARCH.to_key_event();
        assert!(resolver.matches(ActionId::Search, alt_m));
        assert!(!resolver.matches(ActionId::Search, ctrl_f));
    }

    #[test]
    fn resolver_overlay_empty_disables_action() {
        let entries = vec![("search".to_string(), vec![])];
        let mut warnings = Vec::new();
        let resolver = KeybindingResolver::from_overlay(&entries, &mut warnings);
        assert!(warnings.is_empty());
        assert!(resolver.binds(ActionId::Search).is_empty());
        assert!(!resolver.matches(ActionId::Search, key::SEARCH.to_key_event()));
    }

    #[test]
    fn resolver_default_when_no_overlay() {
        let resolver = KeybindingResolver::new();
        assert!(resolver.matches(ActionId::Search, key::SEARCH.to_key_event()));
        assert!(!resolver.is_overridden(ActionId::Search));
    }

    #[test]
    fn resolver_warns_on_unknown_action() {
        let entries = vec![("not_a_real_action".to_string(), vec!["Ctrl+X".to_string()])];
        let mut warnings = Vec::new();
        let resolver = KeybindingResolver::from_overlay(&entries, &mut warnings);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not_a_real_action"));
        assert!(resolver.overlay_is_empty());
    }

    #[test]
    fn resolver_warns_on_unparseable_chord() {
        let entries = vec![("search".to_string(), vec!["ctrl+".to_string()])];
        let mut warnings = Vec::new();
        let resolver = KeybindingResolver::from_overlay(&entries, &mut warnings);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ctrl+"));
        assert!(resolver.overlay_is_empty());
        assert!(resolver.matches(ActionId::Search, key::SEARCH.to_key_event()));
    }
}
