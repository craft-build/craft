use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::acp_client::AcpClient;

pub struct AppState {
    pub craft_binary: PathBuf,
    pub clients: Mutex<HashMap<String, Arc<AcpClient>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            craft_binary: resolve_craft_binary(),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Kills every spawned `craft acp` child. Called from the app-exit run
    /// loop (`RunEvent::ExitRequested`) so closing the window doesn't orphan
    /// the ACP servers, which Tauri's drop order would otherwise leave
    /// running after its tokio runtime has shut down.
    pub async fn kill_all(&self) {
        let clients = self.clients.lock().await.drain().collect::<Vec<_>>();
        for (_, client) in clients {
            client.kill();
        }
    }
}

/// Locates the `craft` binary craft-desktop drives over ACP. Checked in order:
/// 1. `CRAFT_DESKTOP_BINARY` env var (explicit override, e.g. for `cargo run`
///    against a workspace build without installing craft system-wide).
/// 2. The sibling `craft`/`craft.exe` next to this executable (bundled app).
/// 3. `~/.cargo/bin/craft` (where the install script puts it; GUI launches
///    don't inherit the shell `PATH` so `which` can't see it).
/// 4. `craft` resolved from `PATH`.
///
/// Falls back to the bare command name `craft` so the error surfaces clearly
/// (process spawn failure) instead of panicking at startup.
fn resolve_craft_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CRAFT_DESKTOP_BINARY") {
        return PathBuf::from(path);
    }
    let exe_name = if cfg!(windows) { "craft.exe" } else { "craft" };
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join(exe_name);
        if sibling.is_file() {
            return sibling;
        }
    }
    if let Some(home) = craft_storage::paths::home()
        && home.join(".cargo").join("bin").join(exe_name).is_file()
    {
        return home.join(".cargo").join("bin").join(exe_name);
    }
    which::which("craft").unwrap_or_else(|_| PathBuf::from(exe_name))
}
