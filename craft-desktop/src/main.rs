#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acp_client;
mod commands;
mod state;
mod theme;

use state::AppState;
use tauri::{Manager, RunEvent};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::start_session,
            commands::load_session,
            commands::list_sessions,
            commands::send_prompt,
            commands::set_mode,
            commands::set_config_option,
            commands::resolve_permission,
            commands::respond_elicitation,
            commands::cancel_prompt,
            commands::close_tab,
            commands::get_theme,
            commands::list_themes,
            commands::set_theme,
            commands::list_commands,
            commands::craft_command,
        ])
        .build(tauri::generate_context!())
        .expect("error while building craft-desktop");

    app.run(|app_handle, event| {
        // Kill every spawned `craft acp` child before Tauri tears down its
        // tokio runtime. Without this, closing the app orphans the servers:
        // `kill_on_drop`/`Drop` only fire while the runtime is alive, and
        // Tauri drops `AppState` after the runtime is gone.
        if let RunEvent::ExitRequested { .. } = event
            && let Some(state) = app_handle.try_state::<AppState>()
        {
            tauri::async_runtime::block_on(async {
                state.kill_all().await;
            });
        }
    });
}
