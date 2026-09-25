use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb, rgba};

use crate::app::App;
use crate::theme;

use super::messages::comment_preview;

// ---------------------------------------------------------------- composer

pub(super) fn composer_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let pending = app.pending_comments();
    let has_chips = !app.context_chips.is_empty();
    let chips = app.context_chips.clone();
    let composer = app.composer.clone();

    div()
        .debug_selector(|| "composer".into())
        .w_full()
        .min_w(px(0.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .when(has_chips, |d| {
            d.child(div().flex().gap(px(6.)).flex_wrap().mb(px(4.)).children(
                chips.into_iter().enumerate().map(|(idx, path)| {
                    div()
                        .max_w_full()
                        .min_w(px(0.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .px(px(6.))
                        .py(px(2.))
                        .bg(rgb(theme::INPUT_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER))
                        .rounded(px(5.))
                        .text_color(rgb(theme::TEXT_SECONDARY))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .whitespace_normal()
                                .line_height(px(16.))
                                .child(path),
                        )
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
                }),
            ))
        })
        .when(!pending.is_empty(), |d| {
            d.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .mb(px(4.))
                    .children(pending.into_iter().map(|p| {
                        let key = p.key.clone();
                        let idx = p.idx;
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .px(px(6.))
                            .py(px(2.))
                            .bg(rgba(theme::PENDING_CHIP_BG))
                            .border_1()
                            .border_color(rgba(theme::PENDING_CHIP_BORDER))
                            .rounded(px(5.))
                            .text_color(rgb(theme::ACCENT))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .whitespace_normal()
                                    .line_height(px(16.))
                                    .child(format!("{}: {}", p.label, comment_preview(&p.text))),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-pending-{key}-{idx}")))
                                    .flex_shrink_0()
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
