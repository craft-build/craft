use std::fs;

use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn inspect_finds_todos_and_reports_none() {
    let (dir, workspace) = workspace();
    fs::write(
        dir.path().join("a.rs"),
        "fn main() {\n  // TODO: fix this\n}\n",
    )
    .unwrap();
    fs::write(dir.path().join("b.py"), "# FIXME: broken\npass\n").unwrap();
    let tool = Inspect(workspace.clone());
    let output = invoke(&tool, json!({"sections": "todos"})).await.unwrap();
    assert!(output.text.contains("(2 items)"), "got: {}", output.text);
    assert!(output.text.contains("a.rs:2: fix this"));
    assert!(output.text.contains("b.py:1: broken"));

    fs::remove_file(dir.path().join("a.rs")).unwrap();
    fs::remove_file(dir.path().join("b.py")).unwrap();
    fs::write(dir.path().join("clean.rs"), "fn main() {}\n").unwrap();
    let output = invoke(&tool, json!({"sections": "todos"})).await.unwrap();
    assert!(output.text.contains("todos: (none)"));
}

#[tokio::test]
async fn inspect_scopes_todos_to_one_file_and_truncates_previews() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("a.rs"), "// TODO: one\n").unwrap();
    fs::write(dir.path().join("b.rs"), "// TODO: two\n").unwrap();
    let tool = Inspect(workspace.clone());
    let output = invoke(&tool, json!({"sections": "todos", "scope": "a.rs"}))
        .await
        .unwrap();
    assert!(output.text.contains("(1 items)"), "got: {}", output.text);
    assert!(output.text.contains("one"));
    assert!(!output.text.contains("two"));

    fs::write(
        dir.path().join("c.rs"),
        format!("// TODO: {}\n", "x".repeat(100)),
    )
    .unwrap();
    let output = invoke(&tool, json!({"sections": "todos", "scope": "c.rs"}))
        .await
        .unwrap();
    assert!(output.text.contains("..."), "got: {}", output.text);
}

#[tokio::test]
async fn inspect_git_status_scopes_to_pathspec() {
    let (dir, workspace) = workspace();
    let root = dir.path();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("b.txt"), "b\n").unwrap();
    let tool = Inspect(workspace.clone());
    let output = invoke(&tool, json!({"sections": "git_status", "scope": "b.txt"}))
        .await
        .unwrap();
    assert!(output.text.contains("b.txt"), "got: {}", output.text);
    assert!(!output.text.contains("a.txt"), "got: {}", output.text);
}

#[tokio::test]
async fn inspect_git_status_degrades_outside_a_repo() {
    let (_dir, workspace) = workspace();
    let tool = Inspect(workspace.clone());
    let output = invoke(&tool, json!({"sections": "git_status"}))
        .await
        .unwrap();
    assert!(
        output.text.contains("not a git repo"),
        "got: {}",
        output.text
    );
}

#[tokio::test]
async fn inspect_rejects_unknown_sections() {
    let (_dir, workspace) = workspace();
    let tool = Inspect(workspace.clone());
    assert!(invoke(&tool, json!({"sections": "nope"})).await.is_err());
}

#[tokio::test]
async fn inspect_git_status_uses_repo_relative_pathspec_from_subdir_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(
        std::process::Command::new("git")
            .arg("init")
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("sub/b.txt"), "b\n").unwrap();

    let workspace = Workspace::new(root.join("sub")).unwrap();
    let tool = Inspect(workspace.clone());
    let output = invoke(&tool, json!({"sections": "git_status"}))
        .await
        .unwrap();
    assert!(output.text.contains("sub"), "got: {}", output.text);
    assert!(!output.text.contains("a.txt"), "got: {}", output.text);
}
