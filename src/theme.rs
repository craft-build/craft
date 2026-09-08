//! Compact desktop palette. The surfaces intentionally sit close together:
//! panes are separated by hairlines and selection fills rather than shadows.

pub const BG: u32 = 0x1b1d1f;
pub const PANEL_BG: u32 = 0x202225;
pub const INPUT_BG: u32 = 0x27292d;
pub const HOVER_BG: u32 = 0x2b2e33;
pub const FOOTER_BG: u32 = 0x1d1f22;
pub const BORDER: u32 = 0x303338;
pub const TERMINAL_BG: u32 = 0x181a1d;
pub const TERMINAL_BORDER: u32 = 0x292c31;

pub const TEXT_PRIMARY: u32 = 0xd7d9de;
pub const TEXT_SECONDARY: u32 = 0xa7abb3;
pub const TEXT_MUTED: u32 = 0x747983;

pub const ACCENT: u32 = 0x75a7df;
pub const ACCENT_DARK_TEXT: u32 = 0x111820;
pub const SELECTION: u32 = 0x30343a;

pub const DIFF_ADD_TEXT: u32 = 0x78b987;
pub const DIFF_DEL_TEXT: u32 = 0xe06c75;
/// 0.35 alpha over the terminal-green-tinted background used behind add lines.
pub const DIFF_ADD_BG: u32 = 0x0b261259;
/// 0.35 alpha over the red-tinted background used behind del lines.
pub const DIFF_DEL_BG: u32 = 0x3c161859;

pub const PENDING_CHIP_BG: u32 = 0x75a7df1f;
pub const PENDING_CHIP_BORDER: u32 = 0x75a7df66;

pub const OVERLAY: u32 = 0x00000066;

pub const FONT_FAMILY: &str = "Helvetica Neue";
pub const MONO_FONT_FAMILY: &str = "JetBrains Mono";
