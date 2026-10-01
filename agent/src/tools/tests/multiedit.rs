use std::fs;

use rig_core::tool::IntoToolOutput;
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn multiedit_entries_match_fuzzily() {
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join("f.py"),
        "def g():\n    if x:\n        foo()\n",
    )
    .unwrap();
    let tool = MultiEdit(workspace.clone());
    let output = invoke(
        &tool,
        json!({"path": "f.py", "edits": [
            {"old_string": "if x:\nfoo()", "new_string": "if x:\n    foo()\n    bar()"}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(
        output.into_tool_output().unwrap().as_text(),
        Some("applied 1 edit to f.py")
    );
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.py")).unwrap(),
        "def g():\n    if x:\n        foo()\n        bar()\n"
    );
}

#[tokio::test]
async fn multiedit_applies_edits_sequentially() {
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join("f.rs"),
        "fn alpha() {}\nfn beta() {}\n",
    )
    .unwrap();
    let tool = MultiEdit(workspace.clone());
    let output = invoke(
        &tool,
        json!({"path": "f.rs", "edits": [
            {"old_string": "fn alpha() {}", "new_string": "fn one() {}"},
            {"old_string": "fn beta() {}", "new_string": "fn two() {}", "replace_all": true}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(
        output.into_tool_output().unwrap().as_text(),
        Some("applied 2 edits to f.rs")
    );
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.rs")).unwrap(),
        "fn one() {}\nfn two() {}\n"
    );
}

#[tokio::test]
async fn multiedit_failure_leaves_file_unchanged_with_snippet() {
    let (_dir, workspace) = workspace();
    let original = "let a = 1;\n";
    fs::write(workspace.root().join("f.rs"), original).unwrap();
    let tool = MultiEdit(workspace.clone());
    let error = invoke(
        &tool,
        json!({"path": "f.rs", "edits": [
            {"old_string": "let a = 1;", "new_string": "let a = 9;"},
            {"old_string": "MISSING", "new_string": "x"}
        ]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("edits[1] (old_string \"MISSING\")"),
        "got: {error}"
    );
    assert!(error.contains("old_string not found in file"));
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.rs")).unwrap(),
        original
    );

    // A long first line is truncated to 32 chars plus an ellipsis.
    let long = "X".repeat(64);
    let error = invoke(
        &tool,
        json!({"path": "f.rs", "edits": [{"old_string": long, "new_string": "x"}]}),
    )
    .await
    .unwrap_err()
    .to_string();
    let snippet = &error[error.find("(old_string \"").unwrap() + 13..];
    let snippet = &snippet[..snippet.find("\")").unwrap()];
    assert!(snippet.ends_with('…'));
    assert_eq!(snippet.chars().count(), 33);
}

#[tokio::test]
async fn multiedit_rejects_empty_and_ambiguous_edits() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("f.rs"), "dup\ndup\n").unwrap();
    let tool = MultiEdit(workspace.clone());
    let error = invoke(&tool, json!({"path": "f.rs", "edits": []}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("provide at least one edit"));
    let error = invoke(
        &tool,
        json!({"path": "f.rs", "edits": [{"old_string": "dup", "new_string": "x"}]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("matches multiple locations"));
}
