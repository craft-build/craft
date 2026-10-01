use std::fs;

use rig_core::tool::IntoToolOutput;
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn move_renames_and_rewrites_imports_project_wide() {
    let (_dir, workspace) = workspace();
    fs::create_dir_all(workspace.root().join("src/util")).unwrap();
    fs::write(
        workspace.root().join("src/util/old.rs"),
        "pub struct Helper;\n",
    )
    .unwrap();
    fs::write(
        workspace.root().join("src/main.rs"),
        "use util::old::Helper;\n// util::old mentioned in a comment stays\n",
    )
    .unwrap();
    fs::write(
        workspace.root().join("src/web.ts"),
        "import { util_old } from './util/old';\n",
    )
    .unwrap();

    let tool = MoveFile(workspace.clone());
    let out = invoke(
        &tool,
        json!({"source": "src/util/old.rs", "destination": "src/util/new.rs"}),
    )
    .await
    .unwrap();

    assert!(!workspace.root().join("src/util/old.rs").exists());
    assert!(workspace.root().join("src/util/new.rs").exists());
    assert_eq!(
        fs::read_to_string(workspace.root().join("src/main.rs")).unwrap(),
        "use util::new::Helper;\n// util::old mentioned in a comment stays\n"
    );
    assert_eq!(out.source, "src/util/old.rs");
    assert_eq!(out.destination, "src/util/new.rs");
    assert_eq!(out.import_updates, vec![("src/main.rs".to_string(), 1)]);
    assert_eq!(
        out.into_tool_output().unwrap().as_text(),
        Some(
            "moved src/util/old.rs -> src/util/new.rs\nupdated imports in 1 file(s)\n  src/main.rs: 1 reference(s)"
        )
    );
    // TS relative './util/old' is not a module path of the Rust-style mapping;
    // only import lines matching the module path are rewritten.
    assert!(
        fs::read_to_string(workspace.root().join("src/web.ts"))
            .unwrap()
            .contains("./util/old")
    );
}

#[tokio::test]
async fn move_missing_source_and_escapes_are_refused() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("a.txt"), "x").unwrap();
    let tool = MoveFile(workspace.clone());
    assert!(
        invoke(
            &tool,
            json!({"source": "missing.txt", "destination": "b.txt"})
        )
        .await
        .is_err()
    );
    let error = invoke(
        &tool,
        json!({"source": "a.txt", "destination": "../outside.txt"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("workspace") || error.contains("not allowed"),
        "{error}"
    );
}

#[tokio::test]
async fn move_directory_without_import_scan() {
    let (_dir, workspace) = workspace();
    fs::create_dir_all(workspace.root().join("pkg/sub")).unwrap();
    fs::write(workspace.root().join("pkg/sub/f.rs"), "x").unwrap();
    let out = invoke(
        &MoveFile(workspace.clone()),
        json!({"source": "pkg", "destination": "renamed"}),
    )
    .await
    .unwrap();
    assert!(workspace.root().join("renamed/sub/f.rs").exists());
    assert!(out.import_updates.is_empty());
}
