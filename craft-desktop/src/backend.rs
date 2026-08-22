//! In-process backend: owns the tokio runtime, the per-tab `craft acp`
//! clients, and the event channel the UI state drains. UI actions call these
//! methods; results flow back through [`Event`]s so the UI thread never
//! blocks on a child-process round trip.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::acp::{AcpClient, LaunchTarget};

type ClientMap = Arc<Mutex<HashMap<String, Arc<AcpClient>>>>;

static INSTANCE: std::sync::OnceLock<Arc<Backend>> = std::sync::OnceLock::new();

/// Create (once) and return the process-global backend. UI closures call
/// [`get`] instead of capturing an `Arc`, so event handlers stay `'static`.
pub fn init() -> &'static Arc<Backend> {
    INSTANCE.get_or_init(|| Arc::new(Backend::new()))
}

pub fn get() -> &'static Backend {
    init()
}

/// Everything the UI needs to react to, delivered over flume.
pub enum Event {
    SessionUpdate {
        tab_id: String,
        update: Value,
    },
    Permission {
        tab_id: String,
        request_id: Value,
        params: Value,
    },
    Question {
        tab_id: String,
        request_id: Value,
        params: Value,
    },
    Todos {
        tab_id: String,
        todos: Value,
    },
    /// Terminal state of a `session/prompt` round trip.
    PromptDone {
        tab_id: String,
        ok: bool,
        error: Option<String>,
    },
    /// The `craft acp` child for a tab exited (stdout closed).
    Closed {
        tab_id: String,
    },
    /// Result of `start_session`/`load_session`: the `session/new` response
    /// (`{sessionId, modes, configOptions}`) or the failure.
    SessionStarted {
        tab_id: String,
        cwd: String,
        result: Result<Value, String>,
    },
    /// Persisted session listing for the sidebar's history group.
    SessionsListed {
        sessions: Vec<SessionSummary>,
    },
    /// Cached `_craft/listCommands` response for the `/` palette.
    CommandsListed {
        tab_id: String,
        commands: Value,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SshTarget {
    pub host: String,
    pub remote_craft: Option<String>,
}

#[derive(Clone)]
pub struct StartOptions {
    pub cwd: String,
    pub yolo: bool,
    pub ssh: Option<SshTarget>,
    pub mode: Option<String>,
    pub auto_review: bool,
    /// Sent as the first prompt once the session is live.
    pub initial_prompt: Option<String>,
}

pub struct Backend {
    rt: tokio::runtime::Runtime,
    events: flume::Sender<Event>,
    rx: flume::Receiver<Event>,
    clients: ClientMap,
    craft_binary: PathBuf,
}

impl Backend {
    pub fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("craft-desktop backend tokio runtime");
        let (tx, rx) = flume::unbounded::<Event>();
        Self {
            rt,
            events: tx,
            rx,
            clients: Default::default(),
            craft_binary: resolve_craft_binary(),
        }
    }

    pub fn events(&self) -> flume::Receiver<Event> {
        self.rx.clone()
    }

    pub fn craft_binary(&self) -> &Path {
        &self.craft_binary
    }

    fn target(&self, ssh: Option<&SshTarget>) -> LaunchTarget {
        match ssh {
            Some(s) => LaunchTarget::Ssh {
                host: s.host.clone(),
                remote_craft: s.remote_craft.clone(),
            },
            None => LaunchTarget::Local {
                craft_binary: self.craft_binary.clone(),
            },
        }
    }

    fn client(&self, tab_id: &str) -> Result<Arc<AcpClient>, String> {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(tab_id)
            .cloned()
            .ok_or_else(|| format!("no session for tab `{tab_id}`"))
    }

    /// Spawns a dedicated `craft acp` process for this tab, initializes the
    /// protocol handshake, and opens (or resumes, when `load` is set) a
    /// session in `opts.cwd`. One OS process per tab: craft-acp's server only
    /// tracks a single active session at a time, so concurrent tabs each need
    /// their own process.
    pub fn start_session(&self, tab_id: String, load: Option<String>, opts: StartOptions) {
        let events = self.events.clone();
        let target = self.target(opts.ssh.as_ref());
        let handle = self.rt.handle().clone();
        let spawn_handle = handle.clone();
        let clients = Arc::clone(&self.clients);
        let StartOptions {
            cwd,
            yolo,
            ssh: _,
            mode,
            auto_review,
            initial_prompt,
        } = opts;
        let load_cwd = cwd.clone();
        spawn_handle.spawn(async move {
            let handle = handle.clone();
            let result: Result<Value, String> = async {
                let client = Arc::new(
                    AcpClient::spawn(
                        &handle,
                        events.clone(),
                        tab_id.clone(),
                        &target,
                        Path::new(&load_cwd),
                        yolo,
                    )
                    .map_err(|e| e.to_string())?,
                );
                clients
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(tab_id.clone(), Arc::clone(&client));
                let outcome = async {
                    client.initialize().await.map_err(|e| e.to_string())?;
                    let mut resp = match &load {
                        Some(session_id) => client.load_session(session_id, Path::new(&cwd)).await,
                        None => client.new_session(Path::new(&cwd)).await,
                    }
                    .map_err(|e| e.to_string())?;
                    // The ACP `session/load` response omits `sessionId` (the
                    // client supplied it); echo it back so downstream code can
                    // treat both response shapes alike.
                    if let (None, Some(id)) = (resp.get("sessionId"), &load)
                        && let Some(obj) = resp.as_object_mut()
                    {
                        obj.insert("sessionId".to_string(), Value::String(id.clone()));
                    }
                    let Some(session_id) = resp.get("sessionId").and_then(Value::as_str) else {
                        return Err("session response missing sessionId".to_string());
                    };
                    if let Some(mode) = mode.as_deref() {
                        let _ = client.set_mode(session_id, mode).await;
                    }
                    if auto_review {
                        let _ = client
                            .set_config_option(session_id, "auto_review", "true")
                            .await;
                    }
                    Ok(resp)
                }
                .await;
                match outcome {
                    Ok(resp) => {
                        if let Some(prompt) = &initial_prompt
                            && let Some(session_id) = resp.get("sessionId").and_then(Value::as_str)
                        {
                            // Fire and forget: the prompt's completion
                            // arrives via `Event::PromptDone`.
                            let c = Arc::clone(&client);
                            let ev = events.clone();
                            let tab = tab_id.clone();
                            let sid = session_id.to_string();
                            let text = prompt.clone();
                            handle.spawn(async move {
                                let result = c.send_prompt(&sid, &text).await;
                                let _ = ev.send(Event::PromptDone {
                                    tab_id: tab,
                                    ok: result.is_ok(),
                                    error: result.err().map(|e| e.to_string()),
                                });
                            });
                        }
                        Ok(resp)
                    }
                    Err(e) => {
                        // Handshake failed: drop the dead client.
                        clients
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&tab_id);
                        client.kill();
                        Err(e)
                    }
                }
            }
            .await;
            let _ = events.send(Event::SessionStarted {
                tab_id,
                cwd,
                result,
            });
        });
    }

    /// Lists persisted sessions (optionally filtered by `cwd`) via a
    /// short-lived child process, for the sidebar's history group.
    pub fn list_sessions(&self, cwd: Option<String>) {
        let events = self.events.clone();
        let target = self.target(None);
        let handle = self.rt.handle().clone();
        let spawn_handle = handle.clone();
        let probe_cwd = cwd.clone().unwrap_or_else(|| ".".to_string());
        spawn_handle.spawn(async move {
            let sessions = async {
                let Ok(client) = AcpClient::spawn(
                    &handle,
                    events.clone(),
                    "__list_sessions".to_string(),
                    &target,
                    Path::new(&probe_cwd),
                    false,
                ) else {
                    return Vec::new();
                };
                let resp = client.list_sessions(cwd.as_deref().map(Path::new)).await;
                client.kill();
                match resp {
                    Ok(resp) => parse_sessions(&resp),
                    Err(_) => Vec::new(),
                }
            }
            .await;
            let _ = events.send(Event::SessionsListed { sessions });
        });
    }

    pub fn send_prompt(&self, tab_id: String, session_id: String, text: String) {
        if let Ok(client) = self.client(&tab_id) {
            let events = self.events.clone();
            self.rt.spawn(async move {
                let result = client.send_prompt(&session_id, &text).await;
                let _ = events.send(Event::PromptDone {
                    tab_id,
                    ok: result.is_ok(),
                    error: result.err().map(|e| e.to_string()),
                });
            });
        }
    }

    pub fn cancel_prompt(&self, tab_id: String, session_id: String) {
        if let Ok(client) = self.client(&tab_id) {
            client.cancel(&session_id);
        }
    }

    pub fn set_mode(&self, tab_id: String, session_id: String, mode_id: String) {
        if let Ok(client) = self.client(&tab_id) {
            self.rt.spawn(async move {
                let _ = client.set_mode(&session_id, &mode_id).await;
            });
        }
    }

    pub fn set_config_option(
        &self,
        tab_id: String,
        session_id: String,
        config_id: String,
        value: String,
    ) {
        if let Ok(client) = self.client(&tab_id) {
            self.rt.spawn(async move {
                let _ = client
                    .set_config_option(&session_id, &config_id, &value)
                    .await;
            });
        }
    }

    pub fn respond_permission(&self, tab_id: String, request_id: Value, option_id: Option<String>) {
        if let Ok(client) = self.client(&tab_id) {
            client.respond_permission(request_id, option_id.as_deref());
        }
    }

    pub fn respond_elicitation(&self, tab_id: String, request_id: Value, result: Value) {
        if let Ok(client) = self.client(&tab_id) {
            client.respond_elicitation(request_id, result);
        }
    }

    /// Fetches the server-side command palette for this tab's session cwd and
    /// caches it in state for the `/` palette.
    pub fn list_commands(&self, tab_id: String) {
        if let Ok(client) = self.client(&tab_id) {
            let events = self.events.clone();
            self.rt.spawn(async move {
                if let Ok(commands) = client.list_commands().await {
                    let _ = events.send(Event::CommandsListed { tab_id, commands });
                }
            });
        }
    }

    /// Dispatches a `_craft/*` extension request for the tab's session.
    pub fn craft_command(&self, tab_id: String, method: String, params: Value) {
        if let Ok(client) = self.client(&tab_id) {
            self.rt.spawn(async move {
                let _ = client.craft_command(&method, params).await;
            });
        }
    }

    /// Ends a tab: kills its dedicated `craft acp` process. Session history
    /// is persisted continuously during the run, so this is safe mid-session.
    pub fn close_tab(&self, tab_id: &str) {
        if let Some(client) = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(tab_id)
        {
            client.kill();
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let clients = self
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect::<Vec<_>>();
        for (_, client) in clients {
            client.kill();
        }
    }
}

fn parse_sessions(resp: &Value) -> Vec<SessionSummary> {
    let Some(list) = resp.get("sessions").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|s| {
            Some(SessionSummary {
                session_id: s
                    .get("sessionId")
                    .or_else(|| s.get("id"))?
                    .as_str()?
                    .to_string(),
                cwd: s.get("cwd").and_then(Value::as_str).map(str::to_string),
                title: s.get("title").and_then(Value::as_str).map(str::to_string),
                updated_at: s
                    .get("updatedAt")
                    .or_else(|| s.get("updated_at"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
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
pub fn resolve_craft_binary() -> PathBuf {
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

/// Argument payload for `_craft` command dispatch from the `/` palette.
pub fn craft_command_params(
    session_id: &str,
    method: &str,
    name: &str,
    meta_kind: Option<&str>,
    args: &str,
) -> Result<Value, String> {
    let mut params = json!({ "sessionId": session_id });
    if method == "_craft/meta/prompt" {
        let kind = meta_kind.ok_or_else(|| format!("meta command {name} has no kind"))?;
        params["kind"] = Value::String(kind.to_string());
    } else if name == "/cd" {
        params["cwd"] = Value::String(args.trim().to_string());
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sessions_reads_camel_and_snake() {
        let resp = json!({
            "sessions": [
                { "sessionId": "a", "cwd": "/x", "title": "T", "updatedAt": "now" },
                { "id": "b" },
                { "sessionId": 3 }
            ]
        });
        let sessions = parse_sessions(&resp);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "a");
        assert_eq!(sessions[1].session_id, "b");
        assert_eq!(sessions[1].title, None);
    }

    #[test]
    fn craft_command_params_cd_and_meta() {
        let p = craft_command_params("s", "_craft/compact", "/compact", None, "").unwrap();
        assert_eq!(p, json!({ "sessionId": "s" }));
        let p = craft_command_params("s", "_craft/command/run", "/cd", None, "/tmp").unwrap();
        assert_eq!(p["cwd"], json!("/tmp"));
        let p =
            craft_command_params("s", "_craft/meta/prompt", "/review", Some("review"), "").unwrap();
        assert_eq!(p["kind"], json!("review"));
        assert!(craft_command_params("s", "_craft/meta/prompt", "/x", None, "").is_err());
    }
}
