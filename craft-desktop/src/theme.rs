use std::collections::HashMap;

use craft_storage::StateDir;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const DEFAULT_THEME: &str = "dracula";
const RESERVED_KEYS: &[&str] = &["palette", "ui", "inherits"];

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

pub struct ThemeEntry {
    pub name: &'static str,
    pub label: &'static str,
    pub toml: &'static str,
}

pub static BUNDLED_THEMES: &[ThemeEntry] = &[
    ThemeEntry {
        name: "ayu_dark",
        label: "Ayu Dark",
        toml: include_str!("../../craft-ui/src/themes/ayu_dark.toml"),
    },
    ThemeEntry {
        name: "ayu_light",
        label: "Ayu Light",
        toml: include_str!("../../craft-ui/src/themes/ayu_light.toml"),
    },
    ThemeEntry {
        name: "ayu_mirage",
        label: "Ayu Mirage",
        toml: include_str!("../../craft-ui/src/themes/ayu_mirage.toml"),
    },
    ThemeEntry {
        name: "carbonfox",
        label: "Carbonfox",
        toml: include_str!("../../craft-ui/src/themes/carbonfox.toml"),
    },
    ThemeEntry {
        name: "catppuccin_frappe",
        label: "Catppuccin Frappe",
        toml: include_str!("../../craft-ui/src/themes/catppuccin_frappe.toml"),
    },
    ThemeEntry {
        name: "catppuccin_latte",
        label: "Catppuccin Latte",
        toml: include_str!("../../craft-ui/src/themes/catppuccin_latte.toml"),
    },
    ThemeEntry {
        name: "catppuccin_macchiato",
        label: "Catppuccin Macchiato",
        toml: include_str!("../../craft-ui/src/themes/catppuccin_macchiato.toml"),
    },
    ThemeEntry {
        name: "catppuccin_mocha",
        label: "Catppuccin Mocha",
        toml: include_str!("../../craft-ui/src/themes/catppuccin_mocha.toml"),
    },
    ThemeEntry {
        name: "dracula",
        label: "Dracula",
        toml: include_str!("../../craft-ui/src/themes/dracula.toml"),
    },
    ThemeEntry {
        name: "everforest_dark",
        label: "Everforest Dark",
        toml: include_str!("../../craft-ui/src/themes/everforest_dark.toml"),
    },
    ThemeEntry {
        name: "fleet_dark",
        label: "Fleet Dark",
        toml: include_str!("../../craft-ui/src/themes/fleet_dark.toml"),
    },
    ThemeEntry {
        name: "github_dark",
        label: "GitHub Dark",
        toml: include_str!("../../craft-ui/src/themes/github_dark.toml"),
    },
    ThemeEntry {
        name: "gruvbox",
        label: "Gruvbox",
        toml: include_str!("../../craft-ui/src/themes/gruvbox.toml"),
    },
    ThemeEntry {
        name: "gruvbox_light",
        label: "Gruvbox Light",
        toml: include_str!("../../craft-ui/src/themes/gruvbox_light.toml"),
    },
    ThemeEntry {
        name: "kanagawa",
        label: "Kanagawa",
        toml: include_str!("../../craft-ui/src/themes/kanagawa.toml"),
    },
    ThemeEntry {
        name: "material_darker",
        label: "Material Darker",
        toml: include_str!("../../craft-ui/src/themes/material_darker.toml"),
    },
    ThemeEntry {
        name: "monokai_pro",
        label: "Monokai Pro",
        toml: include_str!("../../craft-ui/src/themes/monokai_pro.toml"),
    },
    ThemeEntry {
        name: "night_owl",
        label: "Night Owl",
        toml: include_str!("../../craft-ui/src/themes/night_owl.toml"),
    },
    ThemeEntry {
        name: "nightfox",
        label: "Nightfox",
        toml: include_str!("../../craft-ui/src/themes/nightfox.toml"),
    },
    ThemeEntry {
        name: "nord",
        label: "Nord",
        toml: include_str!("../../craft-ui/src/themes/nord.toml"),
    },
    ThemeEntry {
        name: "onedark",
        label: "Onedark",
        toml: include_str!("../../craft-ui/src/themes/onedark.toml"),
    },
    ThemeEntry {
        name: "rose_pine",
        label: "Rose Pine",
        toml: include_str!("../../craft-ui/src/themes/rose_pine.toml"),
    },
    ThemeEntry {
        name: "rose_pine_dawn",
        label: "Rose Pine Dawn",
        toml: include_str!("../../craft-ui/src/themes/rose_pine_dawn.toml"),
    },
    ThemeEntry {
        name: "rose_pine_moon",
        label: "Rose Pine Moon",
        toml: include_str!("../../craft-ui/src/themes/rose_pine_moon.toml"),
    },
    ThemeEntry {
        name: "solarized_dark",
        label: "Solarized Dark",
        toml: include_str!("../../craft-ui/src/themes/solarized_dark.toml"),
    },
    ThemeEntry {
        name: "solarized_light",
        label: "Solarized Light",
        toml: include_str!("../../craft-ui/src/themes/solarized_light.toml"),
    },
    ThemeEntry {
        name: "tokyonight",
        label: "Tokyo Night",
        toml: include_str!("../../craft-ui/src/themes/tokyonight.toml"),
    },
    ThemeEntry {
        name: "vscode_dark_plus",
        label: "VS Code Dark+",
        toml: include_str!("../../craft-ui/src/themes/vscode_dark_plus.toml"),
    },
    ThemeEntry {
        name: "zenburn",
        label: "Zenburn",
        toml: include_str!("../../craft-ui/src/themes/zenburn.toml"),
    },
];

#[derive(Serialize, Debug)]
pub struct ThemeName {
    pub name: String,
    pub label: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ThemeTokens {
    pub name: String,
    pub label: String,
    pub bg: String,
    pub bg_elevated: String,
    pub bg_inset: String,
    pub border: String,
    pub text: String,
    pub text_dim: String,
    pub text_faint: String,
    pub accent: String,
    pub accent_dim: String,
    pub accent_dim2: String,
    pub accent_text: String,
    pub success: String,
    pub danger: String,
    pub warning: String,
    pub warning_dim: String,
    pub mode_colors: ModeColors,
    pub syntax_theme: Value,
}

#[derive(Serialize, Debug)]
pub struct ModeColors {
    pub build: String,
    pub plan: String,
    pub flow: String,
}

#[derive(Deserialize)]
struct StyleDef {
    #[serde(default)]
    fg: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    bg: Option<String>,
    #[serde(default)]
    modifiers: Vec<String>,
}

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

fn hex_string(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}

fn parse_hex_rgb(s: &str) -> Option<Rgb> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

fn helix_to_textmate_scope(key: &str) -> &str {
    for &(helix, tm) in HELIX_TO_TEXTMATE {
        if key == helix {
            return tm;
        }
    }
    key
}

fn resolve_palette_color(name: &str, palette: &HashMap<String, Rgb>) -> Option<Rgb> {
    if name.starts_with('#') {
        parse_hex_rgb(name)
    } else {
        palette.get(name).copied()
    }
}

fn resolve_palette_color_fallback(
    name: &str,
    palette: &HashMap<String, Rgb>,
    raw_palette: &HashMap<String, String>,
) -> Option<Rgb> {
    resolve_palette_color(name, palette)
        .or_else(|| raw_palette.get(name).and_then(|s| parse_hex_rgb(s)))
}

fn parse_palette(full_table: &toml::Table) -> HashMap<String, Rgb> {
    full_table
        .get("palette")
        .and_then(|v| v.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| v.as_str().and_then(parse_hex_rgb).map(|c| (k.clone(), c)))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_raw_palette(full_table: &toml::Table) -> HashMap<String, String> {
    full_table
        .get("palette")
        .and_then(|v| v.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_ui_defs(full_table: &toml::Table) -> HashMap<String, StyleDef> {
    full_table
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
        .unwrap_or_default()
}

fn scope_fg(
    full_table: &toml::Table,
    palette: &HashMap<String, Rgb>,
    raw_palette: &HashMap<String, String>,
    scope: &str,
) -> Option<Rgb> {
    let table = full_table.get(scope)?.as_table()?;
    let fg_val = table.get("fg")?.as_str()?;
    resolve_palette_color(fg_val, palette)
        .or_else(|| raw_palette.get(fg_val).and_then(|s| parse_hex_rgb(s)))
}

fn lerp(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)) as u8
}

fn lerp_rgb(a: Rgb, b: Rgb, t: f32) -> Rgb {
    Rgb(lerp(a.0, b.0, t), lerp(a.1, b.1, t), lerp(a.2, b.2, t))
}

fn color_of(palette: &HashMap<String, Rgb>, key: &str) -> Option<Rgb> {
    palette.get(key).copied()
}

fn ui_fg(ui: &HashMap<String, StyleDef>, palette: &HashMap<String, Rgb>, key: &str) -> Option<Rgb> {
    ui.get(key).and_then(|d| {
        d.fg.as_ref()
            .and_then(|n| resolve_palette_color(n, palette))
    })
}

fn derived_color(
    palette: &HashMap<String, Rgb>,
    ui: &HashMap<String, StyleDef>,
    full_table: &toml::Table,
    raw_palette: &HashMap<String, String>,
    ui_key: &str,
    scopes: &[&str],
) -> Option<Rgb> {
    if let Some(c) = color_of(palette, ui_key) {
        return Some(c);
    }
    if let Some(c) = ui_fg(ui, palette, ui_key) {
        return Some(c);
    }
    for scope in scopes {
        if let Some(c) = scope_fg(full_table, palette, raw_palette, scope) {
            return Some(c);
        }
    }
    None
}

fn build_syntax_theme(
    full_table: &toml::Table,
    palette: &HashMap<String, Rgb>,
    raw_palette: &HashMap<String, String>,
    background: Option<Rgb>,
    foreground: Option<Rgb>,
) -> Value {
    let mut settings = Map::new();
    if let Some(bg) = background {
        settings.insert("background".to_string(), Value::String(hex_string(bg)));
    }
    if let Some(fg) = foreground {
        settings.insert("foreground".to_string(), Value::String(hex_string(fg)));
        settings.insert("caret".to_string(), Value::String(hex_string(fg)));
    }
    if let Some(c) = resolve_palette_color_fallback("current_line", palette, raw_palette)
        .or_else(|| resolve_palette_color_fallback("selection", palette, raw_palette))
    {
        settings.insert("lineHighlight".to_string(), Value::String(hex_string(c)));
    }
    if let Some(c) = resolve_palette_color_fallback("selection", palette, raw_palette)
        .or_else(|| resolve_palette_color_fallback("current_line", palette, raw_palette))
    {
        settings.insert("selection".to_string(), Value::String(hex_string(c)));
    }

    let mut token_colors: Vec<Value> = Vec::new();

    for (key, value) in full_table {
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
        let Some(fg_name) = def.fg.as_ref() else {
            continue;
        };
        let Some(fg) = resolve_palette_color(fg_name, palette)
            .or_else(|| raw_palette.get(fg_name).and_then(|s| parse_hex_rgb(s)))
        else {
            continue;
        };
        let tm_scope = helix_to_textmate_scope(key);
        let mut entry = Map::new();
        entry.insert("scope".to_string(), Value::String(tm_scope.to_string()));
        let mut style = Map::new();
        style.insert("foreground".to_string(), Value::String(hex_string(fg)));
        if def.modifiers.iter().any(|m| m == "bold") {
            style.insert("fontStyle".to_string(), Value::String("bold".to_string()));
        } else if def.modifiers.iter().any(|m| m == "italic") {
            style.insert("fontStyle".to_string(), Value::String("italic".to_string()));
        } else if def.modifiers.iter().any(|m| m == "underlined") {
            style.insert(
                "fontStyle".to_string(),
                Value::String("underline".to_string()),
            );
        }
        entry.insert("settings".to_string(), Value::Object(style));
        token_colors.push(Value::Object(entry));
    }

    let mut root = Map::new();
    root.insert("name".to_string(), Value::String("craft".to_string()));
    root.insert("type".to_string(), Value::String("dark".to_string()));
    root.insert("settings".to_string(), Value::Object(settings));
    root.insert("tokenColors".to_string(), Value::Array(token_colors));
    Value::Object(root)
}

pub fn parse_theme(name: &str, label: &str, toml_str: &str) -> Result<ThemeTokens, String> {
    let full_table: toml::Table = toml::from_str(toml_str).map_err(|e| e.to_string())?;

    let raw_palette = parse_raw_palette(&full_table);
    let palette = parse_palette(&full_table);
    let ui = parse_ui_defs(&full_table);

    let background = color_of(&palette, "background");
    let foreground = color_of(&palette, "foreground");
    let bg = background.unwrap_or(Rgb(0x16, 0x16, 0x16));
    let fg = foreground.unwrap_or(Rgb(0xf0, 0xf0, 0xf0));

    let bg_elevated = lerp_rgb(bg, fg, 0.04);
    let bg_inset = lerp_rgb(bg, fg, 0.02);

    let comment = resolve_palette_color_fallback("comment", &palette, &raw_palette)
        .unwrap_or(lerp_rgb(fg, bg, 0.5));
    let border_rgb = comment;

    let text_dim = lerp_rgb(fg, comment, 0.45);
    let text_faint = comment;

    let accent = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "accent",
        &["keyword.storage.type", "keyword", "function"],
    )
    .unwrap_or(lerp_rgb(fg, bg, 0.2));

    let accent_text = if (relative_luminance(accent) - relative_luminance(bg)).abs()
        > (relative_luminance(accent) - relative_luminance(fg)).abs()
    {
        bg
    } else {
        fg
    };

    let success = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "todo_completed",
        &["string", "constant.numeric"],
    )
    .or_else(|| resolve_palette_color_fallback("green", &palette, &raw_palette))
    .unwrap_or(accent);

    let danger = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "todo_cancelled",
        &["string.regexp"],
    )
    .or_else(|| resolve_palette_color_fallback("red", &palette, &raw_palette))
    .unwrap_or(accent);

    let warning = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "todo_in_progress",
        &["string", "constant"],
    )
    .or_else(|| resolve_palette_color_fallback("yellow", &palette, &raw_palette))
    .unwrap_or(accent);

    let mode_build = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "mode_build",
        &["keyword.storage.type", "keyword"],
    )
    .unwrap_or(accent);
    let mode_plan = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "mode_plan",
        &["keyword", "keyword.storage.type"],
    )
    .unwrap_or(warning);
    let mode_flow = derived_color(
        &palette,
        &ui,
        &full_table,
        &raw_palette,
        "mode_bash",
        &["function.builtin", "function"],
    )
    .unwrap_or(lerp_rgb(accent, fg, 0.3));

    let syntax_theme =
        build_syntax_theme(&full_table, &palette, &raw_palette, background, foreground);

    Ok(ThemeTokens {
        name: name.to_string(),
        label: label.to_string(),
        bg: hex_string(bg),
        bg_elevated: hex_string(bg_elevated),
        bg_inset: hex_string(bg_inset),
        border: hex_with_alpha(border_rgb, 0.6),
        text: hex_string(fg),
        text_dim: hex_string(text_dim),
        text_faint: hex_string(text_faint),
        accent: hex_string(accent),
        accent_dim: hex_with_alpha(accent, 0.16),
        accent_dim2: hex_with_alpha(accent, 0.22),
        accent_text: hex_string(accent_text),
        success: hex_string(success),
        danger: hex_string(danger),
        warning: hex_string(warning),
        warning_dim: hex_with_alpha(warning, 0.15),
        mode_colors: ModeColors {
            build: hex_string(mode_build),
            plan: hex_string(mode_plan),
            flow: hex_string(mode_flow),
        },
        syntax_theme,
    })
}

fn relative_luminance(c: Rgb) -> f32 {
    let f = |v: u8| {
        let s = v as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * f(c.0) + 0.7152 * f(c.1) + 0.0722 * f(c.2)
}

fn hex_with_alpha(c: Rgb, alpha: f32) -> String {
    let a = (alpha * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}{:02x}", c.0, c.1, c.2, a)
}

pub fn list_theme_names() -> Vec<ThemeName> {
    BUNDLED_THEMES
        .iter()
        .map(|e| ThemeName {
            name: e.name.to_string(),
            label: e.label.to_string(),
        })
        .collect()
}

fn read_theme_name() -> Option<String> {
    let dir = StateDir::resolve().ok()?;
    craft_storage::theme::read_theme_name(&dir)
}

pub fn current_theme_name() -> String {
    read_theme_name().unwrap_or_else(|| DEFAULT_THEME.to_owned())
}

pub fn persist_theme_name(name: &str) {
    if let Ok(dir) = StateDir::resolve() {
        craft_storage::theme::persist_theme_name(&dir, name).ok();
    }
}

pub fn get_theme_by_name(name: &str) -> Result<ThemeTokens, String> {
    let entry = BUNDLED_THEMES
        .iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("unknown theme: {name}"))?;
    parse_theme(entry.name, entry.label, entry.toml)
}

pub fn current_theme() -> ThemeTokens {
    let name = current_theme_name();
    get_theme_by_name(&name).unwrap_or_else(|_| {
        let entry = BUNDLED_THEMES
            .iter()
            .find(|e| e.name == DEFAULT_THEME)
            .expect("default theme must exist");
        parse_theme(entry.name, entry.label, entry.toml).expect("default theme must parse")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_themes_parse() {
        for entry in BUNDLED_THEMES {
            let result = parse_theme(entry.name, entry.label, entry.toml);
            assert!(
                result.is_ok(),
                "theme '{}' failed to parse: {}",
                entry.name,
                result.unwrap_err()
            );
        }
    }

    #[test]
    fn dracula_tokens_populated() {
        let entry = BUNDLED_THEMES
            .iter()
            .find(|e| e.name == "dracula")
            .expect("dracula theme must exist");
        let tokens = parse_theme(entry.name, entry.label, entry.toml).unwrap();
        assert_eq!(tokens.bg, "#282a36");
        assert_eq!(tokens.text, "#f8f8f2");
        assert!(
            !tokens.syntax_theme["tokenColors"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            tokens.syntax_theme["settings"]["background"]
                .as_str()
                .unwrap()
                .starts_with('#')
        );
    }

    #[test]
    fn syntax_theme_has_scopes() {
        let entry = BUNDLED_THEMES.iter().find(|e| e.name == "dracula").unwrap();
        let tokens = parse_theme(entry.name, entry.label, entry.toml).unwrap();
        let scopes = tokens.syntax_theme["tokenColors"].as_array().unwrap();
        assert!(scopes.len() > 20);
        let first = &scopes[0];
        assert!(first["scope"].is_string());
        assert!(first["settings"]["foreground"].is_string());
    }

    #[test]
    fn list_theme_names_returns_all() {
        assert_eq!(list_theme_names().len(), BUNDLED_THEMES.len());
    }

    #[test]
    fn tokens_serialize_to_camel_case() {
        let entry = BUNDLED_THEMES.iter().find(|e| e.name == "dracula").unwrap();
        let tokens = parse_theme(entry.name, entry.label, entry.toml).unwrap();
        let json = serde_json::to_value(&tokens).unwrap();
        let obj = json.as_object().unwrap();
        assert!(
            obj.contains_key("bgElevated"),
            "missing camelCase bgElevated"
        );
        assert!(obj.contains_key("bgInset"), "missing camelCase bgInset");
        assert!(obj.contains_key("accentDim"), "missing camelCase accentDim");
        assert!(
            obj.contains_key("accentText"),
            "missing camelCase accentText"
        );
        assert!(obj.contains_key("textDim"), "missing camelCase textDim");
        assert!(obj.contains_key("textFaint"), "missing camelCase textFaint");
        assert!(
            obj.contains_key("warningDim"),
            "missing camelCase warningDim"
        );
        assert!(
            obj.contains_key("modeColors"),
            "missing camelCase modeColors"
        );
        assert!(
            obj.contains_key("syntaxTheme"),
            "missing camelCase syntaxTheme"
        );
        assert!(
            !obj.contains_key("bg_elevated"),
            "snake_case leaked into JSON"
        );
        assert!(
            !obj.contains_key("mode_colors"),
            "snake_case leaked into JSON"
        );
    }

    #[test]
    fn accent_text_chooses_stronger_contrast_for_all_themes() {
        for entry in BUNDLED_THEMES {
            let tokens = parse_theme(entry.name, entry.label, entry.toml).unwrap();
            let accent = parse_hex_rgb(&tokens.accent).unwrap_or(Rgb(128, 128, 128));
            let accent_text = parse_hex_rgb(&tokens.accent_text).unwrap_or(Rgb(128, 128, 128));
            let bg = parse_hex_rgb(&tokens.bg).unwrap_or(Rgb(0, 0, 0));
            let fg = parse_hex_rgb(&tokens.text).unwrap_or(Rgb(255, 255, 255));
            let accent_lum = relative_luminance(accent);
            let text_lum = relative_luminance(accent_text);
            let bg_lum = relative_luminance(bg);
            let fg_lum = relative_luminance(fg);
            let text_contrast = (accent_lum - text_lum).abs();
            let bg_contrast = (accent_lum - bg_lum).abs();
            let fg_contrast = (accent_lum - fg_lum).abs();
            assert!(
                text_contrast >= bg_contrast || text_contrast >= fg_contrast,
                "theme '{}' accent_text does not use the stronger-contrast option",
                entry.name
            );
        }
    }
}
