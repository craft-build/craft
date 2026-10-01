use std::fs;

use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};
use rig_core::tool::{PortableTool, ToolErrorKind};
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

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
    // Phase 1 of the argosy integration: the argosy knowledge tools and
    // the `review` tool extend the builtin set; the pinned list asserts the
    // core tools all survive registration next to them.
    for expected in [
        "ask",
        "search",
        "write_memory",
        "start_review",
        "read_document",
        "inspect",
        "review",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from the table"
        );
    }
    assert_eq!(
        names
            .into_iter()
            .filter(|name| !crate::knowledge::is_argosy_tool(name) && *name != "review")
            .collect::<Vec<_>>(),
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
            "list",
            "list_tools",
            "move",
            "multiedit",
            "question",
            "read",
            "retrieve",
            "sessions",
            "skill",
            "task",
            "todo_write",
            "view_image",
            "webfetch",
            "websearch",
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
