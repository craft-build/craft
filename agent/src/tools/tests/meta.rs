use std::sync::Arc;

use serde_json::json;

use super::{invoke, invoke_error, output_text, workspace};
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
    .await;
    assert_eq!(
        output_text(&output),
        "T1 [✓] first (@scout)\n  T1.1 [•] nested\nT2 [ ] later"
    );

    let output = invoke(&tool, json!({"todos": []})).await;
    assert_eq!(output_text(&output), "Todos cleared");

    let error = invoke_error(
        &tool,
        json!({"todos": [{"id": "T1", "content": "x", "status": "done"}]}),
    )
    .await;
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
    .await;
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
    let definitions: Vec<_> = workspace
        .registry(&animist::history::History::new())
        .unwrap()
        .definitions()
        .into_iter()
        .filter(|definition| definition.name != "list_tools")
        .collect();
    let tool = ListTools(Arc::new(definitions));
    let output = invoke(&tool, json!({})).await;
    let text = output_text(&output);
    assert!(text.starts_with("Available tools:"), "got: {text}");
    assert!(text.contains("- read: "), "got: {text}");
    assert!(!text.contains("list_tools"), "got: {text}");

    let output = invoke(&tool, json!({"detail": "read"})).await;
    let text = output_text(&output);
    assert!(text.starts_with("read:\n\nInput schema:"), "got: {text}");
    assert!(text.contains("\"path\""));

    let error = invoke_error(&tool, json!({"detail": "nope"})).await;
    assert!(error.contains("unknown tool: nope"), "got: {error}");
}

#[test]
fn read_only_workers_get_a_frozen_read_only_cell() {
    let (_dir, workspace) = workspace();
    let cell = workspace.frozen_read_only_cell();
    let state = cell.read().unwrap();
    assert_eq!(state.policy.mode, crate::sandbox::SandboxMode::ReadOnly);
    assert!(state.policy.writable_roots.is_empty());
    // The frozen cell must not observe later session policy installs.
    workspace.set_sandbox_policy(crate::sandbox::SandboxPolicy::off());
    let cell = workspace.frozen_read_only_cell();
    let state = cell.read().unwrap();
    assert_eq!(state.policy.mode, crate::sandbox::SandboxMode::ReadOnly);
}
