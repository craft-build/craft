use gpui::prelude::*;
use gpui::{Context, FontWeight, SharedString, div, px, rgb};

use crate::app::App;
use crate::theme;

use super::messages::comment_preview;
use super::top_bar::{agent_selector, config_selector};
use super::widgets::{bar_card, meter};

// ---------------------------------------------------------------- composer

const METER_CELLS: usize = 10;

pub(super) fn composer_view(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let pending = app.pending_comments();
    let chips = app.context_chips.clone();
    let composer = app.composer.clone();

    div()
        .debug_selector(|| "composer".into())
        .flex_shrink_0()
        .w_full()
        .min_w(px(0.))
        .px(px(32.))
        .pt(px(4.))
        .pb(px(16.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .font_family(theme::MONO_FONT_FAMILY)
        .when(!chips.is_empty(), |d| {
            d.child(
                strip("context", chips.len()).children(chips.into_iter().enumerate().map(
                    |(idx, path)| {
                        strip_item(
                            SharedString::from(format!("remove-chip-{idx}")),
                            div()
                                .text_color(rgb(theme::TEXT_SECONDARY))
                                .child(format!("@{path}")),
                            cx.listener(move |app, _, _, cx| {
                                app.context_chips.remove(idx);
                                cx.notify();
                            }),
                        )
                    },
                )),
            )
        })
        .when(!pending.is_empty(), |d| {
            d.child(
                strip("comments", pending.len()).children(pending.into_iter().map(|p| {
                    let key = p.key.clone();
                    let idx = p.idx;
                    let target = p.label.lines().next().unwrap_or_default().to_string();
                    strip_item(
                        SharedString::from(format!("remove-pending-{key}-{idx}")),
                        div()
                            .flex()
                            .gap(px(6.))
                            .child(div().text_color(rgb(theme::USER)).child("◆"))
                            .child(div().text_color(rgb(theme::TEXT_MUTED)).child(target))
                            .child(
                                div()
                                    .text_color(rgb(theme::TEXT_SECONDARY))
                                    .child(comment_preview(&p.text)),
                            ),
                        cx.listener(move |app, _, _, cx| app.remove_pending_comment(&key, idx, cx)),
                    )
                })),
            )
        })
        .child(
            bar_card(theme::ACCENT, theme::CARD_BG)
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(10.))
                        .px(px(14.))
                        .pt(px(12.))
                        .pb(px(6.))
                        .min_h(px(40.))
                        .text_size(px(13.))
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(theme::ACCENT))
                                .font_weight(FontWeight::BOLD)
                                .child(">"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(composer),
                        ),
                )
                .child(controls_row(app, cx)),
        )
}

/// Agent, session config, context usage and send, under the prompt.
fn controls_row(app: &mut App, cx: &mut Context<App>) -> impl IntoElement {
    let config_controls = app.session_config_controls.clone();
    let open_config_menu = app.open_config_menu.clone();
    let config_search_input = app.config_search_input.clone();
    let config_search_query = app
        .config_search_input
        .read(cx)
        .content
        .trim()
        .to_lowercase();
    let agent_profiles = app.agent_profiles.clone();
    let selected_agent = app.active_agent_profile_id().map(str::to_string);
    let can_select_agent = app.can_select_agent_for_session();
    let agent_menu_open = app.agent_menu_open;
    let connected = app.acp_client.is_some();
    let thinking = app.thinking;
    let context_label = app
        .context_usage
        .map(|percent| format!("{percent}% context"))
        .unwrap_or_else(|| "context —".into());
    let filled = app
        .context_usage
        .map(|percent| (percent as usize * METER_CELLS).div_ceil(100))
        .unwrap_or(0);

    div()
        .flex()
        .items_center()
        .flex_wrap()
        .gap(px(12.))
        .pl(px(14.))
        .pr(px(10.))
        .py(px(8.))
        .text_size(px(11.))
        .child(agent_selector(
            agent_profiles,
            selected_agent,
            can_select_agent,
            agent_menu_open,
            connected,
            cx,
        ))
        .children(config_controls.into_iter().map(|control| {
            let is_open = open_config_menu.as_deref() == Some(control.id.as_str());
            div()
                .flex()
                .items_center()
                .gap(px(12.))
                .child(div().text_color(rgb(theme::TRACK)).child("│"))
                .child(config_selector(
                    control,
                    is_open,
                    config_search_input.clone(),
                    config_search_query.clone(),
                    cx,
                ))
        }))
        .child(
            div()
                .ml_auto()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_color(rgb(theme::TEXT_MUTED))
                .child(meter(filled, METER_CELLS, theme::ACCENT))
                .child(context_label),
        )
        .child(
            div()
                .id("composer-send")
                .w(px(28.))
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .font_weight(FontWeight::BOLD)
                .text_size(px(13.))
                .when(thinking, |d| {
                    d.bg(rgb(theme::DIVIDER)).text_color(rgb(theme::TEXT_FAINT))
                })
                .when(!thinking, |d| {
                    d.bg(rgb(theme::ACCENT))
                        .text_color(rgb(theme::ACCENT_DARK_TEXT))
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.85))
                        .on_click(cx.listener(|app, _, _, cx| app.submit_composer(cx)))
                })
                .child("↵"),
        )
}

fn strip(title: &'static str, count: usize) -> gpui::Div {
    bar_card(theme::NEUTRAL_BAR, theme::STRIP_BG)
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(18.))
        .gap_y(px(2.))
        .px(px(14.))
        .py(px(6.))
        .text_size(px(11.5))
        .line_height(px(20.))
        .child(
            div()
                .flex()
                .gap(px(6.))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(title)
                .child(
                    div()
                        .font_weight(FontWeight::NORMAL)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(count.to_string()),
                ),
        )
}

fn strip_item(
    remove_id: SharedString,
    content: gpui::Div,
    on_remove: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .max_w_full()
        .min_w(px(0.))
        .flex()
        .items_center()
        .gap(px(6.))
        .child(content.min_w(px(0.)).whitespace_normal())
        .child(
            div()
                .id(remove_id)
                .flex_shrink_0()
                .cursor_pointer()
                .text_color(rgb(theme::TEXT_FAINT))
                .hover(|style| style.text_color(rgb(theme::DIFF_DEL_TEXT)))
                .child("×")
                .on_click(on_remove),
        )
}
