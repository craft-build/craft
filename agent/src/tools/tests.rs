use std::fs;

use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{IntoToolOutput, PortableTool, ToolErrorKind};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::*;

fn workspace() -> (TempDir, Workspace) {
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
async fn edit_requires_exact_unambiguous_matches_and_preserves_bytes() {
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
        json!({"path":"file","old_string":"\r\n","new_string":"\n","replace_all":true}),
    )
    .await
    .unwrap();
    assert_eq!(result.replacements, 3);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\u{feff}α\nsame\nother\n"
    );
    assert_eq!(result.bytes_written, fs::read(&path).unwrap().len());
    assert_eq!(
        fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1,
        "staging file leaked"
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
async fn delete_is_nonrecursive_and_missing_files_are_errors() {
    let (_dir, workspace) = workspace();
    fs::create_dir(workspace.root().join("dir")).unwrap();
    fs::write(workspace.root().join("dir/file"), "keep").unwrap();
    let tool = Delete(workspace.clone());
    assert!(invoke(&tool, json!({"path":"dir"})).await.is_err());
    assert!(invoke(&tool, json!({"path":"."})).await.is_err());
    assert!(workspace.root().join("dir/file").exists());
    let deleted = invoke(&tool, json!({"path":"dir/file"})).await.unwrap();
    assert!(deleted.deleted);
    let error = invoke(&tool, json!({"path":"dir/file"})).await.unwrap_err();
    assert_eq!(error.kind(), ToolErrorKind::NotFound);
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
        let delete = invoke(&Delete(workspace.clone()), json!({"path":path}))
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
            invoke(&Delete(workspace.clone()), json!({"path":path}))
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
    assert!(serde_json::from_value::<DeleteArgs>(json!({"path":"dir","recursive":true})).is_err());
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
            MockStreamEvent::tool_call("7", "delete", json!({"path":"file.txt"})),
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
            "delete",
            "edit",
            "edit_lines",
            "grep",
            "insert_lines",
            "read",
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
            MockStreamEvent::tool_call("1", "delete", json!({"path":"../outside"})),
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
