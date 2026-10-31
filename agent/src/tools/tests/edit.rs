use std::fs;

use serde_json::json;

use super::{invoke, invoke_error, output_text, workspace};
use crate::tools::*;

#[tokio::test]
async fn edit_requires_unambiguous_matches_and_preserves_bytes() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("file");
    fs::write(&path, "\u{feff}α\r\nsame\r\nsame\r\n").unwrap();
    let tool = Edit(workspace);
    assert!(
        invoke_error(
            &tool,
            json!({"path":"file","old_string":"same","new_string":"other"})
        )
        .await
        .contains("matches")
    );
    assert!(
        invoke_error(
            &tool,
            json!({"path":"file","old_string":"missing","new_string":"other"})
        )
        .await
        .contains("not found")
    );
    assert!(
        invoke_error(
            &tool,
            json!({"path":"file","old_string":"","new_string":"other"})
        )
        .await
        .contains("must not be empty")
    );
    assert!(
        !invoke_error(
            &tool,
            json!({"path":"file","old_string":"same","new_string":"other","occurrence":0})
        )
        .await
        .is_empty()
    );
    assert!(invoke_error(
        &tool,
        json!({"path":"file","old_string":"same","new_string":"other","occurrence":1,"replace_all":true})
    )
    .await
    .contains("mutually exclusive"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\r\nsame\r\nsame\r\n"
    );
    let result = invoke(
        &tool,
        json!({"path":"file","old_string":"same","new_string":"other","occurrence":2}),
    )
    .await;
    assert!(
        output_text(&result).starts_with("edited file\n"),
        "{}",
        output_text(&result)
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\r\nsame\r\nother\r\n"
    );
    let result = invoke(
        &tool,
        json!({"path":"file","old_string":"e","new_string":"E","replace_all":true}),
    )
    .await;
    let text = output_text(&result);
    assert!(text.starts_with("edited file"), "{text}");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\r\nsamE\r\nothEr\r\n"
    );
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "staging file leaked"
    );
}

#[tokio::test]
async fn edit_fuzzy_passes_tolerate_model_drift_and_report_the_pass() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("f.py");
    let tool = Edit(workspace.clone());

    // Indentation drift: the engine rebases the replacement into the file's frame.
    fs::write(&path, "def f():\n    if x:\n        a()\n    return 1\n").unwrap();
    let result = invoke(
        &tool,
        json!({"path":"f.py","old_string":"if x:\na()","new_string":"if x:\n    if y:\n        a()"}),
    )
    .await;
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "def f():\n    if x:\n        if y:\n            a()\n    return 1\n"
    );
    assert_eq!(
        output_text(&result),
        "edited f.py (fuzzy match pass 2)\n--- f.py\n+++ f.py\n@@ -1 +1 @@\n  def f():\n      if x:\n-         a()\n+         if y:\n+             a()\n      return 1"
    );

    // Whitespace collapse.
    fs::write(&path, "let   x  =   1;\n").unwrap();
    invoke(
        &tool,
        json!({"path":"f.py","old_string":"let x = 1;","new_string":"let y = 2;"}),
    )
    .await;
    assert_eq!(fs::read_to_string(&path).unwrap(), "let y = 2;\n");

    // Escaped quotes in old_string/new_string are unescaped before matching.
    fs::write(&path, "print(\"hello\")\n").unwrap();
    let escaped_old = "print(\\\"hello\\\")";
    let escaped_new = "print(\\\"world\\\")";
    invoke(
        &tool,
        json!({"path":"f.py","old_string": escaped_old, "new_string": escaped_new}),
    )
    .await;
    assert_eq!(fs::read_to_string(&path).unwrap(), "print(\"world\")\n");

    // Unicode NFKD: FULLWIDTH LATIN CAPITAL LETTER A matches "A".
    fs::write(&path, "let \u{ff21} = 1;\n").unwrap();
    invoke(
        &tool,
        json!({"path":"f.py","old_string":"let A = 1;","new_string":"let B = 2;"}),
    )
    .await;
    assert_eq!(fs::read_to_string(&path).unwrap(), "let B = 2;\n");
}

#[tokio::test]
async fn concurrent_edits_share_one_path_lock() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file"), "one two").unwrap();
    let tool = Edit(workspace.clone());
    let (first, second) = tokio::join!(
        invoke(
            &tool,
            json!({"path":"file","old_string":"one","new_string":"1"})
        ),
        invoke(
            &tool,
            json!({"path":"file","old_string":"two","new_string":"2"})
        ),
    );
    assert!(!first.is_error, "{}", output_text(&first));
    assert!(!second.is_error, "{}", output_text(&second));
    assert_eq!(
        fs::read_to_string(workspace.root().join("file")).unwrap(),
        "1 2"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_edits_preserve_mode_without_modifying_other_hardlinks() {
    use std::os::unix::fs::PermissionsExt;
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("script");
    fs::write(&path, "old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::hard_link(&path, outside.path().join("linked")).unwrap();
    invoke(
        &Edit(workspace),
        json!({"path":"script","old_string":"old","new_string":"new"}),
    )
    .await;
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("linked")).unwrap(),
        "old"
    );
}

#[tokio::test]
async fn edit_lines_replaces_and_deletes_ranges() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("file");
    fs::write(&path, "aaa\nbbb\nccc\nddd\n").unwrap();
    let tool = EditLines(workspace.clone());
    let output = invoke(
        &tool,
        json!({"path":"file","start":2,"end":3,"new_string":"XXX\nYYY"}),
    )
    .await;
    assert!(output_text(&output).starts_with("edited lines in file"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nXXX\nYYY\nddd\n");
    invoke(
        &tool,
        json!({"path":"file","start":2,"end":3,"new_string":""}),
    )
    .await;
    assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nddd\n");
    let error = invoke_error(
        &tool,
        json!({"path":"file","start":9,"end":9,"new_string":"x"}),
    )
    .await;
    assert!(error.contains("out of range"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nddd\n");
}

#[tokio::test]
async fn insert_lines_works_on_empty_and_existing_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("empty"), "").unwrap();
    fs::write(workspace.root().join("file"), "aaa\nbbb\n").unwrap();
    let tool = InsertLines(workspace.clone());
    invoke(
        &tool,
        json!({"path":"empty","line":0,"new_string":"seed\nmore"}),
    )
    .await;
    assert_eq!(
        fs::read_to_string(workspace.root().join("empty")).unwrap(),
        "seed\nmore\n"
    );
    invoke(&tool, json!({"path":"file","line":2,"new_string":"tail"})).await;
    assert_eq!(
        fs::read_to_string(workspace.root().join("file")).unwrap(),
        "aaa\nbbb\ntail\n"
    );
    assert!(
        invoke_error(&tool, json!({"path":"file","line":4,"new_string":"x"}))
            .await
            .contains("out of range")
    );
    assert!(
        invoke_error(&tool, json!({"path":"file","line":0,"new_string":""}))
            .await
            .contains("must not be empty")
    );
}
