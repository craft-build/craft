mod adapters;
mod bridge;
pub mod catalog;
mod convert;
mod stubs;
mod worker;

use std::{collections::BTreeMap, sync::Arc};

use animist::agent::tool::{ToolContext, ToolDefinition, ToolImpl, ToolOutput};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::{config::CodemodeConfig, tools::error};

#[derive(Clone)]
pub struct Code {
    catalog: Arc<catalog::Catalog>,
    config: CodemodeConfig,
}

impl Code {
    pub fn new(catalog: catalog::Catalog, config: CodemodeConfig) -> Self {
        Self {
            catalog: Arc::new(catalog),
            config,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeArgs {
    /// Python source. Await every tool call; print results or end with an expression.
    pub code: String,
    /// Interpreter execution limit in seconds (maximum 120).
    pub timeout_secs: Option<u32>,
}

#[async_trait]
impl ToolImpl for Code {
    type Args = CodeArgs;
    type Output = ToolOutput;
    type Error = error::Error;

    fn definition(&self) -> ToolDefinition {
        let mut description = crate::prompt::CODEMODE_TOOL_GUIDANCE.to_owned();
        description.push_str("\n\nAvailable Python functions:\n```python\n");
        description.push_str(&self.catalog.stubs());
        description.push_str("```\n\nNot callable here: ");
        let uncallable = self.catalog.uncallable_names();
        if uncallable.is_empty() {
            description.push_str("none");
        } else {
            description.push_str(&uncallable.join(", "));
        }
        error::definition::<CodeArgs>("code", &description)
    }

    fn metadata(&self) -> BTreeMap<String, Value> {
        catalog::model_only_metadata()
    }

    async fn call(&self, context: &ToolContext<'_>, args: Self::Args) -> error::Result<ToolOutput> {
        let timeout_secs = args
            .timeout_secs
            .map_or(self.config.timeout_secs, u64::from);
        if timeout_secs == 0 || timeout_secs > 120 {
            return Ok(error::error_result(
                "timeout_secs must be between 1 and 120",
            ));
        }
        let result = bridge::run(context, &self.catalog, self.config, args.code, timeout_secs)
            .await
            .map_err(error::failure);
        error::finish(result.map(error::text_output))
    }
}

#[cfg(test)]
mod tests {
    use animist::{agent::testing::ToolHarness, history::History};
    use serde_json::json;

    use super::*;
    use crate::{config::CodemodeMode, tools::Workspace};

    async fn execute(dir: &tempfile::TempDir, source: &str) -> ToolOutput {
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_codemode(CodemodeConfig {
                mode: CodemodeMode::Alongside,
                wall_clock_secs: 5,
                ..Default::default()
            });
        let registry = workspace.registry(&History::new()).unwrap();
        let catalog = catalog::Catalog::from_registry(&registry);
        let mut harness = ToolHarness::builder().tools(registry).build().unwrap();
        harness
            .call(
                Code::new(catalog, workspace.codemode()),
                json!({"code": source}),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn direct_aliased_and_gathered_calls_execute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        let output = execute(
            &dir,
            "import asyncio\nreader = read_text\nvalues = await asyncio.gather(reader('a.txt'), read_text(path='b.txt'))\nprint(values)",
        )
        .await;
        assert!(!output.is_error, "{:?}", output.content);
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            "['alpha\\n', 'beta\\n']\n"
        );
    }

    #[tokio::test]
    async fn child_errors_are_catchable_tool_errors() {
        let dir = tempfile::tempdir().unwrap();
        let output = execute(
            &dir,
            "try:\n    await read_text('missing.txt')\nexcept ToolError as error:\n    print('caught')\nelse:\n    print('missed')",
        )
        .await;
        assert!(!output.is_error, "{:?}", output.content);
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            "caught\n"
        );
    }

    #[tokio::test]
    async fn missing_await_does_not_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        let output = execute(&dir, "read_text('a.txt').strip()").await;
        assert!(output.is_error, "{:?}", output.content);
        assert!(
            crate::history::ext::tool_content_text(&output.content).contains("Runtime"),
            "{:?}",
            output.content
        );
    }

    #[tokio::test]
    async fn output_over_the_configured_cap_fails_correctably() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_codemode(CodemodeConfig {
                mode: CodemodeMode::Alongside,
                wall_clock_secs: 5,
                max_output_kib: 1,
                ..Default::default()
            });
        let registry = workspace.registry(&History::new()).unwrap();
        let catalog = catalog::Catalog::from_registry(&registry);
        let mut harness = ToolHarness::builder().tools(registry).build().unwrap();
        let output = harness
            .call(
                Code::new(catalog, workspace.codemode()),
                json!({"code": "print('x' * 2048)"}),
            )
            .await
            .unwrap();
        assert!(output.is_error, "{:?}", output.content);
        assert!(
            crate::history::ext::tool_content_text(&output.content).contains("Limit"),
            "{:?}",
            output.content
        );
    }

    #[tokio::test]
    async fn list_tools_is_available_to_scripts_with_full_schemas() {
        let dir = tempfile::tempdir().unwrap();
        let output = execute(
            &dir,
            "tools = await list_tools()\nprint('read_text' in tools)",
        )
        .await;
        assert!(!output.is_error, "{:?}", output.content);
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            "True\n"
        );
    }

    #[tokio::test]
    async fn tool_error_alias_never_rewrites_printed_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let output = execute(&dir, "print('ToolError')").await;
        assert_eq!(
            crate::history::ext::tool_content_text(&output.content),
            "ToolError\n"
        );
    }

    #[tokio::test]
    async fn nested_call_cap_is_enforced_before_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            std::fs::write(dir.path().join(format!("file-{i}.txt")), "text\n").unwrap();
        }
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_codemode(CodemodeConfig {
                mode: CodemodeMode::Alongside,
                wall_clock_secs: 5,
                max_tool_calls: 2,
                ..Default::default()
            });
        let registry = workspace.registry(&History::new()).unwrap();
        let catalog = catalog::Catalog::from_registry(&registry);
        let mut harness = ToolHarness::builder().tools(registry).build().unwrap();
        let output = harness
            .call(
                Code::new(catalog, workspace.codemode()),
                json!({
                    "code": "import asyncio\nawait asyncio.gather(read_text('file-0.txt'), read_text('file-1.txt'), read_text('file-2.txt'))"
                }),
            )
            .await
            .unwrap();
        assert!(output.is_error, "{:?}", output.content);
        assert!(
            crate::history::ext::tool_content_text(&output.content)
                .contains("nested tool-call cap 2"),
            "{:?}",
            output.content
        );
    }

    #[tokio::test]
    async fn interpreter_timeout_is_a_limit_error() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path())
            .unwrap()
            .with_codemode(CodemodeConfig {
                mode: CodemodeMode::Alongside,
                wall_clock_secs: 5,
                ..Default::default()
            });
        let registry = workspace.registry(&History::new()).unwrap();
        let catalog = catalog::Catalog::from_registry(&registry);
        let mut harness = ToolHarness::builder().tools(registry).build().unwrap();
        let output = harness
            .call(
                Code::new(catalog, workspace.codemode()),
                json!({
                    "code": "value = 0\nwhile True:\n    value += 1",
                    "timeout_secs": 1
                }),
            )
            .await
            .unwrap();
        assert!(output.is_error, "{:?}", output.content);
        let text = crate::history::ext::tool_content_text(&output.content);
        assert!(
            text.contains("Limit") || text.contains("time limit"),
            "{text}"
        );
    }
}
