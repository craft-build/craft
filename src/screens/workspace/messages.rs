use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb};

use crate::app::{App, CommentDraft};
use crate::markdown::markdown_view;
use crate::selectable_text::CommentTarget;
use crate::state::{Comment, Message, MessagePart, Role, Steps, Terminal};
use crate::theme;

use super::tool_call::{diff_view, tool_call_view};

fn running_indicator(cx: &mut Context<App>) -> impl IntoElement {
    div()
        .debug_selector(|| "running-indicator".into())
        .flex()
        .items_center()
        .justify_between()
        .border_t_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .pt(px(8.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::ACCENT)))
                .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::ACCENT)))
                .child(div().w(px(5.)).h(px(5.)).bg(rgb(theme::ACCENT)))
                .child(
                    div()
                        .ml(px(4.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Running"),
                ),
        )
        .child(
            div()
                .id("cancel-turn")
                .debug_selector(|| "cancel-turn".into())
                .px(px(8.))
                .py(px(3.))
                .border_1()
                .border_color(rgb(theme::BORDER))
                .text_size(px(11.))
                .text_color(rgb(theme::ACCENT))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                .child("Stop")
                .on_click(cx.listener(|app, _, _, cx| app.cancel_turn(cx))),
        )
}

pub(super) fn message_view(
    m: &Message,
    is_streaming: bool,
    app: &App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    match m.role {
        Role::User => user_message(m, app, cx).into_any_element(),
        Role::Assistant => assistant_message(m, is_streaming, app, cx).into_any_element(),
    }
}

const COMMENT_PREVIEW_CHAR_LIMIT: usize = 120;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_comment_previews_are_unchanged() {
        assert_eq!(comment_preview("Keep this branch"), "Keep this branch");
        assert_eq!(
            comment_preview(&"a".repeat(COMMENT_PREVIEW_CHAR_LIMIT)),
            "a".repeat(COMMENT_PREVIEW_CHAR_LIMIT)
        );
    }

    #[test]
    fn long_comment_previews_are_capped_without_splitting_unicode() {
        let preview = comment_preview(&"é".repeat(COMMENT_PREVIEW_CHAR_LIMIT + 1));

        assert_eq!(preview.chars().count(), COMMENT_PREVIEW_CHAR_LIMIT);
        assert_eq!(
            preview,
            format!("{}…", "é".repeat(COMMENT_PREVIEW_CHAR_LIMIT - 1))
        );
    }
}

pub(super) fn comment_preview(text: &str) -> String {
    let mut chars = text.chars();
    let mut preview = chars
        .by_ref()
        .take(COMMENT_PREVIEW_CHAR_LIMIT)
        .collect::<String>();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    preview
}

fn user_message(m: &Message, app: &App, cx: &mut Context<App>) -> impl IntoElement {
    let markdown_id = format!("user-markdown-{}", m.id);
    let comment_key = app.comment_key(&format!("msg_{}", m.id));
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::ACCENT))
                .child("you"),
        )
        .child(markdown_view(
            &m.body.text(),
            markdown_id,
            CommentTarget {
                key: comment_key.clone(),
                label: "user message".to_string(),
                scroll_handle: app.thread_scroll.clone(),
                focus_handle: app.selection_focus.clone(),
            },
        ))
        .when_some(app.comments.get(&comment_key), |d, comments| {
            d.child(comments_list(comments))
        })
        .when_some(app.comment_drafts.get(&comment_key).cloned(), |d, draft| {
            d.child(comment_box(draft, comment_key, cx))
        })
        .when(!m.context.is_empty(), |d| {
            d.child(
                div()
                    .flex()
                    .gap(px(6.))
                    .flex_wrap()
                    .children(m.context.iter().map(|c| chip_view(c.clone()))),
            )
        })
        .when(!m.attached_comments.is_empty(), |d| {
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
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_normal()
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(rgb(theme::TEXT_SECONDARY))
                            .child(format!("{label}: {}", comment_preview(text)))
                    })),
            )
        })
}

fn chip_view(text: String) -> impl IntoElement {
    div()
        .max_w_full()
        .min_w(px(0.))
        .whitespace_normal()
        .text_size(px(11.))
        .line_height(px(16.))
        .px(px(6.))
        .py(px(2.))
        .bg(rgb(theme::INPUT_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .rounded(px(5.))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .child(text)
}

fn assistant_message(
    m: &Message,
    is_streaming: bool,
    app: &App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let msg_id = m.id.clone();
    let comment_key = app.comment_key(&format!("msg_{}", m.id));
    let steps_expanded = app.expanded_steps.contains(&msg_id);
    let toggle_steps_id = msg_id.clone();
    let toggle_comment_key = comment_key.clone();

    let mut col = div()
        .debug_selector(|| format!("assistant-card-{msg_id}"))
        .w_full()
        .min_w(px(0.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::PANEL_BG))
        .rounded(px(8.))
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
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
                        .px(px(4.))
                        .py(px(2.))
                        .text_size(px(11.))
                        .text_color(rgb(theme::TEXT_MUTED))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(theme::TEXT_SECONDARY)))
                        .child("✎")
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
            format!("inline-diff-{msg_id}"),
            move |idx| {
                (
                    format!("{key_prefix}_{idx}"),
                    format!("{file} line {}", idx + 1),
                )
            },
            app,
            cx,
        ));
    }
    if let Some(term) = &m.terminal {
        col = col.child(terminal_view(term, &msg_id));
    }
    let mut text_index = 0;
    for part in &m.body.parts {
        match part {
            MessagePart::ToolCall(call) => {
                col = col.child(tool_call_view(call, &msg_id, app, cx));
            }
            MessagePart::Text(text) => {
                // Keep the original first-text ID and give each later block its
                // own selection state. Appending content never renumbers blocks.
                let markdown_id = if text_index == 0 {
                    format!("assistant-markdown-{msg_id}")
                } else {
                    format!("assistant-markdown-{msg_id}-text-{text_index}")
                };
                text_index += 1;
                col = col.child(markdown_view(
                    text,
                    markdown_id,
                    CommentTarget {
                        key: comment_key.clone(),
                        label: "assistant reply".to_string(),
                        scroll_handle: app.thread_scroll.clone(),
                        focus_handle: app.selection_focus.clone(),
                    },
                ));
            }
        }
    }
    if is_streaming {
        col = col.child(running_indicator(cx));
    }
    if let Some(cp) = &m.checkpoint_label {
        col = col.child(
            div()
                .debug_selector(|| format!("checkpoint-{msg_id}"))
                .min_h(px(25.))
                .flex()
                .items_end()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_MUTED))
                .border_t_1()
                .border_color(rgb(theme::TERMINAL_BORDER))
                .pt(px(7.))
                .pb(px(2.))
                .child(cp.clone()),
        );
    }
    if let Some(list) = app.comments.get(&comment_key).cloned()
        && !list.is_empty()
    {
        col = col.child(comments_list(&list));
    }
    if let Some(draft) = app.comment_drafts.get(&comment_key).cloned() {
        let submit_key = comment_key.clone();
        col = col.child(comment_box(draft, submit_key, cx));
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
    div()
        .child(
            div()
                .id(SharedString::from(format!("steps-{msg_id}")))
                .flex()
                .items_center()
                .gap(px(6.))
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(if expanded { "▾" } else { "▸" })
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .child(steps.summary.clone()),
                )
                .on_click(cx.listener(move |app, _, _, cx| app.toggle_steps(&msg_id, cx))),
        )
        .when(expanded, |d| {
            d.child(
                div()
                    .mt(px(6.))
                    .pl(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .children(items.into_iter().map(|item| {
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .whitespace_normal()
                            .text_size(px(11.))
                            .line_height(px(16.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(format!("· {item}"))
                    })),
            )
        })
}
fn terminal_view(term: &Terminal, message_id: &str) -> impl IntoElement {
    div()
        .id(SharedString::from(format!("terminal-scroll-{message_id}")))
        .w_full()
        .min_w(px(0.))
        .overflow_x_scroll()
        .bg(rgb(theme::TERMINAL_BG))
        .border_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .rounded(px(6.))
        .font_family(theme::MONO_FONT_FAMILY)
        .px(px(10.))
        .py(px(8.))
        .text_size(px(12.))
        .flex()
        .flex_col()
        .child(
            div()
                .whitespace_nowrap()
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(format!("$ {}", term.cmd)),
        )
        .child(
            div()
                .mt(px(2.))
                .whitespace_nowrap()
                .text_color(rgb(theme::DIFF_ADD_TEXT))
                .child(term.output.clone()),
        )
}

pub(super) fn comments_list(list: &[Comment]) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(3.))
        .border_t_1()
        .border_color(rgb(theme::TERMINAL_BORDER))
        .pt(px(6.))
        .children(list.iter().map(|c| {
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_SECONDARY))
                .child(format!("{}: {}", c.author, c.text))
        }))
}

pub(super) fn comment_box(
    draft: CommentDraft,
    key: String,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let reference = draft.reference_label();
    let input = draft.input;
    div()
        .id(SharedString::from(format!("comment-draft-{key}")))
        .anchor_scroll(draft.scroll_anchor)
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .whitespace_normal()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(comment_preview(&reference)),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
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
                        .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                        .child("Add")
                        .on_click(cx.listener(move |app, _, _, cx| {
                            // This listener is already updating App. Calling
                            // TextInput::submit here would recursively update App.
                            let text = input.update(cx, |input, _| input.take_content());
                            app.submit_comment(key.clone(), text, cx);
                        })),
                ),
        )
}
