use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::fmt::Write;
use strum::EnumIter;
use unicode_width::UnicodeWidthStr;

macro_rules! mod_key {
    ($suffix:expr) => {
        concat!("Ctrl+", $suffix)
    };
}

macro_rules! upper {
    ('a') => {
        "A"
    };
    ('b') => {
        "B"
    };
    ('c') => {
        "C"
    };
    ('d') => {
        "D"
    };
    ('e') => {
        "E"
    };
    ('f') => {
        "F"
    };
    ('g') => {
        "G"
    };
    ('h') => {
        "H"
    };
    ('i') => {
        "I"
    };
    ('j') => {
        "J"
    };
    ('k') => {
        "K"
    };
    ('l') => {
        "L"
    };
    ('m') => {
        "M"
    };
    ('n') => {
        "N"
    };
    ('o') => {
        "O"
    };
    ('p') => {
        "P"
    };
    ('q') => {
        "Q"
    };
    ('r') => {
        "R"
    };
    ('s') => {
        "S"
    };
    ('t') => {
        "T"
    };
    ('u') => {
        "U"
    };
    ('v') => {
        "V"
    };
    ('w') => {
        "W"
    };
    ('x') => {
        "X"
    };
    ('y') => {
        "Y"
    };
    ('z') => {
        "Z"
    };
}

macro_rules! ctrl_bind {
    ($char:tt) => {
        Bind {
            code: KeyCode::Char($char),
            modifiers: KeyModifiers::CONTROL,
            label: mod_key!(upper!($char)),
        }
    };
}

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

pub mod key {
    use super::Bind;
    use crossterm::event::{KeyCode, KeyModifiers};

    pub const QUIT: Bind = ctrl_bind!('c');
    pub const HELP: Bind = ctrl_bind!('h');
    pub const PREV_CHAT: Bind = ctrl_bind!('p');
    pub const NEXT_CHAT: Bind = ctrl_bind!('n');
    pub const SCROLL_HALF_UP: Bind = ctrl_bind!('u');
    pub const SCROLL_HALF_DOWN: Bind = ctrl_bind!('d');
    pub const SCROLL_LINE_UP: Bind = ctrl_bind!('y');
    pub const SCROLL_LINE_DOWN: Bind = ctrl_bind!('e');
    pub const SCROLL_TOP: Bind = ctrl_bind!('g');
    pub const SCROLL_BOTTOM: Bind = ctrl_bind!('b');
    pub const POP_QUEUE: Bind = ctrl_bind!('q');
    pub const DELETE_WORD: Bind = ctrl_bind!('w');
    pub const SEARCH: Bind = ctrl_bind!('f');
    pub const FILE_PICKER: Bind = ctrl_bind!('s');
    pub const OPEN_EDITOR: Bind = ctrl_bind!('o');
    pub const PLAN_TOGGLE: Bind = ctrl_bind!('t');
    pub const TASKS: Bind = ctrl_bind!('x');
    pub const REFRESH: Bind = ctrl_bind!('r');
    pub const SUSPEND: Bind = ctrl_bind!('z');
    pub const DELETE: Bind = ctrl_bind!('d');
    pub const KILL_LINE: Bind = ctrl_bind!('k');
    pub const LINE_START: Bind = ctrl_bind!('a');
    pub const LINE_END: Bind = ctrl_bind!('e');
    pub const EDIT_INPUT: Bind = Bind {
        code: KeyCode::Char('o'),
        modifiers: KeyModifiers::ALT,
        label: "Alt+O",
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumIter)]
pub enum ActionId {
    Quit,
    Help,
    PrevChat,
    NextChat,
    ScrollHalfUp,
    ScrollHalfDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollTop,
    ScrollBottom,
    PopQueue,
    DeleteWord,
    Search,
    FilePicker,
    OpenEditor,
    PlanToggle,
    Tasks,
    Suspend,
    Delete,
    KillLine,
    LineStart,
    LineEnd,
    EditInput,
}

impl ActionId {
    pub const fn snake(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Help => "help",
            Self::PrevChat => "prev_chat",
            Self::NextChat => "next_chat",
            Self::ScrollHalfUp => "scroll_half_up",
            Self::ScrollHalfDown => "scroll_half_down",
            Self::ScrollLineUp => "scroll_line_up",
            Self::ScrollLineDown => "scroll_line_down",
            Self::ScrollTop => "scroll_to_top",
            Self::ScrollBottom => "scroll_to_bottom",
            Self::PopQueue => "pop_queue",
            Self::DeleteWord => "delete_word",
            Self::Search => "search",
            Self::FilePicker => "file_picker",
            Self::OpenEditor => "open_editor",
            Self::PlanToggle => "plan_toggle",
            Self::Tasks => "tasks",
            Self::Suspend => "suspend",
            Self::Delete => "delete",
            Self::KillLine => "kill_line",
            Self::LineStart => "line_start",
            Self::LineEnd => "line_end",
            Self::EditInput => "edit_input",
        }
    }

    pub fn from_snake(s: &str) -> Option<Self> {
        all_action_ids().find(|a| a.snake() == s)
    }

    pub const fn default_binds(self) -> &'static [Bind] {
        match self {
            Self::Quit => &[key::QUIT],
            Self::Help => &[key::HELP],
            Self::PrevChat => &[key::PREV_CHAT],
            Self::NextChat => &[key::NEXT_CHAT],
            Self::ScrollHalfUp => &[key::SCROLL_HALF_UP],
            Self::ScrollHalfDown => &[key::SCROLL_HALF_DOWN],
            Self::ScrollLineUp => &[key::SCROLL_LINE_UP],
            Self::ScrollLineDown => &[key::SCROLL_LINE_DOWN],
            Self::ScrollTop => &[key::SCROLL_TOP],
            Self::ScrollBottom => &[key::SCROLL_BOTTOM],
            Self::PopQueue => &[key::POP_QUEUE],
            Self::DeleteWord => &[key::DELETE_WORD],
            Self::Search => &[key::SEARCH],
            Self::FilePicker => &[key::FILE_PICKER],
            Self::OpenEditor => &[key::OPEN_EDITOR],
            Self::PlanToggle => &[key::PLAN_TOGGLE],
            Self::Tasks => &[key::TASKS],
            Self::Suspend => &[key::SUSPEND],
            Self::Delete => &[key::DELETE],
            Self::KillLine => &[key::KILL_LINE],
            Self::LineStart => &[key::LINE_START],
            Self::LineEnd => &[key::LINE_END],
            Self::EditInput => &[key::EDIT_INPUT],
        }
    }
}

pub fn all_action_ids() -> impl Iterator<Item = ActionId> {
    use strum::IntoEnumIterator;
    ActionId::iter()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum KeybindContext {
    General,
    Editing,
    Streaming,
    Picker,
    FormInput,
    TaskPicker,
    RewindPicker,
    ThemePicker,
    ModelPicker,
    QueueFocus,
    CommandPalette,
    Search,
    FilePicker,
}

impl KeybindContext {
    pub const fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Editing => "Editing",
            Self::Streaming => "While Streaming",
            Self::Picker => "Pickers",
            Self::FormInput => "Form",
            Self::TaskPicker => "Task Picker",
            Self::RewindPicker => "Rewind Picker",
            Self::ThemePicker => "Theme Picker",
            Self::ModelPicker => "Model Picker",
            Self::QueueFocus => "Queue",
            Self::CommandPalette => "Commands",
            Self::Search => "Search",
            Self::FilePicker => "File Picker",
        }
    }

    pub const fn parent(self) -> Option<KeybindContext> {
        match self {
            Self::TaskPicker
            | Self::RewindPicker
            | Self::ThemePicker
            | Self::ModelPicker
            | Self::QueueFocus
            | Self::CommandPalette
            | Self::Search
            | Self::FilePicker => Some(Self::Picker),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    All,
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
    /// Alt on Mac, Single (first) on other platforms
    MacAlt(&'static str, &'static str),
    /// Multi on Mac, Multi (first slice) on other platforms
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
        match self {
            Self::Single(s) => UnicodeWidthStr::width(*s),
            Self::Alt(a, b) => {
                let sep_w = UnicodeWidthStr::width(ALT_SEP);
                UnicodeWidthStr::width(*a) + sep_w + UnicodeWidthStr::width(*b)
            }
            Self::Multi(keys) => {
                let sep_w = UnicodeWidthStr::width(ALT_SEP);
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
        description: "Quit / clear input",
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
        action_id: Some(ActionId::NextChat),
        label: KeyLabel::Alt(key::NEXT_CHAT.label, key::PREV_CHAT.label),
        description: "Next / previous task chat",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::Search),
        label: KeyLabel::Single(key::SEARCH.label),
        description: "Search messages",
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
        action_id: Some(ActionId::OpenEditor),
        label: KeyLabel::Single(key::OPEN_EDITOR.label),
        description: "Open plan in editor",
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
        action_id: Some(ActionId::Tasks),
        label: KeyLabel::Single(key::TASKS.label),
        description: "Open tasks",
        context: KeybindContext::General,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::Suspend),
        label: KeyLabel::Single(key::SUSPEND.label),
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
        label: KeyLabel::MacMulti(&["\\+Enter", "Ctrl+J", "Alt+Enter"], &["⇧↵", "⌃J", "⌥↵"]),
        description: "Newline",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Tab"),
        description: "Toggle mode",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("/command"),
        description: "Open command palette",
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
        label: KeyLabel::MacMulti(&["Alt+←", "Alt+→"], &["⌥←", "⌥→"]),
        description: "Move word left / right",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt(mod_key!("Del"), "⌥Del"),
        description: "Delete word forward",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        action_id: Some(ActionId::KillLine),
        label: KeyLabel::Single(key::KILL_LINE.label),
        description: "Delete to end of line",
        context: KeybindContext::Editing,
        platform: Platform::MacOnly,
    },
    Keybind {
        action_id: Some(ActionId::LineStart),
        label: KeyLabel::Single(key::LINE_START.label),
        description: "Jump to start of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt("Home", "End"),
        description: "Jump to start/end of line",
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
        action_id: Some(ActionId::LineEnd),
        label: KeyLabel::Single(key::LINE_END.label),
        description: "Jump to end of line",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::ScrollTop),
        label: KeyLabel::Single(key::SCROLL_TOP.label),
        description: "Scroll to top",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::ScrollBottom),
        label: KeyLabel::Single(key::SCROLL_BOTTOM.label),
        description: "Scroll to bottom",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: Some(ActionId::PopQueue),
        label: KeyLabel::Single(key::POP_QUEUE.label),
        description: "Pop queue",
        context: KeybindContext::Editing,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Esc Esc"),
        description: "Rewind",
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
        action_id: None,
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate input history",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Esc Esc"),
        description: "Cancel agent",
        context: KeybindContext::Streaming,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Alt("↑", "↓"),
        description: "Navigate options",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Enter"),
        description: "Select option",
        context: KeybindContext::FormInput,
        platform: Platform::All,
    },
    Keybind {
        action_id: None,
        label: KeyLabel::Single("Esc"),
        description: "Close",
        context: KeybindContext::FormInput,
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
        description: "Scroll page up / down",
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
        label: KeyLabel::Single("Enter"),
        description: "Remove item",
        context: KeybindContext::QueueFocus,
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
        label: KeyLabel::Single("!/@/#/$"),
        description: "Set tier (strong/medium/weak/compaction)",
        context: KeybindContext::ModelPicker,
        platform: Platform::All,
    },
];

pub fn all_contexts() -> impl Iterator<Item = KeybindContext> {
    use strum::IntoEnumIterator;
    KeybindContext::iter()
}

pub(crate) fn normalize_key(key: KeyEvent) -> KeyEvent {
    match key.code {
        KeyCode::BackTab => KeyEvent::new(KeyCode::Tab, key.modifiers | KeyModifiers::SHIFT),
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::SHIFT) => {
            KeyEvent::new(KeyCode::Char(c.to_ascii_lowercase()), key.modifiers)
        }
        _ => key,
    }
}

pub(crate) fn key_event_to_string(key: &KeyEvent) -> String {
    let mut s = String::new();
    let mods = key.modifiers;
    let is_char = matches!(key.code, KeyCode::Char(_));
    if mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        s.push_str("alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) && !is_char {
        s.push_str("shift+");
    }
    match key.code {
        KeyCode::Char(' ') => s.push_str("space"),
        KeyCode::Char(c) => s.push(c),
        KeyCode::Enter => s.push_str("enter"),
        KeyCode::Esc => s.push_str("esc"),
        KeyCode::Tab => s.push_str("tab"),
        KeyCode::BackTab => {
            if !s.contains("shift+") {
                s.insert_str(0, "shift+");
            }
            s.push_str("tab");
        }
        KeyCode::Backspace => s.push_str("backspace"),
        KeyCode::Delete => s.push_str("delete"),
        KeyCode::Up => s.push_str("up"),
        KeyCode::Down => s.push_str("down"),
        KeyCode::Left => s.push_str("left"),
        KeyCode::Right => s.push_str("right"),
        KeyCode::Home => s.push_str("home"),
        KeyCode::End => s.push_str("end"),
        KeyCode::PageUp => s.push_str("pageup"),
        KeyCode::PageDown => s.push_str("pagedown"),
        KeyCode::F(n) => write!(s, "f{n}").unwrap(),
        KeyCode::Insert => s.push_str("insert"),
        _ => {}
    }
    s
}

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
        KeyCode::F(n) => {
            // F-key labels are dynamic; leak the formatted string once.
            Box::leak(format!("F{n}").into_boxed_str())
        }
        KeyCode::Char(c) => Box::leak(c.to_ascii_uppercase().to_string().into_boxed_str()),
        _ => "<?>",
    }
}

/// Resolves effective [`Bind`]s per [`ActionId`], applying a user overlay on top
/// of the compile-time defaults. An overlay entry of `[]` disables the action.
#[derive(Debug, Clone, Default)]
pub struct KeybindingResolver {
    overlay: HashMap<ActionId, Vec<Bind>>,
}

impl KeybindingResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a resolver from a user overlay keyed by snake_case action id.
    /// Unknown ids and unparseable chords are reported via `warnings` and dropped.
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
    pub fn overlay_is_empty(&self) -> bool {
        self.overlay.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use test_case::test_case;

    #[test_case(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL), "ctrl+d")]
    #[test_case(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT), "alt+x")]
    #[test_case(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT), "shift+tab")]
    #[test_case(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), "shift+tab")]
    #[test_case(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), "space")]
    #[test_case(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE), "f5")]
    #[test_case(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), "a")]
    fn key_event_to_string_cases(input: KeyEvent, expected: &str) {
        assert_eq!(key_event_to_string(&input), expected);
    }

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
            assert_eq!(
                ActionId::from_snake(s),
                Some(id),
                "snake roundtrip for {id:?}"
            );
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
    #[test_case("F5", "F5" ; "f5")]
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
}
