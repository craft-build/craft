use serde_json::Value;
use tempfile::TempDir;

use super::*;

mod apply_patch;
mod claims;
mod edit;
mod files;
mod grep;
mod integration;
mod mcp;
mod meta;
mod move_file;
mod multiedit;
mod read;
mod view_image;

pub(crate) fn workspace() -> (TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    (dir, workspace)
}

/// The registered names of a registry, in registry (alphabetical) order.
pub(crate) fn names(registry: &animist::agent::tool::ToolRegistry) -> Vec<String> {
    registry
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

/// One shared builtin registry backs the subagent budgets: the restricted
/// set must be exactly the budget-filtered full registry — in particular
/// no wildcard rescue of unlisted names.
#[test]
fn subagent_registration_is_the_budget_filtered_builtin_table() {
    let (_dir, workspace) = workspace();
    let full = names(&workspace.registry().unwrap());
    let research = names(&workspace.subagent_registry(false).unwrap());
    let general = names(&workspace.subagent_registry(true).unwrap());

    for name in &research {
        assert!(full.contains(name), "subagent-only tool {name}");
        // Budget keys name it `move_file`; the tool's wire name is `move`.
        let key = if name == "move" {
            "move_file"
        } else {
            name.as_str()
        };
        assert!(
            crate::subagent::RESEARCH_TOOLS.contains(&key) || name == "list_tools",
            "{name} escaped the research budget"
        );
    }
    for name in &general {
        let key = if name == "move" {
            "move_file"
        } else {
            name.as_str()
        };
        assert!(
            crate::subagent::GENERAL_TOOLS.contains(&key) || name == "list_tools",
            "{name} escaped the general budget"
        );
    }
    // The full-only companions drop out of subagent registrations.
    for banned in ["task", "question", "sessions"] {
        assert!(!research.contains(&banned.to_string()));
        assert!(!general.contains(&banned.to_string()));
        assert!(full.contains(&banned.to_string()));
    }
    // Both budgets include the write family only in `general`.
    assert!(!research.contains(&"write".to_string()));
    assert!(general.contains(&"write".to_string()));
}

/// Direct tool invocation through animist's `ToolHarness`, mirroring the
/// agent's typed decode + output conversion. Returns the tool's outcome,
/// error results included (assert on [`output_text`] / `is_error`).
pub(crate) async fn invoke<T>(tool: &T, args: Value) -> animist::agent::tool::ToolOutput
where
    T: animist::agent::tool::ToolImpl<
            Output = animist::agent::tool::ToolOutput,
            Error = crate::tools::error::Error,
        > + Clone,
{
    let mut harness = animist::agent::testing::ToolHarness::builder()
        .build()
        .expect("tool harness");
    harness
        .call(tool.clone(), args)
        .await
        .expect("tool outcome (error results arrive as outputs)")
}

/// Invoke and assert an error result, returning its model-visible text.
pub(crate) async fn invoke_error<T>(tool: &T, args: Value) -> String
where
    T: animist::agent::tool::ToolImpl<
            Output = animist::agent::tool::ToolOutput,
            Error = crate::tools::error::Error,
        > + Clone,
{
    let output = invoke(tool, args).await;
    assert!(output.is_error, "expected an error result");
    output_text(&output)
}

/// The model-visible text of a tool outcome.
pub(crate) fn output_text(output: &animist::agent::tool::ToolOutput) -> String {
    crate::history::ext::tool_content_text(&output.content)
}
