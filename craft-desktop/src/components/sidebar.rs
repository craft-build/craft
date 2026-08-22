//! Left sidebar: window chrome, primary actions, session tree grouped by
//! project, persisted-session history, and the theme picker in the footer.

use dioxus::prelude::*;

use crate::components::{IconAutomations, IconClose, IconFolder, IconNew, IconSearch, IconSkills};
use crate::state::{
    AppState, Popover, View, close_tab, load_session, open_new_task, open_new_task_in,
    open_project, open_skills, select_task, set_theme, toggle_popover,
};

#[component]
pub fn Sidebar() -> Element {
    let mut s = use_context::<AppState>();
    let backend = crate::backend::get();

    #[cfg(feature = "desktop")]
    let desktop = dioxus::desktop::use_window();

    // Group sessions by project directory (insertion order).
    let groups: Vec<(String, Vec<crate::state::Tab>)> = {
        let tabs = s.tabs.read();
        let mut groups: Vec<(String, Vec<crate::state::Tab>)> = Vec::new();
        for t in tabs.iter() {
            let project = t.project();
            match groups.iter_mut().find(|(name, _)| *name == project) {
                Some((_, rows)) => rows.push(t.clone()),
                None => groups.push((project, vec![t.clone()])),
            }
        }
        groups
    };

    let themes = crate::theme::list_theme_names();
    let current_label = themes
        .iter()
        .find(|t| t.name == *s.theme.read())
        .map(|t| t.label.clone())
        .unwrap_or_else(|| "Theme".to_string());
    let current_tokens = crate::theme::get_theme_by_name(&s.theme.read())
        .unwrap_or_else(|_| crate::theme::current_theme_fallback());
    let swatch = format!(
        "linear-gradient(135deg,{},{} 55%,{})",
        current_tokens.accent, current_tokens.accent_secondary, current_tokens.accent_tertiary
    );
    let in_session = *s.view.read() == View::Session;
    let active_id = s.active_id.read().clone();
    let history_open = *s.history_open.read();
    let history = s.history.read().clone();
    let some_cwd = s.tabs.read().first().map(|t| t.cwd.clone());

    rsx! {
        aside { class: "sidebar",
            div {
                class: "sidebar-top",
                onmousedown: move |_| {
                    #[cfg(feature = "desktop")]
                    desktop.drag();
                },
                TrafficLights {},
                div { class: "grow" }
                div { class: "wordmark", "craft" }
            }

            div { class: "sidebar-actions",
                button { class: "side-action", onclick: move |_| open_new_task(s),
                    IconNew {}
                    span { class: "grow", "New task" }
                    span { class: "kbd", "⌘N" }
                }
                button { class: "side-action", onclick: move |_| {
                        s.palette_open.set(true);
                        s.query.set(String::new());
                    },
                    IconSearch {}
                    span { class: "grow", "Search" }
                    span { class: "kbd", "⌘K" }
                }
                button { class: "side-action", onclick: move |_| s.history_open.toggle(),
                    IconAutomations {}
                    span { class: "grow", "Recent" }
                }
                button {
                    class: "side-action",
                    class: if *s.view.read() == View::Skills { "active" } else { "" },
                    onclick: move |_| open_skills(s),
                    IconSkills {}
                    span { class: "grow", "Skills" }
                }
            }

            nav { class: "sidebar-projects",
                div { class: "projects-header",
                    div { class: "projects-label", "Projects" }
                    button {
                        class: "projects-add",
                        title: "Open project…",
                        onclick: move |_| {
                            let default_dir = s
                                .tabs
                                .read()
                                .last()
                                .map(|t| t.cwd.clone())
                                .or_else(|| {
                                    craft_storage::paths::home()
                                        .map(|h| h.to_string_lossy().into_owned())
                                })
                                .unwrap_or_else(|| "/".to_string());
                            if let Some(picked) = rfd::FileDialog::new()
                                .set_directory(default_dir)
                                .pick_folder()
                            {
                                open_project(s, backend, picked.to_string_lossy().into_owned());
                            }
                        },
                        IconNew {}
                    }
                }
                for (gi, (name, rows)) in groups.iter().enumerate() {
                    {
                        let project_cwd = rows
                            .first()
                            .map(|t| t.cwd.clone())
                            .unwrap_or_default();
                        rsx! {
                            div { class: "project", key: "g{gi}",
                                div { class: "project-row",
                                    IconFolder {}
                                    span { "{name}" }
                                    button {
                                        class: "project-new",
                                        title: "New task in {name}",
                                        onclick: {
                                            let cwd = project_cwd.clone();
                                            move |_| open_new_task_in(s, cwd.clone())
                                        },
                                        IconNew {}
                                    }
                                }
                                for row in rows.iter() {
                                    {
                                        let sel = in_session && row.id == active_id;
                                        let style = format!(
                                            "background:{};color:{}",
                                            if sel { "var(--ink-700)" } else { "transparent" },
                                            if sel { "var(--text-primary)" } else { "var(--text-secondary)" },
                                        );
                                        let dot = row.status_dot();
                                        let id = row.id.clone();
                                        rsx! {
                                            div { class: "task-row", key: "{id}", style: style,
                                                span { class: "task-dot", style: "background:{dot}" }
                                                button {
                                                    class: "task-title",
                                                    onclick: {
                                                        let id = id.clone();
                                                        move |_| select_task(s, id.clone())
                                                    },
                                                    "{row.title}"
                                                }
                                                button {
                                                    class: "task-close",
                                                    onclick: {
                                                        move |_| close_tab(s, backend, id.clone())
                                                    },
                                                    IconClose {}
                                                }
                                            }
                                        }
                                    }
                                }
                                if rows.is_empty() {
                                    div { class: "empty-tasks", "No tasks yet" }
                                }
                            }
                        }
                    }
                }

                if history_open {
                    div { class: "project",
                        div { class: "projects-label", "Recent sessions" }
                        if history.is_empty() {
                            div { class: "empty-tasks", "No past sessions yet." }
                        }
                        for row in history.iter() {
                            {
                                let summary = row.clone();
                                let label = summary
                                    .title
                                    .clone()
                                    .filter(|t| !t.trim().is_empty())
                                    .unwrap_or_else(|| summary.session_id.chars().take(12).collect());
                                rsx! {
                                    button {
                                        class: "task-row history-row",
                                        key: "{summary.session_id}",
                                        onclick: {
                                            move |_| load_session(s, backend, summary.clone())
                                        },
                                        span { class: "task-dot", style: "background:var(--text-disabled)" }
                                        span { class: "task-title", "{label}" }
                                    }
                                }
                            }
                        }
                        button {
                            class: "task-row history-refresh",
                            onclick: {
                                move |_| backend.list_sessions(some_cwd.clone())
                            },
                            "Refresh list"
                        }
                    }
                }
            }

            div { class: "sidebar-footer",
                div { class: "dd",
                    button { class: "theme-btn", onclick: move |_| toggle_popover(s, Popover::Theme),
                        span { class: "theme-swatch", style: "background:{swatch}" }
                        span { class: "grow", "{current_label}" }
                        span { class: "caret", "▾" }
                    }
                    if *s.popover.read() == Popover::Theme {
                        div { class: "menu theme",
                            for (i, t) in themes.iter().enumerate() {
                                {
                                    let selected = t.name == *s.theme.read();
                                    let label = t.label.clone();
                                    let name = t.name.clone();
                                    rsx! {
                                        button {
                                            key: "{i}",
                                            class: if selected { "menu-item selected" } else { "menu-item" },
                                            onclick: {
                                                let name = name.clone();
                                                move |_| {
                                                    set_theme(s, &name);
                                                    s.popover.set(Popover::None);
                                                }
                                            },
                                            span { class: "grow menu-item-title", "{label}" }
                                            span { class: "menu-check", if selected { "✓" } else { "" } }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The in-app traffic lights. On desktop they control the real frameless
/// window (close / minimize / zoom) and swallow mousedown so the surrounding
/// drag region doesn't start a window drag; elsewhere they are decorative.
#[component]
fn TrafficLights() -> Element {
    #[cfg(feature = "desktop")]
    {
        let desktop = dioxus::desktop::use_window();
        let (d_close, d_min, d_zoom) = (desktop.clone(), desktop.clone(), desktop);
        rsx! {
            div { class: "dots",
                button {
                    class: "dot dot-red",
                    title: "Close",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| d_close.close(),
                }
                button {
                    class: "dot dot-yellow",
                    title: "Minimize",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| d_min.window.set_minimized(true),
                }
                button {
                    class: "dot dot-green",
                    title: "Zoom",
                    onmousedown: move |e| e.stop_propagation(),
                    onclick: move |_| d_zoom.toggle_maximized(),
                }
            }
        }
    }
    #[cfg(not(feature = "desktop"))]
    {
        rsx! {
            div { class: "dots",
                span { class: "dot dot-red" }
                span { class: "dot dot-yellow" }
                span { class: "dot dot-green" }
            }
        }
    }
}
