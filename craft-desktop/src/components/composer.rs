//! Message composer: textarea plus the mode / model dropdowns, context ring,
//! send/stop button, image attachments (drag-drop or attach button), the
//! connection-error banner, and the `/` command palette.

use dioxus::html::HasFileData;
use dioxus::prelude::*;

use crate::backend::Backend;
use crate::components::{IconImage, IconList, IconSend, IconShield, IconStop};
use crate::state::{
    AppState, DEFAULT_MODES, Popover, SUGGESTIONS, View, active_tab, attachment_from_path,
    cancel_prompt, send_message, set_config_option, set_mode, set_perm, toggle_popover,
};

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Reads dropped or picked image files into pending composer attachments.
/// Non-image and oversized files are skipped (see `attachment_from_path`).
fn attach_files(mut s: AppState, paths: Vec<std::path::PathBuf>) {
    let new: Vec<_> = paths
        .iter()
        .filter_map(|p| attachment_from_path(p))
        .collect();
    if !new.is_empty() {
        s.attachments.write().extend(new);
    }
}

#[derive(Clone, PartialEq)]
pub struct CmdEntry {
    pub name: String,
    pub description: String,
    pub accepts_args: bool,
    pub strategy: String,
    pub method: Option<String>,
    pub meta_kind: Option<String>,
    pub custom_name: Option<String>,
}

/// Parse the cached `_craft/listCommands` response into palette entries.
pub fn command_entries(commands: Option<&serde_json::Value>) -> Vec<CmdEntry> {
    let mut out = Vec::new();
    let Some(root) = commands else { return out };
    read_entries(
        &mut out,
        root.get("commands").and_then(|v| v.as_array()),
        false,
    );
    read_entries(
        &mut out,
        root.get("custom").and_then(|v| v.as_array()),
        true,
    );
    out.retain(|e| !e.name.is_empty());
    out
}

fn read_entries(out: &mut Vec<CmdEntry>, list: Option<&Vec<serde_json::Value>>, custom: bool) {
    if let Some(items) = list {
        for c in items {
            if custom {
                let display = c.get("displayName").and_then(|v| v.as_str()).unwrap_or("");
                out.push(CmdEntry {
                    name: format!("/{display}"),
                    description: c
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    accepts_args: c
                        .get("acceptsArgs")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    strategy: "craft_request".to_string(),
                    method: Some("_craft/command/run".to_string()),
                    meta_kind: None,
                    custom_name: Some(display.to_string()),
                });
            } else {
                out.push(CmdEntry {
                    name: c
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    description: c
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    accepts_args: c.get("maxArgs").and_then(|v| v.as_u64()).unwrap_or(0) > 0,
                    strategy: c
                        .get("strategy")
                        .and_then(|v| v.as_str())
                        .unwrap_or("passthrough")
                        .to_string(),
                    method: c.get("method").and_then(|v| v.as_str()).map(str::to_string),
                    meta_kind: c
                        .get("metaKind")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    custom_name: None,
                });
            }
        }
    }
}

/// Execute a `/command` from the palette. Ports `routeCommand` from the old
/// ChatPane: `/mode`, `/yolo`, `/auto-review` need bespoke client logic; the
/// rest route through the server-provided `_craft/*` method or pass through
/// as a prompt.
pub fn route_command(
    mut s: AppState,
    backend: &Backend,
    tab: &crate::state::Tab,
    entry: &CmdEntry,
    args: &str,
) {
    let Some(session_id) = tab.session_id.clone() else {
        return;
    };
    match entry.strategy.as_str() {
        "client" if entry.name == "/clear" => {
            if let Some(t) = s.tabs.write().iter_mut().find(|t| t.id == tab.id) {
                t.items.clear();
            }
        }
        "acp_standard" => match entry.name.as_str() {
            "/mode" => {
                let mode = args.trim();
                let mode = if mode.is_empty() { "build" } else { mode };
                set_mode(s, backend, mode.to_string());
            }
            "/yolo" => {
                let next = if tab.config("yolo").map(|c| c.current_value.as_str()) == Some("true") {
                    "false"
                } else {
                    "true"
                };
                set_config_option(s, backend, "yolo".to_string(), next.to_string());
            }
            "/auto-review" => {
                let next = if tab.config("auto_review").map(|c| c.current_value.as_str())
                    == Some("true")
                {
                    "false"
                } else {
                    "true"
                };
                set_config_option(s, backend, "auto_review".to_string(), next.to_string());
            }
            _ => {}
        },
        "craft_request" => {
            if let Some(custom) = &entry.custom_name {
                backend.craft_command(
                    tab.id.clone(),
                    "_craft/command/run".to_string(),
                    serde_json::json!({ "sessionId": session_id, "name": custom, "args": args }),
                );
            } else if let Some(method) = &entry.method {
                match crate::backend::craft_command_params(
                    &session_id,
                    method,
                    &entry.name,
                    entry.meta_kind.as_deref(),
                    args,
                ) {
                    Ok(params) => backend.craft_command(tab.id.clone(), method.clone(), params),
                    Err(e) => tracing::warn!("{e}"),
                }
            }
        }
        _ => {
            backend.send_prompt(
                tab.id.clone(),
                session_id,
                format!("{} {}", entry.name, args).trim().to_string(),
                Vec::new(),
            );
            if let Some(t) = s.tabs.write().iter_mut().find(|t| t.id == tab.id) {
                t.pending = true;
            }
        }
    };
}

/// Subsequence matcher: `Some(match length)` if `pattern` is a subsequence of
/// `text` (case-insensitive).
fn subseq_len(text: &str, pattern: &str) -> Option<usize> {
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let mut count = 0;
    let mut pi = 0;
    for &c in &t {
        if pi < p.len() && p[pi] == c {
            count += 1;
            pi += 1;
        }
    }
    (pi == p.len()).then_some(count)
}

fn slash_matches(entries: &[CmdEntry], word: &str) -> Vec<CmdEntry> {
    if word.is_empty() {
        return entries.iter().take(8).cloned().collect();
    }
    let mut hits: Vec<(usize, &CmdEntry)> = entries
        .iter()
        .filter_map(|e| subseq_len(&e.name, word).map(|n| (n, e)))
        .collect();
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    hits.into_iter().map(|(_, e)| e.clone()).take(8).collect()
}

#[derive(Clone, Copy, PartialEq)]
pub struct Perm {
    pub label: &'static str,
    pub desc: &'static str,
    pub yolo: bool,
    pub auto_review: bool,
}

pub const PERMS: [Perm; 3] = [
    Perm {
        label: "Ask",
        desc: "Ask for writes and commands",
        yolo: false,
        auto_review: false,
    },
    Perm {
        label: "Yolo",
        desc: "Edit and run commands without asking",
        yolo: true,
        auto_review: false,
    },
    Perm {
        label: "Auto-Approve",
        desc: "Let an LLM approve or deny based on risk analysis",
        yolo: false,
        auto_review: true,
    },
];

/// Provider prefix of a model id like "zai/glm-5.3"; ids without a slash
/// count as their own provider.
fn model_provider(id: &str) -> &str {
    id.split('/').next().unwrap_or(id)
}

#[component]
pub fn Composer() -> Element {
    let mut s = use_context::<AppState>();
    let backend = crate::backend::get();
    // Selected provider tab in the model menu; persists while the app runs.
    let mut model_provider_tab = use_signal(String::new);
    let is_new = *s.view.read() == View::New;
    let tab = active_tab(s);
    let pending = tab.as_ref().map_or(*s.starting.read(), |t| t.pending);
    let connection_error = tab.as_ref().and_then(|t| t.connection_error.clone());

    let draft = s.draft.read().clone();
    let attachments = s.attachments.read().clone();
    let focused = *s.focused.read();
    let can_send = (!draft.trim().is_empty() || !attachments.is_empty()) && !pending;
    let placeholder = if is_new {
        "Describe the task"
    } else {
        "Ask for follow-up changes ( / for commands )"
    };
    let composer_border = if focused {
        "var(--border-strong)"
    } else {
        "var(--border-subtle)"
    };

    let entries = command_entries(tab.as_ref().and_then(|t| t.commands.as_ref()));
    let slash_open = draft.starts_with('/') && !pending;
    let stripped = draft.strip_prefix('/').unwrap_or_default();
    let (cmd_word, args) = match stripped.find(char::is_whitespace) {
        Some(i) => (&stripped[..i], stripped[i + 1..].to_string()),
        None => (stripped, String::new()),
    };
    let matches = if slash_open {
        slash_matches(&entries, cmd_word)
    } else {
        Vec::new()
    };
    let selected = (*s.palette_selected.read()).min(matches.len().saturating_sub(1));

    // Context ring geometry: r = 7.5 in a 20x20 viewBox.
    let (used, size) = tab
        .as_ref()
        .map(|t| (t.context_used, t.context_size))
        .unwrap_or((0, 0));
    let pct = if size > 0 {
        (used as f64 / size as f64).min(0.99)
    } else {
        0.0
    };
    let circumference = 2.0 * std::f64::consts::PI * 7.5;
    let dash = format!("{:.1} {:.1}", pct * circumference, circumference);
    let ring_color = if pct > 0.85 {
        "var(--status-danger)"
    } else if pct > 0.65 {
        "var(--status-warning)"
    } else {
        "var(--blue-500)"
    };
    let ctx_label = format!("{}%", (pct * 100.0).round());

    let send_bg = if can_send {
        "var(--grad-brand)"
    } else {
        "var(--bg-surface-raised)"
    };
    // Text/icon color on the brand gradient: themes with light accents need
    // a dark stroke, so read the token the theme system computed for this.
    let accent_text = crate::theme::get_theme_by_name(&s.theme.read())
        .unwrap_or_else(|_| crate::theme::current_theme_fallback())
        .accent_text;
    let send_stroke = if can_send {
        accent_text.clone()
    } else {
        "#5b6784".to_string()
    };

    let modes: Vec<(String, String)> = tab.as_ref().map_or_else(
        || {
            DEFAULT_MODES
                .iter()
                .map(|&m| (m.to_string(), crate::state::mode_label(m).to_string()))
                .collect()
        },
        |t| {
            if t.modes.is_empty() {
                DEFAULT_MODES
                    .iter()
                    .map(|&m| (m.to_string(), crate::state::mode_label(m).to_string()))
                    .collect()
            } else {
                t.modes
                    .iter()
                    .map(|m| (m.id.clone(), m.name.clone()))
                    .collect()
            }
        },
    );
    // A fresh tab has no mode until the session reports current_mode_update;
    // fall back to the app-level selection so the button always has a label.
    let current_mode = tab
        .as_ref()
        .map(|t| t.mode.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| s.mode.read().clone());
    // Live session state wins; before a session exists fall back to the
    // onboarding-level defaults.
    let yolo_on = tab
        .as_ref()
        .map(|t| t.config("yolo").map(|c| c.current_value == "true"))
        .unwrap_or_else(|| Some(*s.yolo.read()))
        .unwrap_or(false);
    let auto_on = tab
        .as_ref()
        .map(|t| t.config("auto_review").map(|c| c.current_value == "true"))
        .unwrap_or_else(|| Some(*s.auto_review.read()))
        .unwrap_or(false);
    let perm_index = if auto_on {
        2
    } else if yolo_on {
        1
    } else {
        0
    };
    let perm = &PERMS[perm_index];
    let perm_label = perm.label;
    let perm_color = if perm_index == 1 {
        "var(--status-warning)"
    } else if perm_index == 2 {
        "var(--violet-400)"
    } else {
        "var(--blue-400)"
    };
    // Escalated policies get a colored, tinted button so the risk state is
    // visible at a glance; Ask stays in the default bar-button chrome.
    let perm_class = match perm_index {
        1 => "bar-btn perm yolo",
        2 => "bar-btn perm auto",
        _ => "bar-btn perm",
    };
    let mode_label = modes
        .iter()
        .find(|(id, _)| *id == current_mode)
        .map(|(_, name)| name.clone())
        .unwrap_or_else(|| current_mode.clone());
    let model_option = tab.as_ref().and_then(|t| t.config("model").cloned());
    let current_model = model_option
        .as_ref()
        .map(|c| {
            c.options
                .iter()
                .find(|(v, _)| *v == c.current_value)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| c.current_value.clone())
        })
        .unwrap_or_else(|| "model".to_string());

    // Model menu: group the flat option list by provider prefix, keep the
    // current model's provider selected until the user switches tabs.
    let providers: Vec<String> = model_option.iter().flat_map(|c| c.options.iter()).fold(
        Vec::new(),
        |mut acc, (value, _)| {
            let provider = model_provider(value).to_string();
            if !acc.contains(&provider) {
                acc.push(provider);
            }
            acc
        },
    );
    let current_model_value = model_option
        .as_ref()
        .map(|c| c.current_value.clone())
        .unwrap_or_default();
    let active_provider = {
        let stored = model_provider_tab.read().clone();
        if providers.contains(&stored) {
            stored
        } else {
            model_provider(&current_model_value).to_string()
        }
    };
    let visible_models: Vec<(String, String, bool)> = model_option
        .iter()
        .flat_map(|c| c.options.iter())
        .filter(|(value, _)| model_provider(value) == active_provider)
        .map(|(value, name)| {
            let short = name
                .strip_prefix(&active_provider)
                .and_then(|n| n.strip_prefix('/'))
                .unwrap_or(name);
            (
                value.clone(),
                short.to_string(),
                *value == current_model_value,
            )
        })
        .collect();

    rsx! {
        div { class: "composer-wrap",
            if slash_open && !matches.is_empty() {
                div { class: "slash-palette",
                    for (i, m) in matches.iter().enumerate() {
                        button {
                            class: if i == selected { "slash-row active" } else { "slash-row" },
                            key: "{m.name}",
                            onclick: {
                                let entry = m.clone();
                                let args = args.clone();
                                move |_| {
                                    if let Some(t) = active_tab(s) {
                                        route_command(s, backend, &t, &entry, &args);
                                    }
                                    s.draft.set(String::new());
                                }
                            },
                            span { class: "slash-name", "{m.name}" }
                            span { class: "slash-strategy", "{m.strategy}" }
                            if !m.description.is_empty() {
                                span { class: "slash-desc", "{m.description}" }
                            }
                        }
                    }
                }
            }
            if let Some(err) = connection_error {
                div { class: "error-banner",
                    span { "⚠ {err}" }
                    button { onclick: move |_| {
                    if let Some(t) = active_tab(s)
                        && let Some(x) = s.tabs.write().iter_mut().find(|x| x.id == t.id)
                    {
                        x.connection_error = None;
                    }
                        },
                        "×"
                    }
                }
            }
            div {
                class: if *s.drop_hint.read() { "composer dropping" } else { "composer" },
                style: "border-color:{composer_border}",
                ondragenter: move |e| {
                    e.prevent_default();
                    s.drop_hint.set(true);
                },
                ondragover: move |e| e.prevent_default(),
                ondragleave: move |_| s.drop_hint.set(false),
                ondrop: move |e| {
                    e.prevent_default();
                    s.drop_hint.set(false);
                    attach_files(s, e.files().iter().map(|f| f.path()).collect());
                },
                if !attachments.is_empty() {
                    div { class: "composer-attachments",
                        for (i, img) in attachments.iter().enumerate() {
                            {
                                let uri = img.data_uri();
                                rsx! {
                                    div { class: "composer-attachment", key: "{img.name}-{i}",
                                        img { src: "{uri}", alt: "{img.name}" }
                                        button {
                                            class: "composer-attachment-remove",
                                            title: "Remove attachment",
                                            onclick: move |_| {
                                                s.attachments.write().remove(i);
                                            },
                                            "×"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                textarea {
                    class: "composer-input",
                    rows: 2,
                    value: "{draft}",
                    placeholder: placeholder,
                    oninput: move |e| s.draft.set(e.value()),
                    onfocus: move |_| s.focused.set(true),
                    onblur: move |_| s.focused.set(false),
                    onkeydown: move |e| {
                        if slash_open && !matches.is_empty() {
                            match e.key() {
                                Key::ArrowDown => {
                                    e.prevent_default();
                                    s.palette_selected.set((selected + 1) % matches.len());
                                    return;
                                }
                                Key::ArrowUp => {
                                    e.prevent_default();
                                    s.palette_selected.set((selected + matches.len() - 1) % matches.len());
                                    return;
                                }
                                Key::Enter if !e.modifiers().shift() => {
                                    e.prevent_default();
                                    if let Some(m) = matches.get(selected) {
                                        if let Some(t) = active_tab(s) {
                                            let entry = m.clone();
                                            route_command(s, backend, &t, &entry, &args);
                                        }
                                        s.draft.set(String::new());
                                    }
                                    return;
                                }
                                Key::Tab => {
                                    e.prevent_default();
                                    if let Some(m) = matches.get(selected) {
                                        let text = if m.accepts_args {
                                            format!("{} ", m.name)
                                        } else {
                                            m.name.clone()
                                        };
                                        s.draft.set(text);
                                    }
                                    return;
                                }
                                _ => {}
                            }
                        }
                        if e.key() == Key::Enter && !e.modifiers().shift() {
                            e.prevent_default();
                            send_message(s, backend);
                        }
                        if e.key() == Key::Escape && pending {
                            cancel_prompt(s, backend);
                        }
                    },
                }
                div { class: "composer-bar",
                    div { class: "dd",
                        button { class: perm_class, onclick: move |_| toggle_popover(s, Popover::Perm),
                            IconShield { stroke: perm_color.to_string() }
                            "{perm_label}"
                            span { class: "caret", "▾" }
                        }
                        if *s.popover.read() == Popover::Perm {
                            div { class: "menu perm",
                                for (i, p) in PERMS.iter().enumerate() {
                                    {
                                        let perm_title = p.label;
                                        let perm_desc = p.desc;
                                        let is_sel = i == perm_index;
                                        rsx! {
                                            button {
                                                class: if is_sel { "menu-item-col selected" } else { "menu-item-col" },
                                                key: "{i}",
                                                onclick: {
                                                    let p = *p;
                                                    move |_| {
                                                        set_perm(s, backend, p.yolo, p.auto_review);
                                                        s.popover.set(Popover::None);
                                                    }
                                                },
                                                span { class: "menu-check", style: "color:{perm_color}", if is_sel { "✓" } else { "" } }
                                                span { class: "menu-item-body",
                                                    span { class: "menu-item-title", "{perm_title}" }
                                                    span { class: "menu-item-desc", "{perm_desc}" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div { class: "dd",
                        button { class: "bar-btn", onclick: move |_| toggle_popover(s, Popover::Mode),
                            IconList {}
                            "{mode_label}"
                            span { class: "caret", "▾" }
                        }
                        if *s.popover.read() == Popover::Mode {
                            div { class: "menu mode",
                                for (i, (id, name)) in modes.iter().enumerate() {
                                    button {
                                        class: if *id == current_mode { "menu-item selected" } else { "menu-item" },
                                        key: "{i}",
                                        onclick: {
                                            let id = id.clone();
                                            move |_| {
                                                set_mode(s, backend, id.clone());
                                                s.popover.set(Popover::None);
                                            }
                                        },
                                        span { class: "grow menu-item-title", "{name}" }
                                        span { class: "menu-check", if *id == current_mode { "✓" } else { "" } }
                                    }
                                }
                            }
                        }
                    }

                    button {
                        class: "bar-btn attach",
                        title: "Attach an image",
                        onclick: move |_| {
                            let paths = rfd::FileDialog::new()
                                .add_filter("Images", IMAGE_EXTS)
                                .pick_files()
                                .unwrap_or_default();
                            attach_files(s, paths);
                        },
                        IconImage {}
                    }

                    if is_new {
                        for (i, (label, text)) in SUGGESTIONS.iter().enumerate() {
                            button {
                                class: "bar-btn suggestion",
                                key: "{i}",
                                onclick: move |_| s.draft.set(text.to_string()),
                                "{label}"
                            }
                        }
                    }

                    div { class: "grow" }

                    if let Some(err) = s.start_error.read().clone() {
                        span { class: "start-error", "{err}" }
                    }

                    if let Some(_tab) = tab.clone() {
                        if model_option.is_some() {
                            div { class: "dd",
                                button { class: "bar-btn model", onclick: move |_| toggle_popover(s, Popover::Model),
                                    "{current_model}"
                                    span { class: "caret", "▾" }
                                }
                                if *s.popover.read() == Popover::Model {
                                    div { class: "menu model",
                                        if providers.len() > 1 {
                                            div { class: "menu-tabs",
                                                for provider in providers.iter() {
                                                    button {
                                                        class: if *provider == active_provider { "menu-tab active" } else { "menu-tab" },
                                                        key: "{provider}",
                                                        onclick: {
                                                            let provider = provider.clone();
                                                            move |_| model_provider_tab.set(provider.clone())
                                                        },
                                                        "{provider}"
                                                    }
                                                }
                                            }
                                        }
                                        div { class: "menu-scroll",
                                            for (value, name, is_selected) in visible_models.iter() {
                                                button {
                                                    class: if *is_selected { "menu-item selected" } else { "menu-item" },
                                                    key: "{value}",
                                                    onclick: {
                                                        let value = value.clone();
                                                        move |_| {
                                                            model_provider_tab.set(model_provider(&value).to_string());
                                                            set_config_option(s, backend, "model".to_string(), value.clone());
                                                            s.popover.set(Popover::None);
                                                        }
                                                    },
                                                    span { class: "grow menu-item-title mono", "{name}" }
                                                    span { class: "menu-check", if *is_selected { "✓" } else { "" } }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if size > 0 {
                        div { class: "ctx", title: "{used} of {size} tokens",
                            svg { class: "ctx-ring", width: 16, height: 16, view_box: "0 0 20 20",
                                circle { cx: 10, cy: 10, r: 7.5, fill: "none", stroke: "var(--ink-600)", stroke_width: 3 }
                                circle {
                                    cx: 10,
                                    cy: 10,
                                    r: 7.5,
                                    fill: "none",
                                    stroke: ring_color,
                                    stroke_width: 3,
                                    stroke_linecap: "round",
                                    stroke_dasharray: "{dash}",
                                }
                            }
                            span { class: "ctx-label", "{ctx_label}" }
                        }
                    }

                    if pending {
                        button { class: "send-btn stop", onclick: move |_| cancel_prompt(s, backend),
                            IconStop { stroke: send_stroke.clone() }
                        }
                    } else {
                        button {
                            class: "send-btn",
                            style: "background:{send_bg}",
                            disabled: !can_send,
                            onclick: move |_| send_message(s, backend),
                            IconSend { stroke: send_stroke.clone() }
                        }
                    }
                }
            }
        }
    }
}
