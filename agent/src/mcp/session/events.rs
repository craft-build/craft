use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::super::request::McpServerRequest;
type ListChangedCb = Arc<dyn Fn(&str) + Send + Sync>;
type LogCb = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;
type DeadCb = Arc<dyn Fn(&str) + Send + Sync>;

/// Server → client notification callbacks, injected by the manager (or the
/// TUI for notices). Callbacks must be cheap and non-blocking: they run on
/// rmcp's transport task.
#[derive(Clone, Default)]
pub struct McpEvents {
    /// `tools/list_changed` / `prompts/list_changed` /
    /// `resources/list_changed`: the manager re-lists that server without
    /// reconnecting.
    pub on_list_changed: Option<ListChangedCb>,
    /// `notifications/message` at warning-or-worse: (server, level, message).
    pub on_log: Option<LogCb>,
    /// Keepalive ping failed: the session is unusable, the manager marks the
    /// entry Failed so the user is offered Reconnect.
    pub on_dead: Option<DeadCb>,
    /// Where server-initiated requests (elicitation) are relayed.
    /// `None` (or a dropped receiver) makes the handler deny them cleanly.
    pub server_requests: Option<mpsc::UnboundedSender<McpServerRequest>>,
    /// Workspace root advertised via `roots/list` and the `roots` capability.
    /// `None` in callers that have no cwd to speak of.
    pub root: Option<PathBuf>,
}

impl McpEvents {
    pub(super) fn list_changed(&self, server: &str) {
        if let Some(cb) = &self.on_list_changed {
            cb(server);
        }
    }

    pub(super) fn log(&self, server: &str, level: &str, message: &str) {
        if let Some(cb) = &self.on_log {
            cb(server, level, message);
        }
    }

    pub(super) fn dead(&self, server: &str) {
        if let Some(cb) = &self.on_dead {
            cb(server);
        }
    }
}
