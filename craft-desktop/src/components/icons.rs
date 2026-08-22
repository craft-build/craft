//! SVG icons ported 1:1 from craft-code. Most have a fixed stroke color; the
//! few that change color at runtime take a `stroke` prop.

use dioxus::prelude::*;

#[component]
pub fn IconNew() -> Element {
    rsx! {
        svg { width: 17, height: 17, view_box: "0 0 24 24", fill: "none", stroke: "#8089a3", stroke_width: "1.6",
            circle { cx: 12, cy: 12, r: 10 }
            path { d: "M8 12h8M12 8v8" }
        }
    }
}

#[component]
pub fn IconSearch() -> Element {
    rsx! {
        svg { width: 17, height: 17, view_box: "0 0 24 24", fill: "none", stroke: "#8089a3", stroke_width: "1.6",
            circle { cx: 11, cy: 11, r: 8 }
            path { d: "m21 21-4.3-4.3" }
        }
    }
}

#[component]
pub fn IconAutomations() -> Element {
    rsx! {
        svg { width: 17, height: 17, view_box: "0 0 24 24", fill: "none", stroke: "#8089a3", stroke_width: "1.6",
            rect { x: 3, y: 4, width: 18, height: 18, rx: 2 }
            path { d: "M8 2v4M16 2v4M3 10h18m-9 6 2 2 4-4" }
        }
    }
}

#[component]
pub fn IconSkills() -> Element {
    rsx! {
        svg { width: 17, height: 17, view_box: "0 0 24 24", fill: "none", stroke: "#8089a3", stroke_width: "1.6",
            path { d: "M9.94 15.5A2 2 0 0 0 8.5 14.06l-6.14-1.58a.5.5 0 0 1 0-.96L8.5 9.94A2 2 0 0 0 9.94 8.5l1.58-6.14a.5.5 0 0 1 .96 0L14.06 8.5A2 2 0 0 0 15.5 9.94l6.14 1.58a.5.5 0 0 1 0 .96L15.5 14.06a2 2 0 0 0-1.44 1.44l-1.58 6.14a.5.5 0 0 1-.96 0z" }
        }
    }
}

#[component]
pub fn IconFolder() -> Element {
    rsx! {
        svg { width: 15, height: 15, view_box: "0 0 24 24", fill: "none", stroke: "#5b6784", stroke_width: "1.6",
            path { d: "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2z" }
        }
    }
}

#[component]
pub fn IconRepo() -> Element {
    rsx! {
        svg { width: 13, height: 13, view_box: "0 0 24 24", fill: "none", stroke: "#5b6784", stroke_width: "1.8",
            path { d: "M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2z" }
        }
    }
}

#[component]
pub fn IconBranch() -> Element {
    rsx! {
        svg { width: 13, height: 13, view_box: "0 0 24 24", fill: "none", stroke: "#5b6784", stroke_width: "1.8",
            path { d: "M6 3v12" }
            circle { cx: 18, cy: 6, r: 3 }
            circle { cx: 6, cy: 18, r: 3 }
            path { d: "M18 9a9 9 0 0 1-9 9" }
        }
    }
}

#[component]
pub fn IconHelp() -> Element {
    rsx! {
        svg { width: 17, height: 17, view_box: "0 0 24 24", fill: "none", stroke: "#5b6784", stroke_width: "1.6",
            circle { cx: 12, cy: 12, r: 10 }
            path { d: "M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3M12 17h.01" }
        }
    }
}

#[component]
pub fn IconPanel(stroke: String) -> Element {
    rsx! {
        svg { width: 17, height: 17, view_box: "0 0 24 24", fill: "none", stroke: stroke, stroke_width: "1.6",
            rect { x: 3, y: 3, width: 18, height: 18, rx: 2 }
            path { d: "M15 3v18" }
        }
    }
}

#[component]
pub fn IconChanges() -> Element {
    rsx! {
        svg { width: 14, height: 14, view_box: "0 0 24 24", fill: "none", stroke: "#8089a3", stroke_width: "1.7",
            path { d: "M12 3v14M5 10h14M5 21h14" }
        }
    }
}

#[component]
pub fn IconPlus() -> Element {
    rsx! {
        svg { width: 18, height: 18, view_box: "0 0 24 24", fill: "none", stroke: "#5b6784", stroke_width: "1.6",
            path { d: "M5 12h14M12 5v14" }
        }
    }
}

#[component]
pub fn IconShield(stroke: String) -> Element {
    rsx! {
        svg { width: 14, height: 14, view_box: "0 0 24 24", fill: "none", stroke: stroke, stroke_width: "1.7",
            path { d: "M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z" }
        }
    }
}

#[component]
pub fn IconList() -> Element {
    rsx! {
        svg { width: 14, height: 14, view_box: "0 0 24 24", fill: "none", stroke: "#8089a3", stroke_width: "1.7",
            path { d: "M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" }
        }
    }
}

#[component]
pub fn IconSend(stroke: String) -> Element {
    rsx! {
        svg { width: 16, height: 16, view_box: "0 0 24 24", fill: "none", stroke: stroke, stroke_width: "1.9",
            path { d: "M12 19V5m-7 7 7-7 7 7" }
        }
    }
}

#[component]
pub fn IconStop(stroke: String) -> Element {
    rsx! {
        svg { width: 14, height: 14, view_box: "0 0 24 24", fill: "none", stroke: stroke, stroke_width: "1.9",
            rect { x: 6, y: 6, width: 12, height: 12, rx: 2 }
        }
    }
}

#[component]
pub fn IconClose() -> Element {
    rsx! {
        svg { width: 12, height: 12, view_box: "0 0 24 24", fill: "none", stroke: "#5b6784", stroke_width: "2",
            path { d: "M6 6l12 12M18 6L6 18" }
        }
    }
}

#[component]
pub fn IconLogo() -> Element {
    rsx! {
        svg { width: 34, height: 34, view_box: "0 0 24 24", fill: "none", stroke: "#fff", stroke_width: "1.6",
            path { d: "m18 16 4-4-4-4M6 8l-4 4 4 4M14.5 4l-5 16" }
        }
    }
}
