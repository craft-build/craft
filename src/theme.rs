//! Color palette transliterated from the Forge design's oklch tokens into
//! sRGB hex, since GPUI has no oklch color type.

pub const BG: u32 = 0x0a0b0d;
pub const PANEL_BG: u32 = 0x111315;
pub const INPUT_BG: u32 = 0x1a1c1e;
pub const HOVER_BG: u32 = 0x222427;
pub const FOOTER_BG: u32 = 0x0c0d0f;
pub const BORDER: u32 = 0x2b2e32;
pub const TERMINAL_BG: u32 = 0x050607;
pub const TERMINAL_BORDER: u32 = 0x1d2022;

pub const TEXT_PRIMARY: u32 = 0xe7e8e9;
pub const TEXT_SECONDARY: u32 = 0x96989b;
pub const TEXT_MUTED: u32 = 0x5b5e61;

pub const ACCENT: u32 = 0x1eb5e0;
pub const ACCENT_DARK_TEXT: u32 = 0x030d11;
pub const SELECTION: u32 = 0x003444;

pub const DIFF_ADD_TEXT: u32 = 0x56ae6c;
pub const DIFF_DEL_TEXT: u32 = 0xe75f66;
/// 0.35 alpha over the terminal-green-tinted background used behind add lines.
pub const DIFF_ADD_BG: u32 = 0x0b261259;
/// 0.35 alpha over the red-tinted background used behind del lines.
pub const DIFF_DEL_BG: u32 = 0x3c161859;

pub const PENDING_CHIP_BG: u32 = 0x00344433;
pub const PENDING_CHIP_BORDER: u32 = 0x1eb5e066;

pub const OVERLAY: u32 = 0x00000066;

pub const FONT_FAMILY: &str = "JetBrains Mono";
