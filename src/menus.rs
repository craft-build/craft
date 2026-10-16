//! The native menu bar (Craft / File / Edit / View / Window) and its
//! keyboard shortcuts.
//!
//! App-wide actions (quit, hide, window management) are handled here with
//! global listeners. Actions that touch app state (`OpenWorkspace`,
//! `ToggleSidebar`, ...) are handled on the root `App` view, and the Edit
//! actions by whichever `TextInput` has focus — falling back to the root
//! view for copying a dragged text selection. gpui dispatches menu actions
//! through the focused element's path, so the deepest handler wins.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

actions!(
    craft,
    [
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        OpenSettings,
        OpenWorkspace,
        ShowWorkspaces,
        CloseWindow,
        Cut,
        Copy,
        Paste,
        SelectAll,
        ToggleSidebar,
        ToggleFileTree,
        ToggleFullScreen,
        Minimize,
        Zoom,
        ShowMainWindow,
    ]
);

/// Registers the global action handlers, key bindings and the menu bar.
/// `open_main_window` is used to bring the window back after it's closed.
pub fn install(cx: &mut App, open_main_window: fn(&mut App)) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(move |_: &ShowMainWindow, cx| {
        // Only ever one main window: each `App` owns and persists the whole
        // state file, so a second one would clobber the first.
        match cx.windows().into_iter().next() {
            Some(handle) => {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
                cx.activate(true);
            }
            None => open_main_window(cx),
        }
    });
    cx.on_action(|_: &CloseWindow, cx| with_active_window(cx, |window| window.remove_window()));
    cx.on_action(|_: &Minimize, cx| with_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| with_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &ToggleFullScreen, cx| {
        with_active_window(cx, |window| window.toggle_fullscreen())
    });

    cx.bind_keys([
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-o", OpenWorkspace, None),
        KeyBinding::new("secondary-shift-o", ShowWorkspaces, None),
        KeyBinding::new("secondary-w", CloseWindow, None),
        KeyBinding::new("secondary-x", Cut, None),
        KeyBinding::new("secondary-c", Copy, None),
        KeyBinding::new("secondary-v", Paste, None),
        KeyBinding::new("secondary-a", SelectAll, None),
        KeyBinding::new("secondary-b", ToggleSidebar, None),
        KeyBinding::new("secondary-alt-b", ToggleFileTree, None),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, None),
        KeyBinding::new("cmd-m", Minimize, None),
    ]);

    cx.set_menus(vec![
        Menu {
            name: "Craft".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide Craft", Hide),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit Craft", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open Workspace…", OpenWorkspace),
                MenuItem::action("All Workspaces", ShowWorkspaces),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Cut", Cut, OsAction::Cut),
                MenuItem::os_action("Copy", Copy, OsAction::Copy),
                MenuItem::os_action("Paste", Paste, OsAction::Paste),
                MenuItem::separator(),
                MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Review Panel", ToggleFileTree),
                MenuItem::separator(),
                MenuItem::action("Enter Full Screen", ToggleFullScreen),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("Craft", ShowMainWindow),
            ],
        },
    ]);
}

fn with_active_window(cx: &mut App, f: impl FnOnce(&mut Window)) {
    if let Some(handle) = cx.active_window() {
        let _ = handle.update(cx, |_, window, _| f(window));
    }
}
