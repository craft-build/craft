use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb};

use crate::app::App;
use crate::theme;

use super::widgets::{ButtonKind, bar_card, button};

pub(super) fn permission_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let (title, choices) = app
        .pending_permission
        .as_ref()
        .map(|permission| {
            (
                permission.title.clone(),
                permission
                    .options
                    .iter()
                    .map(|option| option.name.clone())
                    .collect::<Vec<_>>()
                    .join(" · "),
            )
        })
        .unwrap_or_default();
    prompt_card("?", "permission", title)
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(11.5))
                .line_height(px(18.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(choices),
        )
        .child(
            div()
                .flex()
                .gap(px(8.))
                .pt(px(2.))
                .child(action_button(
                    "permission-allow",
                    "Allow once",
                    ButtonKind::Primary(theme::ACCENT),
                    cx,
                    |app, cx| app.decide_permission(true, cx),
                ))
                .child(action_button(
                    "permission-reject",
                    "Reject",
                    ButtonKind::Secondary,
                    cx,
                    |app, cx| app.decide_permission(false, cx),
                )),
        )
}

/// Attention card for anything the agent is blocked on: a
/// blue bar (it needs *you*), a glyph and a one-line title.
pub(super) fn prompt_card(glyph: &'static str, kind: &'static str, title: String) -> gpui::Div {
    bar_card(theme::USER, theme::USER_CARD_BG)
        .px(px(14.))
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .flex()
                .items_start()
                .gap(px(10.))
                .text_size(px(12.))
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::USER))
                        .child(glyph),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(FontWeight::BOLD)
                        .text_color(rgb(theme::USER))
                        .child(kind),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .whitespace_normal()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(title),
                ),
        )
}

pub(super) fn action_button(
    id: &'static str,
    label: &'static str,
    kind: ButtonKind,
    cx: &mut Context<App>,
    action: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> impl IntoElement {
    button(SharedString::from(id), label, kind, cx, action)
}
