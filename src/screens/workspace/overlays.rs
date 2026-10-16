use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb, rgba};

use crate::app::{App, ReviewTab};
use crate::state::Comment;
use crate::theme;

use super::tool_call::{diff_header, diff_line_view, line_numbers};
use super::widgets::{ButtonKind, bar_card, button};

// ---------------------------------------------------------------- right panels

pub(super) fn right_panels(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let active_diff = app.active_diff_file.clone();

    div()
        .flex()
        .flex_shrink_0()
        .when_some(active_diff.clone(), |d, path| {
            let Some(diff) = app.file_diffs.get(&path).cloned() else {
                return d;
            };
            let comment_count = (0..diff.lines.len())
                .map(|idx| {
                    app.comments
                        .get(&app.comment_key(&format!("f_{path}_{idx}")))
                        .map_or(0, Vec::len)
                })
                .sum();
            let numbers = line_numbers(&diff);
            d.child(
                div()
                    .w(px(400.))
                    .flex_shrink_0()
                    .bg(rgb(theme::BG))
                    .border_l_1()
                    .border_color(rgb(theme::BORDER))
                    .font_family(theme::MONO_FONT_FAMILY)
                    .id("active-diff-scroll")
                    .track_scroll(&app.diff_scroll)
                    .overflow_y_scroll()
                    .child(
                        div()
                            .bg(rgb(theme::CARD_BG))
                            .border_b_1()
                            .border_color(rgb(theme::BORDER))
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .child(diff_header(&diff, comment_count)),
                            )
                            .child(
                                div()
                                    .id("close-diff")
                                    .px(px(12.))
                                    .cursor_pointer()
                                    .text_color(rgb(theme::TEXT_MUTED))
                                    .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                                    .child("×")
                                    .on_click(cx.listener(|app, _, _, cx| {
                                        app.active_diff_file = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_FAINT))
                            .px(px(14.))
                            .py(px(4.))
                            .child(diff.hunk_header.clone()),
                    )
                    .children(diff.lines.iter().zip(numbers).enumerate().map(
                        |(idx, (line, number))| {
                            let key = format!("f_{path}_{idx}");
                            let label = format!("{path} line {}", idx + 1);
                            diff_line_view(
                                line,
                                number,
                                key,
                                label,
                                app.diff_scroll.clone(),
                                app,
                                cx,
                            )
                        },
                    )),
            )
        })
        .child(review_panel(app, cx))
}

fn review_panel(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let tab = app.review_tab;
    let comments = app.session_comments();
    let pending = comments.iter().filter(|(_, _, c)| c.pending).count();
    let files = app.changed_files().to_vec();
    let active_diff = app.active_diff_file.clone();

    div()
        .w(px(320.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .bg(rgb(theme::PANEL_BG))
        .border_l_1()
        .border_color(rgb(theme::BORDER))
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .gap(px(18.))
                .px(px(16.))
                .pt(px(12.))
                .border_b_1()
                .border_color(rgb(theme::BORDER))
                .text_size(px(12.5))
                .child(tab_header(
                    "review-tab-comments",
                    "Comments",
                    comments.len(),
                    tab == ReviewTab::Comments,
                    ReviewTab::Comments,
                    cx,
                ))
                .child(tab_header(
                    "review-tab-changes",
                    "Changes",
                    files.len(),
                    tab == ReviewTab::Changes,
                    ReviewTab::Changes,
                    cx,
                )),
        )
        .child(match tab {
            ReviewTab::Comments => comments_tab(comments, cx).into_any_element(),
            ReviewTab::Changes => {
                changes_tab(&files, active_diff.as_deref(), cx).into_any_element()
            }
        })
        .when(tab == ReviewTab::Comments, |d| {
            d.child(
                div()
                    .flex_shrink_0()
                    .p(px(12.))
                    .border_t_1()
                    .border_color(rgb(theme::BORDER))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .font_family(theme::MONO_FONT_FAMILY)
                            .text_size(px(11.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(format!("{pending} pending"))
                            .child("sent with your next message"),
                    )
                    .child(if pending > 0 {
                        button(
                            "send-review-comments",
                            format!(
                                "Send {pending} comment{} to the agent",
                                if pending == 1 { "" } else { "s" }
                            ),
                            ButtonKind::Primary(theme::USER),
                            cx,
                            |app, cx| app.submit_composer(cx),
                        )
                        .py(px(8.))
                        .flex()
                        .justify_center()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .into_any_element()
                    } else {
                        div()
                            .py(px(8.))
                            .flex()
                            .justify_center()
                            .bg(rgb(theme::DIVIDER))
                            .text_size(px(13.))
                            .text_color(rgb(theme::TEXT_FAINT))
                            .child("No pending comments")
                            .into_any_element()
                    }),
            )
        })
}

fn tab_header(
    id: &'static str,
    label: &'static str,
    count: usize,
    active: bool,
    tab: ReviewTab,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .gap(px(6.))
        .pb(px(10.))
        .border_b_2()
        .border_color(if active { rgb(theme::ACCENT) } else { rgba(0) })
        .text_color(rgb(if active {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_MUTED
        }))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
        .child(label)
        .child(
            div()
                .font_family(theme::MONO_FONT_FAMILY)
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(count.to_string()),
        )
        .on_click(cx.listener(move |app, _, _, cx| app.set_review_tab(tab, cx)))
}

fn comments_tab(
    comments: Vec<(String, usize, Comment)>,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div()
        .id("review-comments-scroll")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .when(comments.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(12.5))
                    .line_height(px(19.))
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child(
                        "Select text in the thread or click ◆ on a diff line to leave a comment.",
                    ),
            )
        })
        .children(comments.into_iter().map(|(key, idx, comment)| {
            let target = comment.label.lines().next().unwrap_or_default().to_string();
            let (bar, bg) = if comment.pending {
                (theme::USER, theme::USER_CARD_BG)
            } else {
                (theme::NEUTRAL_BAR, theme::STRIP_BG)
            };
            bar_card(bar, bg)
                .px(px(12.))
                .py(px(8.))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .font_family(theme::MONO_FONT_FAMILY)
                        .text_size(px(11.))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(rgb(if comment.pending {
                                    theme::TEXT_SECONDARY
                                } else {
                                    theme::TEXT_MUTED
                                }))
                                .child(target),
                        )
                        .when(comment.pending, |d| {
                            d.child(div().text_color(rgb(theme::USER)).child("pending"))
                                .child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "review-remove-{key}-{idx}"
                                        )))
                                        .cursor_pointer()
                                        .text_color(rgb(theme::TEXT_FAINT))
                                        .hover(|style| style.text_color(rgb(theme::DIFF_DEL_TEXT)))
                                        .child("×")
                                        .on_click(cx.listener(move |app, _, _, cx| {
                                            app.remove_pending_comment(&key, idx, cx)
                                        })),
                                )
                        })
                        .when(!comment.pending, |d| {
                            d.child(div().text_color(rgb(theme::TEXT_FAINT)).child("✓ sent"))
                        }),
                )
                .child(
                    div()
                        .pt(px(4.))
                        .whitespace_normal()
                        .text_size(px(13.))
                        .line_height(px(19.5))
                        .text_color(rgb(if comment.pending {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .child(comment.text),
                )
        }))
}

fn changes_tab(
    files: &[crate::app::WorkspaceFile],
    active_diff: Option<&str>,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div()
        .id("changed-files-scroll")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .py(px(8.))
        .font_family(theme::MONO_FONT_FAMILY)
        .when(files.is_empty(), |d| {
            d.child(
                div()
                    .px(px(16.))
                    .py(px(6.))
                    .text_size(px(12.))
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child("Working tree clean"),
            )
        })
        .children(files.iter().map(|f| {
            let path = f.path.clone();
            let supports_text_diff = f.supports_text_diff;
            let is_active = active_diff == Some(f.path.as_str());
            let (sign, color) = match f.status.as_deref() {
                Some("added") => ("+", theme::DIFF_ADD_TEXT),
                Some("deleted") => ("-", theme::DIFF_DEL_TEXT),
                _ => ("~", theme::TEXT_MUTED),
            };
            let (dir, name) = match f.path.rsplit_once('/') {
                Some((dir, name)) => (format!("{dir}/"), name.to_string()),
                None => (String::new(), f.path.clone()),
            };
            let click_path = path.clone();
            let attach_path = path.clone();
            div()
                .id(SharedString::from(format!("changed-file-{path}")))
                .flex()
                .items_center()
                .w_full()
                .overflow_hidden()
                .gap(px(10.))
                .px(px(14.))
                .py(px(5.))
                .border_l_2()
                .border_color(rgba(0))
                .text_size(px(12.))
                .when(is_active, |d| {
                    d.bg(rgb(theme::SELECTION)).border_color(rgb(theme::ACCENT))
                })
                .when(supports_text_diff, |d| {
                    d.cursor_pointer().hover(|s| s.bg(rgb(theme::HOVER_BG)))
                })
                .child(
                    div()
                        .w(px(10.))
                        .flex_shrink_0()
                        .text_color(rgb(color))
                        .child(sign),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(if supports_text_diff {
                                    theme::TEXT_PRIMARY
                                } else {
                                    theme::TEXT_MUTED
                                }))
                                .child(name),
                        )
                        .child(
                            div()
                                .ml(px(8.))
                                .min_w(px(0.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(rgb(theme::TEXT_FAINT))
                                .child(dir),
                        ),
                )
                .when(!supports_text_diff, |d| {
                    d.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(rgb(theme::TEXT_FAINT))
                            .child("binary"),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("attach-file-{path}")))
                        .flex_shrink_0()
                        .px(px(5.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .bg(rgb(theme::DIVIDER))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::ACCENT)))
                        .child("@ add")
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
}

// ---------------------------------------------------------------- checkpoints

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
                .w(px(320.))
                .bg(rgb(theme::PANEL_BG))
                .border_l_1()
                .border_color(rgb(theme::BORDER))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .px(px(16.))
                        .h(px(40.))
                        .border_b_1()
                        .border_color(rgb(theme::BORDER))
                        .text_size(px(12.5))
                        .child("Checkpoints")
                        .child(
                            div()
                                .ml(px(6.))
                                .font_family(theme::MONO_FONT_FAMILY)
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(checkpoints.len().to_string()),
                        )
                        .child(
                            div()
                                .id("close-checkpoints")
                                .ml_auto()
                                .cursor_pointer()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                                .child("×")
                                .on_click(cx.listener(|app, _, _, cx| app.toggle_checkpoints(cx))),
                        ),
                )
                .child(
                    div()
                        .id("checkpoints-scroll")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .p(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .when(checkpoints.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(rgb(theme::TEXT_FAINT))
                                    .child(
                                        "No checkpoints yet. One is saved after each agent turn.",
                                    ),
                            )
                        })
                        .children(checkpoints.into_iter().enumerate().map(
                            |(index, (label, time))| {
                                let restore_label = label.clone();
                                bar_card(
                                    if index == 0 {
                                        theme::ACCENT
                                    } else {
                                        theme::NEUTRAL_BAR
                                    },
                                    theme::CARD_BG,
                                )
                                .px(px(12.))
                                .py(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(px(0.))
                                        .flex()
                                        .flex_col()
                                        .gap(px(2.))
                                        .child(
                                            div()
                                                .text_size(px(13.))
                                                .text_color(rgb(theme::TEXT_PRIMARY))
                                                .child(label),
                                        )
                                        .child(
                                            div()
                                                .font_family(theme::MONO_FONT_FAMILY)
                                                .text_size(px(11.))
                                                .text_color(rgb(theme::TEXT_FAINT))
                                                .child(time),
                                        ),
                                )
                                .child(button(
                                    SharedString::from(format!("restore-{restore_label}")),
                                    "Restore",
                                    ButtonKind::Secondary,
                                    cx,
                                    move |app, cx| app.restore_checkpoint(&restore_label, cx),
                                ))
                            },
                        )),
                ),
        )
}
