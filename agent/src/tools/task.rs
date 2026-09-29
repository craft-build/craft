//! `task` tool (A.5): spawn a child agent (subagent) with its own context
//! window and a restricted tool set. Ported from the reference
//! `craft-agent/src/tools/task.rs`; the child run itself lives in
//! [`crate::subagent`] and hosts install it through the [`SpawnSubagent`]
//! seam (the `question` tool's pattern).

use std::sync::Arc;

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{Result, invalid};
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
    /// Subagent type: "research" (read-only, default) or "general" (can
    /// modify files).
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

impl IntoToolOutput for TaskOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Launch a subagent through the session's [`SpawnSubagent`] seam.
#[derive(Clone)]
pub struct Task(pub Arc<dyn SpawnSubagent>);

impl Task {
    fn execute(&self, args: TaskArgs) -> crate::run::BoxFuture<Result<TaskOutput>> {
        let spawn = Arc::clone(&self.0);
        Box::pin(async move {
            let req = SubagentRequest {
                description: args.description,
                prompt: args.prompt,
                subagent_type: args.subagent_type.unwrap_or_else(|| "research".into()),
                model_tier: args.model_tier,
                context_mode: args.context_mode.unwrap_or_else(|| "none".into()),
                output_schema: args.output_schema,
                isolation: args.isolation.unwrap_or_else(|| "none".into()),
            };
            match spawn.spawn(req).await {
                Ok(SubagentResult::Text(text)) => Ok(TaskOutput { text }),
                Ok(SubagentResult::Json(value)) => {
                    let pretty = serde_json::to_string_pretty(&value)
                        .map_err(|e| invalid(format!("schema result serialization failed: {e}")))?;
                    Ok(TaskOutput { text: pretty })
                }
                Err(e) => Err(invalid(e)),
            }
        })
    }
}

impl PortableTool for Task {
    const NAME: &'static str = "task";
    type Args = TaskArgs;
    type Output = TaskOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Launch an autonomous subagent to perform a task independently. Best combined with batch: \
launch multiple tasks concurrently. The subagent starts fresh (tell it everything it needs in \
the prompt; file paths and line numbers help more than prose) and its final message is its \
result, so ask for a complete, self-contained answer."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(TaskArgs)).expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        let fut = self.execute(args);
        fut.await
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

    #[tokio::test]
    async fn defaults_research_fresh_none_isolation() {
        let (task, mock) = mock(SubagentResult::Text("done".into()));
        let output = task.call(args()).await.unwrap();
        assert_eq!(output.text, "done");
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
        let output = task.call(args()).await.unwrap();
        assert_eq!(output.text, "{\n  \"summary\": \"ok\"\n}");
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
        let err = task.call(args()).await.unwrap_err();
        assert!(err.to_string().contains("sub-agent error: boom"));
    }

    #[tokio::test]
    async fn no_host_seam_reports_unavailable() {
        let task = Task(Arc::new(NoSubagents));
        let err = task.call(args()).await.unwrap_err();
        assert!(err.to_string().contains("not available"));
    }
}
