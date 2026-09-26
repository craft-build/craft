//! Runtime-swappable UI theme, ported from the reference `craft-ui/src/theme.rs`
//! (task F.5). Themes are Helix-style TOML compiled in via `include_str!` from
//! `themes/`; `set()` swaps a global `Arc<Theme>` with a generation counter and
//! restyles syntect highlighting via [`crate::markdown::highlight::set_theme`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use ratatui::style::Color;
use serde::Deserialize;

use crate::tui::provider::Tone;

pub const DEFAULT_THEME: &str = "craft";
const RESERVED_KEYS: &[&str] = &["palette", "ui", "inherits"];

// Fallback values: the pre-theme-picker hardcoded palette. Derivation falls
// back to these so a thin theme TOML degrades to the old look, not to black.
mod fallback {
    use ratatui::style::Color;

    pub const BG_APP: Color = Color::Rgb(0x06, 0x09, 0x11);
    pub const BG_SURFACE: Color = Color::Rgb(0x0a, 0x0e, 0x1a);
    pub const BG_RAISED: Color = Color::Rgb(0x0f, 0x14, 0x24);
    pub const BG_OVERLAY: Color = Color::Rgb(0x16, 0x1d, 0x30);
    pub const BG_SUNKEN: Color = Color::Rgb(0x02, 0x04, 0x0a);
    pub const BORDER_SUBTLE: Color = Color::Rgb(0x1b, 0x23, 0x38);
    pub const BORDER_STRONG: Color = Color::Rgb(0x3a, 0x46, 0x64);
    pub const TEXT_PRIMARY: Color = Color::Rgb(0xf7, 0xf8, 0xfb);
    pub const TEXT_SECONDARY: Color = Color::Rgb(0xa6, 0xac, 0xc0);
    pub const TEXT_TERTIARY: Color = Color::Rgb(0x5b, 0x67, 0x84);
    pub const TEXT_DISABLED: Color = Color::Rgb(0x40, 0x48, 0x5f);
    pub const ACCENT: Color = Color::Rgb(0x4f, 0x8d, 0xff);
    pub const BLUE_400: Color = Color::Rgb(0x6f, 0xa8, 0xff);
    pub const CYAN: Color = Color::Rgb(0x22, 0xd3, 0xee);
    pub const SUCCESS: Color = Color::Rgb(0x3d, 0xdc, 0x84);
    pub const WARNING: Color = Color::Rgb(0xf0, 0xa9, 0x3e);
    pub const DANGER: Color = Color::Rgb(0xf0, 0x45, 0x5f);
    pub const DIFF_ADD_TEXT: Color = Color::Rgb(0x7d, 0xe8, 0xa8);
    pub const DIFF_ADD_BG: Color = Color::Rgb(0x10, 0x27, 0x27);
    pub const DIFF_DEL_TEXT: Color = Color::Rgb(0xf2, 0x8a, 0x97);
    pub const DIFF_DEL_BG: Color = Color::Rgb(0x26, 0x15, 0x22);
}

pub struct ThemeEntry {
    pub name: &'static str,
    pub toml: &'static str,
}

macro_rules! bundled {
    ($($file:literal => $name:literal),* $(,)?) => {
        pub const BUNDLED_THEMES: &[ThemeEntry] = &[$(ThemeEntry { name: $name, toml: include_str!(concat!("themes/", $file)) }),*];
    };
}

bundled! {
    "ayu_dark.toml" => "ayu_dark",
    "ayu_light.toml" => "ayu_light",
    "ayu_mirage.toml" => "ayu_mirage",
    "carbonfox.toml" => "carbonfox",
    "catppuccin_frappe.toml" => "catppuccin_frappe",
    "catppuccin_latte.toml" => "catppuccin_latte",
    "catppuccin_macchiato.toml" => "catppuccin_macchiato",
    "catppuccin_mocha.toml" => "catppuccin_mocha",
    "craft.toml" => "craft",
    "dracula.toml" => "dracula",
    "everforest_dark.toml" => "everforest_dark",
    "fleet_dark.toml" => "fleet_dark",
    "github_dark.toml" => "github_dark",
    "gruvbox.toml" => "gruvbox",
    "gruvbox_light.toml" => "gruvbox_light",
    "kanagawa.toml" => "kanagawa",
    "material_darker.toml" => "material_darker",
    "monokai_pro.toml" => "monokai_pro",
    "night_owl.toml" => "night_owl",
    "nightfox.toml" => "nightfox",
    "nord.toml" => "nord",
    "onedark.toml" => "onedark",
    "rose_pine.toml" => "rose_pine",
    "rose_pine_dawn.toml" => "rose_pine_dawn",
    "rose_pine_moon.toml" => "rose_pine_moon",
    "solarized_dark.toml" => "solarized_dark",
    "solarized_light.toml" => "solarized_light",
    "tokyonight.toml" => "tokyonight",
    "vscode_dark_plus.toml" => "vscode_dark_plus",
    "zenburn.toml" => "zenburn",
}

static THEME: LazyLock<RwLock<Arc<Theme>>> =
    LazyLock::new(|| RwLock::new(Arc::new(Theme::default())));

static GENERATION: AtomicU64 = AtomicU64::new(0);

static CURRENT_NAME: Mutex<Option<String>> = Mutex::new(None);

/// Tests that mutate the global theme share this lock so parallel test
/// threads can't observe each other's swaps.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// The active theme. Clone the `Arc` once per render pass; never hold the
/// guard across a frame.
pub fn current() -> Arc<Theme> {
    THEME.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Swap the active theme, restyle syntect highlighting, and bump the
/// generation so derived caches know they are stale.
pub fn set(theme: Theme) {
    crate::markdown::highlight::set_theme(theme.syntax.clone());
    *THEME.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(theme);
    GENERATION.fetch_add(1, Ordering::Release);
}

pub fn set_named(name: &str) -> Result<(), String> {
    let theme = load_by_name(name)?;
    set(theme);
    // Name lands after the swap so a concurrent reader never sees a name
    // ahead of the palette it names.
    *CURRENT_NAME.lock().unwrap() = Some(name.to_owned());
    Ok(())
}

/// Bumped by every [`set`]. Anything derived from the theme (highlight
/// caches, painted lines) is stale once this changes. Mirrors the reference
/// registry API; highlight uses its own generation internally.
#[allow(dead_code)]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn load_by_name(name: &str) -> Result<Theme, String> {
    BUNDLED_THEMES
        .iter()
        .find(|e| e.name == name)
        .map(|e| Theme::from_toml(e.toml))
        .unwrap_or_else(|| Err(format!("unknown theme: {name}")))
}

pub fn all_theme_names() -> Vec<String> {
    let mut names: Vec<String> = BUNDLED_THEMES.iter().map(|e| e.name.to_owned()).collect();
    names.sort();
    names.dedup();
    names
}

pub fn current_theme_name() -> String {
    CURRENT_NAME
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| DEFAULT_THEME.to_owned())
}

#[derive(Clone)]
pub struct Theme {
    // Surfaces
    pub bg_app: Color,
    pub bg_surface: Color,
    pub bg_raised: Color,
    pub bg_overlay: Color,
    pub bg_sunken: Color,

    // Borders
    pub border_subtle: Color,
    pub border_strong: Color,

    // Text
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_tertiary: Color,
    pub text_disabled: Color,

    // Accents
    pub accent: Color,
    pub blue_400: Color,
    pub cyan: Color,

    // Mode indicator: Build rides the accent, Plan the planning cyan.
    pub mode_build: Color,
    pub mode_plan: Color,

    // Status
    pub success: Color,
    pub warning: Color,
    pub danger: Color,

    // Diff
    pub diff_add_text: Color,
    pub diff_add_bg: Color,
    pub diff_del_text: Color,
    pub diff_del_bg: Color,

    pub syntax: syntect::highlighting::Theme,
}

impl Theme {
    pub fn tone_color(&self, tone: Tone) -> Color {
        match tone {
            Tone::Success => self.success,
            Tone::Warning => self.warning,
            Tone::Danger => self.danger,
            Tone::Info => self.cyan,
            Tone::Neutral => self.text_tertiary,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        load_by_name(DEFAULT_THEME).unwrap_or_else(|_| fallback_theme())
    }
}

fn fallback_theme() -> Theme {
    use fallback::*;
    Theme {
        bg_app: BG_APP,
        bg_surface: BG_SURFACE,
        bg_raised: BG_RAISED,
        bg_overlay: BG_OVERLAY,
        bg_sunken: BG_SUNKEN,
        border_subtle: BORDER_SUBTLE,
        border_strong: BORDER_STRONG,
        text_primary: TEXT_PRIMARY,
        text_secondary: TEXT_SECONDARY,
        text_tertiary: TEXT_TERTIARY,
        text_disabled: TEXT_DISABLED,
        accent: ACCENT,
        blue_400: BLUE_400,
        cyan: CYAN,
        mode_build: MODE_BUILD,
        mode_plan: MODE_PLAN,
        success: SUCCESS,
        warning: WARNING,
        danger: DANGER,
        diff_add_text: DIFF_ADD_TEXT,
        diff_add_bg: DIFF_ADD_BG,
        diff_del_text: DIFF_DEL_TEXT,
        diff_del_bg: DIFF_DEL_BG,
        syntax: build_syntax_theme(&toml::Table::new(), &HashMap::new()),
    }
}

// `mode_build` in `fallback_theme` needs the const-to-const aliasing the old
// module had; replicate it here.
mod mode_aliases {
    pub const MODE_BUILD: ratatui::style::Color = super::fallback::ACCENT;
    pub const MODE_PLAN: ratatui::style::Color = super::fallback::CYAN;
}
use mode_aliases::{MODE_BUILD, MODE_PLAN};

#[derive(Deserialize)]
struct StyleDef {
    fg: Option<String>,
    bg: Option<String>,
    #[serde(default)]
    modifiers: Vec<String>,
}

fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    let hex = s.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some(Color::Rgb(r * 17, g * 17, b * 17))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

fn resolve_color(name: &str, palette: &HashMap<String, Color>) -> Option<Color> {
    palette.get(name).copied().or_else(|| parse_color(name))
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            Color::Rgb(lerp_u8(ar, br, t), lerp_u8(ag, bg, t), lerp_u8(ab, bb, t))
        }
        // Non-RGB ends (Reset, ANSI names) can't be lerped; bias toward `a`.
        _ => a,
    }
}

/// Sunken surface: darker than the base. On light themes (bg brighter than
/// fg) lerp toward the fg side instead of black so the surface stays visible.
fn sunken(bg: Color, fg: Color) -> Color {
    fn luminance(c: &Color) -> Option<f32> {
        match c {
            Color::Rgb(r, g, b) => Some(0.299 * *r as f32 + 0.587 * *g as f32 + 0.114 * *b as f32),
            _ => None,
        }
    }
    match (luminance(&bg), luminance(&fg)) {
        (Some(bg_l), Some(fg_l)) if bg_l > fg_l => lerp_color(bg, fg, 0.25),
        _ => blend_to_black(bg, 0.5),
    }
}

fn blend_to_black(c: Color, t: f32) -> Color {
    match c {
        Color::Rgb(r, g, b) => Color::Rgb(lerp_u8(r, 0, t), lerp_u8(g, 0, t), lerp_u8(b, 0, t)),
        _ => c,
    }
}

fn resolve_modifier(name: &str) -> ratatui::style::Modifier {
    use ratatui::style::Modifier;
    match name {
        "bold" => Modifier::BOLD,
        "italic" => Modifier::ITALIC,
        "underlined" => Modifier::UNDERLINED,
        "crossed_out" => Modifier::CROSSED_OUT,
        "dim" => Modifier::DIM,
        "reversed" => Modifier::REVERSED,
        _ => Modifier::empty(),
    }
}

fn resolve_style(def: &StyleDef, palette: &HashMap<String, Color>) -> ratatui::style::Style {
    let mut style = ratatui::style::Style::new();
    if let Some(fg) = def.fg.as_ref().and_then(|n| resolve_color(n, palette)) {
        style = style.fg(fg);
    }
    if let Some(bg) = def.bg.as_ref().and_then(|n| resolve_color(n, palette)) {
        style = style.bg(bg);
    }
    for m in &def.modifiers {
        style = style.add_modifier(resolve_modifier(m));
    }
    style
}

fn style_bg(def: &StyleDef, palette: &HashMap<String, Color>) -> Option<Color> {
    resolve_style(def, palette).bg
}

fn style_fg(def: &StyleDef, palette: &HashMap<String, Color>) -> Option<Color> {
    resolve_style(def, palette).fg
}

// Helix scope keys -> TextMate scope selectors, ported from the reference.
const HELIX_TO_TEXTMATE: &[(&str, &str)] = &[
    ("comment", "comment, comment punctuation.definition.comment"),
    (
        "comment.line",
        "comment.line, comment.line punctuation.definition.comment",
    ),
    (
        "comment.block",
        "comment.block, comment.block punctuation.definition.comment",
    ),
    (
        "comment.line.documentation",
        "comment.line.documentation, comment.line.documentation punctuation.definition.comment",
    ),
    (
        "comment.block.documentation",
        "comment.block.documentation, comment.block.documentation punctuation.definition.comment",
    ),
    ("string", "string, string punctuation.definition.string"),
    (
        "string.regexp",
        "string.regexp, string.regexp punctuation.definition.string",
    ),
    (
        "string.special",
        "string.special, string.quoted.single punctuation.definition.string, string.quoted.double.raw punctuation.definition.string",
    ),
    ("function", "entity.name.function, variable.function"),
    ("function.builtin", "support.function"),
    (
        "function.call",
        "entity.name.function, variable.function, support.function",
    ),
    (
        "function.macro",
        "entity.name.function.macro, support.macro",
    ),
    (
        "function.method",
        "entity.name.function, meta.function-call",
    ),
    ("constructor", "entity.name.function.constructor"),
    (
        "type",
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.union, entity.name.impl, support.type, support.class, meta.generic",
    ),
    ("type.builtin", "support.type, storage.type.primitive"),
    ("type.enum.variant", "entity.name.type.enum"),
    ("tag", "entity.name.tag"),
    ("tag.attribute", "entity.other.attribute-name"),
    ("tag.delimiter", "punctuation.definition.tag"),
    ("variable", "variable.other"),
    ("variable.builtin", "variable.language"),
    ("variable.parameter", "variable.parameter"),
    (
        "variable.other.member",
        "variable.other.member, variable.other.property",
    ),
    (
        "constant",
        "constant, variable.other.constant, entity.name.constant",
    ),
    ("constant.builtin", "constant.language"),
    (
        "constant.builtin.boolean",
        "constant.language.boolean, constant.language",
    ),
    (
        "constant.character.escape",
        "constant.character.escape, constant.character.escaped",
    ),
    (
        "keyword.storage.type",
        "storage.type, keyword.declaration, keyword.declaration.function, keyword.declaration.class, keyword.declaration.struct, keyword.declaration.enum, keyword.declaration.trait, keyword.declaration.impl",
    ),
    ("keyword.storage.modifier", "storage.modifier"),
    (
        "keyword.function",
        "keyword.declaration.function, storage.type.function",
    ),
    (
        "keyword.control.import",
        "keyword.control.import, keyword.other",
    ),
    ("keyword.return", "keyword.control.return, keyword.control"),
    ("keyword.directive", "meta.preprocessor"),
    ("keyword.control.exception", "keyword.control.exception"),
    ("punctuation", "punctuation, punctuation.accessor.dot"),
    (
        "punctuation.special",
        "punctuation.section.embedded, punctuation.section.interpolation, punctuation.separator.namespace, punctuation.accessor",
    ),
    ("label", "entity.name.label, storage.modifier.lifetime"),
    (
        "attribute",
        "entity.other.attribute-name, meta.annotation, variable.annotation, meta.annotation punctuation.definition.annotation, meta.annotation punctuation.section.group",
    ),
    (
        "namespace",
        "entity.name.namespace, entity.name.module, meta.path",
    ),
    (
        "markup.raw",
        "markup.raw, markup.raw.inline, markup.raw.block",
    ),
    ("markup.link.url", "markup.underline.link"),
    ("operator", "keyword.operator"),
];

fn helix_to_textmate_scope(key: &str) -> &str {
    for &(helix, tm) in HELIX_TO_TEXTMATE {
        if key == helix {
            return tm;
        }
    }
    key
}

fn parse_syn_color_value(s: &str, raw_palette: &HashMap<String, String>) -> Option<SynColor> {
    let resolved = if s.starts_with('#') {
        s
    } else {
        raw_palette.get(s).map(String::as_str).unwrap_or(s)
    };
    parse_color(resolved).and_then(|c| match c {
        Color::Rgb(r, g, b) => Some(SynColor { r, g, b, a: 0xFF }),
        _ => None,
    })
}

use syntect::highlighting::{
    Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, ThemeItem, ThemeSettings,
};

fn resolve_font_style(modifiers: &[String]) -> FontStyle {
    let mut fs = FontStyle::empty();
    for m in modifiers {
        match m.as_str() {
            "bold" => fs |= FontStyle::BOLD,
            "italic" => fs |= FontStyle::ITALIC,
            "underlined" => fs |= FontStyle::UNDERLINE,
            _ => {}
        }
    }
    fs
}

fn style_def_to_syn(def: &StyleDef, raw_palette: &HashMap<String, String>) -> StyleModifier {
    let has_color = def.fg.is_some() || def.bg.is_some();
    StyleModifier {
        foreground: def
            .fg
            .as_ref()
            .and_then(|n| parse_syn_color_value(n, raw_palette)),
        background: def
            .bg
            .as_ref()
            .and_then(|n| parse_syn_color_value(n, raw_palette)),
        font_style: if def.modifiers.is_empty() {
            if has_color {
                Some(FontStyle::empty())
            } else {
                None
            }
        } else {
            Some(resolve_font_style(&def.modifiers))
        },
    }
}

fn build_syntax_theme(
    toml_table: &toml::Table,
    raw_palette: &HashMap<String, String>,
) -> syntect::highlighting::Theme {
    let fg = parse_syn_color_value("foreground", raw_palette);
    let bg = parse_syn_color_value("background", raw_palette);

    let settings = ThemeSettings {
        foreground: fg,
        background: bg,
        caret: fg,
        line_highlight: parse_syn_color_value("current_line", raw_palette)
            .or_else(|| parse_syn_color_value("selection", raw_palette)),
        selection: parse_syn_color_value("selection", raw_palette)
            .or_else(|| parse_syn_color_value("current_line", raw_palette)),
        ..Default::default()
    };

    let mut scopes = Vec::new();
    for (key, value) in toml_table {
        if RESERVED_KEYS.contains(&key.as_str()) || key.starts_with("ui.") {
            continue;
        }
        let Some(table) = value.as_table() else {
            continue;
        };
        let def: StyleDef = match toml::Value::Table(table.clone()).try_into() {
            Ok(d) => d,
            Err(_) => continue,
        };
        let tm_scope = helix_to_textmate_scope(key);
        let Ok(scope) = tm_scope.parse::<ScopeSelectors>() else {
            continue;
        };
        scopes.push(ThemeItem {
            scope,
            style: style_def_to_syn(&def, raw_palette),
        });
    }

    syntect::highlighting::Theme {
        name: None,
        author: None,
        settings,
        scopes,
    }
}

impl Theme {
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        let full_table: toml::Table = toml::from_str(toml_str).map_err(|e| e.to_string())?;

        let raw_palette: HashMap<String, String> = full_table
            .get("palette")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();

        let palette: HashMap<String, Color> = raw_palette
            .iter()
            .filter_map(|(k, v)| parse_color(v).map(|c| (k.clone(), c)))
            .collect();

        let ui: HashMap<String, StyleDef> = full_table
            .get("ui")
            .and_then(|v| v.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| {
                        let def: StyleDef = v.clone().try_into().ok()?;
                        Some((k.clone(), def))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let color = |key: &str| -> Option<Color> { palette.get(key).copied() };
        let bg = color("background").unwrap_or(fallback::BG_APP);
        let fg = color("foreground").unwrap_or(fallback::TEXT_PRIMARY);

        // Derive the layer stack from the palette when the theme does not pin
        // it explicitly: `layer01/02` bg, then `current_line`/`selection`, then
        // a bg<->fg lerp (the reference's `layer_color` approach).
        let layer = |ui_key: &str, primary: &[&str], t: f32| -> Color {
            if let Some(def) = ui.get(ui_key)
                && let Some(c) = style_bg(def, &palette)
            {
                return c;
            }
            for key in primary {
                if let Some(c) = color(key) {
                    return c;
                }
            }
            lerp_color(bg, fg, t)
        };

        let derived = |ui_key: &str, palette_keys: &[&str], default: Color| -> Color {
            if let Some(def) = ui.get(ui_key)
                && let Some(c) = style_fg(def, &palette)
            {
                return c;
            }
            palette_keys
                .iter()
                .find_map(|k| color(k))
                .unwrap_or(default)
        };

        // Borders: ui override, then palette, then a bg<->fg lerp step.
        let derived_border = |ui_key: &str, palette_keys: &[&str], t: f32| -> Color {
            derived(ui_key, palette_keys, lerp_color(bg, fg, t))
        };

        let accent = derived("accent", &["blue", "cyan", "primary"], fallback::ACCENT);

        let bg_surface = layer("surface", &["background_2"], 0.035);
        let bg_raised = layer("layer01", &["current_line"], 0.09);
        // The overlay must read as a step above the raised surface (it is
        // the selected-row background in every picker); if a theme maps both
        // to the same color, push the overlay one lerp step further.
        let bg_overlay = {
            let c = layer("layer02", &["selection"], 0.18);
            if c == bg_raised {
                lerp_color(bg, fg, 0.28)
            } else {
                c
            }
        };

        Ok(Self {
            bg_app: bg,
            bg_surface,
            bg_raised,
            bg_overlay,
            // On light themes blend toward the foreground instead of black,
            // so the sunken surface darkens rather than vanishes.
            bg_sunken: color("background_2").map_or_else(|| sunken(bg, fg), |c| sunken(c, fg)),
            border_subtle: derived_border("border_subtle", &[], 0.12),
            border_strong: derived_border("border_strong", &["border"], 0.28),
            text_primary: derived("text_primary", &["foreground"], fg),
            text_secondary: derived(
                "text_secondary",
                &["fog_200", "comment_lighter"],
                lerp_color(fg, bg, 0.30),
            ),
            text_tertiary: derived("text_helper", &["comment"], lerp_color(fg, bg, 0.50)),
            text_disabled: derived("text_disabled", &["comment"], lerp_color(fg, bg, 0.65)),
            accent,
            blue_400: color("blue_bright").unwrap_or_else(|| lerp_color(accent, fg, 0.35)),
            cyan: derived("cyan", &["cyan"], fallback::CYAN),
            mode_build: accent,
            mode_plan: color("mode_plan")
                .unwrap_or_else(|| color("cyan").unwrap_or(fallback::CYAN)),
            success: derived("success", &["green"], fallback::SUCCESS),
            warning: derived("warning", &["yellow", "orange"], fallback::WARNING),
            danger: derived("error", &["red"], fallback::DANGER),
            diff_add_text: color("green").unwrap_or(fallback::DIFF_ADD_TEXT),
            diff_add_bg: ui
                .get("diff_new")
                .and_then(|d| style_bg(d, &palette))
                .unwrap_or_else(|| {
                    lerp_color(bg, color("green").unwrap_or(fallback::SUCCESS), 0.15)
                }),
            diff_del_text: color("red").unwrap_or(fallback::DIFF_DEL_TEXT),
            diff_del_bg: ui
                .get("diff_old")
                .and_then(|d| style_bg(d, &palette))
                .unwrap_or_else(|| lerp_color(bg, color("red").unwrap_or(fallback::DANGER), 0.15)),
            syntax: build_syntax_theme(&full_table, &raw_palette),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_themes_parse() {
        for entry in BUNDLED_THEMES {
            let theme =
                Theme::from_toml(entry.toml).unwrap_or_else(|e| panic!("{}: {e}", entry.name));
            // Surfaces and text must resolve to concrete RGB colors, not the
            // fallback black hole.
            assert!(
                matches!(theme.bg_app, Color::Rgb(..)),
                "{}: bg_app not RGB",
                entry.name
            );
            assert!(
                matches!(theme.text_primary, Color::Rgb(..)),
                "{}: text_primary not RGB",
                entry.name
            );
        }
    }

    #[test]
    fn all_names_sorted_unique_with_default() {
        let names = all_theme_names();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(names.len() >= 29);
        assert!(names.contains(&DEFAULT_THEME.to_owned()));
    }

    #[test]
    fn unknown_theme_errors() {
        assert!(load_by_name("nope").is_err());
    }

    #[test]
    fn invalid_toml_errors() {
        assert!(Theme::from_toml("not valid {{{{").is_err());
    }

    #[test]
    fn nord_maps_palette() {
        let theme = load_by_name("nord").unwrap();
        assert_eq!(theme.bg_app, Color::Rgb(0x2e, 0x34, 0x40)); // palette background
        assert_eq!(theme.success, Color::Rgb(0xa3, 0xbe, 0x8c)); // palette green
        assert_eq!(theme.cyan, Color::Rgb(0x88, 0xc0, 0xd0));
        // Themes must actually differ from the fallback palette.
        assert_ne!(theme.bg_app, fallback::BG_APP);
    }

    #[test]
    fn default_craft_theme_reproduces_the_original_palette() {
        let t = load_by_name("craft").unwrap();
        assert_eq!(t.bg_app, fallback::BG_APP);
        assert_eq!(t.bg_surface, fallback::BG_SURFACE);
        assert_eq!(t.bg_raised, fallback::BG_RAISED);
        assert_eq!(t.bg_overlay, fallback::BG_OVERLAY);
        assert_eq!(t.border_subtle, fallback::BORDER_SUBTLE);
        assert_eq!(t.border_strong, fallback::BORDER_STRONG);
        assert_eq!(t.text_primary, fallback::TEXT_PRIMARY);
        assert_eq!(t.text_secondary, fallback::TEXT_SECONDARY);
        assert_eq!(t.text_tertiary, fallback::TEXT_TERTIARY);
        assert_eq!(t.text_disabled, fallback::TEXT_DISABLED);
        assert_eq!(t.accent, fallback::ACCENT);
        assert_eq!(t.blue_400, fallback::BLUE_400);
        assert_eq!(t.cyan, fallback::CYAN);
        assert_eq!(t.mode_build, fallback::ACCENT);
        assert_eq!(t.mode_plan, fallback::CYAN);
        assert_eq!(t.success, fallback::SUCCESS);
        assert_eq!(t.warning, fallback::WARNING);
        assert_eq!(t.danger, fallback::DANGER);
    }

    /// The pickers show selection as overlay-vs-raised background; they must
    /// never collide on any bundled theme or the selected row vanishes.
    #[test]
    fn every_theme_keeps_overlay_distinct_from_raised() {
        for entry in BUNDLED_THEMES {
            let t = Theme::from_toml(entry.toml).unwrap();
            assert_ne!(
                t.bg_overlay, t.bg_raised,
                "{}: selected-row bg equals unselected bg",
                entry.name
            );
        }
    }

    #[test]
    fn light_theme_differs_from_dark() {
        let latte = load_by_name("catppuccin_latte").unwrap();
        let mocha = load_by_name("catppuccin_mocha").unwrap();
        assert_ne!(latte.bg_app, mocha.bg_app);
        // A light theme's background is brighter than its foreground is dark...
        // simpler invariant: they differ on both axes.
        assert_ne!(latte.text_primary, mocha.text_primary);
    }

    #[test]
    fn syntax_theme_has_scopes_and_settings() {
        let theme = load_by_name("nord").unwrap();
        assert!(theme.syntax.settings.foreground.is_some());
        assert!(theme.syntax.scopes.iter().any(|i| {
            i.scope
                .selectors
                .iter()
                .any(|s| s.path.to_string().contains("string"))
        }));
    }

    #[test]
    fn set_bumps_generation_and_swaps() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = generation();
        set_named("dracula").unwrap();
        assert_eq!(generation(), before + 1);
        assert_eq!(current_theme_name(), "dracula");
        assert_ne!(current().bg_app, fallback::BG_APP);
        // restore default for other tests
        set_named(DEFAULT_THEME).unwrap();
        assert_eq!(current_theme_name(), DEFAULT_THEME);
    }
}
