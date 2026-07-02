use std::path::PathBuf;

use craft_storage::StateDir;
use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

const VCC_RECALL_TOOL_NAME: &str = "vcc_recall";
const SESSIONS_SUBDIR: &str = "sessions";

#[derive(Tool, Debug, Clone, Deserialize)]
pub(crate) struct VccRecall {
    #[param(
        description = "Search terms or regex pattern (e.g. 'auth|login', 'fail.*build'). Multi-word queries are OR-ranked by relevance. Omit to browse recent history."
    )]
    query: Option<String>,
    #[param(
        description = "Entry indices to return full untruncated content for. Works alone or alongside a query."
    )]
    expand: Option<Vec<usize>>,
    #[param(description = "Page number (1-based) for paginated search results. Default: 1.")]
    page: Option<usize>,
}

impl VccRecall {
    pub const NAME: &str = VCC_RECALL_TOOL_NAME;
    pub const DESCRIPTION: &str = "Search the current session's full history (across compactions) losslessly. Supports regex queries, paging, and full-content expansion. Use to recall prior work, decisions, or context that was summarized away. Omit the query to browse recent entries.";
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"query": "auth token refresh"}, {"expand": [42, 47]}]"#);

    fn session_path(ctx: &crate::tools::ToolContext) -> Option<PathBuf> {
        let id = ctx.session_id.as_ref()?;
        let state = StateDir::resolve().ok()?;
        Some(
            state
                .path()
                .join(SESSIONS_SUBDIR)
                .join(format!("{id}.jsonl")),
        )
    }

    pub async fn execute(&self, ctx: &crate::tools::ToolContext) -> Result<ToolOutput, String> {
        let path = match Self::session_path(ctx) {
            Some(p) => p,
            None => return Ok(ToolOutput::Plain("No session file available.".into())),
        };
        if !path.exists() {
            return Ok(ToolOutput::Plain(format!(
                "Session file not found: {}",
                path.display()
            )));
        }
        let page = self.page.unwrap_or(1);
        let expand = self.expand.as_deref();
        let output = crate::agent::vcc::recall::run(&path, self.query.as_deref(), page, expand)
            .map_err(|e| format!("failed to read session: {e}"))?;
        Ok(ToolOutput::Plain(output))
    }
}

crate::tools::impl_tool!(
    VccRecall,
    audience = crate::tools::ToolAudience::MAIN | crate::tools::ToolAudience::RESEARCH_SUB,
);

impl crate::tools::ToolInvocation for VccRecall {
    fn start_header(&self) -> crate::tools::HeaderFuture {
        crate::tools::HeaderFuture::Ready(crate::tools::HeaderResult::plain(match &self.query {
            Some(q) => format!("vcc_recall {q}"),
            None => "vcc_recall (browse)".to_string(),
        }))
    }
    fn execute<'a>(
        self: Box<Self>,
        ctx: &'a crate::tools::ToolContext,
    ) -> crate::tools::ExecFuture<'a> {
        Box::pin(async move { VccRecall::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use craft_providers::{ContentBlock, Message, Role};
    use std::io::Write;

    fn write_session(dir: &std::path::Path, id: &str, msgs: &[Message]) -> std::path::PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{"t":"header","v":1,"id":"{id}","model":"m","cwd":"/p","created_at":0}}"#
        )
        .unwrap();
        for m in msgs {
            let d = serde_json::to_string(m).unwrap();
            writeln!(f, r#"{{"t":"msg","d":{d}}}"#).unwrap();
        }
        path
    }

    fn msgs() -> Vec<Message> {
        vec![
            Message::user("Fix the auth token refresh bug".into()),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "I will fix the refresh logic".into(),
                }],
                ..Default::default()
            },
            Message::user("now add tests for auth".into()),
        ]
    }

    #[test]
    fn recall_search_finds_matches() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_session(tmp.path(), "s1", &msgs());
        let out = crate::agent::vcc::recall::run(&path, Some("auth"), 1, None).unwrap();
        assert!(out.contains("auth"), "output: {out}");
        assert!(out.contains("[user]"));
    }

    #[test]
    fn recall_browse_returns_recent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_session(tmp.path(), "s1", &msgs());
        let out = crate::agent::vcc::recall::run(&path, None, 1, None).unwrap();
        assert!(out.contains("Session history"));
        assert!(out.contains("Fix the auth token refresh bug"));
    }

    #[test]
    fn recall_expand_returns_full_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_session(tmp.path(), "s1", &msgs());
        let out = crate::agent::vcc::recall::run(&path, None, 1, Some(&[0])).unwrap();
        assert!(out.contains("Expanded entries"));
        assert!(out.contains("Fix the auth token refresh bug"));
    }

    #[test]
    fn recall_regex_query() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_session(tmp.path(), "s1", &msgs());
        let out = crate::agent::vcc::recall::run(&path, Some("auth|refresh"), 1, None).unwrap();
        assert!(
            out.contains("auth") || out.contains("refresh"),
            "output: {out}"
        );
    }
}
