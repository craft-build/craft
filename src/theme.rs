//! Craft desktop palette. Panes are separated by hairlines; blocks in the
//! thread are flat cards marked by a 3px left bar whose color says who or
//! what produced them (accent = agent/tool, blue = you, slate = neutral).

use std::borrow::Cow;

pub const BG: u32 = 0x11151d;
pub const PANEL_BG: u32 = 0x0e121a;
pub const CARD_BG: u32 = 0x161c27;
pub const USER_CARD_BG: u32 = 0x1a2230;
pub const STRIP_BG: u32 = 0x141a24;
pub const INPUT_BG: u32 = 0x11151d;
pub const HOVER_BG: u32 = 0x171e2b;
pub const BORDER: u32 = 0x1e2430;
pub const DIVIDER: u32 = 0x232a38;
pub const TRACK: u32 = 0x2a3140;
pub const NEUTRAL_BAR: u32 = 0x3d4658;
pub const TERMINAL_BORDER: u32 = 0x1e2430;

pub const TEXT_PRIMARY: u32 = 0xe6e9ef;
pub const TEXT_SECONDARY: u32 = 0xa6adbd;
pub const TEXT_MUTED: u32 = 0x6b7385;
pub const TEXT_FAINT: u32 = 0x4a536a;

/// Agent / tool / success color.
pub const ACCENT: u32 = 0x7ee787;
/// Everything authored by the user: messages, comments, review actions.
pub const USER: u32 = 0x6cb6ff;
pub const ACCENT_DARK_TEXT: u32 = 0x0d1017;
pub const SELECTION: u32 = 0x1a2130;

pub const DIFF_ADD_TEXT: u32 = ACCENT;
pub const DIFF_DEL_TEXT: u32 = 0xff8080;
pub const DIFF_ADD_BG: u32 = 0x18291fff;
pub const DIFF_DEL_BG: u32 = 0x2a1c22ff;
pub const DIFF_ADD_GUTTER: u32 = 0x4a7a55;
pub const DIFF_DEL_GUTTER: u32 = 0x7a4a52;

pub const OVERLAY: u32 = 0x00000080;

pub const FONT_FAMILY: &str = "IBM Plex Sans";
pub const MONO_FONT_FAMILY: &str = "Space Mono";

/// Bundled faces so the UI renders the same on machines without them.
pub fn load_fonts(cx: &mut gpui::App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Regular.otf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-Medium.otf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/IBMPlexSans-SemiBold.otf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/SpaceMono-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../assets/fonts/SpaceMono-Bold.ttf")),
    ];
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        eprintln!("Could not load bundled fonts: {error}");
    }
}
