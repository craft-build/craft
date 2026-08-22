//! Session transcript: renders the folded item stream for the active
//! session, plus the collapsible plan card and todo list.

use dioxus::prelude::*;

use crate::components::{DiffLines, Markdown, StatusPill, diff};
use crate::state::{
    AppState, ChatItem, PlanState, TodoStatus, active_tab, flatten_todos, respond_permission,
    respond_question, toggle_question_option, toggle_tool,
};

const TOOL_ICON: fn(&str) -> &str = |kind| match kind {
    "read" => "◎",
    "edit" => "✎",
    "execute" => "❱",
    "search" => "◎",
    "fetch" => "◎",
    "delete" => "✗",
    "move" => "→",
    "think" => "•",
    "switch_mode" => "⇄",
    _ => "●",
};

#[component]
pub fn Transcript() -> Element {
    let s = use_context::<AppState>();
    let Some(tab) = active_tab(s) else {
        return rsx! {};
    };

    rsx! {
        div { class: "transcript", id: "craft-transcript",
            div { class: "transcript-inner",
                if tab.items.is_empty() {
                    div { class: "empty-hint", "Ask craft to make a change in {tab.cwd}." }
                }
                for item in tab.items.iter() {
                    {match item {
                        ChatItem::User { id, text, images } => rsx! {
                            div { class: "msg-user-row", key: "{id}",
                                div { class: "msg-user",
                                    if !text.is_empty() {
                                        "{text}"
                                    }
                                    if !images.is_empty() {
                                        div { class: "msg-user-images",
                                            for (i, img) in images.iter().enumerate() {
                                                {
                                                    let uri = img.data_uri();
                                                    rsx! {
                                                        img { class: "msg-user-image", key: "{i}", src: "{uri}", alt: if img.name.is_empty() { "image" } else { img.name.as_str() } }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        ChatItem::Assistant { id, text } => rsx! {
                            div { class: "msg-agent-row", key: "{id}",
                                span { class: "msg-agent-dot", "●" }
                                Markdown { text: text.clone() }
                            }
                        },
                        ChatItem::Thinking { id, text } => rsx! {
                            div { class: "thinking-row", key: "{id}",
                                if text.is_empty() { "craft is thinking…" } else { "{text}" }
                            }
                        },
                        ChatItem::Tool { id, .. } => rsx! {
                            ToolRow { id: *id, key: "{id}" }
                        },
                        ChatItem::Permission { id, .. } => rsx! {
                            PermissionCard { id: *id, tab_id: tab.id.clone(), key: "{id}" }
                        },
                        ChatItem::Question { id, .. } => rsx! {
                            QuestionCard { id: *id, tab_id: tab.id.clone(), key: "{id}" }
                        },
                    }}
                }
            }
        }
    }
}

#[component]
fn ToolRow(id: u64) -> Element {
    let s = use_context::<AppState>();
    let tab = active_tab(s).unwrap_or_default();
    let Some(ChatItem::Tool { call, .. }) = tab.items.iter().find(|i| i.id() == id) else {
        return rsx! {};
    };
    let diff = call.content.iter().find_map(|c| match c {
        crate::state::ToolContent::Diff(d) => Some(d.clone()),
        _ => None,
    });
    let text = call
        .content
        .iter()
        .filter_map(|c| match c {
            crate::state::ToolContent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let images = call
        .content
        .iter()
        .filter_map(|c| match c {
            crate::state::ToolContent::Image(i) => Some(i.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let has_output = diff.is_some() || !text.is_empty() || !images.is_empty();
    let expanded = s.expanded_tools.read().contains(&id);
    let stats = diff
        .as_ref()
        .map(|d| diff::diff_stat(d.old_text.as_deref().unwrap_or(""), &d.new_text));

    rsx! {
        div { class: "tool-row",
            div { class: "tool-kind", "{call.kind}" }
            button {
                class: if has_output { "tool-head clickable" } else { "tool-head" },
                onclick: move |_| {
                    if has_output {
                        toggle_tool(s, id);
                    }
                },
                span { class: "tool-icon", "{TOOL_ICON(&call.kind)}" }
                span { class: "tool-title", "{call.title}" }
                if let Some((add, del)) = stats {
                    span { class: "diff-adds", "+{add}" }
                    span { class: "diff-dels", "-{del}" }
                }
                if has_output {
                    span { class: "tool-caret", if expanded { "▾" } else { "▸" } }
                }
                StatusPill { state: pill_of(call.status) }
            }
            if expanded && has_output {
                div { class: "tool-body",
                    if let Some(d) = &diff {
                        div { class: "diff-card",
                            div { class: "diff-head",
                                span { class: "diff-file", "{d.path}" }
                            }
                            DiffLines { lines: diff::diff_lines(d.old_text.as_deref().unwrap_or(""), &d.new_text) }
                        }
                    }
                    if !images.is_empty() {
                        div { class: "tool-images",
                            for (i, img) in images.iter().enumerate() {
                                {
                                    let uri = img.data_uri();
                                    rsx! {
                                        img { class: "tool-image", key: "{i}", src: "{uri}", alt: "tool output image" }
                                    }
                                }
                            }
                        }
                    }
                    if !text.is_empty() {
                        div { class: "tool-text",
                            for (i, line) in text.lines().enumerate() {
                                div { class: "tool-text-line", key: "{i}", "{line}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn pill_of(status: crate::state::ToolStatus) -> crate::components::PillState {
    use crate::components::PillState;
    match status {
        crate::state::ToolStatus::Pending => PillState::Waiting,
        crate::state::ToolStatus::InProgress => PillState::Running,
        crate::state::ToolStatus::Completed => PillState::Done,
        crate::state::ToolStatus::Failed => PillState::Failed,
    }
}

#[component]
fn PermissionCard(id: u64, tab_id: String) -> Element {
    let s = use_context::<AppState>();
    let backend = crate::backend::get();
    let tab = active_tab(s).unwrap_or_default();
    let Some(ChatItem::Permission { item, .. }) = tab.items.iter().find(|i| i.id() == id) else {
        return rsx! {};
    };
    let allow_once = item.options.iter().find(|o| o.kind == "allow_once");
    let reject_once = item.options.iter().find(|o| o.kind == "reject_once");
    let allow_id = allow_once.map(|o| o.option_id.clone());
    let reject_id = reject_once.map(|o| o.option_id.clone());
    let others = item
        .options
        .iter()
        .filter(|o| {
            Some(&o.option_id) != allow_id.as_ref() && Some(&o.option_id) != reject_id.as_ref()
        })
        .cloned()
        .collect::<Vec<_>>();
    let resolved = item.resolved;
    let chosen = item.chosen_option_id.clone();
    let title = item.title.clone();

    rsx! {
        div { class: "perm-card",
            div { class: "perm-head",
                span { class: "perm-warn", "⚠" }
                span { "Permission requested" }
            }
            div { class: "perm-body", "{title}" }
            if !resolved {
                div { class: "perm-actions",
                    if let Some(reject) = reject_once {
                        button { class: "btn btn-secondary btn-sm",
                            onclick: {
                                let rid = reject.option_id.clone();
                                let tid = tab_id.clone();
                                move |_| respond_permission(s, backend, &tid, id, Some(rid.clone()))
                            },
                            "Deny"
                        }
                    }
                    if let Some(allow) = allow_once {
                        button { class: "btn btn-primary btn-sm",
                            onclick: {
                                let rid = allow.option_id.clone();
                                let tid = tab_id.clone();
                                move |_| respond_permission(s, backend, &tid, id, Some(rid.clone()))
                            },
                            "Allow once"
                        }
                    }
                    for o in others {
                        button { class: "btn btn-ghost btn-sm", key: "{o.option_id}",
                            onclick: {
                                let rid = o.option_id.clone();
                                let tid = tab_id.clone();
                                move |_| respond_permission(s, backend, &tid, id, Some(rid.clone()))
                            },
                            "{o.name}"
                        }
                    }
                }
            } else {
                div { class: "perm-resolved",
                    if chosen.is_some() && chosen == allow_once.map(|o| o.option_id.clone()) {
                        "Allowed once."
                    } else {
                        "Resolved."
                    }
                }
            }
        }
    }
}

#[component]
fn QuestionCard(id: u64, tab_id: String) -> Element {
    let s = use_context::<AppState>();
    let backend = crate::backend::get();
    let tab = active_tab(s).unwrap_or_default();
    let Some(ChatItem::Question { item, .. }) = tab.items.iter().find(|i| i.id() == id).cloned()
    else {
        return rsx! {};
    };
    let questions = item.questions.clone();
    let selections = s
        .selections
        .read()
        .iter()
        .find(|(sid, _)| *sid == id)
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let can_submit = questions
        .iter()
        .enumerate()
        .all(|(qi, _)| selections.get(qi).is_some_and(|v| !v.is_empty()));

    let qcount = questions.len();
    let submit_backend = backend;
    let dismiss_backend = backend;
    rsx! {
        div { class: "q-card",
            div { class: "perm-head",
                span { class: "q-mark", "?" }
                span {
                    if qcount == 1 { "Question" } else { "{qcount} questions" }
                }
            }
            for (qi, q) in questions.iter().enumerate() {
                div { class: "q-question", key: "q{qi}",
                    div { class: "q-text", "{q.question}" }
                    for (oi, o) in q.options.iter().enumerate() {
                        {
                            let chosen = selections.get(qi).is_some_and(|v| v.contains(&oi));
                            let cls = if chosen { "q-option chosen" } else { "q-option" };
                            let mark = if q.multi_select {
                                if chosen { "☑" } else { "☐" }
                            } else if chosen {
                                "●"
                            } else {
                                "○"
                            };
                            rsx! {
                                button {
                                    class: cls,
                                    key: "o{oi}",
                                    disabled: item.resolved,
                                    onclick: {
                                        let resolved = item.resolved;
                                        let multi = q.multi_select;
                                        move |_| {
                                            if !resolved {
                                                toggle_question_option(s, id, qi, oi, multi);
                                            }
                                        }
                                    },
                                    span { class: "q-mark-option", "{mark}" }
                                    span {
                                        span { "{o.label}" }
                                        if let Some(desc) = &o.description {
                                            span { class: "q-desc", "{desc}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !item.resolved {
                div { class: "perm-actions",
                    button {
                        class: if can_submit { "btn btn-primary btn-sm" } else { "btn btn-ghost btn-sm" },
                        disabled: !can_submit,
                        onclick: {
                            let tid = tab_id.clone();
                            move |_| respond_question(s, submit_backend, &tid, id, true)
                        },
                        "Submit"
                    }
                    button { class: "btn btn-secondary btn-sm",
                        onclick: {
                            let tid = tab_id.clone();
                            move |_| respond_question(s, dismiss_backend, &tid, id, false)
                        },
                        "Dismiss"
                    }
                }
            } else {
                div { class: "perm-resolved", "Answered." }
            }
        }
    }
}

#[component]
pub fn PlanCard() -> Element {
    let mut s = use_context::<AppState>();
    let Some(tab) = active_tab(s) else {
        return rsx! {};
    };
    if tab.plan.is_empty() {
        return rsx! {};
    }
    let open = *s.plan_open.read();
    let plan_len = tab.plan.len();
    let done = tab
        .plan
        .iter()
        .filter(|e| e.status == PlanState::Completed)
        .count();
    let pct = (done as f64 / plan_len.max(1) as f64 * 100.0).round() as i64;

    rsx! {
        div { class: "plan-wrap",
            div { class: "plan-card",
                button { class: "plan-head", onclick: move |_| s.plan_open.toggle(),
                    span { class: "plan-caret", if open { "▾" } else { "▸" } }
                    span { class: "plan-title", "Plan" }
                    span { class: "plan-summary", "{done} of {plan_len} done" }
                    div { class: "plan-progress",
                        div { class: "plan-progress-fill", style: "width:{pct}%" }
                    }
                }
                if open {
                    div { class: "plan-body",
                        for (i, entry) in tab.plan.iter().enumerate() {
                            {let (glyph, color) = match entry.status {
                                PlanState::Completed => ("✓", "var(--status-success)"),
                                PlanState::InProgress => ("▸", "var(--status-info)"),
                                PlanState::Pending => ("○", "var(--text-disabled)"),
                            };
                            rsx! {
                                div { class: "plan-step", key: "{i}",
                                    span { class: "plan-glyph", style: "color:{color}", "{glyph}" }
                                    span { class: "plan-label", "{entry.content}" }
                                }
                            }}
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn TodoList() -> Element {
    let mut s = use_context::<AppState>();
    let Some(tab) = active_tab(s) else {
        return rsx! {};
    };
    if tab.todos.is_empty() {
        return rsx! {};
    }
    let flat = flatten_todos(&tab.todos);
    let open = *s.todo_open.read();
    let done = tab
        .todos
        .iter()
        .filter(|t| matches!(t.status, TodoStatus::Completed))
        .count();
    let total = tab.todos.len();
    rsx! {
        div { class: "todo-wrap",
            div { class: "todo-card",
                button { class: "todo-head", onclick: move |_| s.todo_open.toggle(),
                    span { class: "plan-caret", if open { "▾" } else { "▸" } }
                    span { "Todos" }
                    span { class: "todo-summary", "{done} of {total} done" }
                }
                if open {
                    for (i, (depth, item)) in flat.iter().enumerate() {
                        {
                            let (mark, color, strike) = match item.status {
                                TodoStatus::Completed => ("✓", "var(--status-success)", true),
                                TodoStatus::Cancelled => ("x", "var(--text-disabled)", true),
                                TodoStatus::InProgress => ("▸", "var(--blue-400)", false),
                                TodoStatus::Pending => ("○", "var(--text-disabled)", false),
                            };
                            rsx! {
                                div { class: "todo-item", key: "{i}", style: format!("padding-left:{}px", depth * 14 + 8),
                                    span { class: "todo-mark", style: "color:{color}", "{mark}" }
                                    span { class: if strike { "todo-text done" } else { "todo-text" }, "{item.content}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
