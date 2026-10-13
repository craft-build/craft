//! Styling types for the TUI, backed by `superlighttui` (`slt`).
//!
//! This module is the single import site for colors, styles, and text
//! modifiers so the rest of the TUI stays independent of any particular
//! rendering crate. [`Style`] and [`Color`] are `slt`'s own types;
//! [`StyleExt`] adds the few builder helpers the code base relies on
//! (`add_modifier`, `remove_modifier`, `patch`).

pub use slt::style::{Color, Style, UnderlineStyle};
/// Text-modifier bitflags. Aliased as `Modifier` because most of the code
/// base spells it that way (ratatui heritage).
pub use slt::style::Modifiers;

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Style {}
}

/// Builder helpers on [`Style`] that mirror the ratatui-style call sites.
///
/// `slt`'s `Style` natively provides `fg`, `bg`, `bold`, `dim`, `italic`,
/// `underline`, `reversed`, `strikethrough`, `blink`, `overline`,
/// `underline_color`, and [`Style::default`]; this trait adds the
/// modifier-union operations and `patch` used across the TUI.
pub trait StyleExt: sealed::Sealed + Sized {
    /// Union `modifier` into the style's modifier bits.
    fn add_modifier(self, modifier: Modifiers) -> Style;

    /// Remove `modifier` from the style's modifier bits.
    fn remove_modifier(self, modifier: Modifiers) -> Style;

    /// Overlay `other` on `self`, ratatui `Style::patch` semantics: `other`'s
    /// set fields replace `self`'s, and modifier bits union.
    fn patch(self, other: Style) -> Style;
}

impl StyleExt for Style {
    fn add_modifier(mut self, modifier: Modifiers) -> Style {
        self.modifiers.0 |= modifier.0;
        self
    }

    fn remove_modifier(mut self, modifier: Modifiers) -> Style {
        self.modifiers.0 &= !modifier.0;
        self
    }

    fn patch(mut self, other: Style) -> Style {
        if other.fg.is_some() {
            self.fg = other.fg;
        }
        if other.bg.is_some() {
            self.bg = other.bg;
        }
        if other.underline_color.is_some() {
            self.underline_color = other.underline_color;
        }
        if other.underline_style != UnderlineStyle::Straight {
            self.underline_style = other.underline_style;
        }
        self.modifiers.0 |= other.modifiers.0;
        self
    }
}

/// Alias matching the ratatui-era spelling used by most call sites.
pub use Modifiers as Modifier;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_remove_modifier_roundtrip() {
        let s = Style::default().add_modifier(Modifiers::BOLD);
        assert!(s.modifiers.contains(Modifiers::BOLD));
        let s = s.remove_modifier(Modifiers::BOLD);
        assert!(!s.modifiers.contains(Modifiers::BOLD));
    }

    #[test]
    fn patch_overlays_set_fields_only() {
        let base = Style::default().fg(Color::Red).bg(Color::Blue);
        let over = Style::default().bg(Color::Green).add_modifier(Modifiers::BOLD);
        let p = base.patch(over);
        // fg kept from base, bg replaced, modifier unioned.
        assert_eq!(p.fg, Some(Color::Red));
        assert_eq!(p.bg, Some(Color::Green));
        assert!(p.modifiers.contains(Modifiers::BOLD));
    }

    #[test]
    fn slt_builders_still_available() {
        let s = Style::new().fg(Color::Rgb(1, 2, 3)).bold();
        assert_eq!(s.fg, Some(Color::Rgb(1, 2, 3)));
        assert!(s.modifiers.contains(Modifiers::BOLD));
    }
}
