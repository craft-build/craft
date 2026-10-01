use rig_core::tool::PortableTool;
use serde_json::Value;
use tempfile::TempDir;

use super::*;

mod apply_patch;
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

/// One shared builtin table backs both registrations: the subagent's
/// restricted dispatch must be exactly the budget-filtered full table —
/// in particular no wildcard rescue of unlisted names.
#[test]
fn subagent_registration_is_the_budget_filtered_builtin_table() {
    let (_dir, workspace) = workspace();
    let full: Vec<String> = workspace.register().names();
    let research: Vec<String> = workspace.register_subagent(false).names();
    let general: Vec<String> = workspace.register_subagent(true).names();

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

async fn invoke<T>(tool: &T, args: Value) -> Result<T::Output>
where
    T: PortableTool<Error = ToolExecutionError>,
{
    tool.call(serde_json::from_value(args).unwrap()).await
}
