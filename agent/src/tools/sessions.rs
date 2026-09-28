use std::path::Path;

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, invalid};
use crate::headless::StoredSession;
use crate::storage::StateDir;

/// Sessions listed per call, newest first.
const MAX_ROWS: usize = 50;
const NO_STORAGE: &str = "no session storage directory is available";

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionsArgs {
    /// List sessions from every working directory instead of only this one.
    #[serde(default)]
    pub all_cwds: bool,
}

#[derive(Debug)]
pub struct SessionsOutput {
    pub text: String,
}

impl IntoToolOutput for SessionsOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Lists persisted sessions (id, title, age) for the workspace's cwd — the
/// same data the `/sessions` picker shows, so the model can answer questions
/// about past sessions without the user opening the picker. Resuming stays a
/// user action (`/sessions`); the tool is read-only.
#[derive(Clone)]
pub struct Sessions {
    cwd: String,
    dir: Option<StateDir>,
}

impl Sessions {
    pub fn new(cwd: impl Into<String>, dir: Option<StateDir>) -> Self {
        Self {
            cwd: cwd.into(),
            dir,
        }
    }

    /// Convenience for tests and callers that resolve the workspace root from
    /// the filesystem.
    pub fn from_root(root: &Path) -> Self {
        Self::new(root.display().to_string(), StateDir::resolve().ok())
    }

    fn execute(&self, args: SessionsArgs) -> Result<SessionsOutput> {
        let Some(dir) = &self.dir else {
            return Err(invalid(NO_STORAGE));
        };
        let cwd = (!args.all_cwds).then_some(self.cwd.as_str());
        let summaries = StoredSession::list(cwd, dir)
            .map_err(|e| invalid(format!("failed to list sessions: {e}")))?;
        if summaries.is_empty() {
            return Ok(SessionsOutput {
                text: "No sessions yet for this directory.".into(),
            });
        }
        let mut text = String::new();
        for summary in summaries.iter().take(MAX_ROWS) {
            let id: String = summary.id.as_str().chars().take(8).collect();
            text.push_str(&format!(
                "{id}  {}  {}\n",
                summary.title,
                rel_age(summary.updated_at)
            ));
        }
        let mut text = text.trim_end().to_owned();
        if summaries.len() > MAX_ROWS {
            text.push_str(&format!("\n(+{} older)", summaries.len() - MAX_ROWS));
        }
        Ok(SessionsOutput { text })
    }
}

/// Relative age ("2h ago") of an epoch timestamp; mirrors the picker's
/// sidebar rendering.
fn rel_age(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(epoch);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

impl PortableTool for Sessions {
    const NAME: &'static str = "sessions";
    type Args = SessionsArgs;
    type Output = SessionsOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "List persisted sessions for this working directory (short id, title, \
         last-updated age), newest first. Pass all_cwds to include other \
         directories. Read-only: to resume a session, ask the user to run \
         /sessions."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SessionsArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        self.execute(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SessionRef;
    use crate::storage::sessions::{SESSIONS_DIR, SessionLog};
    use tempfile::TempDir;

    const CWD: &str = "/w/proj";
    const OTHER_CWD: &str = "/w/other";

    fn state_dir(tmp: &TempDir) -> StateDir {
        StateDir::from_path(tmp.path().to_path_buf())
    }

    fn write_session(
        dir: &StateDir,
        cwd: &str,
        title: &str,
        updated_at: u64,
        i: u32,
    ) -> SessionRef {
        let id = SessionRef::from_id(
            // Distinct high bytes too: sessions minted in the same
            // millisecond share an 8-char base58 prefix otherwise.
            format!("01{i:06x}-4c71-7f00-8000-000000000000")
                .parse()
                .unwrap(),
        );
        let mut session = StoredSession::new("test-model", cwd);
        session.id = id.clone();
        session.title = title.into();
        session.updated_at = updated_at;
        // `Session::save` stamps `now` over `updated_at`, so write the log
        // directly to keep the requested ordering timestamps.
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR).unwrap();
        SessionLog::rewrite(&sessions_dir, &session).unwrap();
        id
    }

    fn tool_in(tmp: &TempDir) -> Sessions {
        Sessions::new(CWD, Some(state_dir(tmp)))
    }

    async fn call(tool: &Sessions, all_cwds: bool) -> String {
        tool.call(SessionsArgs { all_cwds }).await.unwrap().text
    }

    #[tokio::test]
    async fn lists_sessions_for_this_cwd_newest_first() {
        let tmp = TempDir::new().unwrap();
        let dir = state_dir(&tmp);
        let old = write_session(&dir, CWD, "older work", 1_000, 1);
        let new = write_session(&dir, CWD, "newer work", 2_000, 2);
        write_session(&dir, OTHER_CWD, "elsewhere", 3_000, 3);
        let text = call(&tool_in(&tmp), false).await;
        let newer = text.lines().next().unwrap();
        assert!(newer.starts_with(new.as_str().chars().take(8).collect::<String>().as_str()));
        assert!(newer.contains("newer work"));
        assert!(text.contains(old.as_str().chars().take(8).collect::<String>().as_str()));
        assert!(text.contains("older work"));
        assert!(!text.contains("elsewhere"), "{text}");
        assert!(newer.ends_with("d ago"), "{newer}");
    }

    #[tokio::test]
    async fn all_cwds_includes_other_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = state_dir(&tmp);
        write_session(&dir, CWD, "here", 1_000, 1);
        write_session(&dir, OTHER_CWD, "elsewhere", 2_000, 2);
        let text = call(&tool_in(&tmp), true).await;
        assert!(text.contains("here"));
        assert!(text.contains("elsewhere"));
        // Newest first across cwds.
        assert!(text.lines().next().unwrap().contains("elsewhere"));
    }

    #[tokio::test]
    async fn empty_directory_reports_no_sessions() {
        let tmp = TempDir::new().unwrap();
        let text = call(&tool_in(&tmp), false).await;
        assert_eq!(text, "No sessions yet for this directory.");
    }

    #[tokio::test]
    async fn missing_state_dir_is_an_invalid_args_error() {
        let tool = Sessions::new(CWD, None);
        let error = tool.call(SessionsArgs::default()).await.unwrap_err();
        assert!(error.to_string().contains("no session storage"), "{error}");
    }

    #[tokio::test]
    async fn caps_rows_and_reports_the_overflow() {
        let tmp = TempDir::new().unwrap();
        let dir = state_dir(&tmp);
        for i in 0..(MAX_ROWS + 5) {
            write_session(&dir, CWD, &format!("s{i}"), 1_000 + i as u64, i as u32);
        }
        let text = call(&tool_in(&tmp), false).await;
        assert_eq!(text.lines().count(), MAX_ROWS + 1, "{text}");
        assert!(text.lines().last().unwrap().contains("(+5 older)"));
    }

    #[test]
    fn registered_in_the_workspace_dispatch_table() {
        let tmp = TempDir::new().unwrap();
        let workspace = crate::tools::Workspace::new(tmp.path()).unwrap();
        let names = workspace.register().names();
        assert!(names.contains(&Sessions::NAME.to_owned()), "{names:?}");
    }
}
