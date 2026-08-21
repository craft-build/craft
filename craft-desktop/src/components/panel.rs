//! The changes side panel: per-file collapsible diffs.

use dioxus::prelude::*;

use crate::components::{DiffLines, diff};
use crate::state::{AppState, ToolDiff, active_tab, toggle_file};

/// One card per file: later edits of the same path supersede earlier diffs.
fn dedupe_by_path(diffs: Vec<ToolDiff>) -> Vec<ToolDiff> {
    let mut out: Vec<ToolDiff> = Vec::new();
    for d in diffs {
        match out.iter_mut().find(|x| x.path == d.path) {
            Some(slot) => *slot = d,
            None => out.push(d),
        }
    }
    out
}

struct Card {
    path: String,
    open: bool,
    adds: usize,
    dels: usize,
    lines: Vec<diff::DiffLine>,
}

#[component]
pub fn ChangesPanel() -> Element {
    let mut s = use_context::<AppState>();
    let Some(tab) = active_tab(s) else {
        return rsx! {};
    };
    let open_files = s.open_files.read();
    let cards: Vec<Card> = dedupe_by_path(tab.diffs())
        .into_iter()
        .map(|d| {
            let old = d.old_text.as_deref().unwrap_or("");
            let (adds, dels) = diff::diff_stat(old, &d.new_text);
            Card {
                open: open_files.contains(&d.path),
                path: d.path,
                adds,
                dels,
                lines: diff::diff_lines(old, &d.new_text),
            }
        })
        .collect();
    drop(open_files);

    rsx! {
        div { class: "changes-panel",
            div { class: "panel-head",
                span { class: "panel-title", "Changes" }
                div { class: "grow" }
                button { class: "panel-close", onclick: move |_| s.panel_open.set(false), "×" }
            }
            div { class: "panel-body",
                if cards.is_empty() {
                    div { class: "empty-tasks", "No file changes yet." }
                }
                for card in cards {
                    {
                        let path = card.path.clone();
                        rsx! {
                            div { class: "file-card", key: "{path}",
                                button {
                                    class: "file-head",
                                    onclick: move |_| toggle_file(s, &path),
                                    span { class: "file-caret", if card.open { "▾" } else { "▸" } }
                                    span { class: "file-path", "{card.path}" }
                                    span { class: "file-adds", "+{card.adds}" }
                                    span { class: "file-dels", "-{card.dels}" }
                                }
                                if card.open {
                                    DiffLines { lines: card.lines }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
