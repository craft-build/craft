use std::fs;

use rig_core::tool::IntoToolOutput;
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn grep_text_groups_files_and_reports_partial_searches() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("a.rs"), "needle\n  needle again\n").unwrap();
    fs::write(workspace.root().join("b.rs"), "no\nneedle βeta\n").unwrap();
    let tool = Grep(workspace.clone());
    let full = invoke(&tool, json!({"pattern":"needle"})).await.unwrap();
    assert_eq!(
        full.into_tool_output().unwrap().as_text(),
        Some("a.rs:\n  1: needle\n  2:   needle again\n\nb.rs:\n  2: needle βeta")
    );
    let empty = invoke(&tool, json!({"pattern":"missing"})).await.unwrap();
    assert_eq!(
        empty.into_tool_output().unwrap().as_text(),
        Some("No files found")
    );

    fs::write(workspace.root().join("0.binary"), [0, 255]).unwrap();
    let limited = invoke(&tool, json!({"pattern":"needle", "max_matches":1}))
        .await
        .unwrap()
        .into_tool_output()
        .unwrap();
    let text = limited.as_text().unwrap();
    assert!(text.starts_with("a.rs:\n  1: needle\n"));
    assert!(text.contains("Search truncated: more matches may exist"));
    assert!(text.contains("Skipped 1 files; results cover only the files searched"));

    let empty_partial = invoke(&tool, json!({"pattern":"missing"}))
        .await
        .unwrap()
        .into_tool_output()
        .unwrap();
    assert_eq!(
        empty_partial.as_text(),
        Some("No files found\n\n[Skipped 1 files; results cover only the files searched.]")
    );
}

#[tokio::test]
async fn grep_obeys_ignore_rules_even_with_globs_and_narrowed_paths() {
    let (_dir, workspace) = workspace();
    fs::create_dir(workspace.root().join("src")).unwrap();
    fs::create_dir(workspace.root().join(".git")).unwrap();
    fs::write(workspace.root().join(".gitignore"), "*.generated.rs\n").unwrap();
    for path in [
        "src/a.rs",
        "src/b.rs",
        "src/ignored.generated.rs",
        "src/.hidden.rs",
        ".git/config",
    ] {
        fs::write(workspace.root().join(path), "needle\n").unwrap();
    }
    fs::write(workspace.root().join("src/c.txt"), "needle\n").unwrap();
    let result = invoke(
        &Grep(workspace.clone()),
        json!({
            "path":"src", "pattern":"needle", "glob":"*.rs"
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        result
            .matches
            .iter()
            .map(|m| m.path.as_str())
            .collect::<Vec<_>>(),
        ["src/a.rs", "src/b.rs"]
    );
    assert!(!result.truncated);
    let result = invoke(
        &Grep(workspace),
        json!({"path":"src/a.rs","pattern":"needle"}),
    )
    .await
    .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line, 1);
}

#[tokio::test]
async fn grep_supports_regex_literal_case_and_limits() {
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join("text"),
        "Hello.*\nhello world\nunrelated",
    )
    .unwrap();
    fs::write(workspace.root().join("binary"), [0, 255]).unwrap();
    let tool = Grep(workspace);
    let result = invoke(
        &tool,
        json!({"pattern":"hello.*","literal":true,"case_sensitive":false}),
    )
    .await
    .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].line, 1);
    assert_eq!(result.skipped_files, 1);
    let result = invoke(&tool, json!({"pattern":"(?i)^hello","max_matches":1}))
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert!(result.truncated);
    let result = invoke(&tool, json!({"pattern":"not-present"}))
        .await
        .unwrap();
    assert!(result.matches.is_empty());
    assert!(!result.truncated);
    for args in [
        json!({"pattern":"["}),
        json!({"pattern":"x","glob":"["}),
        json!({"pattern":"x","max_matches":0}),
    ] {
        assert!(invoke(&tool, args).await.is_err());
    }
}

#[tokio::test]
async fn grep_long_line_excerpt_contains_the_match() {
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join("long"),
        format!("{}needle", "🦀".repeat(2000)),
    )
    .unwrap();
    let result = invoke(&Grep(workspace), json!({"pattern":"needle"}))
        .await
        .unwrap();
    let found = &result.matches[0];
    assert!(found.text.contains("needle"));
    assert_eq!(found.column, 8001);
    assert!(found.text_start_column > 1);
    assert!(found.truncated);
    assert!(found.text.len() <= MAX_LINE_BYTES);
    let start = found.text_start_column;
    let rendered = result.into_tool_output().unwrap();
    let text = rendered.as_text().unwrap();
    assert!(text.contains("needle"));
    assert!(text.ends_with(&format!(
        "[line truncated; excerpt starts at byte column {start}; match at byte column 8001]"
    )));
}

#[cfg(unix)]
#[tokio::test]
async fn grep_preserves_backslashes_in_filenames() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("back\\slash"), "needle").unwrap();
    let result = invoke(&Grep(workspace), json!({"pattern":"needle"}))
        .await
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].path, "back\\slash");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn grep_skips_non_unicode_names() {
    // macOS filesystems reject these names at creation time.
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let (_dir, workspace) = workspace();
    fs::write(
        workspace.root().join(OsString::from_vec(vec![0xff])),
        "needle",
    )
    .unwrap();
    let result = invoke(&Grep(workspace), json!({"pattern":"needle"}))
        .await
        .unwrap();
    assert!(result.matches.is_empty());
    assert_eq!(result.skipped_files, 1);
}
