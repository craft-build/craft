//! `task` tool (A.5): spawn a child agent (subagent) with its own context
//! window and a restricted tool set. Ported from the reference
//! `craft-agent/src/tools/task.rs`; the child run itself lives in
//! [`crate::subagent`] and hosts install it through the [`SpawnSubagent`]
//! seam (the `question` tool's pattern).

use std::sync::Arc;

use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::error::{Error, Result, definition, finish, invalid};
use crate::subagent::{SubagentRequest, SubagentResult};

/// Host seam: launch one subagent and run it to completion. The TUI and
/// headless surfaces install a [`crate::subagent::SubagentLauncher`] per
/// turn; the default ([`NoSubagents`]) reports that subagents are
/// unavailable, the same degradation the reference's hostless sessions
/// produce.
pub trait SpawnSubagent: Send + Sync {
    fn spawn(
        &self,
        req: SubagentRequest,
    ) -> crate::run::BoxFuture<std::result::Result<SubagentResult, String>>;
}

/// Default seam: no host installed, so the tool cannot run.
pub struct NoSubagents;

impl SpawnSubagent for NoSubagents {
    fn spawn(
        &self,
        _req: SubagentRequest,
    ) -> crate::run::BoxFuture<std::result::Result<SubagentResult, String>> {
        Box::pin(async { Err("task tool is not available in this session".to_string()) })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskArgs {
    /// Short (3-5 words) description of the task.
    pub description: String,
    /// Detailed task prompt for the agent.
    pub prompt: String,
    /// Subagent type: "research" (read-only, default), "general" (can
    /// modify files), or "reviewer" (read-only code reviewer with the
    /// argosy review tools).
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// Model tier (optional, omit to use the current model, capped at the
    /// current tier): "strong" (deep reasoning, ~5x cost), "medium"
    /// (balanced), or "weak" (fast/cheap: search, summarize, boilerplate).
    #[serde(default)]
    pub model_tier: Option<String>,
    /// Parent context to pass to the subagent: "none" (default: fresh),
    /// "summary" (last few parent messages), or "full".
    #[serde(default)]
    pub context_mode: Option<String>,
    /// Optional JSON Schema (object) describing the structured object the
    /// subagent must return as its final message. When set, the subagent is
    /// told to emit a final JSON object matching the schema; that object is
    /// validated and returned to you as structured data instead of prose.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// Isolation mode for a general subagent: "none" (default: run in the
    /// current working tree) or "worktree" (run inside a fresh linked git
    /// worktree so file mutations do not touch the parent tree). Requires
    /// a git repo; falls back to none otherwise.
    #[serde(default)]
    pub isolation: Option<String>,
}

#[derive(Debug)]
pub struct TaskOutput {
    pub text: String,
}

impl super::error::IntoToolResult for TaskOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(self.text)
    }
}

/// Launch a subagent through the session's [`SpawnSubagent`] seam.
#[derive(Clone)]
pub struct Task(pub Arc<dyn SpawnSubagent>);

impl Task {
    async fn execute(&self, context: &ToolContext<'_>, args: TaskArgs) -> Result<TaskOutput> {
        {
            let req = SubagentRequest {
                tool_use_id: context.call.id.clone(),
                description: args.description,
                prompt: args.prompt,
                subagent_type: args.subagent_type.unwrap_or_else(|| "research".into()),
                model_tier: args.model_tier,
                context_mode: args.context_mode.unwrap_or_else(|| "none".into()),
                output_schema: args.output_schema,
                isolation: args.isolation.unwrap_or_else(|| "none".into()),
            };
            match self.0.spawn(req).await {
                Ok(SubagentResult::Text(text)) => Ok(TaskOutput { text }),
                Ok(SubagentResult::Json(value)) => {
                    let pretty = serde_json::to_string_pretty(&value)
                        .map_err(|e| invalid(format!("schema result serialization failed: {e}")))?;
                    Ok(TaskOutput { text: pretty })
                }
                Err(e) => Err(invalid(e)),
            }
        }
    }
}

const TASK_DESCRIPTION: &str = "Launch an autonomous subagent to perform a task independently. Best combined with batch: \
launch multiple tasks concurrently. The subagent starts fresh (tell it everything it needs in \
the prompt; file paths and line numbers help more than prose) and its final message is its \
result, so ask for a complete, self-contained answer.";

#[async_trait]
impl ToolImpl for Task {
    type Args = TaskArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<TaskArgs>("task", TASK_DESCRIPTION)
    }

    // Exclusive: the child agent runs with its own tool registry and can
    // touch anything.

    async fn call(&self, context: &ToolContext<'_>, args: Self::Args) -> Result<ToolOutput> {
        finish(self.execute(context, args).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockSpawn {
        seen: Mutex<Vec<SubagentRequest>>,
        result: SubagentResult,
    }

    impl SpawnSubagent for MockSpawn {
        fn spawn(
            &self,
            req: SubagentRequest,
        ) -> crate::run::BoxFuture<std::result::Result<SubagentResult, String>> {
            self.seen.lock().unwrap().push(req);
            let result = match &self.result {
                SubagentResult::Text(t) => SubagentResult::Text(t.clone()),
                SubagentResult::Json(v) => SubagentResult::Json(v.clone()),
            };
            Box::pin(async move { Ok(result) })
        }
    }

    fn args() -> TaskArgs {
        TaskArgs {
            description: "Find auth middleware".into(),
            prompt: "Search for auth middleware.".into(),
            subagent_type: None,
            model_tier: None,
            context_mode: None,
            output_schema: None,
            isolation: None,
        }
    }

    fn mock(result: SubagentResult) -> (Task, Arc<MockSpawn>) {
        let mock = Arc::new(MockSpawn {
            seen: Mutex::new(Vec::new()),
            result,
        });
        (Task(mock.clone()), mock)
    }

    async fn invoke(task: &Task, args: TaskArgs) -> animist::agent::tool::ToolOutput {
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .build()
            .expect("harness");
        harness
            .call(
                task.clone(),
                serde_json::to_value(args_fields(&args)).unwrap(),
            )
            .await
            .expect("task outcome")
    }

    /// The wire shape of `TaskArgs`: `args()` builds the typed struct, this
    /// rebuilds its JSON (TaskArgs is Deserialize-only).
    fn args_fields(args: &TaskArgs) -> serde_json::Value {
        serde_json::json!({
            "description": args.description,
            "prompt": args.prompt,
            "subagent_type": args.subagent_type,
            "model_tier": args.model_tier,
            "context_mode": args.context_mode,
            "output_schema": args.output_schema,
            "isolation": args.isolation,
        })
    }

    fn text(output: &animist::agent::tool::ToolOutput) -> String {
        crate::history::ext::tool_content_text(&output.content)
    }

    #[tokio::test]
    async fn defaults_research_fresh_none_isolation() {
        let (task, mock) = mock(SubagentResult::Text("done".into()));
        let output = invoke(&task, args()).await;
        assert_eq!(text(&output), "done");
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].subagent_type, "research");
        assert_eq!(seen[0].context_mode, "none");
        assert_eq!(seen[0].isolation, "none");
        assert!(seen[0].model_tier.is_none());
    }

    #[tokio::test]
    async fn json_result_is_pretty_printed() {
        let (task, mock) = mock(SubagentResult::Json(serde_json::json!({"summary": "ok"})));
        let output = invoke(&task, args()).await;
        assert_eq!(text(&output), "{\n  \"summary\": \"ok\"\n}");
        assert_eq!(mock.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn seam_error_becomes_invalid_tool_result() {
        struct Failing;
        impl SpawnSubagent for Failing {
            fn spawn(
                &self,
                _req: SubagentRequest,
            ) -> crate::run::BoxFuture<std::result::Result<SubagentResult, String>> {
                Box::pin(async { Err("sub-agent error: boom".into()) })
            }
        }
        let task = Task(Arc::new(Failing));
        let output = invoke(&task, args()).await;
        assert!(output.is_error);
        assert!(text(&output).contains("sub-agent error: boom"));
    }

    #[tokio::test]
    async fn no_host_seam_reports_unavailable() {
        let task = Task(Arc::new(NoSubagents));
        let output = invoke(&task, args()).await;
        assert!(output.is_error);
        assert!(text(&output).contains("not available"));
    }
}
