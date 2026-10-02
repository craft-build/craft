use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn todo_write_replaces_renders_and_clears() {
    let (_dir, workspace) = workspace();
    let tool = TodoWrite(workspace.clone());
    let output = invoke(
        &tool,
        json!({"todos": [
            {"id": "T1", "content": "first", "status": "completed", "owner": "scout"},
            {"id": "T1.1", "parent": "T1", "content": "nested", "status": "in_progress"},
            {"id": "T2", "content": "later", "status": "pending"}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(
        output.text,
        "T1 [✓] first (@scout)\n  T1.1 [•] nested\nT2 [ ] later"
    );

    let output = invoke(&tool, json!({"todos": []})).await.unwrap();
    assert_eq!(output.text, "Todos cleared");

    let error = invoke(
        &tool,
        json!({"todos": [{"id": "T1", "content": "x", "status": "done"}]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("status must be"), "got: {error}");
}

#[tokio::test]
async fn clear_todos_drops_the_previous_sessions_plan() {
    let (_dir, workspace) = workspace();
    let tool = TodoWrite(workspace.clone());
    invoke(
        &tool,
        json!({"todos": [{"id": "T1", "content": "plan", "status": "pending"}]}),
    )
    .await
    .unwrap();
    assert_eq!(workspace.todos().len(), 1);
    workspace.clear_todos();
    assert!(
        workspace.todos().is_empty(),
        "the todo store must not survive a session reset"
    );
}

#[tokio::test]
async fn list_tools_lists_and_details_registered_tools() {
    let (_dir, workspace) = workspace();
    let dispatch = workspace.register();
    let definitions: Vec<_> = dispatch
        .definitions()
        .into_iter()
        .filter(|definition| definition.name != "list_tools")
        .collect();
    let tool = ListTools(std::sync::Arc::new(definitions));
    let output = invoke(&tool, json!({})).await.unwrap();
    assert!(
        output.text.starts_with("Available tools:"),
        "got: {}",
        output.text
    );
    assert!(output.text.contains("- read: "), "got: {}", output.text);
    assert!(!output.text.contains("list_tools"), "got: {}", output.text);

    let output = invoke(&tool, json!({"detail": "read"})).await.unwrap();
    assert!(
        output.text.starts_with("read:\n\nInput schema:"),
        "got: {}",
        output.text
    );
    assert!(output.text.contains("\"path\""));

    let error = invoke(&tool, json!({"detail": "nope"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown tool: nope"), "got: {error}");
}

#[test]
fn plan_mode_sandbox_cell_is_read_only_with_plan_file_grant() {
    let (_dir, workspace) = workspace();
    let plan = _dir.path().join("plans").join("p.md");
    let cell = workspace.turn_sandbox_cell(&crate::run::AgentMode::Plan(plan.clone()), false);
    let state = cell.read().unwrap();
    assert_eq!(state.policy.mode, crate::sandbox::SandboxMode::ReadOnly);
    assert_eq!(state.policy.writable_roots, vec![plan]);
}

#[test]
fn build_mode_and_general_subagents_share_the_session_cell() {
    let (_dir, workspace) = workspace();
    let build_cell = workspace.turn_sandbox_cell(&crate::run::AgentMode::Build, false);
    let general_cell = workspace.turn_sandbox_cell(&crate::run::AgentMode::Build, false);
    assert!(
        std::sync::Arc::ptr_eq(&build_cell, &workspace.sandbox_cell()),
        "build turns share the session cell"
    );
    assert!(std::sync::Arc::ptr_eq(&general_cell, &build_cell));
}

#[test]
fn read_only_workers_get_a_frozen_read_only_cell() {
    let (_dir, workspace) = workspace();
    for cell in [workspace.turn_sandbox_cell(&crate::run::AgentMode::Build, true)] {
        let state = cell.read().unwrap();
        assert_eq!(state.policy.mode, crate::sandbox::SandboxMode::ReadOnly);
        assert!(state.policy.writable_roots.is_empty());
    }
    // The frozen cell must not observe later session policy installs.
    workspace.set_sandbox_policy(crate::sandbox::SandboxPolicy::off());
    let cell = workspace.turn_sandbox_cell(&crate::run::AgentMode::Build, true);
    let state = cell.read().unwrap();
    assert_eq!(state.policy.mode, crate::sandbox::SandboxMode::ReadOnly);
}
