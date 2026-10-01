//! `review` tool (Phase 5 of the argosy integration): spawn a read-only
//! reviewer subagent over the working-tree diff (or one committed revision),
//! built from argosy's reviewer material. The reviewer records findings via
//! the argosy review tools (`start_review` / `review_diff` /
//! `report_finding` / `review_findings`) and returns a
//! prioritized verdict.

use std::sync::Arc;

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, invalid};
use crate::subagent::{SubagentRequest, SubagentResult};
use crate::tools::SpawnSubagent;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewArgs {
    /// What to review (the diff scope, focus, and any context the reviewer
    /// needs; file paths and line numbers help).
    pub task: String,
    /// Files to focus on; omitted reviews every changed file.
    #[serde(default)]
    pub focus_files: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct ReviewOutput {
    pub text: String,
}

impl IntoToolOutput for ReviewOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// The reviewer's system prompt, built from the same material argosy
/// ships for its harness reviewers (vendored: argosy's `Harness` has no
/// craft variant and `REVIEWER_PROMPT` is crate-private).
pub const REVIEWER_PROMPT: &str = "You are a read-only code reviewer launched by a parent agent. \
You never modify, create, or delete files. Review the changes you were given against the \
repository and its styleguide rules, verifying every suspicion by reading the actual code \
before reporting it. Record each verified defect with the report_finding tool \
(priorities P0-P3) and end your final message with a prioritized verdict: counts per \
priority, an overall assessment, and the most important next step. Report only verified \
defects, not style taste or speculation. Your final message is returned verbatim to the \
calling agent. Be concise.";

/// Spawn a read-only reviewer subagent through the session's
/// [`SpawnSubagent`] seam (the `task` tool's pattern).
#[derive(Clone)]
pub struct Review(pub Arc<dyn SpawnSubagent>);

impl Review {
    fn execute(&self, args: ReviewArgs) -> crate::run::BoxFuture<Result<ReviewOutput>> {
        let spawn = Arc::clone(&self.0);
        Box::pin(async move {
            let mut prompt = String::from(
                "Review the current changes and report verified defects with priorities.\n\n\
                 Start with start_review (cwd is this project's root) to snapshot the \
                 diff, then review_diff for the changed files. Use search_rules \
                 to find the styleguide rules that govern the changed code, and \
                 report_finding for every verified defect (P0-P3, file:line, a concrete \
                 failure scenario, and a fix). Finish with review_findings to audit your \
                 finding set, then end with a prioritized verdict.\n\nTask:\n",
            );
            prompt.push_str(&args.task);
            if let Some(focus) = args.focus_files.filter(|f| !f.is_empty()) {
                prompt.push_str("\n\nFocus files:\n");
                for file in focus {
                    prompt.push_str("- ");
                    prompt.push_str(&file);
                    prompt.push('\n');
                }
            }
            let req = SubagentRequest {
                tool_use_id: crate::run::dispatch::current_call_id().unwrap_or_default(),
                description: "code review".into(),
                prompt,
                subagent_type: "reviewer".into(),
                model_tier: Some("strong".into()),
                context_mode: "none".into(),
                output_schema: None,
                isolation: "none".into(),
            };
            match spawn.spawn(req).await {
                Ok(SubagentResult::Text(text)) => Ok(ReviewOutput { text }),
                Ok(SubagentResult::Json(value)) => {
                    let pretty = serde_json::to_string_pretty(&value)
                        .map_err(|e| invalid(format!("review result serialization failed: {e}")))?;
                    Ok(ReviewOutput { text: pretty })
                }
                Err(e) => Err(invalid(e)),
            }
        })
    }
}

impl PortableTool for Review {
    const NAME: &'static str = "review";
    type Args = ReviewArgs;
    type Output = ReviewOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Spawn a code review subagent for the current working-tree changes (or a commit). Use \
         proactively after completing a feature, bugfix, refactor, or multi-file change. The \
         reviewer is read-only: it snapshots the diff with the argosy review tools, checks \
         styleguide rules, records prioritized findings (P0-P3), and returns a verdict with \
         per-priority counts."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(ReviewArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        self.execute(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockSpawn(Mutex<Vec<SubagentRequest>>);

    impl SpawnSubagent for MockSpawn {
        fn spawn(
            &self,
            req: SubagentRequest,
        ) -> crate::run::BoxFuture<std::result::Result<SubagentResult, String>> {
            self.0.lock().unwrap().push(req.clone());
            Box::pin(async move {
                Ok(SubagentResult::Text(format!(
                    "verdict for: {}",
                    req.subagent_type
                )))
            })
        }
    }

    async fn run(args: ReviewArgs) -> (String, SubagentRequest) {
        let mock: Arc<MockSpawn> = Arc::new(MockSpawn(Mutex::new(Vec::new())));
        let spawn: Arc<dyn SpawnSubagent> = mock.clone();
        let tool = Review(spawn);
        let out = tool.call(args).await.unwrap();
        let req = mock.0.lock().unwrap().remove(0);
        (out.text, req)
    }

    #[tokio::test]
    async fn spawns_a_strong_readonly_reviewer_with_focus() {
        let (text, req) = run(ReviewArgs {
            task: "the auth refactor".into(),
            focus_files: Some(vec!["src/auth.rs".into()]),
        })
        .await;
        assert!(text.contains("verdict for: reviewer"));
        assert_eq!(req.subagent_type, "reviewer");
        assert_eq!(req.model_tier.as_deref(), Some("strong"));
        assert!(req.prompt.contains("the auth refactor"));
        assert!(req.prompt.contains("src/auth.rs"));
    }
}
