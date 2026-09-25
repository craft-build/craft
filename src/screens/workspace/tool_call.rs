use agent_client_protocol::schema::v1::{ContentBlock, ToolCall, ToolCallContent, ToolCallStatus};
use gpui::prelude::*;
use gpui::{Context, SharedString, div, px, rgb, rgba};

use std::cell::RefCell;

use crate::app::{App, render_acp_diff};
use crate::selectable_text::{CommentTarget, SelectableText};
use crate::state::{Diff, DiffLine, DiffLineKind};
use crate::theme;

use super::messages::comment_box;

// Rendered ACP diffs are memoized by (tool_call_id, content index); an entry
// is recomputed only when the underlying old/new text changes.
type AcpDiffCache = std::collections::HashMap<(String, usize), (Option<String>, String, Diff)>;

thread_local! {
    static ACP_DIFF_CACHE: RefCell<(String, AcpDiffCache)> =
        RefCell::new((String::new(), std::collections::HashMap::new()));
}

/// Runs `f` with the rendered diff for `diff`, reusing the previous render
/// when the old/new text is unchanged. Rendering happens on the main thread
/// only, so a thread-local cache is sufficient.
fn with_rendered_acp_diff<R>(
    scope: String,
    tool_call_id: impl std::fmt::Display,
    index: usize,
    diff: &agent_client_protocol::schema::v1::Diff,
    f: impl FnOnce(&Diff) -> R,
) -> R {
    ACP_DIFF_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.0 != scope {
            cache.0 = scope;
            cache.1.clear();
        }
        let entry = cache
            .1
            .entry((format!("{tool_call_id}"), index))
            .or_insert_with(|| {
                (
                    diff.old_text.clone(),
                    diff.new_text.clone(),
                    render_acp_diff(diff.clone()),
                )
            });
        if entry.0 != diff.old_text || entry.1 != diff.new_text {
            *entry = (
                diff.old_text.clone(),
                diff.new_text.clone(),
                render_acp_diff(diff.clone()),
            );
        }
        f(&entry.2)
    })
}

pub(super) fn tool_call_view(
    call: &ToolCall,
    message_id: &str,
    app: &App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let id = format!("tool-call-{message_id}-{}", call.tool_call_id);
    let expanded = app.expanded_tool_calls.contains(&id);
    let toggle_id = id.clone();
    let (status, color) = match call.status {
        ToolCallStatus::Pending => ("Pending", theme::TEXT_MUTED),
        ToolCallStatus::InProgress => ("Running", theme::ACCENT),
        ToolCallStatus::Completed => ("Completed", theme::TEXT_MUTED),
        ToolCallStatus::Failed => ("Failed", theme::DIFF_DEL_TEXT),
        _ => ("", theme::TEXT_MUTED),
    };
    let mut card = div()
        .id(SharedString::from(id.clone()))
        .debug_selector(|| id.clone())
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .bg(rgb(theme::TERMINAL_BG))
        .border_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .rounded(px(6.))
        .px(px(10.))
        .py(px(8.))
        .child(
            div()
                .id(SharedString::from(format!("{id}-header")))
                .flex()
                .items_start()
                .gap(px(8.))
                .text_size(px(12.))
                .line_height(px(18.))
                .cursor_pointer()
                .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
                .on_click(cx.listener(move |app, _, _, cx| app.toggle_tool_call(&toggle_id, cx)))
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(if expanded { "▾" } else { "▸" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(call.title.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(rgb(color))
                        .child(status),
                ),
        );
    if !expanded {
        return card.into_any_element();
    }
    for (index, content) in call.content.iter().enumerate() {
        let content_id = format!("{id}-content-{index}");
        match content {
            ToolCallContent::Diff(diff) => {
                // Memoized render: recompute only when the ACP content changes,
                // not on every frame.
                let key_prefix = content_id.clone();
                let scope = format!(
                    "{}:{}",
                    app.active_project
                        .as_ref()
                        .map(|p| p.id.as_str())
                        .unwrap_or("untitled"),
                    app.active_session_id.as_deref().unwrap_or("<none>")
                );
                card = card.child(with_rendered_acp_diff(
                    scope,
                    &call.tool_call_id,
                    index,
                    diff,
                    |rendered| {
                        let file = rendered.file.clone();
                        diff_view(
                            rendered,
                            content_id,
                            move |line| {
                                (
                                    format!("{key_prefix}_{line}"),
                                    format!("{file} line {}", line + 1),
                                )
                            },
                            app,
                            cx,
                        )
                        .into_any_element()
                    },
                ));
            }
            _ => {
                let text = match content {
                    ToolCallContent::Content(content) => match &content.content {
                        ContentBlock::Text(text) => text.text.clone(),
                        _ => continue,
                    },
                    ToolCallContent::Terminal(_) => {
                        "Interactive terminal is managed by the connected agent".into()
                    }
                    _ => continue,
                };
                card = card.child(
                    div()
                        .id(SharedString::from(content_id.clone()))
                        .debug_selector(|| content_id)
                        .w_full()
                        .min_w(px(0.))
                        .overflow_x_scroll()
                        .font_family(theme::MONO_FONT_FAMILY)
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .whitespace_nowrap()
                        .text_color(rgb(if call.status == ToolCallStatus::Failed {
                            theme::DIFF_DEL_TEXT
                        } else {
                            theme::DIFF_ADD_TEXT
                        }))
                        .child(text),
                );
            }
        }
    }
    card.into_any_element()
}
// ---------------------------------------------------------------- diff rendering (shared)

pub(super) fn diff_view(
    diff: &Diff,
    scroll_id: String,
    key_fn: impl Fn(usize) -> (String, String) + 'static,
    app: &App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    div()
        .id(SharedString::from(scroll_id))
        .w_full()
        .font_family(theme::MONO_FONT_FAMILY)
        .min_w(px(0.))
        .overflow_x_scroll()
        .border_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(10.))
                .py(px(6.))
                .bg(rgb(theme::INPUT_BG))
                .border_b_1()
                .border_color(rgb(theme::TERMINAL_BORDER))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(px(12.))
                        .child(diff.file.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(diff.stat.clone()),
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
            let (key, label) = key_fn(idx);
            diff_line_view(line, key, label, app.thread_scroll.clone(), app, cx)
        }))
}

pub(super) fn diff_line_view(
    line: &DiffLine,
    key: String,
    label: String,
    scroll_handle: gpui::ScrollHandle,
    app: &App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let (gutter, gutter_color, bg): (&str, u32, Option<u32>) = match line.kind {
        DiffLineKind::Add => ("+", theme::DIFF_ADD_TEXT, Some(theme::DIFF_ADD_BG)),
        DiffLineKind::Del => ("-", theme::DIFF_DEL_TEXT, Some(theme::DIFF_DEL_BG)),
        DiffLineKind::Ctx => (" ", theme::TEXT_MUTED, None),
    };

    let key = app.comment_key(&key);
    let comments = app.comments.get(&key).cloned().unwrap_or_default();
    let toggle_key = key.clone();
    let toggle_label = label.clone();

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
                .min_w(px(0.))
                .text_size(px(12.))
                .whitespace_nowrap()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .debug_selector(|| format!("diff-text-{key}"))
                .child(
                    SelectableText::new(
                        format!("diff-text-{key}"),
                        gpui::StyledText::new(line.text.clone()),
                        line.text.clone(),
                    )
                    .comment_target(CommentTarget {
                        key: key.clone(),
                        label: label.clone(),
                        scroll_handle,
                        focus_handle: app.selection_focus.clone(),
                    }),
                ),
        )
        .child(
            div()
                .id(SharedString::from(format!("edit-{key}")))
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .cursor_pointer()
                .child("✎")
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.open_comment_box(toggle_key.clone(), toggle_label.clone(), cx)
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
                        .w_full()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .text_size(px(11.))
                        .line_height(px(16.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(format!("{}: {}", c.author, c.text))
                })),
        );
    }
    if let Some(draft) = app.comment_drafts.get(&key).cloned() {
        let submit_key = key.clone();
        wrapper = wrapper.child(
            div()
                .bg(rgb(theme::INPUT_BG))
                .pl(px(26.))
                .pr(px(10.))
                .py(px(4.))
                .child(comment_box(draft, submit_key, cx)),
        );
    }
    wrapper.into_any_element()
}
