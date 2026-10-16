mod acp;
mod app;
mod async_runtime;
mod checkpoint;
mod chrome;
mod config;
mod markdown;
mod menus;
mod persistence;
mod screens;
mod selectable_text;
mod state;
mod text_input;
mod theme;

use gpui::prelude::*;
use gpui::{
    Application, Bounds, TitlebarOptions, WindowBounds, WindowDecorations, WindowOptions, point,
    px, size,
};

fn main() {
    let application = Application::new();
    // On macOS the app keeps running after its last window closes, so
    // clicking the dock icon has to bring a window back.
    application.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_main_window(cx);
        }
    });
    application.run(|cx: &mut gpui::App| {
        theme::load_fonts(cx);
        menus::install(cx, open_main_window);
        // Elsewhere there's no dock/menu bar to get a window back from, so
        // closing the last window quits.
        if !cfg!(target_os = "macos") {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        }

        open_main_window(cx);
        cx.activate(true);
    });
    app::flush_state_saves();
}

fn open_main_window(cx: &mut gpui::App) {
    // A previous window's queued state snapshots must be on disk before the
    // new `App` loads them.
    app::flush_state_saves();
    let bounds = Bounds::centered(None, size(px(1440.0), px(860.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Craft".into()),
                // No native white titlebar — every screen draws its own
                // 40px bar (see `chrome.rs`) and reserves room on the
                // left for the still-native macOS traffic lights, which
                // are just repositioned (not hand-drawn) via
                // `traffic_light_position` below.
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.0), px(14.0))),
            }),
            // Client-side decorations on Linux, so there's no native
            // titlebar strip left for our chrome to sit under either.
            window_decorations: Some(WindowDecorations::Client),
            window_min_size: Some(size(px(760.0), px(480.0))),
            ..Default::default()
        },
        |_window, cx| cx.new(app::App::new),
    )
    .unwrap();
}
