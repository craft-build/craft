use std::fs;

use rig_core::tool::IntoToolOutput;
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn apply_patch_updates_adds_and_deletes_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("f.txt"), "foo\nbar\n").unwrap();
    fs::write(workspace.root().join("old.txt"), "stale\n").unwrap();
    let tool = ApplyPatch(workspace.clone());
    let output = invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Update File: f.txt\n@@\n foo\n-bar\n+baz\n*** Add File: new.txt\n+Hello world\n*** Delete File: old.txt\n*** End Patch"}),
    )
    .await
    .unwrap();
    let rendered = output.into_tool_output().unwrap();
    let text = rendered.as_text().unwrap();
    assert!(text.contains("f.txt: modified"), "got: {text}");
    assert!(text.contains("new.txt: created"), "got: {text}");
    assert!(text.contains("old.txt: deleted"), "got: {text}");
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.txt")).unwrap(),
        "foo\nbaz\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.root().join("new.txt")).unwrap(),
        "Hello world\n"
    );
    assert!(!workspace.root().join("old.txt").exists());
}

#[tokio::test]
async fn apply_patch_handles_multiple_chunks_context_and_eof_append() {
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join("f.py"),
        "class Foo:\n    def bar(self):\n        pass\n    def baz(self):\n        pass\n",
    )
    .unwrap();
    let tool = ApplyPatch(workspace.clone());
    invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Update File: f.py\n@@ def baz(self):\n-        pass\n+        return 42\n@@\n+tail\n*** End of File\n*** End Patch"}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.py")).unwrap(),
        "class Foo:\n    def bar(self):\n        pass\n    def baz(self):\n        return 42\ntail\n"
    );
}

#[tokio::test]
async fn apply_patch_fuzzy_matches_whitespace_and_reports_missing_lines() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("f.txt"), "foo   \nbar\t\n").unwrap();
    let tool = ApplyPatch(workspace.clone());
    invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Update File: f.txt\n@@\n foo\n-bar\n+BAR\n*** End Patch"}),
    )
    .await
    .unwrap();
    // Reference semantics: a fuzzily matched context line is rewritten to
    // the patch's version of it.
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.txt")).unwrap(),
        "foo\nBAR\n"
    );

    let error = invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Update File: f.txt\n@@\n-nope\n+yes\n*** End Patch"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Failed to find expected lines"), "{error}");
    // The failed update must not have modified the file.
    assert_eq!(
        fs::read_to_string(workspace.root().join("f.txt")).unwrap(),
        "foo\nBAR\n"
    );
}

#[tokio::test]
async fn apply_patch_rejects_bad_input_and_escapes() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("f.txt"), "x\n").unwrap();
    let tool = ApplyPatch(workspace);

    let error = invoke(&tool, json!({"patch_text": "random text"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("*** Begin Patch"), "{error}");

    let error = invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Update File: missing.txt\n@@\n-a\n+b\n*** End Patch"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("No such file") || error.contains("missing.txt"),
        "{error}"
    );

    let error = invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Add File: f.txt\n+duplicate\n*** End Patch"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("already exists"), "{error}");

    let error = invoke(
        &tool,
        json!({"patch_text": "*** Begin Patch\n*** Add File: ../escape.txt\n+no\n*** End Patch"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("workspace") || error.contains("not allowed"),
        "{error}"
    );
}
