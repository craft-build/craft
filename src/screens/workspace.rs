use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, Window, div, px, rgb, rgba};

use crate::app::App;
use crate::chrome;
use crate::state::{Comment, Diff, DiffLine, DiffLineKind, Message, Role, Steps, Terminal};
use crate::theme;

pub fn render(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .relative()
        .child(top_bar(app, window, cx))
        .child(
            div()
                .flex_1()
                .flex()
                .overflow_x_hidden()
                .overflow_y_hidden()
                .when(app.sidebar_visible, |d| d.child(sidebar(app, cx)))
                .child(main_column(app, window, cx))
                .when(app.file_tree_visible, |d| d.child(right_panels(app, cx))),
        )
        .child(footer_bar(app, cx))
        .when(app.show_checkpoints, |d| d.child(checkpoints_overlay(app, cx)))
        .when(app.toast.is_some(), |d| d.child(toast_view(app)))
}

// ---------------------------------------------------------------- top bar

fn top_bar(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let name = app
        .active_project
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let path = app
        .active_project
        .as_ref()
        .map(|p| p.path.clone())
        .unwrap_or_default();
    let checkpoint_count = app.checkpoints_for().len();

    chrome::draggable(
        div()
            .h(px(44.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .pl(px(chrome::leading_inset()))
            .pr(px(12.))
            .border_b_1()
            .border_color(rgb(theme::BORDER)),
    )
    .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(icon_button("toggle-sidebar", "☰", cx, |app, cx| app.toggle_sidebar(cx)))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(name),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(path),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .id("toggle-checkpoints")
                        .px(px(10.))
                        .py(px(6.))
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .text_size(px(12.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .cursor_pointer()
                        .child(format!("Checkpoints ({checkpoint_count})"))
                        .on_click(cx.listener(|app, _, _, cx| app.toggle_checkpoints(cx))),
                )
                .child(
                    div()
                        .id("goto-settings")
                        .px(px(10.))
                        .py(px(6.))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .text_size(px(12.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .cursor_pointer()
                        .child("Settings")
                        .on_click(cx.listener(|app, _, _, cx| app.go_settings(cx))),
                )
                .child(icon_button("toggle-file-tree", "☰", cx, |app, cx| {
                    app.toggle_file_tree(cx)
                }))
                .child(chrome::window_controls(window, cx)),
        )
}

fn icon_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<App>,
    on_click: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .text_size(px(14.))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .cursor_pointer()
        .px(px(4.))
        .py(px(2.))
        .child(label)
        .on_click(cx.listener(move |app, _, _, cx| on_click(app, cx)))
}

// ---------------------------------------------------------------- sidebar

fn sidebar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_project_id = app.active_project.as_ref().map(|p| p.id.clone());
    let active_session_id = app.active_session_id.clone();

    div()
        .w(px(190.))
        .flex_shrink_0()
        .border_r_1()
        .border_color(rgb(theme::BORDER))
        .id("sidebar-scroll")
        .overflow_y_scroll()
        .py(px(10.))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .px(px(12.))
                .pb(px(8.))
                .child("PROJECTS"),
        )
        .children(app.projects.clone().into_iter().map(|p| {
            let expanded = !app.collapsed_projects.contains(&p.id);
            let is_active = active_project_id.as_deref() == Some(p.id.as_str());
            let sessions = app
                .sessions_by_project
                .get(&p.id)
                .cloned()
                .unwrap_or_default();
            let toggle_id = p.id.clone();
            let add_id = p.id.clone();

            div()
                .child(
                    div()
                        .id(SharedString::from(format!("proj-{}", p.id)))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(12.))
                        .py(px(5.))
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                        .child(
                            div()
                                .w(px(10.))
                                .text_size(px(10.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(if expanded { "▾" } else { "▸" }),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(if is_active {
                                    theme::ACCENT
                                } else {
                                    theme::TEXT_PRIMARY
                                }))
                                .child(p.name.clone()),
                        )
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.toggle_project_collapse(&toggle_id, cx)
                        })),
                )
                .when(expanded, |d| {
                    d.child(div().children(sessions.into_iter().map(|s| {
                        let session_active =
                            is_active && active_session_id.as_deref() == Some(s.id.as_str());
                        let project_id = p.id.clone();
                        let session_id = s.id.clone();
                        div()
                            .id(SharedString::from(format!("sess-{}-{}", p.id, s.id)))
                            .py(px(5.))
                            .pl(px(28.))
                            .pr(px(12.))
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(if session_active {
                                theme::TEXT_PRIMARY
                            } else {
                                theme::TEXT_SECONDARY
                            }))
                            .when(session_active, |d| d.bg(rgb(theme::INPUT_BG)))
                            .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                            .child(s.name.clone())
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.open_session(&project_id, &session_id, cx)
                            }))
                    })))
                    .child(
                        div()
                            .id(SharedString::from(format!("add-sess-{}", p.id)))
                            .py(px(5.))
                            .pl(px(28.))
                            .pr(px(12.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .hover(|s| s.text_color(rgb(theme::ACCENT)))
                            .child("+ new session")
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.add_session_to_project(&add_id, cx)
                            })),
                    )
                })
        }))
}

// ---------------------------------------------------------------- main column

fn main_column(app: &mut App, _window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let messages = app.active_messages();
    let last_index = messages.len().saturating_sub(1);
    app.thread_scroll.scroll_to_item(last_index);
    let scroll_handle = app.thread_scroll.clone();

    div()
        .flex_1()
        .min_w(px(340.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .id("thread-scroll")
                .track_scroll(&scroll_handle)
                .flex_1()
                .overflow_y_scroll()
                .px(px(20.))
                .py(px(18.))
                .flex()
                .flex_col()
                .gap(px(14.))
                .children(messages.into_iter().map(|m| message_view(m, app, cx)))
                .when(app.thinking, |d| d.child(thinking_view()))
                .child(composer_view(app, cx)),
        )
}

fn thinking_view() -> impl IntoElement {
    div().max_w(px(760.)).child(
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::TEXT_SECONDARY))
                    .child("assistant"),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::TEXT_SECONDARY)))
                    .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::TEXT_SECONDARY)))
                    .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::TEXT_SECONDARY))),
            ),
    )
}

fn message_view(m: Message, app: &mut App, cx: &mut Context<App>) -> gpui::AnyElement {
    match m.role {
        Role::User => user_message(m).into_any_element(),
        Role::Assistant => assistant_message(m, app, cx).into_any_element(),
    }
}

fn user_message(m: Message) -> impl IntoElement {
    div().max_w(px(760.)).flex().flex_col().gap(px(4.)).child(
        div()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(theme::ACCENT))
            .child("you"),
    ).child(
        div()
            .text_size(px(13.))
            .line_height(px(21.))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .child(m.text.clone()),
    ).when(!m.context.is_empty(), |d| {
        d.child(
            div()
                .flex()
                .gap(px(6.))
                .flex_wrap()
                .children(m.context.iter().map(|c| chip_view(c.clone()))),
        )
    }).when(!m.attached_comments.is_empty(), |d| {
        d.child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.))
                .border_l_2()
                .border_color(rgb(theme::BORDER))
                .pl(px(8.))
                .children(m.attached_comments.iter().map(|(label, text)| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(format!("{label}: {text}"))
                })),
        )
    })
}

fn chip_view(text: String) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .px(px(6.))
        .py(px(2.))
        .bg(rgb(theme::INPUT_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .child(text)
}

fn assistant_message(m: Message, app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let msg_id = m.id.clone();
    let comment_key = format!("msg_{}", m.id);
    let steps_expanded = app.expanded_steps.contains(&msg_id);
    let toggle_steps_id = msg_id.clone();
    let toggle_comment_key = comment_key.clone();

    let mut col = div()
        .max_w(px(760.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::PANEL_BG))
        .p(px(14.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child("assistant"),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("comment-toggle-{msg_id}")))
                        .text_size(px(12.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .cursor_pointer()
                        .child("💬")
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.open_comment_box(
                                toggle_comment_key.clone(),
                                "assistant reply".to_string(),
                                cx,
                            )
                        })),
                ),
        );

    if let Some(steps) = &m.steps {
        col = col.child(steps_view(steps, steps_expanded, toggle_steps_id, cx));
    }
    if let Some(diff) = &m.diff {
        let key_prefix = msg_id.clone();
        let file = diff.file.clone();
        col = col.child(diff_view(
            diff,
            move |idx| (format!("{key_prefix}_{idx}"), format!("{file} line {}", idx + 1)),
            app,
            cx,
        ));
    }
    if let Some(term) = &m.terminal {
        col = col.child(terminal_view(term));
    }
    col = col.child(
        div()
            .text_size(px(13.))
            .line_height(px(21.))
            .text_color(rgb(theme::TEXT_PRIMARY))
            .child(m.text.clone()),
    );
    if let Some(cp) = &m.checkpoint_label {
        col = col.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .border_t_1()
                .border_color(rgb(theme::TERMINAL_BORDER))
                .pt(px(6.))
                .child(cp.clone()),
        );
    }
    if let Some(list) = app.comments.get(&comment_key).cloned() {
        if !list.is_empty() {
            col = col.child(comments_list(&list));
        }
    }
    if app.open_comment_boxes.contains(&comment_key) {
        if let Some(input) = app.comment_inputs.get(&comment_key).cloned() {
            let submit_key = comment_key.clone();
            col = col.child(comment_box(input, submit_key, cx));
        }
    }
    col
}

fn steps_view(
    steps: &Steps,
    expanded: bool,
    msg_id: String,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let items = steps.items.clone();
    div().child(
        div()
            .id(SharedString::from(format!("steps-{msg_id}")))
            .flex()
            .items_center()
            .gap(px(6.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(theme::TEXT_MUTED))
            .child(if expanded { "▾" } else { "▸" })
            .child(steps.summary.clone())
            .on_click(cx.listener(move |app, _, _, cx| app.toggle_steps(&msg_id, cx))),
    ).when(expanded, |d| {
        d.child(
            div()
                .mt(px(6.))
                .pl(px(16.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .children(items.into_iter().map(|item| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(format!("· {item}"))
                })),
        )
    })
}

fn terminal_view(term: &Terminal) -> impl IntoElement {
    div()
        .bg(rgb(theme::TERMINAL_BG))
        .border_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .px(px(10.))
        .py(px(8.))
        .text_size(px(12.))
        .flex()
        .flex_col()
        .child(
            div()
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(format!("$ {}", term.cmd)),
        )
        .child(
            div()
                .mt(px(2.))
                .text_color(rgb(theme::DIFF_ADD_TEXT))
                .child(term.output.clone()),
        )
}

fn comments_list(list: &[Comment]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .border_t_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .pt(px(6.))
        .children(list.iter().map(|c| {
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(format!("{}: {}", c.author, c.text))
        }))
}

fn comment_box(
    input: gpui::Entity<crate::text_input::TextInput>,
    key: String,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div()
        .flex()
        .gap(px(6.))
        .child(
            div()
                .flex_1()
                .bg(rgb(theme::INPUT_BG))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .text_size(px(11.))
                .px(px(6.))
                .py(px(4.))
                .child(input.clone()),
        )
        .child(
            div()
                .id(SharedString::from(format!("add-comment-{key}")))
                .bg(rgb(theme::SELECTION))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_PRIMARY))
                .px(px(8.))
                .py(px(4.))
                .cursor_pointer()
                .child("Add")
                .on_click(cx.listener(move |_app, _, window, cx| {
                    input.update(cx, |ti, cx| ti.submit(window, cx));
                })),
        )
}

// ---------------------------------------------------------------- diff rendering (shared)

fn diff_view(
    diff: &Diff,
    key_fn: impl Fn(usize) -> (String, String) + 'static,
    app: &mut App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div().border_1().border_color(rgb(theme::TERMINAL_BORDER)).child(
        div()
            .flex()
            .justify_between()
            .px(px(10.))
            .py(px(6.))
            .bg(rgb(theme::INPUT_BG))
            .border_b_1()
            .border_color(rgb(theme::TERMINAL_BORDER))
            .child(div().text_size(px(12.)).child(diff.file.clone()))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(diff.stat.clone()),
            ),
    ).child(
        div()
            .text_size(px(11.))
            .text_color(rgb(theme::TEXT_MUTED))
            .px(px(10.))
            .py(px(4.))
            .child(diff.hunk_header.clone()),
    ).children(diff.lines.iter().enumerate().map(|(idx, line)| {
        let (key, label) = key_fn(idx);
        diff_line_view(line, key, label, app, cx)
    }))
}

fn diff_line_view(
    line: &DiffLine,
    key: String,
    label: String,
    app: &mut App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let (gutter, gutter_color, bg): (&str, u32, Option<u32>) = match line.kind {
        DiffLineKind::Add => ("+", theme::DIFF_ADD_TEXT, Some(theme::DIFF_ADD_BG)),
        DiffLineKind::Del => ("-", theme::DIFF_DEL_TEXT, Some(theme::DIFF_DEL_BG)),
        DiffLineKind::Ctx => (" ", theme::TEXT_MUTED, None),
    };

    let comments = app.comments.get(&key).cloned().unwrap_or_default();
    let open = app.open_comment_boxes.contains(&key);
    let toggle_key = key.clone();

    let row = div()
        .flex()
        .when_some(bg, |d, bg| d.bg(rgba(bg)))
        .px(px(10.))
        .py(px(1.))
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_size(px(12.))
                .text_color(rgb(gutter_color))
                .child(gutter),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(12.))
                .whitespace_nowrap()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(line.text.clone()),
        )
        .child(
            div()
                .id(SharedString::from(format!("edit-{key}")))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .cursor_pointer()
                .child("✎")
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.open_comment_box(toggle_key.clone(), label.clone(), cx)
                })),
        );

    let mut wrapper = div().child(row);
    if !comments.is_empty() {
        wrapper = wrapper.child(
            div()
                .bg(rgb(theme::INPUT_BG))
                .pl(px(26.))
                .pr(px(10.))
                .py(px(4.))
                .flex()
                .flex_col()
                .gap(px(3.))
                .children(comments.iter().map(|c| {
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(format!("{}: {}", c.author, c.text))
                })),
        );
    }
    if open {
        if let Some(input) = app.comment_inputs.get(&key).cloned() {
            let submit_key = key.clone();
            wrapper = wrapper.child(
                div()
                    .bg(rgb(theme::INPUT_BG))
                    .pl(px(26.))
                    .pr(px(10.))
                    .py(px(4.))
                    .child(comment_box(input, submit_key, cx)),
            );
        }
    }
    wrapper.into_any_element()
}

// ---------------------------------------------------------------- composer

fn composer_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let pending = app.pending_comments();
    let has_chips = !app.context_chips.is_empty();
    let chips = app.context_chips.clone();
    let composer = app.composer.clone();

    div().max_w(px(760.)).flex().flex_col().gap(px(4.))
        .when(has_chips, |d| {
            d.child(
                div()
                    .flex()
                    .gap(px(6.))
                    .flex_wrap()
                    .mb(px(4.))
                    .children(chips.into_iter().enumerate().map(|(idx, path)| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .px(px(6.))
                            .py(px(2.))
                            .bg(rgb(theme::INPUT_BG))
                            .border_1()
                            .border_color(rgb(theme::BORDER))
                            .text_color(rgb(theme::TEXT_SECONDARY))
                            .child(path)
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-chip-{idx}")))
                                    .cursor_pointer()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .child("×")
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        app.context_chips.remove(idx);
                                        cx.notify();
                                    })),
                            )
                    })),
            )
        })
        .when(!pending.is_empty(), |d| {
            d.child(
                div()
                    .flex()
                    .gap(px(6.))
                    .flex_wrap()
                    .mb(px(4.))
                    .children(pending.into_iter().map(|p| {
                        let key = p.key.clone();
                        let idx = p.idx;
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .px(px(6.))
                            .py(px(2.))
                            .bg(rgba(theme::PENDING_CHIP_BG))
                            .border_1()
                            .border_color(rgba(theme::PENDING_CHIP_BORDER))
                            .text_color(rgb(theme::ACCENT))
                            .child(format!("{}: {}", p.label, p.text))
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-pending-{key}-{idx}")))
                                    .cursor_pointer()
                                    .child("×")
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        app.remove_pending_comment(&key, idx, cx)
                                    })),
                            )
                    })),
            )
        })
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::ACCENT))
                .child("you"),
        )
        .child(
            div()
                .text_size(px(13.))
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(composer),
        )
}

// ---------------------------------------------------------------- right panels

fn right_panels(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_diff = app.active_diff_file.clone();
    let files = app.changed_files();

    div().flex().flex_shrink_0()
        .when_some(active_diff.clone(), |d, path| {
            let Some(diff) = app.file_diffs.get(&path).cloned() else {
                return d;
            };
            d.child(
                div()
                    .w(px(360.))
                    .flex_shrink_0()
                    .border_l_1()
                    .border_color(rgb(theme::BORDER))
                    .id("active-diff-scroll")
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .px(px(10.))
                            .py(px(8.))
                            .bg(rgb(theme::INPUT_BG))
                            .border_b_1()
                            .border_color(rgb(theme::TERMINAL_BORDER))
                            .child(div().text_size(px(12.)).child(diff.file.clone()))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .child(diff.stat.clone()),
                                    )
                                    .child(
                                        div()
                                            .id("close-diff")
                                            .cursor_pointer()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .child("×")
                                            .on_click(cx.listener(|app, _, _, cx| {
                                                app.active_diff_file = None;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .px(px(10.))
                            .py(px(4.))
                            .child(diff.hunk_header.clone()),
                    )
                    .children(diff.lines.iter().enumerate().map(|(idx, line)| {
                        let key = format!("f_{path}_{idx}");
                        let label = format!("{path} line {}", idx + 1);
                        diff_line_view(line, key, label, app, cx)
                    })),
            )
        })
        .child(
            div()
                .w(px(200.))
                .flex_shrink_0()
                .border_l_1()
                .border_color(rgb(theme::BORDER))
                .id("changed-files-scroll")
                .overflow_y_scroll()
                .py(px(10.))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .px(px(12.))
                        .pb(px(8.))
                        .child("CHANGED FILES"),
                )
                .children(files.iter().map(|f| {
                    let path = f.path.to_string();
                    let is_active = active_diff.as_deref() == Some(f.path);
                    let dot = if f.status == Some("added") {
                        theme::DIFF_ADD_TEXT
                    } else {
                        theme::ACCENT
                    };
                    let label = f.path.rsplit('/').next().unwrap_or(f.path).to_string();
                    let click_path = path.clone();
                    div()
                        .id(SharedString::from(format!("changed-file-{path}")))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .px(px(12.))
                        .py(px(4.))
                        .cursor_pointer()
                        .when(is_active, |d| d.bg(rgb(theme::INPUT_BG)))
                        .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                        .child(div().w(px(6.)).h(px(6.)).flex_shrink_0().bg(rgb(dot)))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(label),
                        )
                        .on_click(cx.listener(move |app, _, _, cx| {
                            app.toggle_diff_file(&click_path, cx)
                        }))
                })),
        )
}

// ---------------------------------------------------------------- footer

fn footer_bar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let percent = (18 + app.sent_count as i32 * 9).min(100);
    let status_label = if app.thinking { "Running" } else { "Idle" };
    let status_color = if app.thinking { theme::ACCENT } else { theme::TEXT_MUTED };
    let selected_model = app.selected_model.clone();
    let menu_open = app.model_menu_open;

    div()
        .h(px(32.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_between()
        .px(px(16.))
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::FOOTER_BG))
        .child(
            div().relative().child(
                div()
                    .id("model-menu-toggle")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_SECONDARY))
                    .cursor_pointer()
                    .child(selected_model)
                    .child(div().text_color(rgb(theme::TEXT_MUTED)).child("▾"))
                    .on_click(cx.listener(|app, _, _, cx| app.toggle_model_menu(cx)))
            ).when(menu_open, |d| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(28.))
                        .left(px(0.))
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .min_w(px(150.))
                        .children(crate::state::MODEL_NAMES.iter().map(|&name| {
                            div()
                                .id(SharedString::from(format!("footer-model-{name}")))
                                .px(px(10.))
                                .py(px(8.))
                                .text_size(px(12.))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                                .child(name)
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    app.select_model(name, cx)
                                }))
                        })),
                )
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .child(
                            div()
                                .w(px(14.))
                                .h(px(14.))
                                .rounded_full()
                                .bg(rgb(theme::ACCENT))
                                .opacity((percent as f32 / 100.0).clamp(0.15, 1.0)),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(format!("{percent}% context")),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(
                            div()
                                .w(px(6.))
                                .h(px(6.))
                                .rounded_full()
                                .bg(rgb(status_color)),
                        )
                        .child(status_label),
                ),
        )
}

// ---------------------------------------------------------------- checkpoints & toast

fn checkpoints_overlay(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let mut checkpoints = app.checkpoints_for();
    checkpoints.reverse();

    div()
        .absolute()
        .inset_0()
        .child(
            div()
                .id("checkpoints-backdrop")
                .absolute()
                .inset_0()
                .bg(rgba(theme::OVERLAY))
                .on_click(cx.listener(|app, _, _, cx| app.toggle_checkpoints(cx))),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(300.))
                .bg(rgb(theme::PANEL_BG))
                .border_l_1()
                .border_color(rgb(theme::BORDER))
                .id("checkpoints-scroll")
                .overflow_y_scroll()
                .p(px(14.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .mb(px(12.))
                        .child("Checkpoint history"),
                )
                .children(checkpoints.into_iter().map(|(label, time)| {
                    let restore_label = label.clone();
                    div()
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .p(px(10.))
                        .mb(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(time),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("restore-{restore_label}")))
                                .mt(px(4.))
                                .px(px(8.))
                                .py(px(4.))
                                .border_1()
                                .border_color(rgb(theme::BORDER))
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_SECONDARY))
                                .cursor_pointer()
                                .child("Restore")
                                .on_click(cx.listener(move |app, _, _, cx| {
                                    app.restore_checkpoint(&restore_label, cx)
                                })),
                        )
                })),
        )
}

fn toast_view(app: &App) -> impl IntoElement {
    div()
        .absolute()
        .bottom(px(16.))
        .right(px(16.))
        .bg(rgb(theme::INPUT_BG))
        .border_1()
        .border_color(rgb(theme::SELECTION))
        .text_color(rgb(theme::TEXT_PRIMARY))
        .text_size(px(12.))
        .px(px(12.))
        .py(px(8.))
        .child(app.toast.clone().unwrap_or_default())
}
