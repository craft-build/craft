use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb, rgba};

use crate::app::App;
use crate::theme;

use super::SIDEBAR_WIDTH;
use super::widgets::{SPINNER, initials};

pub(super) fn sidebar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_project_id = app.active_project.as_ref().map(|p| p.id.clone());
    let active_session_id = app.active_session_id.clone();
    let active_name = app
        .active_project
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "No project".into());

    div()
        .w(px(SIDEBAR_WIDTH))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(rgb(theme::PANEL_BG))
        .border_r_1()
        .border_color(rgb(theme::BORDER))
        .text_size(px(13.))
        .child(
            div()
                .id("sidebar-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(10.))
                .pt(px(14.))
                .pb(px(8.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .px(px(8.))
                        .pb(px(6.))
                        .text_size(px(12.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Projects")
                        .child(
                            div()
                                .id("sidebar-add-project")
                                .ml_auto()
                                .px(px(4.))
                                .text_size(px(15.))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                                .child("+")
                                .on_click(cx.listener(|app, _, _, cx| app.select_workspace(cx))),
                        ),
                )
                .children(app.projects.iter().enumerate().map(|(index, p)| {
                    sidebar_project_row(p, index, app, &active_project_id, &active_session_id, cx)
                })),
        )
        .child(
            div()
                .flex_shrink_0()
                .mx(px(10.))
                .py(px(10.))
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .id("sidebar-all-projects")
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(8.))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(px(22.))
                                .rounded_full()
                                .bg(rgb(theme::USER_CARD_BG))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(10.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::USER))
                                .child(initials(&active_name)),
                        )
                        .child(
                            div()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(active_name),
                        )
                        .on_click(cx.listener(|app, _, _, cx| app.go_projects(cx))),
                )
                .child(
                    div()
                        .id("sidebar-settings")
                        .flex_shrink_0()
                        .px(px(6.))
                        .text_size(px(14.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                        .child("⚙")
                        .on_click(cx.listener(|app, _, _, cx| app.go_settings(cx))),
                ),
        )
}

fn sidebar_project_row(
    p: &crate::state::Project,
    index: usize,
    app: &App,
    active_project_id: &Option<String>,
    active_session_id: &Option<String>,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let expanded = !app.collapsed_projects.contains(&p.id);
    let is_active = active_project_id.as_deref() == Some(p.id.as_str());
    let sessions = app.sessions_by_project.get(&p.id);
    let active_sessions = sessions
        .into_iter()
        .flatten()
        .filter(|session| !session.archived)
        .collect::<Vec<_>>();
    let archived_sessions = sessions
        .into_iter()
        .flatten()
        .filter(|session| session.archived)
        .collect::<Vec<_>>();
    let archives_expanded = app.expanded_archives.contains(&p.id);
    let pending_delete = app.pending_session_delete.clone();
    let toggle_id = p.id.clone();
    let row_state = SessionRowState {
        thinking: app.thinking,
        pending_comments: app.pending_comments().len(),
    };

    div()
        .when(index > 0, |d| d.mt(px(8.)))
        .child(
            div()
                .id(SharedString::from(format!("proj-{}", p.id)))
                .flex()
                .items_center()
                .gap(px(8.))
                .px(px(8.))
                .py(px(6.))
                .cursor_pointer()
                .text_color(rgb(if is_active {
                    theme::TEXT_PRIMARY
                } else {
                    theme::TEXT_SECONDARY
                }))
                .hover(|s| s.bg(rgb(theme::HOVER_BG)))
                .child(
                    div()
                        .w(px(14.))
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(if expanded { "▾" } else { "▸" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(p.name.clone()),
                )
                .on_click(
                    cx.listener(move |app, _, _, cx| app.toggle_project_collapse(&toggle_id, cx)),
                ),
        )
        .when(expanded, |d| {
            archived_sessions_section(
                active_sessions_section(
                    d,
                    p,
                    is_active,
                    active_session_id,
                    &active_sessions,
                    row_state,
                    cx,
                ),
                p,
                &archived_sessions,
                archives_expanded,
                pending_delete.as_deref(),
                cx,
            )
        })
        .into_any_element()
}

#[derive(Clone, Copy)]
struct SessionRowState {
    thinking: bool,
    pending_comments: usize,
}

fn active_sessions_section(
    d: gpui::Div,
    p: &crate::state::Project,
    is_active: bool,
    active_session_id: &Option<String>,
    active_sessions: &[&crate::state::Session],
    state: SessionRowState,
    cx: &mut Context<App>,
) -> gpui::Div {
    let add_id = p.id.clone();
    d.when(active_sessions.is_empty(), |d| {
        d.child(
            div()
                .py(px(6.))
                .pl(px(30.))
                .text_color(rgb(theme::TEXT_FAINT))
                .child("No threads yet"),
        )
    })
    .children(active_sessions.iter().map(|s| {
        let session_active = is_active && active_session_id.as_deref() == Some(s.id.as_str());
        let project_id = p.id.clone();
        let archive_project_id = p.id.clone();
        let session_id = s.id.clone();
        let archive_session_id = s.id.clone();
        let group = SharedString::from(format!("sess-row-{}-{}", p.id, s.id));
        let (glyph, glyph_color) = if session_active && state.thinking {
            (SPINNER, theme::ACCENT)
        } else if s.messages.is_empty() {
            ("·", theme::TEXT_FAINT)
        } else {
            ("✓", theme::TEXT_FAINT)
        };
        div()
            .id(SharedString::from(format!("sess-{}-{}", p.id, s.id)))
            .group(group.clone())
            .flex()
            .items_center()
            .gap(px(8.))
            .py(px(6.))
            .pl(px(28.))
            .pr(px(8.))
            .border_l_2()
            .border_color(rgba(0))
            .cursor_pointer()
            .text_color(rgb(if session_active {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_SECONDARY
            }))
            .when(session_active, |d| {
                d.bg(rgb(theme::SELECTION)).border_color(rgb(theme::ACCENT))
            })
            .hover(|s| s.bg(rgb(theme::HOVER_BG)))
            .child(
                div()
                    .w(px(10.))
                    .flex_shrink_0()
                    .font_family(theme::MONO_FONT_FAMILY)
                    .text_size(px(11.))
                    .text_color(rgb(glyph_color))
                    .child(glyph),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(s.name.clone()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("archive-session-{}", s.id)))
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgba(0))
                    .group_hover(group, |style| style.text_color(rgb(theme::TEXT_MUTED)))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme::ACCENT)))
                    .child("archive")
                    .on_click(cx.listener(move |app, _, _, cx| {
                        cx.stop_propagation();
                        app.archive_session(&archive_project_id, &archive_session_id, cx)
                    })),
            )
            .when(session_active && state.pending_comments > 0, |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .font_family(theme::MONO_FONT_FAMILY)
                        .text_size(px(10.))
                        .text_color(rgb(theme::USER))
                        .child(state.pending_comments.to_string()),
                )
            })
            .on_click(
                cx.listener(move |app, _, _, cx| app.open_session(&project_id, &session_id, cx)),
            )
    }))
    .child(
        div()
            .id(SharedString::from(format!("add-sess-{}", p.id)))
            .py(px(5.))
            .pl(px(30.))
            .pr(px(12.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(theme::TEXT_FAINT))
            .hover(|s| s.text_color(rgb(theme::ACCENT)))
            .child("+ new thread")
            .on_click(cx.listener(move |app, _, _, cx| app.add_session_to_project(&add_id, cx))),
    )
}

fn archived_sessions_section(
    d: gpui::Div,
    p: &crate::state::Project,
    archived_sessions: &[&crate::state::Session],
    archives_expanded: bool,
    pending_delete: Option<&str>,
    cx: &mut Context<App>,
) -> gpui::Div {
    d.when(!archived_sessions.is_empty(), |d| {
        let archive_toggle_id = p.id.clone();
        d.child(
            div()
                .id(SharedString::from(format!("archived-sessions-{}", p.id)))
                .flex()
                .items_center()
                .gap(px(5.))
                .py(px(5.))
                .pl(px(30.))
                .pr(px(8.))
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_FAINT))
                .cursor_pointer()
                .hover(|style| style.text_color(rgb(theme::TEXT_SECONDARY)))
                .child(if archives_expanded { "▾" } else { "▸" })
                .child(format!("Archived ({})", archived_sessions.len()))
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.toggle_archived_sessions(&archive_toggle_id, cx)
                })),
        )
        .when(archives_expanded, |d| {
            d.children(archived_sessions.iter().map(|session| {
                let restore_project_id = p.id.clone();
                let delete_project_id = p.id.clone();
                let restore_session_id = session.id.clone();
                let delete_session_id = session.id.clone();
                let delete_armed = pending_delete == Some(session.id.as_str());
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .py(px(5.))
                    .pl(px(36.))
                    .pr(px(8.))
                    .text_size(px(12.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(session.name.clone()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "restore-session-{}",
                                session.id
                            )))
                            .cursor_pointer()
                            .text_color(rgb(theme::ACCENT))
                            .child("restore")
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.restore_session(&restore_project_id, &restore_session_id, cx)
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("delete-session-{}", session.id)))
                            .cursor_pointer()
                            .text_color(rgb(if delete_armed {
                                theme::DIFF_DEL_TEXT
                            } else {
                                theme::TEXT_MUTED
                            }))
                            .child(if delete_armed { "confirm" } else { "×" })
                            .on_click(cx.listener(move |app, _, _, cx| {
                                app.request_delete_session(
                                    &delete_project_id,
                                    &delete_session_id,
                                    cx,
                                )
                            })),
                    )
            }))
        })
    })
}
