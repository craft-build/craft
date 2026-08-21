//! craft-desktop: Dioxus desktop GUI for the craft coding harness.

mod acp;
mod backend;
mod components;
mod markdown;
mod state;
mod theme;

use dioxus::prelude::*;

use components::{
    ChangesPanel, Composer, NewTaskView, Onboarding, Palette, SessionView, Sidebar, TopBar,
};
use state::{Popover, View, provide_state};

const FAVICON: Asset = asset!("/assets/favicon.ico");
const CRAFT_CSS: Asset = asset!("/assets/craft.css");

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Syntax highlighting: use the persisted theme's syntax colors and warm
    // the syntect sets.
    let current = theme::current_theme();
    craft_highlight::set_theme(current.syntax_theme.clone());
    craft_highlight::warmup();

    // Frameless window: the app draws its own chrome (traffic lights + the
    // sidebar header act as the titlebar). The `desktop!` macro drops the
    // config entirely on other platforms.
    dioxus::LaunchBuilder::new()
        .with_cfg(desktop! {
            let config = dioxus::desktop::Config::default();
            // `with_menu` must precede `with_window`: dioxus only accepts a
            // menu while decorations are still on, then keeps it.
            #[cfg(target_os = "macos")]
            let config = config.with_menu(edit_menu());
            config.with_window(
                dioxus::desktop::WindowBuilder::new()
                    .with_title("craft")
                    .with_decorations(false)
                    .with_transparent(true)
                    .with_inner_size(dioxus::desktop::LogicalSize::new(1560.0, 940.0))
                    .with_min_inner_size(dioxus::desktop::LogicalSize::new(960.0, 600.0)),
            )
        })
        .launch(App);
}

/// Frameless windows get no default menu bar from dioxus, but on macOS the
/// Edit menu's key equivalents are what route Cmd+C/X/V/A to the webview.
/// Reinstall the standard Edit items.
#[cfg(all(feature = "desktop", target_os = "macos"))]
fn edit_menu() -> dioxus::desktop::muda::Menu {
    use dioxus::desktop::muda::{Menu, PredefinedMenuItem, Submenu};

    let menu = Menu::new();
    let edit = Submenu::new("Edit", true);
    edit.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::select_all(None),
    ])
    .expect("predefined menu items are valid");
    menu.append(&edit).expect("submenu append is valid");
    menu
}

#[component]
fn App() -> Element {
    let mut s = provide_state();
    let backend = backend::init();

    // Focus the shell on mount so global keyboard shortcuts work before the
    // user has clicked into an input.
    use_effect(move || {
        document::eval("document.getElementById('craft-root')?.focus()");
    });

    // Drain backend events into the signals for the life of the app.
    use_future(move || {
        let b = backend;
        let rx = b.events();
        async move {
            while let Ok(event) = rx.recv_async().await {
                state::apply_event(s, b, event);
            }
        }
    });

    // Resolve the active theme: parse on demand (themes are small toml
    // files; parsing 29 of them per switch would be wasteful, one is fine).
    let theme_name = s.theme.read().clone();
    let tokens =
        theme::get_theme_by_name(&theme_name).unwrap_or_else(|_| theme::current_theme_fallback());
    let theme_style = tokens.css_vars();

    let has_tabs = !s.tabs.read().is_empty();
    let is_new = *s.view.read() == View::New;

    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Stylesheet { href: CRAFT_CSS }

        // The desktop window is transparent outside the rounded corners; on
        // other platforms give the page a matching backdrop instead.
        if !cfg!(feature = "desktop") {
            document::Style { "html,body{{background:#03050a}}" }
        }

        div {
            id: "craft-root",
            class: "app-shell",
            tabindex: 0,
            style: theme_style,
            onkeydown: move |e| {
                if e.modifiers().meta() || e.modifiers().ctrl() {
                    if matches!(e.key(), Key::Character(ref c) if c.eq_ignore_ascii_case("k")) {
                        e.prevent_default();
                        s.palette_open.set(true);
                        s.query.set(String::new());
                        s.palette_selected.set(0);
                    }
                    if matches!(e.key(), Key::Character(ref c) if c.eq_ignore_ascii_case("n")) && has_tabs {
                        e.prevent_default();
                        state::open_new_task(s);
                    }
                }
                if e.key() == Key::Escape {
                    s.palette_open.set(false);
                    s.popover.set(Popover::None);
                }
            },

            div { class: "app-window",
                if has_tabs {
                    Sidebar {}

                    div { class: "main-col",
                        TopBar {}
                        div { class: "content-area",
                            div { class: "center-col",
                                if is_new {
                                    NewTaskView {}
                                } else {
                                    SessionView {}
                                }
                                Composer {}
                            }
                            if *s.panel_open.read() && !is_new {
                                ChangesPanel {}
                            }
                        }
                    }
                } else {
                    div { class: "main-col",
                        TopBar {}
                        div { class: "content-area",
                            div { class: "center-col",
                                Onboarding {}
                            }
                        }
                    }
                }

                if *s.palette_open.read() {
                    Palette {}
                }
            }
        }
    }
}
