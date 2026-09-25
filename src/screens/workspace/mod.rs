use gpui::prelude::*;
use gpui::{Context, Window, div, px, rgb};

use crate::app::App;
use crate::state::{Message, Role};
use crate::theme;

mod composer;
mod elicitation;
mod messages;
mod overlays;
mod permission;
mod sidebar;
mod tool_call;
mod top_bar;

use composer::composer_view;
use elicitation::elicitation_view;
use messages::message_view;
use overlays::{checkpoints_overlay, right_panels};
use permission::permission_view;
use sidebar::sidebar;
use top_bar::{footer_bar, top_bar};
pub fn render(app: &mut App, window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .relative()
        .child(top_bar(app, window, cx))
        .child(
            div()
                .flex_1()
                .flex()
                .overflow_x_hidden()
                .overflow_y_hidden()
                .when(app.sidebar_visible, |d| d.child(sidebar(app, cx)))
                .child(main_column(app, window, cx))
                .when(app.file_tree_visible, |d| d.child(right_panels(app, cx))),
        )
        .child(footer_bar(app, cx))
        .when(app.show_checkpoints, |d| {
            d.child(checkpoints_overlay(app, cx))
        })
}
// ---------------------------------------------------------------- main column

fn active_session_ref(app: &App) -> Option<&crate::state::Session> {
    let key = app
        .active_project
        .as_ref()
        .map(|p| p.id.as_str())
        .unwrap_or("untitled");
    app.sessions_by_project.get(key).and_then(|list| {
        list.iter()
            .find(|s| Some(&s.id) == app.active_session_id.as_ref())
            .or_else(|| list.first())
    })
}

fn main_column(app: &mut App, _window: &mut Window, cx: &mut Context<App>) -> impl IntoElement {
    let messages: &[Message] = active_session_ref(app)
        .map(|session| session.messages.as_slice())
        .unwrap_or(&[]);
    let last_index = messages.len().saturating_sub(1);
    let scroll_handle = app.thread_scroll.clone();

    div()
        .flex_1()
        .min_w(px(340.))
        .flex_shrink_0()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(rgb(theme::BG))
        .child(
            div()
                .id("thread-scroll")
                .track_scroll(&scroll_handle)
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(18.))
                .py(px(14.))
                .flex()
                .items_start()
                .child(
                    // Shrink only horizontally: the row resolves the thread's width
                    // before measuring wrapped text. A max-width constraint can leave
                    // GPUI's card heights or scroll extent measured at the wrong width.
                    div()
                        .w(px(760.))
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap(px(16.))
                        .children(messages.iter().enumerate().map(|(index, message)| {
                            let is_streaming = app.thinking
                                && index == last_index
                                && matches!(message.role, Role::Assistant);
                            message_view(message, is_streaming, &*app, cx)
                        }))
                        .when(app.pending_permission.is_some(), |d| {
                            d.child(permission_view(app, cx))
                        })
                        .when(app.pending_elicitation.is_some(), |d| {
                            d.child(elicitation_view(app, cx))
                        })
                        .child(composer_view(app, cx)),
                ),
        )
}
