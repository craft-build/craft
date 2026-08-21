//! Command palette (⌘K): filter over sessions, recent history, slash
//! commands, and app actions.

use dioxus::prelude::*;

use crate::backend::Backend;
use crate::components::composer::{CmdEntry, command_entries, route_command};
use crate::state::{AppState, active_tab, load_session, open_new_task, select_task, set_theme};

#[derive(Clone)]
enum Action {
    SelectTask(String),
    LoadSession(crate::backend::SessionSummary),
    RunCommand(CmdEntry, String),
    NewTask,
    TogglePanel,
    SetTheme(String),
}

#[component]
pub fn Palette() -> Element {
    let mut s = use_context::<AppState>();
    let backend = crate::backend::get();
    let query = s.query.read().clone();
    let q = query.trim().to_lowercase();
    let selected = *s.palette_selected.read();

    let mut entries: Vec<(String, String, String, Action)> = Vec::new();
    for t in s.tabs.read().iter() {
        entries.push((
            "session".to_string(),
            t.title.clone(),
            t.mode_label().to_string(),
            Action::SelectTask(t.id.clone()),
        ));
    }
    for h in s.history.read().iter() {
        let label = h
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| h.session_id.chars().take(12).collect());
        entries.push((
            "recent".to_string(),
            label,
            h.updated_at.clone().unwrap_or_default(),
            Action::LoadSession(h.clone()),
        ));
    }
    if let Some(tab) = active_tab(s) {
        for c in command_entries(tab.commands.as_ref()) {
            entries.push((
                "cmd".to_string(),
                c.name.clone(),
                c.description.clone(),
                Action::RunCommand(c, String::new()),
            ));
        }
    }
    entries.push((
        "cmd".to_string(),
        "New task".into(),
        "⌘N".into(),
        Action::NewTask,
    ));
    entries.push((
        "cmd".to_string(),
        "Toggle changes panel".into(),
        "⌘\\".into(),
        Action::TogglePanel,
    ));
    for t in crate::theme::list_theme_names() {
        entries.push((
            "theme".to_string(),
            format!("Theme: {}", t.label),
            String::new(),
            Action::SetTheme(t.name.clone()),
        ));
    }

    let results: Vec<(String, String, String, Action)> = entries
        .into_iter()
        .filter(|e| q.is_empty() || e.1.to_lowercase().contains(&q))
        .take(10)
        .collect();

    rsx! {
        div {
            class: "palette-overlay",
            onclick: move |_| s.palette_open.set(false),
            div { class: "palette", onclick: move |e| e.stop_propagation(),
                input {
                    class: "palette-input",
                    value: "{query}",
                    placeholder: "Search sessions and commands",
                    autofocus: true,
                    oninput: move |e| {
                        s.palette_selected.set(0);
                        s.query.set(e.value());
                    },
                    onkeydown: move |e| {
                        match e.key() {
                            Key::ArrowDown => {
                                e.prevent_default();
                                s.palette_selected.set((selected + 1) % results.len().max(1));
                            }
                            Key::ArrowUp => {
                                e.prevent_default();
                                s.palette_selected.set((selected + results.len().saturating_sub(1)) % results.len().max(1));
                            }
                            Key::Enter => {
                                e.prevent_default();
                                if let Some(r) = results.get(selected) {
                                    run_action(s, backend, &r.3);
                                }
                            }
                            Key::Escape => s.palette_open.set(false),
                            _ => {}
                        }
                    },
                }
                div { class: "palette-results",
                    for (i, r) in results.iter().enumerate() {
                        button {
                            class: if i == selected { "palette-row active" } else { "palette-row" },
                            key: "{i}",
                            onclick: {
                                let action = r.3.clone();
                                move |_| run_action(s, backend, &action)
                            },
                            span { class: "palette-kind", "{r.0}" }
                            span { class: "palette-label", "{r.1}" }
                            span { class: "palette-hint", "{r.2}" }
                        }
                    }
                    if results.is_empty() {
                        div { class: "palette-empty", "Nothing matches that." }
                    }
                }
            }
        }
    }
}

fn run_action(mut s: AppState, backend: &Backend, action: &Action) {
    s.palette_open.set(false);
    match action {
        Action::SelectTask(id) => select_task(s, id.clone()),
        Action::LoadSession(summary) => load_session(s, backend, summary.clone()),
        Action::RunCommand(entry, args) => {
            if let Some(tab) = active_tab(s) {
                route_command(s, backend, &tab, entry, args);
            }
        }
        Action::NewTask => open_new_task(s),
        Action::TogglePanel => s.panel_open.toggle(),
        Action::SetTheme(id) => set_theme(s, id),
    }
}
