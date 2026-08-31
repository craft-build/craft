use craft_tool_macro::Tool;
use serde::Deserialize;

use argosy::codetools::inspect::{self, InspectParams};

use crate::ToolOutput;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Inspect {
    #[param(description = "Sections: todos, git_status, or all (default all)")]
    sections: Option<String>,
    #[param(description = "File or directory to scope (default: cwd)")]
    scope: Option<String>,
}

impl Inspect {
    pub const NAME: &str = "inspect";
    pub const DESCRIPTION: &str = include_str!("inspect.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"sections": "all"}, {"sections": "todos", "scope": "src/lib.rs"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let params = InspectParams {
            sections: self.sections.clone(),
            scope: self.scope.clone(),
        };
        let report = inspect::run(&ctx.code_tools, params).map_err(|e| e.to_string())?;
        Ok(ToolOutput::Plain(report.text))
    }

    pub fn start_header(&self) -> String {
        format!("inspect {}", self.sections.as_deref().unwrap_or("all"))
    }
}

super::impl_tool!(Inspect, kind = "inspect", tier = super::ToolTier::Core,);

impl super::ToolInvocation for Inspect {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Inspect::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Inspect::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todos_find_todo_and_fixme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.rs"),
            "fn main() {\n  // TODO: fix this\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("test.py"), "# FIXME: broken\npass\n").unwrap();
        let tool = Inspect {
            sections: Some("todos".into()),
            scope: Some(dir.path().to_string_lossy().into_owned()),
        };
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        let out = futures::executor::block_on(tool.execute(&ctx)).unwrap();
        let text = out.as_text();
        assert!(text.contains("(2 items)"), "got: {text}");
        assert!(text.contains("fix this"));
        assert!(text.contains("broken"));
    }

    #[test]
    fn todos_none_reports_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("clean.rs"), "fn main() {}\n").unwrap();
        let tool = Inspect {
            sections: Some("todos".into()),
            scope: Some(dir.path().to_string_lossy().into_owned()),
        };
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        let out = futures::executor::block_on(tool.execute(&ctx)).unwrap();
        assert!(out.as_text().contains("todos: (none)"));
    }

    #[test]
    fn long_todo_preview_is_truncated() {
        let long = format!("// TODO: {}\n", "x".repeat(100));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), long).unwrap();
        let tool = Inspect {
            sections: Some("todos".into()),
            scope: Some(dir.path().to_string_lossy().into_owned()),
        };
        let ctx = crate::tools::test_support::stub_ctx(&crate::AgentMode::Build);
        let out = futures::executor::block_on(tool.execute(&ctx)).unwrap();
        let text = out.as_text();
        assert!(text.contains("..."), "got: {text}");
    }
}
