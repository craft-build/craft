//! `review` tool: spawn a read-only
//! reviewer subagent over the working-tree diff (or one committed revision),
//! or over the current state of named paths, built from argosy's reviewer
//! material. The reviewer records findings via the argosy review tools
//! (`start_review` / `review_diff` / `report_finding` / `review_findings`)
//! and returns a prioritized verdict.

use std::sync::Arc;

use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::error::{Error, Result, definition, finish, invalid};
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
    /// Repo-relative paths (files or directories) to review as current
    /// state, not as a diff: every tracked line under them is in scope.
    /// Set for a whole-area review of unchanged code; omit for a diff
    /// review of the working tree or a commit named in `task`.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct ReviewOutput {
    pub text: String,
}

impl super::error::IntoToolResult for ReviewOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(self.text)
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
    async fn execute(&self, context: &ToolContext<'_>, args: ReviewArgs) -> Result<ReviewOutput> {
        {
            let state_paths: Vec<String> = args
                .paths
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
            let mut prompt = String::from(
                "Report only verified defects, each with a priority (P0-P3), file:line, a \
                 concrete failure scenario, and a fix. Use search_rules to find the \
                 styleguide rules that govern the code. Finish with review_findings to \
                 audit your finding set, then end with a prioritized verdict.\n\n",
            );
            if state_paths.is_empty() {
                prompt.push_str(
                    "Review the current changes: start with start_review (cwd is this \
                     project's root) to snapshot the diff, then review_diff for the \
                     changed files.\n\nTask:\n",
                );
            } else {
                prompt.push_str(
                    "Review the current state of the paths below as a whole, not a diff: \
                     start with start_review (cwd is this project's root) with \
                     base \"empty\" and these paths — every tracked line counts as added, \
                     so any line validates for report_finding — then review_diff per \
                     file, reading surrounding code as needed.\n\nPaths:\n",
                );
                for path in &state_paths {
                    prompt.push_str("- ");
                    prompt.push_str(path);
                    prompt.push('\n');
                }
                prompt.push_str("\nTask:\n");
            }
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
                tool_use_id: context.call.id.clone(),
                description: "code review".into(),
                prompt,
                subagent_type: "reviewer".into(),
                model_tier: Some("strong".into()),
                context_mode: "none".into(),
                output_schema: None,
                isolation: "none".into(),
            };
            match self.0.spawn(req).await {
                Ok(SubagentResult::Text(text)) => Ok(ReviewOutput { text }),
                Ok(SubagentResult::Json(value)) => {
                    let pretty = serde_json::to_string_pretty(&value)
                        .map_err(|e| invalid(format!("review result serialization failed: {e}")))?;
                    Ok(ReviewOutput { text: pretty })
                }
                Err(e) => Err(invalid(e)),
            }
        }
    }
}

const REVIEW_DESCRIPTION: &str = "Spawn a code review subagent. Diff mode reviews the current \
working-tree changes (or a commit named in the task); state mode reviews the current code of \
the paths argument as a whole. Use proactively after completing a feature, bugfix, refactor, \
or multi-file change, or when asked to review an area. The reviewer is read-only: it \
snapshots the scope with the argosy review tools, checks styleguide rules, records \
prioritized findings (P0-P3), and returns a verdict with per-priority counts.";

#[async_trait]
impl ToolImpl for Review {
    type Args = ReviewArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<ReviewArgs>("review", REVIEW_DESCRIPTION)
    }

    // Same claim as `task`: sibling reviewers share a wave; the child's
    // own conversation claims its resources.
    fn scheduling(&self, _input: &serde_json::Value) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    async fn call(&self, context: &ToolContext<'_>, args: Self::Args) -> Result<ToolOutput> {
        finish(self.execute(context, args).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::NoSubagents;
    use std::sync::Mutex;

    struct MockSpawn(Mutex<Vec<SubagentRequest>>);

    impl SpawnSubagent for MockSpawn {
        fn spawn(
            &self,
            req: SubagentRequest,
        ) -> crate::harness::BoxFuture<std::result::Result<SubagentResult, String>> {
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
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .build()
            .expect("harness");
        let out = harness
            .call(
                tool,
                serde_json::json!({
                    "task": args.task,
                    "focus_files": args.focus_files,
                    "paths": args.paths,
                }),
            )
            .await
            .expect("review outcome");
        let req = mock.0.lock().unwrap().remove(0);
        (crate::history::ext::tool_content_text(&out.content), req)
    }

    #[tokio::test]
    async fn spawns_a_strong_readonly_reviewer_with_focus() {
        let (text, req) = run(ReviewArgs {
            task: "the auth refactor".into(),
            focus_files: Some(vec!["src/auth.rs".into()]),
            paths: None,
        })
        .await;
        assert!(text.contains("verdict for: reviewer"));
        assert_eq!(req.subagent_type, "reviewer");
        assert_eq!(req.model_tier.as_deref(), Some("strong"));
        assert!(req.prompt.contains("the auth refactor"));
        assert!(req.prompt.contains("src/auth.rs"));
        assert!(req.prompt.contains("snapshot the diff"));
    }

    #[tokio::test]
    async fn paths_switch_the_reviewer_to_whole_state_mode() {
        let (_text, req) = run(ReviewArgs {
            task: "audit the harness module".into(),
            focus_files: None,
            paths: Some(vec!["agent/src/harness".into()]),
        })
        .await;
        assert!(req.prompt.contains("base \"empty\""));
        assert!(req.prompt.contains("agent/src/harness"));
        assert!(req.prompt.contains("audit the harness module"));
        assert!(!req.prompt.contains("snapshot the diff"));
    }

    /// Same claim shape as `task`: resource-less, so sibling review launches
    /// share a wave.
    #[test]
    fn scheduling_lets_siblings_share_a_wave() {
        let tool = Review(Arc::new(NoSubagents));
        let claim = <Review as ToolImpl>::scheduling(&tool, &serde_json::json!({}));
        assert!(matches!(
            &claim,
            ToolScheduling::Resources { reads, writes } if reads.is_empty() && writes.is_empty()
        ));
    }
}
