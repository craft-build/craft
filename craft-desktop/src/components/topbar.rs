//! Top bar of the main column: task title, project/mode chips, status pill,
//! and the changes-panel toggle.

use dioxus::prelude::*;

use crate::components::ui::PillState;
use crate::components::{IconBranch, IconHelp, IconPanel, IconRepo, StatusPill};
use crate::state::{AppState, View, active_tab};

#[component]
pub fn TopBar() -> Element {
    let mut s = use_context::<AppState>();
    let is_new = *s.view.read() == View::New;
    let tab = active_tab(s);

    let (title, project, mode) = match (&is_new, &tab) {
        (true, _) => (
            "New task".to_string(),
            "craft".to_string(),
            "build".to_string(),
        ),
        (false, Some(t)) => (t.title.clone(), t.project(), t.mode_label().to_string()),
        (false, None) => (
            "craft".to_string(),
            "craft".to_string(),
            "build".to_string(),
        ),
    };
    let state = tab.as_ref().map_or(PillState::Done, |t| {
        if t.items
            .iter()
            .any(|i| matches!(i, crate::state::ChatItem::Permission { item, .. } if !item.resolved))
        {
            PillState::Waiting
        } else if t.pending {
            PillState::Running
        } else {
            PillState::Done
        }
    });
    let panel_stroke = if *s.panel_open.read() {
        "var(--blue-400)"
    } else {
        "#8089a3"
    };

    #[cfg(feature = "desktop")]
    let desktop = dioxus::desktop::use_window();

    rsx! {
        header {
            class: "topbar",
            onmousedown: move |_| {
                #[cfg(feature = "desktop")]
                desktop.drag();
            },
            div { class: "topbar-title", "{title}" }
            div { class: "chip",
                IconRepo {}
                "{project}"
            }
            div { class: "chip",
                IconBranch {}
                "{mode}"
            }
            StatusPill { state: state }
            div { class: "topbar-divider" }
            IconHelp {}
            button {
                class: "icon-btn",
                onmousedown: move |e| e.stop_propagation(),
                onclick: move |_| s.panel_open.toggle(),
                IconPanel { stroke: panel_stroke.to_string() }
            }
        }
    }
}
