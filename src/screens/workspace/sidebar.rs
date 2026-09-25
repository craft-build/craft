use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb};

use crate::app::App;
use crate::theme;

pub(super) fn sidebar(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_project_id = app.active_project.as_ref().map(|p| p.id.clone());
    let active_session_id = app.active_session_id.clone();

    div()
        .w(px(210.))
        .flex_shrink_0()
        .bg(rgb(theme::PANEL_BG))
        .border_r_1()
        .border_color(rgb(theme::BORDER))
        .id("sidebar-scroll")
        .overflow_y_scroll()
        .py(px(8.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .px(px(12.))
                .pb(px(6.))
                .child("Projects"),
        )
        .children(
            app.projects
                .iter()
                .map(|p| sidebar_project_row(p, app, &active_project_id, &active_session_id, cx)),
        )
}

fn sidebar_project_row(
    p: &crate::state::Project,
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

    div()
        .child(
            div()
                .id(SharedString::from(format!("proj-{}", p.id)))
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .py(px(4.))
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
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(if is_active {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_SECONDARY
                        }))
                        .child(p.name.clone()),
                )
                .on_click(
                    cx.listener(move |app, _, _, cx| app.toggle_project_collapse(&toggle_id, cx)),
                ),
        )
        .when(expanded, |d| {
            archived_sessions_section(
                active_sessions_section(d, p, is_active, active_session_id, &active_sessions, cx),
                p,
                &archived_sessions,
                archives_expanded,
                pending_delete.as_deref(),
                cx,
            )
        })
        .into_any_element()
}

fn active_sessions_section(
    d: gpui::Div,
    p: &crate::state::Project,
    is_active: bool,
    active_session_id: &Option<String>,
    active_sessions: &[&crate::state::Session],
    cx: &mut Context<App>,
) -> gpui::Div {
    let add_id = p.id.clone();
    d.child(div().children(active_sessions.iter().map(|s| {
        let session_active = is_active && active_session_id.as_deref() == Some(s.id.as_str());
        let project_id = p.id.clone();
        let archive_project_id = p.id.clone();
        let session_id = s.id.clone();
        let archive_session_id = s.id.clone();
        div()
            .id(SharedString::from(format!("sess-{}-{}", p.id, s.id)))
            .flex()
            .items_center()
            .py(px(4.))
            .pl(px(28.))
            .pr(px(8.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(if session_active {
                theme::TEXT_PRIMARY
            } else {
                theme::TEXT_SECONDARY
            }))
            .when(session_active, |d| d.bg(rgb(theme::SELECTION)))
            .hover(|s| s.bg(rgb(theme::HOVER_BG)))
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
                    .px(px(4.))
                    .text_size(px(10.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme::ACCENT)))
                    .child("archive")
                    .on_click(cx.listener(move |app, _, _, cx| {
                        cx.stop_propagation();
                        app.archive_session(&archive_project_id, &archive_session_id, cx)
                    })),
            )
            .on_click(
                cx.listener(move |app, _, _, cx| app.open_session(&project_id, &session_id, cx)),
            )
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
                .pl(px(28.))
                .pr(px(8.))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
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
                    .pl(px(34.))
                    .pr(px(8.))
                    .text_size(px(11.))
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
