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
