use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tauri::{AppHandle, Emitter, State};

use crate::acp_client::AcpClient;
use crate::state::AppState;
use crate::theme::{self, ThemeName, ThemeTokens};

fn to_err<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

async fn get_client(state: &State<'_, AppState>, tab_id: &str) -> Result<Arc<AcpClient>, String> {
    state
        .clients
        .lock()
        .await
        .get(tab_id)
        .cloned()
        .ok_or_else(|| format!("no session for tab `{tab_id}`"))
}

/// Spawns a dedicated `craft acp` process for this tab, initializes the
/// protocol handshake, and opens a fresh session in `cwd`. One OS process per
/// tab: craft-acp's server only tracks a single active session at a time, so
/// concurrent tabs each need their own process.
#[tauri::command]
pub async fn start_session(
    app: AppHandle,
    state: State<'_, AppState>,
    tab_id: String,
    cwd: String,
    yolo: bool,
) -> Result<Value, String> {
    let client = AcpClient::spawn(
        app,
        tab_id.clone(),
        &state.craft_binary,
        &PathBuf::from(&cwd),
        yolo,
    )
    .map_err(to_err)?;
    client.initialize().await.map_err(to_err)?;
    let resp = client
        .new_session(&PathBuf::from(&cwd))
        .await
        .map_err(to_err)?;
    state.clients.lock().await.insert(tab_id, Arc::new(client));
    Ok(resp)
}

/// Same as `start_session` but resumes a persisted session's history instead
/// of starting fresh.
#[tauri::command]
pub async fn load_session(
    app: AppHandle,
    state: State<'_, AppState>,
    tab_id: String,
    session_id: String,
    cwd: String,
) -> Result<Value, String> {
    let client = AcpClient::spawn(
        app,
        tab_id.clone(),
        &state.craft_binary,
        &PathBuf::from(&cwd),
        false,
    )
    .map_err(to_err)?;
    client.initialize().await.map_err(to_err)?;
    let resp = client
        .load_session(&session_id, &PathBuf::from(&cwd))
        .await
        .map_err(to_err)?;
    state.clients.lock().await.insert(tab_id, Arc::new(client));
    Ok(resp)
}

/// Lists persisted sessions (optionally filtered by `cwd`) via a short-lived
/// process, for the History panel. Not tied to any tab.
#[tauri::command]
pub async fn list_sessions(
    app: AppHandle,
    state: State<'_, AppState>,
    cwd: Option<String>,
) -> Result<Value, String> {
    let probe_id = format!("__list_sessions_{}", probe_nonce());
    let probe_cwd = cwd.clone().unwrap_or_else(|| ".".to_string());
    let client = AcpClient::spawn(
        app,
        probe_id,
        &state.craft_binary,
        &PathBuf::from(&probe_cwd),
        false,
    )
    .map_err(to_err)?;
    client.initialize().await.map_err(to_err)?;
    let resp = client
        .list_sessions(cwd.as_deref().map(std::path::Path::new))
        .await
        .map_err(to_err);
    client.kill();
    resp
}

#[tauri::command]
pub async fn send_prompt(
    app: AppHandle,
    state: State<'_, AppState>,
    tab_id: String,
    session_id: String,
    text: String,
) -> Result<(), String> {
    let client = get_client(&state, &tab_id).await?;
    tokio::spawn(async move {
        let result = client.send_prompt(&session_id, &text).await;
        let payload = match result {
            Ok(resp) => {
                serde_json::json!({ "tabId": tab_id, "sessionId": session_id, "ok": true, "response": resp })
            }
            Err(e) => {
                serde_json::json!({ "tabId": tab_id, "sessionId": session_id, "ok": false, "error": e.to_string() })
            }
        };
        let _ = app.emit("acp://prompt-done", payload);
    });
    Ok(())
}

#[tauri::command]
pub async fn set_mode(
    state: State<'_, AppState>,
    tab_id: String,
    session_id: String,
    mode_id: String,
) -> Result<Value, String> {
    let client = get_client(&state, &tab_id).await?;
    client.set_mode(&session_id, &mode_id).await.map_err(to_err)
}

#[tauri::command]
pub async fn set_config_option(
    state: State<'_, AppState>,
    tab_id: String,
    session_id: String,
    config_id: String,
    value: String,
) -> Result<Value, String> {
    let client = get_client(&state, &tab_id).await?;
    client
        .set_config_option(&session_id, &config_id, &value)
        .await
        .map_err(to_err)
}

#[tauri::command]
pub async fn resolve_permission(
    state: State<'_, AppState>,
    tab_id: String,
    request_id: Value,
    option_id: Option<String>,
) -> Result<(), String> {
    let client = get_client(&state, &tab_id).await?;
    client.respond_permission(request_id, option_id.as_deref());
    Ok(())
}

#[tauri::command]
pub async fn resolve_question(
    state: State<'_, AppState>,
    tab_id: String,
    request_id: Value,
    result: Value,
) -> Result<(), String> {
    let client = get_client(&state, &tab_id).await?;
    client.respond_question(request_id, result);
    Ok(())
}

#[tauri::command]
pub async fn cancel_prompt(
    state: State<'_, AppState>,
    tab_id: String,
    session_id: String,
) -> Result<(), String> {
    let client = get_client(&state, &tab_id).await?;
    client.cancel(&session_id);
    Ok(())
}

/// Fetches the server-side command palette for this tab's session cwd.
/// Returns `{ commands: [...], custom: [...] }` — the frontend caches this
/// per session and uses it to render the `/` palette. The server already
/// runs in the session's cwd so no cwd parameter is sent.
#[tauri::command]
pub async fn list_commands(state: State<'_, AppState>, tab_id: String) -> Result<Value, String> {
    let client = get_client(&state, &tab_id).await?;
    client.list_commands().await.map_err(to_err)
}

/// Dispatches a `_craft/*` extension request. `method` MUST start with
/// `_craft/`; the client enforces this before sending. Used by the desktop
/// command palette for `/compact`, `/cd`, `/dream`, `/wiki`, etc.
#[tauri::command]
pub async fn craft_command(
    state: State<'_, AppState>,
    tab_id: String,
    method: String,
    params: Value,
) -> Result<Value, String> {
    let client = get_client(&state, &tab_id).await?;
    client.craft_command(&method, params).await.map_err(to_err)
}

/// Ends a tab: kills its dedicated `craft acp` process. Session history is
/// persisted continuously during the run (every turn), so this is safe even
/// mid-session — nothing to explicitly flush.
#[tauri::command]
pub async fn close_tab(state: State<'_, AppState>, tab_id: String) -> Result<(), String> {
    if let Some(client) = state.clients.lock().await.remove(&tab_id) {
        client.kill();
    }
    Ok(())
}

/// Returns the active theme's tokens (resolved from the persisted theme name
/// via `craft_storage::theme`, falling back to "dracula"). The webview calls
/// this once on startup.
#[tauri::command]
pub fn get_theme() -> Result<ThemeTokens, String> {
    Ok(theme::current_theme())
}

/// Lists all bundled themes for the settings picker.
#[tauri::command]
pub fn list_themes() -> Result<Vec<ThemeName>, String> {
    Ok(theme::list_theme_names())
}

/// Persists the chosen theme name (shared with the TUI via the same
/// `StateDir/theme` file) and returns the new tokens.
#[tauri::command]
pub fn set_theme(name: String) -> Result<ThemeTokens, String> {
    theme::persist_theme_name(&name);
    theme::get_theme_by_name(&name)
}

/// A throwaway hex token, unique enough for one run to key the probe client
/// map entry. Not a uuid.
fn probe_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}
