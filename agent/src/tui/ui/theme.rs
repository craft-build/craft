//! Design tokens, ported from prototype/_ds/.../tokens/colors.css.

use ratatui::style::Color;

use crate::tui::provider::Tone;

// Surfaces
pub const BG_APP: Color = Color::Rgb(0x06, 0x09, 0x11); // ink-950
pub const BG_SURFACE: Color = Color::Rgb(0x0a, 0x0e, 0x1a); // ink-900
pub const BG_RAISED: Color = Color::Rgb(0x0f, 0x14, 0x24); // ink-800
pub const BG_OVERLAY: Color = Color::Rgb(0x16, 0x1d, 0x30); // ink-700
pub const BG_SUNKEN: Color = Color::Rgb(0x02, 0x04, 0x0a);

// Borders
pub const BORDER_SUBTLE: Color = Color::Rgb(0x1b, 0x23, 0x38); // line-800
pub const BORDER_STRONG: Color = Color::Rgb(0x3a, 0x46, 0x64); // line-600

// Text
pub const TEXT_PRIMARY: Color = Color::Rgb(0xf7, 0xf8, 0xfb);
pub const TEXT_SECONDARY: Color = Color::Rgb(0xa6, 0xac, 0xc0); // fog-300
pub const TEXT_TERTIARY: Color = Color::Rgb(0x5b, 0x67, 0x84); // fog-500
pub const TEXT_DISABLED: Color = Color::Rgb(0x40, 0x48, 0x5f);

// Accents
pub const ACCENT: Color = Color::Rgb(0x4f, 0x8d, 0xff); // blue-500
pub const BLUE_400: Color = Color::Rgb(0x6f, 0xa8, 0xff);
pub const CYAN: Color = Color::Rgb(0x22, 0xd3, 0xee);

// Status
pub const SUCCESS: Color = Color::Rgb(0x3d, 0xdc, 0x84);
pub const WARNING: Color = Color::Rgb(0xf0, 0xa9, 0x3e);
pub const DANGER: Color = Color::Rgb(0xf0, 0x45, 0x5f);

// Diff (add/del bg approximates the 12%-alpha tint over the surface)
pub const DIFF_ADD_TEXT: Color = Color::Rgb(0x7d, 0xe8, 0xa8);
pub const DIFF_ADD_BG: Color = Color::Rgb(0x10, 0x27, 0x27);
pub const DIFF_DEL_TEXT: Color = Color::Rgb(0xf2, 0x8a, 0x97);
pub const DIFF_DEL_BG: Color = Color::Rgb(0x26, 0x15, 0x22);

pub fn tone_color(tone: Tone) -> Color {
    match tone {
        Tone::Success => SUCCESS,
        Tone::Warning => WARNING,
        Tone::Danger => DANGER,
        Tone::Info => CYAN,
        Tone::Neutral => TEXT_TERTIARY,
    }
}
