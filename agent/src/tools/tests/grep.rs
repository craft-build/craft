use std::fs;

use serde_json::json;

use super::{invoke, invoke_error, output_text, workspace};
use crate::tools::*;

#[tokio::test]
async fn grep_text_groups_files_and_reports_partial_searches() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("a.rs"), "needle\n  needle again\n").unwrap();
    fs::write(workspace.root().join("b.rs"), "no\nneedle βeta\n").unwrap();
    let tool = Grep(workspace.clone());
    let full = invoke(&tool, json!({"pattern":"needle"})).await;
    assert_eq!(
        output_text(&full),
        "a.rs:\n  1: needle\n  2:   needle again\n\nb.rs:\n  2: needle βeta"
    );
    let empty = invoke(&tool, json!({"pattern":"missing"})).await;
    assert_eq!(output_text(&empty), "No files found");

    fs::write(workspace.root().join("0.binary"), [0, 255]).unwrap();
    let limited = invoke(&tool, json!({"pattern":"needle", "max_matches":1})).await;
    let text = output_text(&limited);
    assert!(text.starts_with("a.rs:\n  1: needle\n"), "{text}");
    assert!(
        text.contains("Search truncated: more matches may exist"),
        "{text}"
    );
    assert!(
        text.contains("Skipped 1 files; results cover only the files searched"),
        "{text}"
    );

    let empty_partial = invoke(&tool, json!({"pattern":"missing"})).await;
    assert_eq!(
        output_text(&empty_partial),
        "No files found\n\n[Skipped 1 files; results cover only the files searched.]"
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
    .await;
    assert_eq!(
        output_text(&result),
        "src/a.rs:\n  1: needle\n\nsrc/b.rs:\n  1: needle"
    );
    let result = invoke(
        &Grep(workspace),
        json!({"path":"src/a.rs","pattern":"needle"}),
    )
    .await;
    assert_eq!(output_text(&result), "src/a.rs:\n  1: needle");
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
    .await;
    let text = output_text(&result);
    assert!(
        text.starts_with("text:\n  1: Hello.*"),
        "literal case-insensitive match on line 1: {text}"
    );
    let result = invoke(&tool, json!({"pattern":"(?i)^hello","max_matches":1})).await;
    let text = output_text(&result);
    assert!(
        text.starts_with("text:\n  1: Hello.*"),
        "one match kept: {text}"
    );
    assert!(
        text.contains("Search truncated: more matches may exist"),
        "{text}"
    );
    let result = invoke(&tool, json!({"pattern":"not-present"})).await;
    assert!(!output_text(&result).contains("truncated"));
    for args in [
        json!({"pattern":"["}),
        json!({"pattern":"x","glob":"["}),
        json!({"pattern":"x","max_matches":0}),
    ] {
        assert!(!invoke_error(&tool, args).await.is_empty());
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
    let result = invoke(&Grep(workspace), json!({"pattern":"needle"})).await;
    let text = output_text(&result);
    assert!(text.contains("needle"), "{text}");
    assert!(
        text.ends_with(
            "[line truncated; excerpt starts at byte column 7745; match at byte column 8001]"
        ),
        "{text}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn grep_preserves_backslashes_in_filenames() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("back\\slash"), "needle").unwrap();
    let result = invoke(&Grep(workspace), json!({"pattern":"needle"})).await;
    assert_eq!(output_text(&result), "back\\slash:\n  1: needle");
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
    let result = invoke(&Grep(workspace), json!({"pattern":"needle"})).await;
    assert_eq!(
        output_text(&result),
        "No files found\n\n[Skipped 1 files; results cover only the files searched.]"
    );
}
