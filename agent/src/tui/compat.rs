//! Test/event construction helpers for the `slt` event model.
//!
//! The app's styled-text model (`tui::text`) and event types
//! (`slt::event`) are authoritative, and the paint layer runs on `slt`
//! end to end. What remains here is [`slt_key`]: the production
//! keybinding normalizer (and many tests) need to construct slt
//! `KeyEvent` values, which are `#[non_exhaustive]` and cannot be built
//! with a struct literal outside the crate.

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
