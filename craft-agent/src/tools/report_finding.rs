use craft_tool_macro::Tool;
use serde::Deserialize;

use super::ToolContext;
use crate::tools::ToolInvocation;
use crate::types::{Finding, Priority, ToolOutput};

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct ReportFinding {
    #[param(
        description = "Imperative title, prefixed with priority (e.g. '[P1] Add error handling')"
    )]
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
        let priority = parse_priority(&self.priority)?;
        let finding = Finding {
            title: self.title.clone(),
            body: self.body.trim().to_string(),
            priority,
            confidence: self.confidence.clamp(0.0, 1.0),
            file_path: self.file_path.clone(),
            line_start: self.line_start,
            line_end: self.line_end,
            rule_ids: self.rule_ids.clone().unwrap_or_default(),
            suggestion: self.suggestion.clone(),
        };

        Ok(ToolOutput::Findings(vec![finding]))
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

super::impl_tool!(
    ReportFinding,
    audience = super::ToolAudience::MAIN | super::ToolAudience::RESEARCH_SUB,
    kind = "think"
);

impl ToolInvocation for ReportFinding {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(ReportFinding::start_header(
            self,
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { ReportFinding::execute(&self, ctx).await })
    }
}
