use std::fs;

use rig_core::tool::{IntoToolOutput, ToolErrorKind};
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn delete_reports_skips_and_requires_recursive_for_dirs() {
    let (_dir, workspace) = workspace();
    fs::create_dir(workspace.root().join("dir")).unwrap();
    fs::write(workspace.root().join("dir/file"), "keep").unwrap();
    let tool = Delete(workspace.clone());
    // Non-empty dir without recursive is skipped, not deleted.
    let error = invoke(&tool, json!({"files":["dir"]})).await.unwrap_err();
    assert!(error.to_string().contains("set recursive=true"));
    assert!(workspace.root().join("dir/file").exists());
    assert!(invoke(&tool, json!({"files":["."]})).await.is_err());
    // The workspace root itself is refused even with recursive=true.
    let error = invoke(&tool, json!({"files":["."], "recursive":true}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("workspace root"), "got: {error}");
    assert!(workspace.root().join("dir/file").exists());
    let output = invoke(&tool, json!({"files":["dir/file","missing"]}))
        .await
        .unwrap();
    assert_eq!(output.deleted, ["dir/file"]);
    assert_eq!(output.skipped, ["missing (not found)"]);
    // Nothing deleted, all missing -> NotFound.
    let error = invoke(&tool, json!({"files":["dir/file"]}))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::NotFound);
    // Recursive removes the tree and captures contents for /undo.
    fs::write(workspace.root().join("dir/file"), "keep").unwrap();
    let output = invoke(&tool, json!({"files":["dir"],"recursive":true}))
        .await
        .unwrap();
    assert_eq!(output.deleted, ["dir"]);
    assert!(!workspace.root().join("dir").exists());
    workspace.snapshots().rollback().await;
    assert_eq!(
        fs::read_to_string(workspace.root().join("dir/file")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn write_creates_overwrites_and_refuses_bad_targets() {
    let (_dir, workspace) = workspace();
    let tool = Write(workspace.clone());
    let output = invoke(
        &tool,
        json!({"path":"nested/dir/new.txt","content":"fresh\n"}),
    )
    .await
    .unwrap();
    assert!(output.created);
    assert_eq!(output.bytes_written, 6);
    assert_eq!(
        fs::read_to_string(workspace.root().join("nested/dir/new.txt")).unwrap(),
        "fresh\n"
    );
    let output = invoke(
        &tool,
        json!({"path":"nested/dir/new.txt","content":"replace"}),
    )
    .await
    .unwrap();
    assert!(!output.created);
    assert_eq!(
        fs::read_to_string(workspace.root().join("nested/dir/new.txt")).unwrap(),
        "replace"
    );
    fs::create_dir(workspace.root().join("dir")).unwrap();
    assert!(
        invoke(&tool, json!({"path":"../outside","content":"x"}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"path":".git/config","content":"x"}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"path":"dir","content":"x"}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"path":"ok","content":"\u{0}"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn glob_finds_matches_newest_first_and_respects_gitignore() {
    use std::time::{Duration, SystemTime};

    let (dir, workspace) = workspace();
    fs::write(dir.path().join("old.rs"), "").unwrap();
    fs::write(dir.path().join("new.rs"), "").unwrap();
    fs::create_dir_all(dir.path().join("nested")).unwrap();
    fs::write(dir.path().join("nested/deep.rs"), "").unwrap();
    fs::write(dir.path().join("ignored.rs"), "").unwrap();
    fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
    let older = SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(dir.path().join("old.rs"))
        .unwrap()
        .set_modified(older)
        .unwrap();

    let tool = Glob(workspace.clone());
    let output = invoke(&tool, json!({"pattern": "**/*.rs"})).await.unwrap();
    assert!(
        output.paths == vec!["nested/deep.rs", "new.rs", "old.rs"]
            || output.paths == vec!["new.rs", "nested/deep.rs", "old.rs"],
        "got: {:?}",
        output.paths
    );
    assert_eq!(output.paths.last(), Some(&"old.rs".to_string()));

    // A narrower path only reports matches under it.
    let output = invoke(&tool, json!({"pattern": "**/*.rs", "path": "nested"}))
        .await
        .unwrap();
    assert_eq!(output.paths, vec!["nested/deep.rs"]);
}

#[tokio::test]
async fn glob_reports_no_files_found_and_rejects_bad_input() {
    let (_dir, workspace) = workspace();
    let tool = Glob(workspace.clone());
    let output = invoke(&tool, json!({"pattern": "**/*.xyzzy"}))
        .await
        .unwrap();
    assert_eq!(
        output.into_tool_output().unwrap().as_text(),
        Some("No files found")
    );
    let error = invoke(&tool, json!({"pattern": "!"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("pattern must be a nonempty include glob"));
    fs::write(workspace.root().join("file.txt"), "").unwrap();
    assert!(
        invoke(&tool, json!({"pattern": "*", "path": "file.txt"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn list_sorts_dirs_first_and_hides_instruction_files() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("b.txt"), "").unwrap();
    fs::write(dir.path().join("a.rs"), "").unwrap();
    fs::create_dir(dir.path().join("zdir")).unwrap();
    fs::create_dir(dir.path().join("adir")).unwrap();
    fs::write(dir.path().join("AGENTS.md"), "rules").unwrap();

    let tool = List(workspace.clone());
    let output = invoke(&tool, json!({})).await.unwrap();
    assert_eq!(output.entries, vec!["adir/", "zdir/", "a.rs", "b.txt"]);
    assert!(output.instructions.is_empty());
}

#[tokio::test]
async fn list_injects_subdirectory_instructions_once() {
    let (dir, workspace) = workspace();
    fs::create_dir_all(dir.path().join("src/api")).unwrap();
    fs::write(dir.path().join("src/AGENTS.md"), "sub rules").unwrap();
    fs::write(dir.path().join("src/api/lib.rs"), "").unwrap();

    let tool = List(workspace.clone());
    let output = invoke(&tool, json!({"path": "src/api"})).await.unwrap();
    assert_eq!(output.instructions.len(), 1);
    assert!(output.instructions[0].0.ends_with("AGENTS.md"));
    let text = output
        .into_tool_output()
        .unwrap()
        .as_text()
        .unwrap()
        .to_string();
    assert!(text.ends_with("sub rules"));

    // Listing a sibling again does not repeat the instruction file.
    let output = invoke(&tool, json!({"path": "src/api"})).await.unwrap();
    assert!(output.instructions.is_empty());

    // Non-directory paths are rejected.
    assert!(
        invoke(&tool, json!({"path": "src/api/lib.rs"}))
            .await
            .is_err()
    );
}
