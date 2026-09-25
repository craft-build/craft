mod acp;
mod app;
mod async_runtime;
mod checkpoint;
mod chrome;
mod config;
mod markdown;
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
    Application::new().run(|cx: &mut gpui::App| {
        let bounds = Bounds::centered(None, size(px(1440.0), px(860.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Craft".into()),
                    // No native white titlebar — every screen draws its own
                    // 36px bar (see `chrome.rs`) and reserves room on the
                    // left for the still-native macOS traffic lights, which
                    // are just repositioned (not hand-drawn) via
                    // `traffic_light_position` below.
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.0), px(12.0))),
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
        cx.activate(true);
    });
    app::flush_state_saves();
}
