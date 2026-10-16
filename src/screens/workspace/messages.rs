use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb};

use crate::app::{App, CommentDraft};
use crate::markdown::markdown_view;
use crate::selectable_text::CommentTarget;
use crate::state::{Comment, Message, MessagePart, Role, Steps, Terminal};
use crate::theme;

use super::tool_call::{diff_view, tool_call_view};
use super::widgets::{ButtonKind, SPINNER, badge, bar_card, button, ghost};

fn running_indicator(cx: &mut Context<App>) -> impl IntoElement {
    bar_card(theme::ACCENT, theme::CARD_BG)
        .debug_selector(|| "running-indicator".into())
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(7.))
        .text_size(px(12.))
        .child(div().text_color(rgb(theme::ACCENT)).child(SPINNER))
        .child(
            div()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child("working"),
        )
        .child(div().w(px(7.)).h(px(14.)).bg(rgb(theme::ACCENT)))
        .child(
            div()
                .ml_auto()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(div().text_color(rgb(theme::ACCENT)).child("running"))
                .child(
                    ghost("cancel-turn", "■ stop")
                        .debug_selector(|| "cancel-turn".into())
                        .px(px(8.))
                        .py(px(1.))
                        .bg(rgb(theme::DIVIDER))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .hover(|style| style.text_color(rgb(theme::DIFF_DEL_TEXT)))
                        .on_click(cx.listener(|app, _, _, cx| app.cancel_turn(cx))),
                ),
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
    let comments = app.comments.get(&comment_key).cloned().unwrap_or_default();
    let draft = app.comment_drafts.get(&comment_key).cloned();
    bar_card(theme::USER, theme::USER_CARD_BG)
        .px(px(14.))
        .py(px(8.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::USER))
                        .child("you"),
                )
                .when_some(m.time.clone(), |d, time| {
                    d.child(
                        div()
                            .ml_auto()
                            .text_color(rgb(theme::TEXT_FAINT))
                            .child(time),
                    )
                }),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .text_size(px(13.))
                .line_height(px(21.))
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(markdown_view(
                    &m.body.text(),
                    markdown_id,
                    CommentTarget {
                        key: comment_key.clone(),
                        label: "user message".to_string(),
                        scroll_handle: app.thread_scroll.clone(),
                        focus_handle: app.selection_focus.clone(),
                    },
                )),
        )
        .when(!m.context.is_empty(), |d| {
            d.child(
                div()
                    .pt(px(2.))
                    .flex()
                    .gap(px(6.))
                    .flex_wrap()
                    .children(m.context.iter().map(|c| chip_view(c.clone()))),
            )
        })
        .when(!m.attached_comments.is_empty(), |d| {
            d.child(div().pt(px(2.)).flex().flex_col().gap(px(2.)).children(
                m.attached_comments.iter().map(|(label, text)| {
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .flex()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .line_height(px(17.))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(theme::USER))
                                .child("◆"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .whitespace_normal()
                                .text_color(rgb(theme::TEXT_MUTED))
                                .child(format!(
                                    "{}: {}",
                                    label.lines().next().unwrap_or_default(),
                                    comment_preview(text)
                                )),
                        )
                }),
            ))
        })
        .when(!comments.is_empty() || draft.is_some(), |d| {
            d.child(div().pt(px(4.)).child(comment_thread(&comments).when_some(
                draft,
                |t, draft| {
                    t.child(div().px(px(14.)).py(px(8.)).child(comment_box(
                        draft,
                        comment_key.clone(),
                        cx,
                    )))
                },
            )))
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
        .py(px(1.))
        .bg(rgb(theme::DIVIDER))
        .text_color(rgb(theme::TEXT_SECONDARY))
        .child(format!("@{text}"))
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
    let agent_name = app
        .active_agent_profile_id()
        .and_then(|id| app.agent_profiles.iter().find(|profile| profile.id == id))
        .map(|profile| profile.name.to_lowercase())
        .unwrap_or_else(|| "agent".into());

    // Agent turns are flat: prose sits on the thread background and tools
    // carry their own cards. The vertical padding keeps a turn's footer
    // clear of the next block.
    let mut col = div()
        .debug_selector(|| format!("assistant-card-{msg_id}"))
        .w_full()
        .min_w(px(0.))
        .py(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .text_size(px(12.))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::ACCENT))
                        .child(agent_name),
                )
                .child(
                    ghost(
                        SharedString::from(format!("comment-toggle-{msg_id}")),
                        "◆ comment",
                    )
                    .ml_auto()
                    .text_size(px(11.))
                    .text_color(rgb(theme::TEXT_FAINT))
                    .hover(|style| style.text_color(rgb(theme::USER)))
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
                col = col.child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .pl(px(3.))
                        .font_family(theme::FONT_FAMILY)
                        .text_size(px(14.))
                        .line_height(px(23.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(markdown_view(
                            text,
                            markdown_id,
                            CommentTarget {
                                key: comment_key.clone(),
                                label: "assistant reply".to_string(),
                                scroll_handle: app.thread_scroll.clone(),
                                focus_handle: app.selection_focus.clone(),
                            },
                        )),
                );
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
                .gap(px(8.))
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_FAINT))
                .border_t_1()
                .border_color(rgb(theme::BORDER))
                .pt(px(7.))
                .pb(px(2.))
                .child(div().text_color(rgb(theme::NEUTRAL_BAR)).child("◇"))
                .child(cp.clone()),
        );
    }
    let comments = app.comments.get(&comment_key).cloned().unwrap_or_default();
    let draft = app.comment_drafts.get(&comment_key).cloned();
    if !comments.is_empty() || draft.is_some() {
        let mut thread = comment_thread(&comments);
        if let Some(draft) = draft {
            thread = thread.child(div().px(px(14.)).py(px(8.)).child(comment_box(
                draft,
                comment_key.clone(),
                cx,
            )));
        }
        col = col.child(thread);
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
    bar_card(theme::NEUTRAL_BAR, theme::STRIP_BG)
        .px(px(14.))
        .py(px(6.))
        .text_size(px(11.5))
        .child(
            div()
                .id(SharedString::from(format!("steps-{msg_id}")))
                .flex()
                .items_center()
                .gap(px(8.))
                .cursor_pointer()
                .text_color(rgb(theme::TEXT_MUTED))
                .hover(|style| style.text_color(rgb(theme::TEXT_PRIMARY)))
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
                    .mt(px(4.))
                    .pl(px(18.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .children(items.into_iter().map(|item| {
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .gap(px(8.))
                            .line_height(px(19.))
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child(div().text_color(rgb(theme::ACCENT)).child("[✓]"))
                            .child(div().flex_1().min_w(px(0.)).whitespace_normal().child(item))
                    })),
            )
        })
}
fn terminal_view(term: &Terminal, message_id: &str) -> impl IntoElement {
    bar_card(theme::ACCENT, theme::CARD_BG)
        .id(SharedString::from(format!("terminal-scroll-{message_id}")))
        .overflow_x_scroll()
        .text_size(px(12.))
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .gap(px(10.))
                .px(px(14.))
                .py(px(7.))
                .whitespace_nowrap()
                .child(div().text_color(rgb(theme::ACCENT)).child("$"))
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(term.cmd.clone()),
                ),
        )
        .child(
            div()
                .pl(px(36.))
                .pr(px(14.))
                .pb(px(8.))
                .text_size(px(11.5))
                .line_height(px(19.))
                .whitespace_nowrap()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(term.output.clone()),
        )
}

/// A blue-barred thread of review comments: who wrote each one, where it
/// points, and whether it is still waiting to be sent.
pub(super) fn comment_thread(list: &[Comment]) -> gpui::Div {
    bar_card(theme::USER, theme::USER_CARD_BG)
        .flex()
        .flex_col()
        .children(list.iter().map(|c| {
            let by_user = c.author == "you";
            let target = c.label.lines().next().unwrap_or_default().to_string();
            div()
                .px(px(14.))
                .pt(px(8.))
                .pb(px(10.))
                .when(!by_user, |d| {
                    d.bg(rgb(theme::CARD_BG))
                        .border_l_3()
                        .border_color(rgb(theme::ACCENT))
                        .ml(px(-3.))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .child(
                            div()
                                .font_family(theme::MONO_FONT_FAMILY)
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(if by_user { theme::USER } else { theme::ACCENT }))
                                .child(c.author.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .font_family(theme::MONO_FONT_FAMILY)
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_FAINT))
                                .child(format!("on {target}")),
                        )
                        .child(if c.pending {
                            badge("PENDING", theme::USER).into_any_element()
                        } else {
                            div()
                                .flex_shrink_0()
                                .font_family(theme::MONO_FONT_FAMILY)
                                .text_size(px(11.))
                                .text_color(rgb(theme::TEXT_FAINT))
                                .child("✓ sent")
                                .into_any_element()
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .min_w(px(0.))
                        .pt(px(4.))
                        .whitespace_normal()
                        .font_family(theme::FONT_FAMILY)
                        .text_size(px(13.5))
                        .line_height(px(21.))
                        .text_color(rgb(if c.pending {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_SECONDARY
                        }))
                        .child(c.text.clone()),
                )
        }))
}

pub(super) fn comment_box(
    draft: CommentDraft,
    key: String,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let reference = draft.reference_label();
    let input = draft.input;
    let cancel_key = key.clone();
    div()
        .id(SharedString::from(format!("comment-draft-{key}")))
        .anchor_scroll(draft.scroll_anchor)
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .whitespace_normal()
                .font_family(theme::MONO_FONT_FAMILY)
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_FAINT))
                .child(comment_preview(&reference)),
        )
        .child(
            div()
                .w_full()
                .flex()
                .items_start()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .bg(rgb(theme::INPUT_BG))
                        .font_family(theme::FONT_FAMILY)
                        .text_size(px(12.5))
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .px(px(10.))
                        .py(px(6.))
                        .child(input.clone()),
                )
                .child(button(
                    SharedString::from(format!("add-comment-{key}")),
                    "Comment",
                    ButtonKind::Primary(theme::USER),
                    cx,
                    move |app, cx| {
                        // This listener is already updating App. Calling
                        // TextInput::submit here would recursively update App.
                        let text = input.update(cx, |input, _| input.take_content());
                        app.submit_comment(key.clone(), text, cx);
                    },
                ))
                .child(button(
                    SharedString::from(format!("cancel-comment-{cancel_key}")),
                    "Cancel",
                    ButtonKind::Secondary,
                    cx,
                    move |app, cx| app.discard_comment_draft(&cancel_key, cx),
                )),
        )
}
