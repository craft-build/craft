//! User chord-string parsing: [`parse_chord`] and its modifier prefixes.

use crossterm::event::{KeyCode, KeyModifiers};

use super::table::Bind;

pub(super) const MOD_PREFIXES: &[(&str, KeyModifiers)] = &[
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
///
/// The rendered label is intentionally `Box::leak`ed: chords are parsed only
/// at config-load volume and the `&'static str` labels must live for the
/// process lifetime (see the module docs in `mod.rs`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

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
}
