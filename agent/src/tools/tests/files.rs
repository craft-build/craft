use std::fs;

use serde_json::json;

use super::{invoke, invoke_error, output_text, workspace};
use crate::tools::*;

#[tokio::test]
async fn delete_reports_skips_and_requires_recursive_for_dirs() {
    let (_dir, workspace) = workspace();
    fs::create_dir(workspace.root().join("dir")).unwrap();
    fs::write(workspace.root().join("dir/file"), "keep").unwrap();
    let tool = Delete(workspace.clone());
    // Non-empty dir without recursive is skipped, not deleted.
    let error = invoke_error(&tool, json!({"files":["dir"]})).await;
    assert!(error.contains("set recursive=true"), "{error}");
    assert!(workspace.root().join("dir/file").exists());
    assert!(
        invoke_error(&tool, json!({"files":["."], "recursive":true}))
            .await
            .contains("workspace root")
    );
    // The workspace root itself is refused even with recursive=true.
    assert!(workspace.root().join("dir/file").exists());
    let output = invoke(&tool, json!({"files":["dir/file","missing"]})).await;
    let text = output_text(&output);
    assert!(!output.is_error, "{text}");
    assert!(text.contains("deleted: dir/file"), "{text}");
    assert!(text.contains("skipped: missing (not found)"), "{text}");
    // Nothing deleted, all missing -> NotFound text.
    let error = invoke_error(&tool, json!({"files":["dir/file"]})).await;
    assert!(
        error.to_lowercase().contains("no such file") || error.contains("not found"),
        "not-found text: {error}"
    );
    // Recursive removes the tree and captures contents for /undo.
    fs::write(workspace.root().join("dir/file"), "keep").unwrap();
    let output = invoke(&tool, json!({"files":["dir"],"recursive":true})).await;
    assert!(output_text(&output).contains("deleted: dir"));
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
    .await;
    assert!(output_text(&output).contains("wrote nested/dir/new.txt (6 bytes)"));
    assert_eq!(
        fs::read_to_string(workspace.root().join("nested/dir/new.txt")).unwrap(),
        "fresh\n"
    );
    let output = invoke(
        &tool,
        json!({"path":"nested/dir/new.txt","content":"replace"}),
    )
    .await;
    assert!(output_text(&output).contains("overwrote nested/dir/new.txt"));
    assert_eq!(
        fs::read_to_string(workspace.root().join("nested/dir/new.txt")).unwrap(),
        "replace"
    );
    fs::create_dir(workspace.root().join("dir")).unwrap();
    assert!(
        invoke_error(&tool, json!({"path":"../outside","content":"x"}))
            .await
            .contains("not allowed")
    );
    assert!(
        invoke_error(&tool, json!({"path":".git/config","content":"x"}))
            .await
            .contains("Git metadata")
    );
    assert!(
        invoke_error(&tool, json!({"path":"dir","content":"x"}))
            .await
            .contains("regular file")
    );
    assert!(
        invoke_error(&tool, json!({"path":"ok","content":"\u{0}"}))
            .await
            .contains("NUL")
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
    let output = invoke(&tool, json!({"pattern": "**/*.rs"})).await;
    let text = output_text(&output);
    let mut paths: Vec<&str> = text.lines().collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["nested/deep.rs", "new.rs", "old.rs"], "{text}");
    assert!(text.ends_with("old.rs"), "oldest last: {text}");

    // A narrower path only reports matches under it.
    let output = invoke(&tool, json!({"pattern": "**/*.rs", "path": "nested"})).await;
    assert_eq!(output_text(&output), "nested/deep.rs");
}

#[tokio::test]
async fn glob_reports_no_files_found_and_rejects_bad_input() {
    let (_dir, workspace) = workspace();
    let tool = Glob(workspace.clone());
    let output = invoke(&tool, json!({"pattern": "**/*.xyzzy"})).await;
    assert_eq!(output_text(&output), "No files found");
    let error = invoke_error(&tool, json!({"pattern": "!"})).await;
    assert!(error.contains("pattern must be a nonempty include glob"));
    fs::write(workspace.root().join("file.txt"), "").unwrap();
    assert!(
        invoke_error(&tool, json!({"pattern": "*", "path": "file.txt"}))
            .await
            .contains("directory")
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
    let output = invoke(&tool, json!({})).await;
    assert_eq!(output_text(&output), "adir/\nzdir/\na.rs\nb.txt");
}

#[tokio::test]
async fn list_injects_subdirectory_instructions_once() {
    let (dir, workspace) = workspace();
    fs::create_dir_all(dir.path().join("src/api")).unwrap();
    fs::write(dir.path().join("src/AGENTS.md"), "sub rules").unwrap();
    fs::write(dir.path().join("src/api/lib.rs"), "").unwrap();

    let tool = List(workspace.clone());
    let output = invoke(&tool, json!({"path": "src/api"})).await;
    let text = output_text(&output);
    assert!(text.contains("Instructions from:"), "{text}");
    assert!(text.ends_with("sub rules"), "{text}");

    // Listing a sibling again does not repeat the instruction file.
    let output = invoke(&tool, json!({"path": "src/api"})).await;
    assert!(!output_text(&output).contains("Instructions from:"));

    // Non-directory paths are rejected.
    assert!(
        invoke_error(&tool, json!({"path": "src/api/lib.rs"}))
            .await
            .contains("directory")
    );
}
