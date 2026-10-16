use agent_client_protocol::schema::v1::{ContentBlock, ToolCall, ToolCallContent, ToolCallStatus};
use gpui::prelude::*;
use gpui::{Context, FontWeight, HighlightStyle, SharedString, StyledText, div, px, rgb, rgba};

use std::cell::RefCell;

use crate::app::{App, render_acp_diff};
use crate::selectable_text::{CommentTarget, SelectableText};
use crate::state::{Diff, DiffLine, DiffLineKind};
use crate::theme;

use super::messages::{comment_box, comment_thread};
use super::widgets::{SPINNER, bar_card};

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
    let (glyph, status, color) = match call.status {
        ToolCallStatus::Pending => ("○", "pending", theme::TEXT_MUTED),
        ToolCallStatus::InProgress => (SPINNER, "running", theme::ACCENT),
        ToolCallStatus::Completed => ("✓", "", theme::ACCENT),
        ToolCallStatus::Failed => ("✕", "failed", theme::DIFF_DEL_TEXT),
        _ => ("·", "", theme::TEXT_MUTED),
    };
    let bar = match call.status {
        ToolCallStatus::Pending => theme::NEUTRAL_BAR,
        _ => color,
    };
    let has_content = !call.content.is_empty();
    let mut card = bar_card(bar, theme::CARD_BG)
        .id(SharedString::from(id.clone()))
        .debug_selector(|| id.clone())
        .flex()
        .flex_col()
        .child(
            div()
                .id(SharedString::from(format!("{id}-header")))
                .flex()
                .items_start()
                .gap(px(10.))
                .px(px(14.))
                .py(px(7.))
                .text_size(px(12.))
                .line_height(px(18.))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(theme::HOVER_BG)))
                .on_click(cx.listener(move |app, _, _, cx| app.toggle_tool_call(&toggle_id, cx)))
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(12.))
                        .text_color(rgb(color))
                        .child(glyph),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(command_title(&call.title)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .gap(px(8.))
                        .text_color(rgb(theme::TEXT_FAINT))
                        .when(!status.is_empty(), |d| {
                            d.child(div().text_color(rgb(color)).child(status))
                        })
                        .when(has_content, |d| {
                            d.child(if expanded { "▾" } else { "▸" })
                        }),
                ),
        );
    if !expanded || !has_content {
        return card.into_any_element();
    }
    let mut body = div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .pl(px(36.))
        .pr(px(14.))
        .pb(px(10.))
        .pt(px(2.));
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
                body = body.child(with_rendered_acp_diff(
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
                body = body.child(
                    div()
                        .id(SharedString::from(content_id.clone()))
                        .debug_selector(|| content_id)
                        .w_full()
                        .min_w(px(0.))
                        .overflow_x_scroll()
                        .text_size(px(11.5))
                        .line_height(px(19.))
                        .whitespace_nowrap()
                        .text_color(rgb(if call.status == ToolCallStatus::Failed {
                            theme::DIFF_DEL_TEXT
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .child(text),
                );
            }
        }
    }
    card = card.child(body);
    card.into_any_element()
}

/// `craft add --git x` → the command word bold and bright, its arguments
/// left in the surrounding muted color.
fn command_title(title: &str) -> StyledText {
    let head = title.find(char::is_whitespace).unwrap_or(title.len());
    StyledText::new(title.to_string()).with_highlights([(
        0..head,
        HighlightStyle {
            color: Some(rgb(theme::TEXT_PRIMARY).into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        },
    )])
}
// ---------------------------------------------------------------- diff rendering (shared)

/// First old/new line numbers from a unified hunk header like
/// `@@ -12,3 +12,6 @@`; defaults to 1 when the header has none.
pub(super) fn hunk_starts(header: &str) -> (usize, usize) {
    let start = |sign: char| {
        header
            .split_whitespace()
            .find_map(|part| part.strip_prefix(sign))
            .and_then(|range| range.split(',').next())
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n.max(1))
            .unwrap_or(1)
    };
    (start('-'), start('+'))
}

/// Display line number for every line of a hunk: removed lines count in the
/// old file, everything else in the new one.
pub(super) fn line_numbers(diff: &Diff) -> Vec<usize> {
    let (mut old, mut new) = hunk_starts(&diff.hunk_header);
    diff.lines
        .iter()
        .map(|line| match line.kind {
            DiffLineKind::Del => {
                old += 1;
                old - 1
            }
            DiffLineKind::Add => {
                new += 1;
                new - 1
            }
            DiffLineKind::Ctx => {
                old += 1;
                new += 1;
                new - 1
            }
        })
        .collect()
}

fn diff_counts(diff: &Diff) -> (usize, usize) {
    diff.lines
        .iter()
        .fold((0, 0), |(add, del), line| match line.kind {
            DiffLineKind::Add => (add + 1, del),
            DiffLineKind::Del => (add, del + 1),
            DiffLineKind::Ctx => (add, del),
        })
}

/// File header for a diff block: `~ file · N comments   +a -d`.
pub(super) fn diff_header(diff: &Diff, comments: usize) -> gpui::Div {
    let (adds, dels) = diff_counts(diff);
    div()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(14.))
        .py(px(7.))
        .text_size(px(12.))
        .child(div().text_color(rgb(theme::TEXT_MUTED)).child("~"))
        .child(
            div()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(diff.file.clone()),
        )
        .when(comments > 0, |d| {
            d.child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child(format!(
                        "· {comments} comment{}",
                        if comments == 1 { "" } else { "s" }
                    )),
            )
        })
        .child(
            div()
                .ml_auto()
                .flex_shrink_0()
                .flex()
                .gap(px(8.))
                .child(
                    div()
                        .text_color(rgb(theme::DIFF_ADD_TEXT))
                        .child(format!("+{adds}")),
                )
                .child(
                    div()
                        .text_color(rgb(theme::DIFF_DEL_TEXT))
                        .child(format!("-{dels}")),
                ),
        )
}

pub(super) fn diff_view(
    diff: &Diff,
    scroll_id: String,
    key_fn: impl Fn(usize) -> (String, String) + 'static,
    app: &App,
    cx: &mut Context<App>,
) -> impl IntoElement {
    let keys = (0..diff.lines.len()).map(&key_fn).collect::<Vec<_>>();
    let comment_count = keys
        .iter()
        .map(|(key, _)| app.comments.get(&app.comment_key(key)).map_or(0, Vec::len))
        .sum();
    let numbers = line_numbers(diff);
    bar_card(theme::NEUTRAL_BAR, theme::CARD_BG)
        .id(SharedString::from(scroll_id))
        .font_family(theme::MONO_FONT_FAMILY)
        .overflow_x_scroll()
        .child(diff_header(diff, comment_count))
        .child(
            div()
                .text_size(px(11.))
                .text_color(rgb(theme::TEXT_FAINT))
                .px(px(14.))
                .pb(px(2.))
                .child(diff.hunk_header.clone()),
        )
        .child(
            div()
                .pb(px(8.))
                .children(diff.lines.iter().zip(keys).zip(numbers).map(
                    |((line, (key, label)), number)| {
                        diff_line_view(line, number, key, label, app.thread_scroll.clone(), app, cx)
                    },
                )),
        )
}

pub(super) fn diff_line_view(
    line: &DiffLine,
    number: usize,
    key: String,
    label: String,
    scroll_handle: gpui::ScrollHandle,
    app: &App,
    cx: &mut Context<App>,
) -> gpui::AnyElement {
    let (sign, text_color, number_color, bg): (&str, u32, u32, Option<u32>) = match line.kind {
        DiffLineKind::Add => (
            "+",
            theme::DIFF_ADD_TEXT,
            theme::DIFF_ADD_GUTTER,
            Some(theme::DIFF_ADD_BG),
        ),
        DiffLineKind::Del => (
            "-",
            theme::DIFF_DEL_TEXT,
            theme::DIFF_DEL_GUTTER,
            Some(theme::DIFF_DEL_BG),
        ),
        DiffLineKind::Ctx => (" ", theme::TEXT_SECONDARY, theme::TEXT_FAINT, None),
    };

    let key = app.comment_key(&key);
    let comments = app.comments.get(&key).cloned().unwrap_or_default();
    let draft = app.comment_drafts.get(&key).cloned();
    let marked = !comments.is_empty() || draft.is_some();
    let toggle_key = key.clone();
    let toggle_label = label.clone();
    let group = SharedString::from(format!("diff-line-{key}"));

    let row = div()
        .group(group.clone())
        .flex()
        .items_center()
        .when_some(bg, |d, bg| d.bg(rgba(bg)))
        .when(marked, |d| d.border_l_3().border_color(rgb(theme::USER)))
        .pl(px(if marked { 11. } else { 14. }))
        .pr(px(14.))
        .text_size(px(12.))
        .line_height(px(21.))
        .child(
            div()
                .w(px(32.))
                .flex_shrink_0()
                .text_color(rgb(number_color))
                .child(number.to_string()),
        )
        .child(
            div()
                .w(px(16.))
                .flex_shrink_0()
                .text_color(rgb(text_color))
                .child(sign),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .whitespace_nowrap()
                .text_color(rgb(text_color))
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
                .flex_shrink_0()
                .pl(px(8.))
                .text_size(px(11.))
                .cursor_pointer()
                .when(comments.is_empty(), |d| {
                    d.text_color(rgba(0))
                        .group_hover(group, |style| style.text_color(rgb(theme::TEXT_MUTED)))
                        .child("◆ +")
                })
                .when(!comments.is_empty(), |d| {
                    d.text_color(rgb(theme::USER))
                        .child(format!("◆ {}", comments.len()))
                })
                .hover(|style| style.text_color(rgb(theme::USER)))
                .on_click(cx.listener(move |app, _, _, cx| {
                    app.open_comment_box(toggle_key.clone(), toggle_label.clone(), cx)
                })),
        );

    let mut wrapper = div().child(row);
    if marked {
        let mut thread = comment_thread(&comments);
        if let Some(draft) = draft {
            thread = thread.child(div().px(px(14.)).py(px(8.)).child(comment_box(
                draft,
                key.clone(),
                cx,
            )));
        }
        wrapper = wrapper.child(
            div()
                .pl(px(46.))
                .pr(px(14.))
                .pt(px(6.))
                .pb(px(8.))
                .child(thread),
        );
    }
    wrapper.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: DiffLineKind) -> DiffLine {
        DiffLine {
            kind,
            text: String::new(),
        }
    }

    #[test]
    fn line_numbers_follow_the_hunk_header() {
        let diff = Diff {
            file: "craft.toml".into(),
            stat: String::new(),
            hunk_header: "@@ -12,2 +12,3 @@".into(),
            lines: vec![
                line(DiffLineKind::Ctx),
                line(DiffLineKind::Del),
                line(DiffLineKind::Add),
                line(DiffLineKind::Add),
            ],
        };
        assert_eq!(line_numbers(&diff), [12, 13, 13, 14]);
        assert_eq!(diff_counts(&diff), (2, 1));
    }

    #[test]
    fn hunk_headers_without_ranges_start_at_one() {
        assert_eq!(hunk_starts("@@ new file @@"), (1, 1));
        assert_eq!(hunk_starts("@@ -0,0 +1 @@"), (1, 1));
    }
}
