use std::fs;

use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{IntoToolOutput, PortableTool, ToolErrorKind};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

pub(crate) fn workspace() -> (TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    (dir, workspace)
}

async fn invoke<T>(tool: &T, args: Value) -> Result<T::Output>
where
    T: PortableTool<Error = ToolExecutionError>,
{
    tool.call(serde_json::from_value(args).unwrap()).await
}

#[tokio::test]
async fn read_pages_utf8_crlf_and_empty_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file.txt"), "first\r\nβeta\r\nlast").unwrap();
    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"file.txt","offset":2,"limit":1}))
        .await
        .unwrap();
    assert_eq!(page.total_lines, 3);
    assert_eq!(page.lines[0].number, 2);
    assert_eq!(page.lines[0].text, "βeta");
    assert_eq!(page.next_offset, Some(3));
    assert_eq!(
        page.into_tool_output().unwrap().as_text(),
        Some("2: βeta\n\n...\n\nTruncated lines: 3-3. Use offset=3 to read further.")
    );
    assert_eq!(
        invoke(&tool, json!({"path":"file.txt","offset":4}))
            .await
            .unwrap()
            .lines
            .len(),
        0
    );
    assert!(
        invoke(&tool, json!({"path":"file.txt","offset":0}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"path":"file.txt","offset":5}))
            .await
            .is_err()
    );
    fs::write(workspace.root().join("empty"), "").unwrap();
    let empty = invoke(&tool, json!({"path":"empty"})).await.unwrap();
    assert_eq!(empty.total_lines, 0);
    assert_eq!(empty.next_offset, None);
    assert_eq!(empty.into_tool_output().unwrap().as_text(), Some(""));
}

#[tokio::test]
async fn read_reports_line_and_page_truncation() {
    let (_dir, workspace) = workspace();
    let content = format!("{}\n", "🦀".repeat(1024)).repeat(100);
    fs::write(workspace.root().join("long"), content).unwrap();
    let page = invoke(&Read(workspace), json!({"path":"long","limit":0}))
        .await
        .unwrap();
    assert!(
        page.lines
            .iter()
            .all(|line| line.truncated && line.text.len() <= MAX_LINE_BYTES)
    );
    assert!(page.lines.iter().map(|line| line.text.len()).sum::<usize>() <= MAX_OUTPUT_BYTES);
    assert_eq!(page.next_offset, Some(page.lines.len() + 1));
    let next = page.next_offset.unwrap();
    let rendered = page.into_tool_output().unwrap();
    let text = rendered.as_text().unwrap();
    assert!(text.starts_with(&format!("1: {}...\n2: ", "🦀".repeat(MAX_LINE_BYTES / 4))));
    assert!(text.ends_with(&format!(
        "Truncated lines: {next}-100. Use offset={next} to read further."
    )));
}

#[tokio::test]
async fn read_full_and_eof_pages_are_literal_numbered_text() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file"), "{\"lines\": []}\n\n  βeta\n").unwrap();
    let tool = Read(workspace);
    let full = invoke(&tool, json!({"path":"file"})).await.unwrap();
    assert_eq!(
        full.into_tool_output().unwrap().as_text(),
        Some("1: {\"lines\": []}\n2: \n3:   βeta")
    );
    let eof = invoke(&tool, json!({"path":"file", "offset":4}))
        .await
        .unwrap();
    assert_eq!(eof.into_tool_output().unwrap().as_text(), Some(""));
}

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
async fn read_rejects_binary_and_oversized_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("binary"), [0, 255]).unwrap();
    fs::File::create(workspace.root().join("large"))
        .unwrap()
        .set_len((MAX_FILE_BYTES + 1) as u64)
        .unwrap();
    for path in ["binary", "large"] {
        assert!(
            invoke(&Read(workspace.clone()), json!({"path":path}))
                .await
                .is_err()
        );
    }
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
async fn edit_requires_unambiguous_matches_and_preserves_bytes() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("file");
    fs::write(&path, "\u{feff}α\r\nsame\r\nsame\r\n").unwrap();
    let tool = Edit(workspace);
    assert!(
        invoke(
            &tool,
            json!({"path":"file","old_string":"same","new_string":"other"})
        )
        .await
        .is_err()
    );
    assert!(
        invoke(
            &tool,
            json!({"path":"file","old_string":"missing","new_string":"other"})
        )
        .await
        .is_err()
    );
    assert!(
        invoke(
            &tool,
            json!({"path":"file","old_string":"","new_string":"other"})
        )
        .await
        .is_err()
    );
    assert!(
        invoke(
            &tool,
            json!({"path":"file","old_string":"same","new_string":"other","occurrence":0})
        )
        .await
        .is_err()
    );
    assert!(invoke(&tool, json!({"path":"file","old_string":"same","new_string":"other","occurrence":1,"replace_all":true})).await.is_err());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\r\nsame\r\nsame\r\n"
    );
    let result = invoke(
        &tool,
        json!({"path":"file","old_string":"same","new_string":"other","occurrence":2}),
    )
    .await
    .unwrap();
    assert_eq!(result.replacements, 1);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\r\nsame\r\nother\r\n"
    );
    let result = invoke(
        &tool,
        json!({"path":"file","old_string":"e","new_string":"E","replace_all":true}),
    )
    .await
    .unwrap();
    assert_eq!(result.replacements, 2);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\r\nsamE\r\nothEr\r\n"
    );
    assert_eq!(result.bytes_written, fs::read(&path).unwrap().len());
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
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "def f():\n    if x:\n        if y:\n            a()\n    return 1\n"
    );
    assert_eq!(
        result.into_tool_output().unwrap().as_text(),
        Some("edited f.py (fuzzy match pass 2)")
    );

    // Whitespace collapse.
    fs::write(&path, "let   x  =   1;\n").unwrap();
    invoke(
        &tool,
        json!({"path":"f.py","old_string":"let x = 1;","new_string":"let y = 2;"}),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "let y = 2;\n");

    // Escaped quotes in old_string/new_string are unescaped before matching.
    fs::write(&path, "print(\"hello\")\n").unwrap();
    let escaped_old = "print(\\\"hello\\\")";
    let escaped_new = "print(\\\"world\\\")";
    invoke(
        &tool,
        json!({"path":"f.py","old_string": escaped_old, "new_string": escaped_new}),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "print(\"world\")\n");

    // Unicode NFKD: FULLWIDTH LATIN CAPITAL LETTER A matches "A".
    fs::write(&path, "let \u{ff21} = 1;\n").unwrap();
    invoke(
        &tool,
        json!({"path":"f.py","old_string":"let A = 1;","new_string":"let B = 2;"}),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "let B = 2;\n");
}

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
async fn concurrent_edits_share_one_workspace_lock() {
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
    first.unwrap();
    second.unwrap();
    assert_eq!(
        fs::read_to_string(workspace.root().join("file")).unwrap(),
        "1 2"
    );
}

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
async fn all_tools_reject_outside_paths_and_git_metadata() {
    let (_dir, workspace) = workspace();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("file"), "old").unwrap();
    fs::create_dir(workspace.root().join(".git")).unwrap();
    fs::write(workspace.root().join(".git/config"), "old").unwrap();
    for path in [
        "../file".into(),
        outside.path().join("file").to_string_lossy().into_owned(),
        ".git/config".into(),
    ] {
        let read = invoke(&Read(workspace.clone()), json!({"path":path}))
            .await
            .unwrap_err();
        let grep = invoke(
            &Grep(workspace.clone()),
            json!({"path":path,"pattern":"old"}),
        )
        .await
        .unwrap_err();
        let edit = invoke(
            &Edit(workspace.clone()),
            json!({"path":path,"old_string":"old","new_string":"new"}),
        )
        .await
        .unwrap_err();
        let delete = invoke(&Delete(workspace.clone()), json!({"files":[path]}))
            .await
            .unwrap_err();
        for error in [read, grep, edit, delete] {
            assert_eq!(error.kind(), ToolErrorKind::PermissionDenied);
        }
    }
    assert_eq!(
        fs::read_to_string(outside.path().join("file")).unwrap(),
        "old"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_components_and_special_files() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    // macOS's normal temporary directory can exceed the Unix socket path limit.
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("file"), "old").unwrap();
    symlink(outside.path(), workspace.root().join("linked-dir")).unwrap();
    symlink(
        outside.path().join("file"),
        workspace.root().join("linked-file"),
    )
    .unwrap();
    let _socket = UnixListener::bind(workspace.root().join("socket")).unwrap();
    for path in ["linked-dir/file", "linked-file", "socket"] {
        assert!(
            invoke(&Read(workspace.clone()), json!({"path":path}))
                .await
                .is_err()
        );
        assert!(
            invoke(
                &Edit(workspace.clone()),
                json!({"path":path,"old_string":"old","new_string":"new"})
            )
            .await
            .is_err()
        );
        assert!(
            invoke(&Delete(workspace.clone()), json!({"files":[path]}))
                .await
                .is_err()
        );
    }
    let result = invoke(&Grep(workspace), json!({"pattern":"old"}))
        .await
        .unwrap();
    assert!(result.matches.is_empty());
    assert_eq!(
        fs::read_to_string(outside.path().join("file")).unwrap(),
        "old"
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
    .await
    .unwrap();
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

#[test]
fn schemas_match_strict_arguments() {
    let (_dir, workspace) = workspace();
    let schemas = [
        Read(workspace.clone()).parameters(),
        Grep(workspace.clone()).parameters(),
        Edit(workspace.clone()).parameters(),
        EditLines(workspace.clone()).parameters(),
        InsertLines(workspace.clone()).parameters(),
        Write(workspace.clone()).parameters(),
        Delete(workspace).parameters(),
    ];
    for schema in schemas {
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
    }
    assert!(serde_json::from_value::<ReadArgs>(json!({"path":"file","typo":true})).is_err());
    assert!(serde_json::from_value::<DeleteArgs>(json!({"files":["dir"],"typo":true})).is_err());
}

#[tokio::test]
async fn dispatch_loop_executes_all_seven_tools_and_returns_results_to_model() {
    let (_dir, workspace) = workspace();
    let path = workspace.root().join("file.txt");
    fs::write(&path, "old text\nsecond\n").unwrap();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("1", "read", json!({"path":"file.txt"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::tool_call("2", "grep", json!({"pattern":"old"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::tool_call(
                "3",
                "edit",
                json!({"path":"file.txt","old_string":"old","new_string":"new"}),
            ),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::tool_call(
                "4",
                "edit_lines",
                json!({"path":"file.txt","start":2,"end":2,"new_string":"SECOND"}),
            ),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::tool_call(
                "5",
                "insert_lines",
                json!({"path":"file.txt","line":2,"new_string":"inserted"}),
            ),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::tool_call(
                "6",
                "write",
                json!({"path":"nested/dir/new.txt","content":"fresh\n"}),
            ),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::tool_call("7", "delete", json!({"files":["file.txt"]})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
    ]);
    let tools = workspace.register();
    let (_, cancel) = crate::run::cancel_channel();
    let mut history = Vec::new();
    let outcome = crate::run::run(
        &model,
        &crate::run::RunParams::default(),
        &tools,
        &mut history,
        "read, search, edit, then delete file.txt",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, crate::run::RunOutcome::Done { ref reply } if reply == "done"));
    assert_eq!(model.request_count(), 8);
    assert!(!path.exists());
    assert_eq!(
        fs::read_to_string(workspace.root().join("nested/dir/new.txt")).unwrap(),
        "fresh\n"
    );
    let requests = model.requests();
    let mut names: Vec<_> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "apply_patch",
            "bash",
            "bash_kill",
            "bash_status",
            "bash_watch",
            "batch",
            "delete",
            "edit",
            "edit_lines",
            "glob",
            "grep",
            "insert_lines",
            "inspect",
            "list",
            "list_tools",
            "move",
            "multiedit",
            "read",
            "retrieve",
            "todo_write",
            "webfetch",
            "write"
        ]
    );
    let transcript = serde_json::to_string(&requests[7].chat_history).unwrap();
    assert!(transcript.contains("old text"));
    assert!(transcript.contains("edited file.txt"));
    assert!(transcript.contains("deleted"));
}

#[tokio::test]
async fn dispatch_loop_preserves_permission_refusals() {
    let (_dir, workspace) = workspace();
    let model = MockCompletionModel::from_stream_turns([
        vec![
            MockStreamEvent::tool_call("1", "delete", json!({"files":["../outside"]})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("permission denied"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
    ]);
    let tools = workspace.register();
    let (_, cancel) = crate::run::cancel_channel();
    let mut history = Vec::new();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = crate::run::run(
        &model,
        &crate::run::RunParams::default(),
        &tools,
        &mut history,
        "delete outside",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    assert!(
        matches!(outcome, crate::run::RunOutcome::Done { ref reply } if reply == "permission denied")
    );
    assert_eq!(model.request_count(), 2);
    // The refusal is recorded as an errored tool result; the next model turn
    // can explain it.
    let events = events.lock().unwrap();
    let refused = events
        .iter()
        .find_map(|event| match event {
            crate::run::Event::ToolDone { result, .. } => Some(result),
            _ => None,
        })
        .expect("a tool result event");
    assert!(refused.is_error);
    assert!(refused.content.iter().any(|item| {
        item.to_text()
            .contains("paths must remain inside the workspace")
    }));
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
    .await
    .unwrap();
    assert_eq!(output.path, "file");
    assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nXXX\nYYY\nddd\n");
    invoke(
        &tool,
        json!({"path":"file","start":2,"end":3,"new_string":""}),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "aaa\nddd\n");
    let error = invoke(
        &tool,
        json!({"path":"file","start":9,"end":9,"new_string":"x"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::InvalidArgs);
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
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.root().join("empty")).unwrap(),
        "seed\nmore\n"
    );
    invoke(&tool, json!({"path":"file","line":2,"new_string":"tail"}))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(workspace.root().join("file")).unwrap(),
        "aaa\nbbb\ntail\n"
    );
    assert!(
        invoke(&tool, json!({"path":"file","line":4,"new_string":"x"}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"path":"file","line":0,"new_string":""}))
            .await
            .is_err()
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
async fn read_injects_subdirectory_instructions_once() {
    let (dir, workspace) = workspace();
    fs::create_dir_all(dir.path().join("src/api")).unwrap();
    fs::write(dir.path().join("src/AGENTS.md"), "api rules").unwrap();
    fs::write(dir.path().join("src/api/handler.rs"), "fn main() {}").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"src/api/handler.rs"}))
        .await
        .unwrap();
    assert_eq!(page.instructions.len(), 1);
    assert!(page.instructions[0].0.ends_with("AGENTS.md"));
    assert_eq!(page.instructions[0].1, "api rules");
    let text = page
        .into_tool_output()
        .unwrap()
        .as_text()
        .unwrap()
        .to_string();
    assert!(text.contains("\n\n---\nInstructions from: "));
    assert!(text.ends_with("api rules"));

    // Second read of a sibling file: the instruction file is not repeated.
    fs::write(dir.path().join("src/api/other.rs"), "fn other() {}").unwrap();
    let page = invoke(&tool, json!({"path":"src/api/other.rs"}))
        .await
        .unwrap();
    assert!(page.instructions.is_empty());
}

#[tokio::test]
async fn read_of_instruction_file_injects_nothing() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"AGENTS.md"})).await.unwrap();
    assert!(page.instructions.is_empty());
}

#[tokio::test]
async fn read_files_at_workspace_root_inject_nothing() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
    fs::write(dir.path().join("file.txt"), "content").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"file.txt"})).await.unwrap();
    assert!(page.instructions.is_empty());
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
