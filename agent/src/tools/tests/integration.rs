use std::fs;

use serde_json::json;

use super::{invoke, invoke_error, output_text, workspace};
use crate::tools::*;

/// A run model over the scripted attempts, and its recorder.
fn run_model(
    turns: Vec<crate::testing::Attempt>,
) -> (
    crate::provider::ModelHandle,
    crate::testing::ScriptedProvider,
) {
    let provider = crate::testing::ScriptedProvider::new(Vec::new());
    for turn in turns {
        provider.push(turn);
    }
    (
        crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("tools", 200_000, 8_192, false, false),
        )),
        provider,
    )
}

/// One tool-call spec for `crate::testing::tool_turn`.
fn tool_event(id: &str, name: &str, args: serde_json::Value) -> crate::testing::ToolCallSpec {
    (id.to_owned(), name.to_owned(), args)
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
        let read = invoke_error(&Read(workspace.clone()), json!({"path":path})).await;
        let grep = invoke_error(
            &Grep(workspace.clone()),
            json!({"path":path,"pattern":"old"}),
        )
        .await;
        let edit = invoke_error(
            &Edit(workspace.clone()),
            json!({"path":path,"old_string":"old","new_string":"new"}),
        )
        .await;
        let delete = invoke_error(&Delete(workspace.clone()), json!({"files":[path]})).await;
        for error in [read, grep, edit, delete] {
            // Every refusal is the containment or Git-metadata denial.
            assert!(
                error.contains("paths must remain inside the workspace")
                    || error.contains("access to Git metadata is not allowed")
                    || error.contains("path is outside the workspace"),
                "{error}"
            );
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
            !invoke_error(&Read(workspace.clone()), json!({"path":path}))
                .await
                .is_empty()
        );
        assert!(
            !invoke_error(
                &Edit(workspace.clone()),
                json!({"path":path,"old_string":"old","new_string":"new"})
            )
            .await
            .is_empty()
        );
        assert!(
            !invoke_error(&Delete(workspace.clone()), json!({"files":[path]}))
                .await
                .is_empty()
        );
    }
    let result = invoke(&Grep(workspace), json!({"pattern":"old"})).await;
    assert_eq!(output_text(&result), "No files found");
    assert_eq!(
        fs::read_to_string(outside.path().join("file")).unwrap(),
        "old"
    );
}

#[test]
fn schemas_match_strict_arguments() {
    let (_dir, workspace) = workspace();
    let registry = workspace
        .registry(&animist::history::History::new())
        .unwrap();
    let names = [
        "read",
        "grep",
        "edit",
        "edit_lines",
        "insert_lines",
        "write",
        "delete",
    ];
    let schemas: Vec<_> = registry
        .definitions()
        .into_iter()
        .filter(|definition| names.contains(&definition.name.as_str()))
        .map(|definition| definition.input_schema)
        .collect();
    assert_eq!(schemas.len(), names.len());
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
    let (model, provider) = run_model(vec![
        crate::testing::tool_turn(vec![tool_event("1", "read", json!({"path":"file.txt"}))]),
        crate::testing::tool_turn(vec![tool_event("2", "grep", json!({"pattern":"old"}))]),
        crate::testing::tool_turn(vec![tool_event(
            "3",
            "edit",
            json!({"path":"file.txt","old_string":"old","new_string":"new"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "4",
            "edit_lines",
            json!({"path":"file.txt","start":2,"end":2,"new_string":"SECOND"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "5",
            "insert_lines",
            json!({"path":"file.txt","line":2,"new_string":"inserted"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "6",
            "write",
            json!({"path":"nested/dir/new.txt","content":"fresh\n"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "7",
            "delete",
            json!({"files":["file.txt"]}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let tools = workspace
        .registry(&animist::history::History::new())
        .unwrap();
    let (sink, _stream) = crate::harness::EventSink::new();
    let conversation = crate::harness::ConversationBuilder::new(
        &crate::config::Config::parse("").expect("default config"),
        &workspace.root().display().to_string(),
        "",
        model,
        tools,
        sink,
    )
    .workspace(workspace.clone())
    .build()
    .expect("conversation builds");
    let outcome = crate::harness::run_turn(
        &conversation,
        "read, search, edit, then delete file.txt",
        &[],
        animist::agent::Cancellation::default(),
    )
    .await;
    assert!(matches!(outcome, crate::harness::TurnOutcome::Done { ref reply } if reply == "done"));
    assert_eq!(provider.requests().len(), 8);
    assert!(!path.exists());
    assert_eq!(
        fs::read_to_string(workspace.root().join("nested/dir/new.txt")).unwrap(),
        "fresh\n"
    );
    let requests = provider.requests();
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
    let transcript = serde_json::to_string(&requests[7].messages).unwrap();
    assert!(transcript.contains("old text"));
    assert!(transcript.contains("edited file.txt"));
    assert!(transcript.contains("deleted"));
}

#[tokio::test]
async fn dispatch_loop_preserves_permission_refusals() {
    let (_dir, workspace) = workspace();
    let (model, provider) = run_model(vec![
        crate::testing::tool_turn(vec![tool_event(
            "1",
            "delete",
            json!({"files":["../outside"]}),
        )]),
        crate::testing::text_turn("permission denied"),
    ]);
    let tools = workspace
        .registry(&animist::history::History::new())
        .unwrap();
    let (sink, stream) = crate::harness::EventSink::new();
    let conversation = crate::harness::ConversationBuilder::new(
        &crate::config::Config::parse("").expect("default config"),
        &workspace.root().display().to_string(),
        "",
        model,
        tools,
        sink,
    )
    .workspace(workspace.clone())
    .build()
    .expect("conversation builds");
    let outcome = crate::harness::run_turn(
        &conversation,
        "delete outside",
        &[],
        animist::agent::Cancellation::default(),
    )
    .await;
    assert!(
        matches!(outcome, crate::harness::TurnOutcome::Done { ref reply } if reply == "permission denied")
    );
    assert_eq!(provider.requests().len(), 2);
    // The refusal is recorded as an errored tool result; the next model turn
    // can explain it.
    let refused = {
        let mut stream = stream;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let event = tokio::time::timeout_at(deadline.into(), stream.next())
                .await
                .expect("the run's events drain")
                .expect("the stream outlives the conversation");
            if let crate::harness::Event::Agent(animist::agent::AgentEvent::ToolFinished {
                output,
                ..
            }) = &event
            {
                break output.clone();
            }
            assert!(
                !matches!(
                    event,
                    crate::harness::Event::Craft(crate::harness::CraftEvent::Done { .. })
                ),
                "the terminal Done arrived without a tool result"
            );
        }
    };
    assert!(refused.is_error);
    assert!(
        crate::history::ext::tool_content_text(&refused.content)
            .contains("paths must remain inside the workspace")
    );
}
