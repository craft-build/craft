use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::tools::ToolInvocation;
use crate::types::{Priority, ToolOutput};

const DEFAULT_LIMIT: usize = 50;
const NO_FINDINGS_MSG: &str =
    "No findings recorded yet in this session. Run a `review` first; findings reported via `report_finding` are stored automatically.";

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct ReadFindings {
    #[param(description = "Optional priority filter (P0, P1, P2, P3)")]
    priority: Option<String>,
    #[param(description = "Optional substring match against file_path")]
    file_path_contains: Option<String>,
    #[param(description = "Maximum findings to return (default 50)")]
    limit: Option<usize>,
}

impl ReadFindings {
    pub const NAME: &str = "read_findings";
    pub const DESCRIPTION: &str =
        "Retrieve detailed code review findings recorded by review subagents during this session. Use this when you need the original priority, file path, line numbers, body, suggested fix, and rule IDs after a review tool has finished.";
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"priority": "P0"}, {"file_path_contains": "auth", "limit": 10}]"#,
    );

    pub fn start_header(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(p) = &self.priority {
            parts.push(format!("priority={p}"));
        }
        if let Some(s) = &self.file_path_contains {
            parts.push(format!("file~{s}"));
        }
        if parts.is_empty() {
            "read_findings()".into()
        } else {
            format!("read_findings({})", parts.join(", "))
        }
    }

    pub async fn execute(&self, ctx: &ToolContext) -> Result<ToolOutput, String> {
        let priority = match self.priority.as_deref() {
            None => None,
            Some(s) => Some(parse_priority(s)?),
        };

        let Some(store) = ctx.findings_store.as_ref() else {
            return Ok(ToolOutput::Markdown(NO_FINDINGS_MSG.to_owned()));
        };

        let limit = self.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let entries = store
            .lock()
            .unwrap()
            .filter(priority, self.file_path_contains.as_deref(), limit);

        if entries.is_empty() {
            return Ok(ToolOutput::Markdown(NO_FINDINGS_MSG.to_owned()));
        }

        let findings = entries.into_iter().map(|e| e.finding).collect();
        Ok(ToolOutput::Findings(findings))
    }
}

fn parse_priority(s: &str) -> Result<Priority, String> {
    match s.to_uppercase().as_str() {
        "P0" => Ok(Priority::P0),
        "P1" => Ok(Priority::P1),
        "P2" => Ok(Priority::P2),
        "P3" => Ok(Priority::P3),
        _ => Err(format!("invalid priority '{s}', expected P0-P3")),
    }
}

super::impl_tool!(ReadFindings, audience = super::ToolAudience::MAIN, kind = "search");

impl ToolInvocation for ReadFindings {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(ReadFindings::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { ReadFindings::execute(&self, ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::agent::FindingsStore;
    use crate::tools::test_support::stub_ctx;
    use crate::types::Finding;
    use serde_json::json;

    fn finding(priority: Priority, file_path: &str, title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: "body".into(),
            priority,
            confidence: 0.9,
            file_path: file_path.into(),
            line_start: 1,
            line_end: 1,
            rule_ids: vec![],
            suggestion: None,
        }
    }

    #[tokio::test]
    async fn returns_markdown_when_store_absent() {
        let ctx = stub_ctx(&AgentMode::Build);
        let tool = ReadFindings::parse_input(&json!({})).unwrap();
        let out = tool.execute(&ctx).await.unwrap();
        match out {
            ToolOutput::Markdown(s) => assert!(s.contains("No findings recorded")),
            other => panic!("expected Markdown, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_markdown_when_store_empty() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        ctx.findings_store = Some(FindingsStore::new_shared());
        let tool = ReadFindings::parse_input(&json!({})).unwrap();
        let out = tool.execute(&ctx).await.unwrap();
        assert!(matches!(out, ToolOutput::Markdown(_)));
    }

    #[tokio::test]
    async fn priority_filter_applied() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        let store = FindingsStore::new_shared();
        store.lock().unwrap().extend(
            "task",
            vec![
                finding(Priority::P0, "a.rs", "x"),
                finding(Priority::P1, "b.rs", "y"),
            ],
        );
        ctx.findings_store = Some(store);

        let tool = ReadFindings::parse_input(&json!({"priority": "P0"})).unwrap();
        let out = tool.execute(&ctx).await.unwrap();
        match out {
            ToolOutput::Findings(f) => {
                assert_eq!(f.len(), 1);
                assert_eq!(f[0].priority, Priority::P0);
            }
            other => panic!("expected Findings, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_path_substring_filter_applied() {
        let mut ctx = stub_ctx(&AgentMode::Build);
        let store = FindingsStore::new_shared();
        store.lock().unwrap().extend(
            "task",
            vec![
                finding(Priority::P1, "src/auth/login.rs", "x"),
                finding(Priority::P1, "src/db/query.rs", "y"),
            ],
        );
        ctx.findings_store = Some(store);

        let tool =
            ReadFindings::parse_input(&json!({"file_path_contains": "auth"})).unwrap();
        let out = tool.execute(&ctx).await.unwrap();
        match out {
            ToolOutput::Findings(f) => {
                assert_eq!(f.len(), 1);
                assert!(f[0].file_path.contains("auth"));
            }
            other => panic!("expected Findings, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_priority_returns_error() {
        let ctx = stub_ctx(&AgentMode::Build);
        let tool = ReadFindings::parse_input(&json!({"priority": "P9"})).unwrap();
        let err = tool.execute(&ctx).await.unwrap_err();
        assert!(err.contains("invalid priority"));
    }
}
