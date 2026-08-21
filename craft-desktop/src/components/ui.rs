//! Shared UI pieces: the status pill.

use dioxus::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum PillState {
    Thinking,
    Running,
    Waiting,
    Done,
    Failed,
}

impl PillState {
    pub fn label(self) -> &'static str {
        match self {
            PillState::Thinking => "thinking",
            PillState::Running => "running",
            PillState::Waiting => "awaiting approval",
            PillState::Done => "done",
            PillState::Failed => "failed",
        }
    }
    pub fn class(self) -> &'static str {
        match self {
            PillState::Thinking => "pill-thinking",
            PillState::Running => "pill-running",
            PillState::Waiting => "pill-waiting",
            PillState::Done => "pill-done",
            PillState::Failed => "pill-failed",
        }
    }
    pub fn pulse(self) -> bool {
        matches!(self, PillState::Thinking | PillState::Running)
    }
}

#[component]
pub fn StatusPill(state: PillState) -> Element {
    let pulse = state.pulse();
    rsx! {
        span { class: "status-pill {state.class()}",
            span { class: if pulse { "status-dot pulse" } else { "status-dot" } }
            "{state.label()}"
        }
    }
}
