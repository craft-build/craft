#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod acp_client;
mod commands;
mod state;
mod theme;

use state::AppState;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tauri::Builder::default()
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
            commands::resolve_question,
            commands::cancel_prompt,
            commands::close_tab,
            commands::get_theme,
            commands::list_themes,
            commands::set_theme,
        ])
        .run(tauri::generate_context!())
        .expect("error while running craft-desktop");
}
