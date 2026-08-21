//! The changes side panel: per-file collapsible diffs.

use dioxus::prelude::*;

use crate::components::{DiffLines, diff};
use crate::state::{AppState, active_tab, toggle_file};

#[component]
pub fn ChangesPanel() -> Element {
    let mut s = use_context::<AppState>();
    let Some(tab) = active_tab(s) else {
        return rsx! {};
    };
    let diffs = tab.diffs();

    rsx! {
        div { class: "changes-panel",
            div { class: "panel-head",
                span { class: "panel-title", "Changes" }
                div { class: "grow" }
                button { class: "panel-close", onclick: move |_| s.panel_open.set(false), "×" }
            }
            div { class: "panel-body",
                if diffs.is_empty() {
                    div { class: "empty-tasks", "No file changes yet." }
                }
                for (i, d) in diffs.iter().enumerate() {
                    {
                        let open = s.open_files.read().contains(&d.path) || (s.open_files.read().is_empty() && i == 0);
                        let caret = if open { "▾" } else { "▸" };
                        let (adds, dels) = diff::diff_stat(d.old_text.as_deref().unwrap_or(""), &d.new_text);
                        let lines = diff::diff_lines(d.old_text.as_deref().unwrap_or(""), &d.new_text);
                        let path = d.path.clone();
                        rsx! {
                            div { class: "file-card", key: "{i}",
                                button { class: "file-head", onclick: move |_| toggle_file(s, &path),
                                    span { class: "file-caret", "{caret}" }
                                    span { class: "file-path", "{path}" }
                                    span { class: "file-adds", "+{adds}" }
                                    span { class: "file-dels", "-{dels}" }
                                }
                                if open {
                                    DiffLines { lines: lines }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
