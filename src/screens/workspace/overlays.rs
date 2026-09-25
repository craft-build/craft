use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb, rgba};

use crate::app::App;
use crate::theme;

use super::tool_call::diff_line_view;

// ---------------------------------------------------------------- right panels

pub(super) fn right_panels(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_diff = app.active_diff_file.clone();
    let files = app.changed_files().to_vec();

    div()
        .flex()
        .flex_shrink_0()
        .when_some(active_diff.clone(), |d, path| {
            let Some(diff) = app.file_diffs.get(&path).cloned() else {
                return d;
            };
            d.child(
                div()
                    .w(px(360.))
                    .flex_shrink_0()
                    .bg(rgb(theme::BG))
                    .border_l_1()
                    .border_color(rgb(theme::BORDER))
                    .id("active-diff-scroll")
                    .track_scroll(&app.diff_scroll)
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
                        diff_line_view(line, key, label, app.diff_scroll.clone(), app, cx)
                    })),
            )
        })
        .child(changed_files_panel(&files, active_diff.as_deref(), app, cx))
}

// ---------------------------------------------------------------- checkpoints & toast

pub(super) fn checkpoints_overlay(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
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
fn changed_files_panel(
    files: &[crate::app::WorkspaceFile],
    active_diff: Option<&str>,
    _app: &App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    div()
        .w(px(200.))
        .flex_shrink_0()
        .overflow_hidden()
        .bg(rgb(theme::PANEL_BG))
        .border_l_1()
        .border_color(rgb(theme::BORDER))
        .id("changed-files-scroll")
        .overflow_y_scroll()
        .py(px(10.))
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .px(px(12.))
                .pb(px(8.))
                .child("Changed files"),
        )
        .when(files.is_empty(), |d| {
            d.child(
                div()
                    .px(px(12.))
                    .py(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_MUTED))
                    .child("Working tree clean"),
            )
        })
        .children(files.iter().map(|f| {
            let path = f.path.clone();
            let supports_text_diff = f.supports_text_diff;
            let is_active = active_diff == Some(f.path.as_str());
            let dot = if f.status.as_deref() == Some("added") {
                theme::DIFF_ADD_TEXT
            } else {
                theme::ACCENT
            };
            let label = f.path.rsplit('/').next().unwrap_or(&f.path).to_string();
            let click_path = path.clone();
            let attach_path = path.clone();
            div()
                .id(SharedString::from(format!("changed-file-{path}")))
                .flex()
                .items_center()
                .w_full()
                .overflow_hidden()
                .gap(px(6.))
                .px(px(12.))
                .py(px(4.))
                .when(supports_text_diff, |d| d.cursor_pointer())
                .when(is_active, |d| d.bg(rgb(theme::INPUT_BG)))
                .when(supports_text_diff, |d| {
                    d.hover(|s| s.bg(rgb(theme::HOVER_BG)))
                })
                .child(div().w(px(6.)).h(px(6.)).flex_shrink_0().bg(rgb(dot)))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_size(px(12.))
                        .text_color(rgb(if supports_text_diff {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(label),
                )
                .when(!supports_text_diff, |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(9.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("binary"),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("attach-file-{path}")))
                        .ml_auto()
                        .flex_shrink_0()
                        .px(px(2.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::ACCENT))
                        .cursor_pointer()
                        .child("+")
                        .on_click(cx.listener(move |app, _, _, cx| {
                            cx.stop_propagation();
                            app.add_context_file(&attach_path, cx);
                        })),
                )
                .when(supports_text_diff, |d| {
                    d.on_click(
                        cx.listener(move |app, _, _, cx| app.toggle_diff_file(&click_path, cx)),
                    )
                })
        }))
        .into_any_element()
}
