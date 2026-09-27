//! Data-driven keybindings (F.1): a compile-time `KEYBINDS` table plus a
//! user-rebindable overlay resolved by [`KeybindingResolver`]. Ported from
//! the reference `craft-ui/src/components/keybindings.rs`, adapted to this
//! repo's action set (the reference's chat-queue / rewind / task-chat keys
//! are not ported because those subsystems do not exist here yet).

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

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
/// modal both read these through [`ActionId::default_binds`] so the overlay
/// can replace them wholesale.
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
}

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
        }
    }
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

#[derive(Debug, Clone, Copy)]
pub enum KeyLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    /// Alt on Mac, Single (first) on other platforms.
    MacAlt(&'static str, &'static str),
    /// Multi on Mac, Multi (first slice) on other platforms.
    MacMulti(&'static [&'static str], &'static [&'static str]),
}

pub const ALT_SEP: &str = " / ";

#[derive(Debug, Clone)]
pub enum ResolvedLabel {
    Single(&'static str),
    Alt(&'static str, &'static str),
    Multi(Box<[&'static str]>),
}

impl ResolvedLabel {
    pub fn display_width(&self) -> usize {
        let sep_w = UnicodeWidthStr::width(ALT_SEP);
        match self {
            Self::Single(s) => UnicodeWidthStr::width(*s),
            Self::Alt(a, b) => UnicodeWidthStr::width(*a) + sep_w + UnicodeWidthStr::width(*b),
            Self::Multi(keys) => {
                keys.iter()
                    .map(|k| UnicodeWidthStr::width(*k))
                    .sum::<usize>()
                    + sep_w * keys.len().saturating_sub(1)
            }
        }
    }
}

impl KeyLabel {
    pub fn resolve(self) -> ResolvedLabel {
        match self {
            Self::Single(s) => ResolvedLabel::Single(s),
            Self::Alt(a, b) => ResolvedLabel::Alt(a, b),
            Self::MacAlt(a, b) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Alt(a, b)
                } else {
                    ResolvedLabel::Single(a)
                }
            }
            Self::MacMulti(normal, mac) => {
                if cfg!(target_os = "macos") {
                    ResolvedLabel::Multi(Box::from(mac))
                } else {
                    ResolvedLabel::Multi(Box::from(normal))
                }
            }
        }
    }

    #[cfg(test)]
    fn flat_str(&self) -> String {
        match self.resolve() {
            ResolvedLabel::Single(s) => s.to_string(),
            ResolvedLabel::Alt(a, b) => format!("{a}/{b}"),
            ResolvedLabel::Multi(keys) => keys.join("/"),
        }
    }
}

/// One row of the help table. `action_id: None` marks a fixed (non-remappable)
/// key the table still documents (Enter, Tab, arrows, ...).
pub struct Keybind {
    pub action_id: Option<ActionId>,
    pub label: KeyLabel,
    pub description: &'static str,
    pub context: KeybindContext,
    pub platform: Platform,
}

pub const KEYBINDS: &[Keybind] = &[
    Keybind {
        action_id: Some(ActionId::Quit),
        label: KeyLabel::Single(key::QUIT.label),
        description: "Quit / clear input / interrupt",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::Palette),
        label: KeyLabel::Single(key::PALETTE.label),
        description: "Command palette",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::Help),
        label: KeyLabel::Single(key::HELP.label),
        description: "Show keybindings",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::Sidebar),
        label: KeyLabel::Single(key::SIDEBAR.label),
        description: "Toggle context panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::ModelMenu),
        label: KeyLabel::Single(key::MODEL_MENU.label),
        description: "Model menu",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::Search),
        label: KeyLabel::Single(key::SEARCH.label),
        description: "Search the transcript",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::FilePicker),
        label: KeyLabel::Single(key::FILE_PICKER.label),
        description: "File picker",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::PlanToggle),
        label: KeyLabel::Single(key::PLAN_TOGGLE.label),
        description: "Toggle todo / plan panel",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::OpenEditor),
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
        action_id: Some(ActionId::DeleteWord),
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
        action_id: Some(ActionId::KillLine),
        label: KeyLabel::Single(key::KILL_LINE.label),
        description: "Delete to end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::LineStart),
        label: KeyLabel::Single(key::LINE_START.label),
        description: "Jump to start of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::LineEnd),
        label: KeyLabel::Single(key::LINE_END.label),
        description: "Jump to end of line / bottom",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::CycleEffort),
        label: KeyLabel::Single(key::EFFORT_CYCLE.label),
        description: "Cycle reasoning effort",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::EditInput),
        label: KeyLabel::Single(key::EDIT_INPUT.label),
        description: "Edit input in external editor",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::ScrollHalfUp),
        label: KeyLabel::Alt(key::SCROLL_HALF_UP.label, key::SCROLL_HALF_DOWN.label),
        description: "Scroll half page up / down",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::ApproveDiff),
        label: KeyLabel::Alt(key::APPROVE_DIFF.label, key::REJECT_DIFF.label),
        description: "Approve / reject pending edit",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::ApproveDiffAlways),
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

const MOD_PREFIXES: &[(&str, KeyModifiers)] = &[
    ("ctrl+", KeyModifiers::CONTROL),
    ("control+", KeyModifiers::CONTROL),
    ("alt+", KeyModifiers::ALT),
    ("option+", KeyModifiers::ALT),
    ("shift+", KeyModifiers::SHIFT),
    ("super+", KeyModifiers::SUPER),
    ("cmd+", KeyModifiers::SUPER),
    ("meta+", KeyModifiers::SUPER),
];

fn parse_special_key(rest: &str) -> Option<KeyCode> {
    let lower = rest.to_ascii_lowercase();
    Some(match lower.as_str() {
        "enter" | "return" | "cr" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backspace" | "bs" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "space" | "spacebar" => KeyCode::Char(' '),
        "insert" | "ins" => KeyCode::Insert,
        _ if lower.starts_with('f') && lower.len() >= 2 => {
            let n: u8 = lower[1..].parse().ok()?;
            (1..=12).contains(&n).then_some(KeyCode::F(n))?
        }
        _ => return None,
    })
}

/// Parse a human chord like `"Ctrl+P"`, `"Alt+M"`, `"Shift+Tab"` into a [`Bind`].
/// Returns `None` on an unparseable chord.
pub fn parse_chord(chord: &str) -> Option<Bind> {
    let original = chord.trim();
    if original.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut rest = original.to_ascii_lowercase();
    let mut changed = true;
    while changed {
        changed = false;
        for (prefix, flag) in MOD_PREFIXES {
            if let Some(stripped) = rest.strip_prefix(prefix) {
                modifiers |= *flag;
                rest = stripped.to_string();
                changed = true;
                break;
            }
        }
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let code = if let Some(c) = rest.chars().next()
        && rest.len() == c.len_utf8()
        && !c.is_whitespace()
    {
        KeyCode::Char(c)
    } else {
        parse_special_key(rest)?
    };
    let label: &'static str = Box::leak(render_label(code, modifiers).into_boxed_str());
    Some(Bind {
        code,
        modifiers,
        label,
    })
}

/// Render a canonical display label for a parsed key, in fixed modifier order
/// (Ctrl, Alt, Shift, Cmd) so it is independent of the input chord's ordering.
fn render_label(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut out = String::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    if modifiers.contains(KeyModifiers::SUPER) {
        out.push_str("Cmd+");
    }
    out.push_str(key_code_label(code));
    out
}

fn key_code_label(code: KeyCode) -> &'static str {
    match code {
        KeyCode::Enter => "Enter",
        KeyCode::Esc => "Esc",
        KeyCode::Tab => "Tab",
        KeyCode::Backspace => "Backspace",
        KeyCode::Delete => "Delete",
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PageUp",
        KeyCode::PageDown => "PageDown",
        KeyCode::Insert => "Insert",
        KeyCode::Char(' ') => "Space",
        KeyCode::F(n) => Box::leak(format!("F{n}").into_boxed_str()),
        KeyCode::Char(c) => Box::leak(c.to_ascii_uppercase().to_string().into_boxed_str()),
        _ => "<?>",
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

/// Effective label for a help-table row, applying user overrides.
/// Returns `None` when the action is disabled (empty overlay).
pub fn effective_label(kb: &Keybind, resolver: &KeybindingResolver) -> Option<ResolvedLabel> {
    match kb.action_id {
        Some(id) => {
            let binds = resolver.binds(id);
            if binds.is_empty() {
                return None;
            }
            if !resolver.is_overridden(id) {
                return Some(kb.label.resolve());
            }
            let labels: Vec<&'static str> = binds.iter().map(|b| b.label).collect();
            match labels.len() {
                1 => Some(ResolvedLabel::Single(labels[0])),
                _ => Some(ResolvedLabel::Multi(labels.into_boxed_slice())),
            }
        }
        None => Some(kb.label.resolve()),
    }
}

/// The help modal's body, grouped by context (children nested under their
/// parent) with the resolver's effective labels. Auto-generated from
/// [`KEYBINDS`]; returns (lines, key-column width) so callers can pad.
pub fn help_lines(resolver: &KeybindingResolver) -> (Vec<ratatui::text::Line<'static>>, usize) {
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    let t = crate::tui::ui::theme::current();
    let key_style = Style::default().fg(t.cyan);
    let desc_style = Style::default().fg(t.text_tertiary);
    let section_style = Style::default().fg(t.text_secondary);

    let key_col_width = KEYBINDS
        .iter()
        .filter(|kb| kb.platform.is_visible())
        .filter_map(|kb| effective_label(kb, resolver))
        .map(|label| label.display_width())
        .max()
        .unwrap_or(0)
        + 2;

    let key_spans = |label: ResolvedLabel, pad: usize, prefix: &str| -> Vec<Span<'static>> {
        let keys: Vec<&'static str> = match label {
            ResolvedLabel::Single(s) => vec![s],
            ResolvedLabel::Alt(a, b) => vec![a, b],
            ResolvedLabel::Multi(keys) => keys.into_vec(),
        };
        let sep_w = UnicodeWidthStr::width(ALT_SEP);
        let content_w: usize = keys
            .iter()
            .map(|k| UnicodeWidthStr::width(*k))
            .sum::<usize>()
            + sep_w * keys.len().saturating_sub(1);
        let trailing = pad.saturating_sub(content_w);
        let mut spans = Vec::with_capacity(keys.len() * 2);
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(ALT_SEP, desc_style));
            }
            let text = if i == 0 && i == keys.len() - 1 {
                format!("{prefix}{k}{:trailing$}", "")
            } else if i == 0 {
                format!("{prefix}{k}")
            } else if i == keys.len() - 1 {
                format!("{k}{:trailing$}", "")
            } else {
                (*k).to_string()
            };
            spans.push(Span::styled(text, key_style));
        }
        spans
    };

    let mut lines: Vec<Line> = Vec::new();
    let mut first = true;
    for ctx in all_contexts() {
        if ctx.parent().is_some() {
            continue;
        }
        if !first {
            lines.push(Line::default());
        }
        first = false;

        lines.push(Line::from(Span::styled(
            format!("  {}", ctx.label()),
            section_style,
        )));
        for kb in KEYBINDS
            .iter()
            .filter(|kb| kb.context == ctx && kb.platform.is_visible())
        {
            let Some(label) = effective_label(kb, resolver) else {
                continue;
            };
            let mut spans = key_spans(label, key_col_width, "  ");
            spans.push(Span::styled(kb.description, desc_style));
            lines.push(Line::from(spans));
        }

        for child in all_contexts() {
            if child.parent() != Some(ctx) {
                continue;
            }
            let child_binds: Vec<_> = KEYBINDS
                .iter()
                .filter(|kb| kb.context == child && kb.platform.is_visible())
                .collect();
            if child_binds.is_empty() {
                continue;
            }
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                format!("    {}", child.label()),
                section_style,
            )));
            for kb in child_binds {
                let Some(label) = effective_label(kb, resolver) else {
                    continue;
                };
                let mut spans = key_spans(label, key_col_width.saturating_sub(2), "    ");
                spans.push(Span::styled(kb.description, desc_style));
                lines.push(Line::from(spans));
            }
        }
    }
    (lines, key_col_width)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test_case("Ctrl+P", KeyCode::Char('p'), KeyModifiers::CONTROL ; "ctrl_p")]
    #[test_case("Alt+M", KeyCode::Char('m'), KeyModifiers::ALT ; "alt_m")]
    #[test_case("ctrl+shift+t", KeyCode::Char('t'), KeyModifiers::CONTROL | KeyModifiers::SHIFT ; "ctrl_shift_t")]
    #[test_case("F5", KeyCode::F(5), KeyModifiers::NONE ; "f5")]
    #[test_case("shift+tab", KeyCode::Tab, KeyModifiers::SHIFT ; "shift_tab")]
    fn parse_chord_cases(chord: &str, code: KeyCode, mods: KeyModifiers) {
        let bind = parse_chord(chord).unwrap_or_else(|| panic!("failed to parse `{chord}`"));
        assert_eq!(bind.code, code);
        assert_eq!(bind.modifiers, mods);
    }

    #[test_case("Ctrl+P", "Ctrl+P" ; "ctrl_p")]
    #[test_case("alt+ctrl+p", "Ctrl+Alt+P" ; "order_independent")]
    #[test_case("control+p", "Ctrl+P" ; "control_alias")]
    #[test_case("option+m", "Alt+M" ; "option_alias")]
    #[test_case("shift+tab", "Shift+Tab" ; "shift_tab")]
    #[test_case("cmd+s", "Cmd+S" ; "cmd_alias")]
    #[test_case("ctrl+shift+f1", "Ctrl+Shift+F1" ; "mixed_modifiers_fkey")]
    fn parse_chord_label_canonical(chord: &str, expected_label: &str) {
        let bind = parse_chord(chord).unwrap_or_else(|| panic!("failed to parse `{chord}`"));
        assert_eq!(
            bind.label, expected_label,
            "label should be canonical regardless of input order/alias"
        );
    }

    #[test_case("" ; "empty")]
    #[test_case("   " ; "whitespace")]
    #[test_case("ctrl+" ; "modifier_only")]
    #[test_case("f99" ; "f_key_out_of_range")]
    fn parse_chord_rejects_invalid(chord: &str) {
        assert!(parse_chord(chord).is_none());
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

    #[test]
    fn help_lines_render_and_honor_overrides() {
        let (lines, _) = help_lines(&KeybindingResolver::new());
        assert!(lines.len() > KEYBINDS.len(), "section headers add lines");

        let entries = vec![("search".to_string(), vec![])];
        let mut warnings = Vec::new();
        let disabled = KeybindingResolver::from_overlay(&entries, &mut warnings);
        let (disabled_lines, _) = help_lines(&disabled);
        assert!(
            disabled_lines.len() < lines.len(),
            "disabled action drops its row"
        );
    }
}
