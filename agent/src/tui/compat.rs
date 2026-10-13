//! Interop shims for the ratatui → `slt` migration. **Temporary.**
//!
//! The app's styled-text model (`tui::text`) and event types
//! (`slt::event`) are authoritative; this module bridges them to the
//! legacy layer while it still exists:
//!
//! * [`rat_style`] / [`rat_line`] / [`rat_text`] convert app text into
//!   ratatui values at paint sites (Paragraph render sites, until the
//!   paint layer moves to `slt`).
//! * [`slt_event`] translates raw crossterm 0.29 events into `slt` events
//!   while the legacy input pump still reads crossterm directly.
//!
//! Everything in here is deleted once the render loop and the paint layer
//! are fully on `slt`.

use std::borrow::Cow;

use super::text::{Line, Span};

// ─────────────────────────────────────────────────────────────────────────
// Text painting: app text → ratatui text
// ─────────────────────────────────────────────────────────────────────────

pub fn rat_color(c: slt::style::Color) -> ratatui::style::Color {
    use ratatui::style::Color as C;
    match c {
        slt::style::Color::Reset => C::Reset,
        slt::style::Color::Black => C::Black,
        slt::style::Color::Red => C::Red,
        slt::style::Color::Green => C::Green,
        slt::style::Color::Yellow => C::Yellow,
        slt::style::Color::Blue => C::Blue,
        slt::style::Color::Magenta => C::Magenta,
        slt::style::Color::Cyan => C::Cyan,
        slt::style::Color::White => C::White,
        slt::style::Color::DarkGray => C::DarkGray,
        slt::style::Color::LightRed => C::LightRed,
        slt::style::Color::LightGreen => C::LightGreen,
        slt::style::Color::LightYellow => C::LightYellow,
        slt::style::Color::LightBlue => C::LightBlue,
        slt::style::Color::LightMagenta => C::LightMagenta,
        slt::style::Color::LightCyan => C::LightCyan,
        slt::style::Color::Rgb(r, g, b) => C::Rgb(r, g, b),
        slt::style::Color::Indexed(i) => C::Indexed(i),
        // slt's Color is non-exhaustive; unknown values can't be painted.
        _ => C::Reset,
    }
}

pub fn rat_style(s: slt::style::Style) -> ratatui::style::Style {
    use ratatui::style::Modifier as RM;
    let mut out = ratatui::style::Style::new();
    if let Some(fg) = s.fg {
        out = out.fg(rat_color(fg));
    }
    if let Some(bg) = s.bg {
        out = out.bg(rat_color(bg));
    }
    if let Some(uc) = s.underline_color {
        let _ = uc; // underline color is unused by the legacy paint layer
    }
    let m = s.modifiers;
    let mut rat_mod = ratatui::style::Modifier::empty();
    if m.contains(slt::style::Modifiers::BOLD) {
        rat_mod |= RM::BOLD;
    }
    if m.contains(slt::style::Modifiers::DIM) {
        rat_mod |= RM::DIM;
    }
    if m.contains(slt::style::Modifiers::ITALIC) {
        rat_mod |= RM::ITALIC;
    }
    if m.contains(slt::style::Modifiers::UNDERLINE) {
        rat_mod |= RM::UNDERLINED;
    }
    if m.contains(slt::style::Modifiers::REVERSED) {
        rat_mod |= RM::REVERSED;
    }
    if m.contains(slt::style::Modifiers::STRIKETHROUGH) {
        rat_mod |= RM::CROSSED_OUT;
    }
    if m.contains(slt::style::Modifiers::BLINK) {
        rat_mod |= RM::SLOW_BLINK;
    }
    out = out.add_modifier(rat_mod);
    out
}

fn rat_span<'a>(span: &Span<'a>) -> ratatui::text::Span<'a> {
    let content: Cow<'a, str> = span.content.clone();
    ratatui::text::Span {
        content,
        style: rat_style(span.style),
    }
}

/// Borrow an app line as a ratatui line without cloning the content.
pub fn rat_line<'a>(line: &Line<'a>) -> ratatui::text::Line<'a> {
    ratatui::text::Line {
        spans: line.spans.iter().map(rat_span).collect(),
        ..ratatui::text::Line::default()
    }
}

/// Borrow app lines as ratatui text (for `Paragraph::new`).
pub fn rat_text<'a>(lines: &'a [Line<'a>]) -> ratatui::text::Text<'a> {
    ratatui::text::Text {
        lines: lines.iter().map(rat_line).collect(),
        ..ratatui::text::Text::default()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Events: crossterm 0.29 (legacy pump) → slt events
// ─────────────────────────────────────────────────────────────────────────

/// Construct an slt `KeyEvent` from code + modifiers (kind `Press`).
///
/// slt's `KeyEvent` is `#[non_exhaustive]`, so it cannot be built with a
/// struct literal outside the crate; this goes through the crate's own
/// `Event::key` builder and patches the modifier bits.
pub fn slt_key(
    code: slt::event::KeyCode,
    modifiers: slt::event::KeyModifiers,
) -> slt::event::KeyEvent {
    let mut ev = slt::Event::key(code);
    if let slt::Event::Key(ref mut key) = ev {
        key.modifiers = modifiers;
    }
    match ev {
        slt::Event::Key(key) => key,
        _ => unreachable!("Event::key always produces a Key event"),
    }
}

/// Translate a raw crossterm event into the `slt` event the app handlers
/// consume. Returns `None` for event kinds `slt` does not model.
pub fn slt_event(raw: crossterm::event::Event) -> Option<slt::Event> {
    use crossterm::event as ct;
    use slt::event::{KeyCode, KeyEventKind, MouseKind};

    match raw {
        ct::Event::Key(k) => {
            let kind = match k.kind {
                ct::KeyEventKind::Press => KeyEventKind::Press,
                ct::KeyEventKind::Repeat => KeyEventKind::Repeat,
                ct::KeyEventKind::Release => KeyEventKind::Release,
            };
            let code = match k.code {
                ct::KeyCode::Char(c) => KeyCode::Char(c),
                ct::KeyCode::Enter => KeyCode::Enter,
                ct::KeyCode::Backspace => KeyCode::Backspace,
                ct::KeyCode::Tab => KeyCode::Tab,
                ct::KeyCode::BackTab => KeyCode::BackTab,
                ct::KeyCode::Esc => KeyCode::Esc,
                ct::KeyCode::Up => KeyCode::Up,
                ct::KeyCode::Down => KeyCode::Down,
                ct::KeyCode::Left => KeyCode::Left,
                ct::KeyCode::Right => KeyCode::Right,
                ct::KeyCode::Home => KeyCode::Home,
                ct::KeyCode::End => KeyCode::End,
                ct::KeyCode::PageUp => KeyCode::PageUp,
                ct::KeyCode::PageDown => KeyCode::PageDown,
                ct::KeyCode::Delete => KeyCode::Delete,
                ct::KeyCode::Insert => KeyCode::Insert,
                ct::KeyCode::Null => KeyCode::Null,
                ct::KeyCode::CapsLock => KeyCode::CapsLock,
                ct::KeyCode::ScrollLock => KeyCode::ScrollLock,
                ct::KeyCode::NumLock => KeyCode::NumLock,
                ct::KeyCode::PrintScreen => KeyCode::PrintScreen,
                ct::KeyCode::Pause => KeyCode::Pause,
                ct::KeyCode::Menu => KeyCode::Menu,
                ct::KeyCode::KeypadBegin => KeyCode::KeypadBegin,
                ct::KeyCode::F(n) => KeyCode::F(n),
                // Media/Modifier keys are dropped (slt has no Media variant;
                // Modifier keys only arrive with the kitty protocol, which
                // the legacy pump never enables).
                ct::KeyCode::Media(_) | ct::KeyCode::Modifier(_) => return None,
            };
            // slt's KeyEvent is non-exhaustive; start from its own builder
            // and patch the modifier/kind fields in place.
            let mut ev = slt::Event::key(code);
            if let slt::Event::Key(ref mut key) = ev {
                key.modifiers = slt_modifiers(k.modifiers);
                key.kind = kind;
            }
            Some(ev)
        }
        ct::Event::Mouse(m) => {
            let kind = match m.kind {
                ct::MouseEventKind::Down(b) => MouseKind::Down(slt_mouse_button(b)),
                ct::MouseEventKind::Up(b) => MouseKind::Up(slt_mouse_button(b)),
                ct::MouseEventKind::Drag(b) => MouseKind::Drag(slt_mouse_button(b)),
                ct::MouseEventKind::Moved => MouseKind::Moved,
                ct::MouseEventKind::ScrollUp => MouseKind::ScrollUp,
                ct::MouseEventKind::ScrollDown => MouseKind::ScrollDown,
                ct::MouseEventKind::ScrollLeft => MouseKind::ScrollLeft,
                ct::MouseEventKind::ScrollRight => MouseKind::ScrollRight,
            };
            Some(slt::Event::Mouse(slt::event::MouseEvent::new(
                kind,
                m.column as u32,
                m.row as u32,
                slt_modifiers(m.modifiers),
                None,
                None,
            )))
        }
        ct::Event::Resize(w, h) => Some(slt::Event::Resize(w as u32, h as u32)),
        ct::Event::Paste(s) => Some(slt::Event::Paste(s)),
        ct::Event::FocusGained => Some(slt::Event::FocusGained),
        ct::Event::FocusLost => Some(slt::Event::FocusLost),
    }
}

fn slt_mouse_button(b: crossterm::event::MouseButton) -> slt::event::MouseButton {
    match b {
        crossterm::event::MouseButton::Left => slt::event::MouseButton::Left,
        crossterm::event::MouseButton::Right => slt::event::MouseButton::Right,
        crossterm::event::MouseButton::Middle => slt::event::MouseButton::Middle,
    }
}

/// crossterm and slt use identical modifier bit assignments.
fn slt_modifiers(m: crossterm::event::KeyModifiers) -> slt::event::KeyModifiers {
    slt::event::KeyModifiers(m.bits() as u8)
}
