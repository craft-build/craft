//! The "Start a task" landing screen shown before a task exists.

use dioxus::prelude::*;

use crate::components::IconLogo;
use crate::state::AppState;

#[component]
pub fn NewTaskView() -> Element {
    let s = use_context::<AppState>();

    let cwd = s.new_task_cwd.read().clone();

    rsx! {
        div { class: "new-hero",
            div { class: "hero-logo", IconLogo {} }
            div { class: "hero-copy",
                div { class: "hero-title", "Start a task" }
                div { class: "hero-desc", "Craft can edit files, run commands, and review changes on your behalf. Describe what you want done." }
                if let Some(cwd) = &cwd {
                    div { class: "hero-cwd", "{cwd}" }
                }
            }
        }
    }
}
