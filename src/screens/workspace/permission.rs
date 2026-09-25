use gpui::prelude::*;
use gpui::{Context, FontWeight, div, px, rgb};

use crate::app::App;
use crate::theme;

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
    div()
        .w_full()
        .border_1()
        .border_color(rgb(theme::ACCENT))
        .rounded(px(8.))
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("Permission required · {title}")),
        )
        .child(
            div()
                .w_full()
                .min_w(px(0.))
                .whitespace_normal()
                .text_size(px(11.))
                .line_height(px(16.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(choices),
        )
        .child(
            div()
                .flex()
                .gap(px(8.))
                .child(action_button(
                    "permission-reject",
                    "Reject",
                    cx,
                    |app, cx| app.decide_permission(false, cx),
                ))
                .child(action_button(
                    "permission-allow",
                    "Allow once",
                    cx,
                    |app, cx| app.decide_permission(true, cx),
                )),
        )
}
pub(super) fn action_button(
    id: &'static str,
    label: &'static str,
    cx: &mut Context<App>,
    action: impl Fn(&mut App, &mut Context<App>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px(px(10.))
        .py(px(5.))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .text_size(px(11.))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::HOVER_BG)))
        .child(label)
        .on_click(cx.listener(move |app, _, _, cx| action(app, cx)))
}
