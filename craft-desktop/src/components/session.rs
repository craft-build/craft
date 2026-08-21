//! The session screen: floating changes chip, transcript, todos, and plan
//! card. The composer is rendered by the app shell so it also covers the
//! new-task screen.

use dioxus::prelude::*;

use crate::components::{IconChanges, PlanCard, TodoList, Transcript};
use crate::state::{AppState, active_tab};

#[component]
pub fn SessionView() -> Element {
    let mut s = use_context::<AppState>();
    let Some(tab) = active_tab(s) else {
        return rsx! { Transcript {} };
    };
    let diffs = tab.diffs();
    let (adds, dels): (usize, usize) = diffs
        .iter()
        .map(|d| {
            crate::components::diff::diff_stat(d.old_text.as_deref().unwrap_or(""), &d.new_text)
        })
        .fold((0, 0), |(a, b), (x, y)| (a + x, b + y));

    rsx! {
        if !diffs.is_empty() {
            div { class: "changes-chip-wrap",
                button { class: "changes-chip", onclick: move |_| s.panel_open.toggle(),
                    IconChanges {}
                    "Changes"
                    span { class: "diff-adds", "+{adds}" }
                    span { class: "diff-dels", "-{dels}" }
                }
            }
        }
        Transcript {}
        TodoList {}
        PlanCard {}
    }
}
