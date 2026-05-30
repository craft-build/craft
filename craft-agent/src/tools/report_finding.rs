use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::tools::ToolInvocation;
use crate::ToolOutput;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct ReportFinding {
    #[param(description = "Imperative title, prefixed with priority (e.g. '[P1] Add error handling')")]
    title: String,
    #[param(description = "Markdown body: what, why, rule, fix")]
    body: String,
    #[param(description = "Priority: P0, P1, P2, or P3")]
    priority: String,
    #[param(description = "Confidence 0.0-1.0")]
    confidence: f64,
    #[param(description = "Absolute file path")]
    file_path: String,
    #[param(description = "Start line number")]
    line_start: usize,
    #[param(description = "End line number")]
    line_end: usize,
    #[param(description = "Styleguide rule IDs")]
    rule_ids: Option<Vec<String>>,
    #[param(description = "Suggested fix or code snippet")]
    suggestion: Option<String>,
}

impl ReportFinding {
    pub const NAME: &str = "report_finding";
    pub const DESCRIPTION: &str =
        "Report a code review finding with priority, location, and optional rule references.";
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"title": "[P1] Add error handling for file read", "body": "The read call can fail but the error is silently ignored.", "priority": "P1", "confidence": 0.9, "file_path": "/src/main.rs", "line_start": 42, "line_end": 42, "rule_ids": ["CHECK-RETURN-VALUES"], "suggestion": "Use `let data = fs::read(&path)?;`"}]"#,
    );

    pub fn start_header(&self) -> String {
        self.title.clone()
    }

    pub async fn execute(&self, _ctx: &ToolContext) -> Result<ToolOutput, String> {
        let confidence_pct = (self.confidence.clamp(0.0, 1.0) * 100.0) as u8;

        let mut lines = vec![
            format!("# {} [{}]", self.priority.to_uppercase(), self.title),
            format!("Location: {}:{}-{}", self.file_path, self.line_start, self.line_end),
            format!("Priority: {} | Confidence: {}%", self.priority.to_uppercase(), confidence_pct),
            String::new(),
            self.body.trim().to_string(),
        ];

        if let Some(ref ids) = self.rule_ids
            && !ids.is_empty()
        {
            lines.push(format!("\nRules: {}", ids.join(", ")));
        }

        if let Some(ref fix) = self.suggestion {
            lines.push(String::new());
            lines.push("**Suggested fix:**".into());
            lines.push(fix.trim().to_string());
        }

        Ok(ToolOutput::Plain(lines.join("\n")))
    }
}

super::impl_tool!(ReportFinding, audience = super::ToolAudience::MAIN | super::ToolAudience::RESEARCH_SUB);

impl ToolInvocation for ReportFinding {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(ReportFinding::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { ReportFinding::execute(&self, ctx).await })
    }
}
