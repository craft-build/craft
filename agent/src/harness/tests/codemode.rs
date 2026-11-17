//! Codemode end to end: provider visibility, script dispatch, and the
//! code-scoped guardrail/policy path.

use super::*;
use crate::config::CodemodeMode;

fn codemode_config(mode: CodemodeMode) -> Config {
    codemode_config_with(mode, false)
}

fn codemode_config_with(mode: CodemodeMode, telemetry: bool) -> Config {
    Config::parse(&format!(
        "codemode {{ mode = \"{}\"\n telemetry = {telemetry} }}",
        match mode {
            CodemodeMode::Off => "off",
            CodemodeMode::Alongside => "alongside",
            CodemodeMode::Exclusive => "exclusive",
        }
    ))
    .expect("valid codemode config")
}

async fn conversation(
    dir: &tempfile::TempDir,
    provider: &ScriptedProvider,
    mode: CodemodeMode,
) -> (Conversation, EventStream) {
    let workspace = Workspace::new(dir.path())
        .expect("workspace")
        .with_codemode(codemode_config(mode).codemode);
    let history = History::default();
    conversation_over(
        provider,
        &codemode_config(mode),
        &dir.path().display().to_string(),
        workspace.registry(&history).expect("registry"),
        Some(history),
    )
    .await
}

fn code_call(id: &str, source: &str) -> Attempt {
    crate::testing::tool_turn(vec![tool_event(
        id,
        "code",
        serde_json::json!({"code": source}),
    )])
}

fn request_tool_names(provider: &ScriptedProvider, index: usize) -> Vec<String> {
    provider.requests()[index]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect()
}

#[tokio::test]
async fn provider_visibility_follows_the_codemode_mode() {
    let dir = tempfile::tempdir().unwrap();
    for (mode, alongside, exclusive) in [
        (CodemodeMode::Off, false, false),
        (CodemodeMode::Alongside, true, false),
        (CodemodeMode::Exclusive, false, true),
    ] {
        let provider = script(vec![crate::testing::text_turn("ok")]);
        let (conversation, mut stream) = conversation(&dir, &provider, mode).await;
        let outcome = run_turn(&conversation, "go").await;
        assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
        let _ = events_until_done(&mut stream).await;
        let names = request_tool_names(&provider, 0);
        assert_eq!(names.contains(&"code".to_owned()), alongside || exclusive);
        assert!(!names.contains(&"read_text".to_owned()));
        if exclusive {
            for allowed in ["code", "question", "todo_write", "task"] {
                assert!(names.contains(&allowed.to_owned()), "missing {allowed}");
            }
            assert_eq!(
                names,
                ["code", "question", "task", "todo_write"]
                    .map(String::from)
                    .to_vec(),
                "exclusive advertises only the four allowed tools"
            );
        }
        if mode == CodemodeMode::Off {
            assert!(!names.contains(&"code".to_owned()));
            assert!(!names.contains(&"read_text".to_owned()));
        }
    }
}

#[tokio::test]
async fn scripts_dispatch_hidden_children_and_commit_shaped_output() {
    let source = "import asyncio\nvalues = await asyncio.gather(read_text('a.txt'), read_text(path='b.txt'))\nprint(values)";
    let provider = script(vec![
        code_call("t1", source),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(
        results.len(),
        1,
        "the code parent owns the model-visible result"
    );
    assert_eq!(result_text(&results[0]), "['alpha\\n', 'beta\\n']\n");
    let names = request_tool_names(&provider, 0);
    assert!(names.contains(&"code".to_owned()));
    assert!(!names.contains(&"read_text".to_owned()));
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn code_children_persist_as_presentation_metadata_only() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let source =
        "import asyncio\nprint(await asyncio.gather(read_text('a.txt'), read_text(path='b.txt')))";
    let provider = script(vec![
        code_call("t1", source),
        crate::testing::text_turn("done"),
    ]);
    let state = tempfile::tempdir().unwrap();
    let state_dir = crate::storage::StateDir::from_path(state.path().to_path_buf());
    let session_ref = crate::id::SessionRef::generate();
    let persister = crate::harness::Persister::open_in(
        state_dir,
        session_ref,
        &dir.path().display().to_string(),
        "scripted/scripted",
    )
    .await
    .unwrap();
    let history = persister.history();
    let workspace = Workspace::new(dir.path())
        .unwrap()
        .with_codemode(codemode_config(CodemodeMode::Alongside).codemode);
    let registry = workspace.registry(&history).unwrap();
    let persister = std::sync::Arc::new(tokio::sync::Mutex::new(persister));
    let (conversation, mut stream) = conversation_tuned(
        &provider,
        &codemode_config(CodemodeMode::Alongside),
        &dir.path().display().to_string(),
        registry,
        Some(history),
        |builder| builder.persist(Some(persister.clone())),
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let _ = events_until_done(&mut stream).await;
    let entries_count = tool_results(&conversation).await.len();
    assert_eq!(entries_count, 1);

    let guard = persister.lock().await;
    let runs = guard.store().codemode_runs();
    let run = runs.get("t1").expect("code run projection");
    assert_eq!(run.children.len(), 2);
    assert_eq!(run.children[0].id, "code-0");
    assert_eq!(run.children[0].tool, "read_text");
    assert_eq!(run.children[0].output, "alpha\n");
    assert_eq!(run.children[1].id, "code-1");
    assert_eq!(run.failure, None);

    let replay = crate::harness::replay::transcript_with_codemode(guard.entries(), runs);
    let ids: Vec<&str> = replay
        .iter()
        .filter_map(|item| match item {
            crate::harness::ReplayItem::ToolResult { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["t1", "t1:code-0", "t1:code-1"]);
    let parent = replay
        .iter()
        .find_map(|item| match item {
            crate::harness::ReplayItem::ToolResult {
                id, input, code, ..
            } if id == "t1" => Some((input, code)),
            _ => None,
        })
        .expect("code parent replays with source and run summary");
    let (Some(input), Some(run)) = parent else {
        panic!("code parent replays with source and run summary");
    };
    assert_eq!(
        input.get("code").and_then(serde_json::Value::as_str),
        Some(source)
    );
    assert_eq!(run.children, 2);
}

#[tokio::test]
async fn direct_model_read_text_is_unknown_but_code_selection_admits_it() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read_text",
            serde_json::json!({"path": "a.txt"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert!(
        result_text(&results[0])
            .to_lowercase()
            .contains("unknown tool"),
        "got {}",
        result_text(&results[0])
    );
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn twenty_reads_finish_and_share_dedup_without_guardrail_noise() {
    let mut source = String::from("import asyncio\ncalls = []\n");
    let dir = tempfile::tempdir().unwrap();
    for i in 0..20 {
        std::fs::write(dir.path().join(format!("file-{i}.txt")), "line\n").unwrap();
        source.push_str(&format!("calls.append(read_text(path='file-{i}.txt'))\n"));
    }
    source.push_str("print(len(await asyncio.gather(*calls)))");
    let provider = script(vec![
        code_call("t1", &source),
        crate::testing::text_turn("done"),
    ]);
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;

    let started = std::time::Instant::now();
    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(
        results.len(),
        1,
        "the code parent owns the model-visible result"
    );
    assert_eq!(result_text(&results[0]), "20\n");
    assert!(
        !result_text(&results[0]).contains("guardrail"),
        "code children do not trip per-tool guardrails"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn mid_script_failure_reports_partial_progress_class_and_hint() {
    let source = "import asyncio\nfirst = await read_text('a.txt')\nsecond = await read_text('b.txt')\nawait edit('a.txt', old_string='missing', new_string='new')\nprint(first, second)";
    let provider = script(vec![
        code_call("t1", source),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    let text = result_text(&results[0]);
    assert!(
        text.contains("Completed 2 of 3 tool calls before failing: read_text x2 ok\n"),
        "got {text}"
    );
    assert!(
        text.contains("ToolError line 4: ToolError from edit: "),
        "class, line, and child tool name: got {text}"
    );
    assert!(
        text.contains("hint: wrap the call in `try/except ToolError`"),
        "got {text}"
    );
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn unknown_tool_suggests_closest_names_and_lists_uncallables() {
    let provider = script(vec![
        code_call("t1", "print(await globb('*.txt'))"),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    let text = result_text(&results[0]);
    assert!(
        text.contains("UnknownTool line 1: NameError: name 'globb' is not defined"),
        "got {text}"
    );
    assert!(
        text.contains("hint: did you mean: glob?; not callable from code:"),
        "suggestions plus the uncallable list: got {text}"
    );
    assert!(
        text.contains("batch"),
        "the uncallable list names model-only tools: got {text}"
    );
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn telemetry_records_one_jsonl_line_per_code_call() {
    let state = tempfile::tempdir().unwrap();
    crate::codemode::telemetry::set_test_state_dir(Some(state.path().to_path_buf()));
    let source = "print(await read_text('a.txt'))";
    let provider = script(vec![
        code_call("t1", source),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let workspace = Workspace::new(dir.path())
        .expect("workspace")
        .with_codemode(codemode_config_with(CodemodeMode::Alongside, true).codemode)
        .with_model("mock/main")
        .with_session_id("s1");
    let history = History::default();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &codemode_config_with(CodemodeMode::Alongside, true),
        &dir.path().display().to_string(),
        workspace.registry(&history).expect("registry"),
        Some(history),
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;
    crate::codemode::telemetry::set_test_state_dir(None);

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let path = state.path().join("codemode.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut lines = text.lines();
    let record: serde_json::Value =
        serde_json::from_str(lines.next().expect("one record")).unwrap();
    assert_eq!(lines.next(), None, "exactly one line per code call: {text}");
    assert_eq!(record["model"], "mock/main");
    assert_eq!(record["session_id"], "s1");
    assert_eq!(record["outcome"], "ok");
    assert_eq!(record["calls"], 1);
    assert_eq!(record["script_lines"], 1);
    assert_eq!(
        record["tools"]["read_text"],
        serde_json::json!({"ok": 1, "err": 0})
    );
    let _ = events_until_done(&mut stream).await;
}

struct BlockingTool {
    release: tokio::sync::Notify,
    cancellation: Cancellation,
}

#[async_trait::async_trait]
impl ToolImpl for BlockingTool {
    type Args = Value;
    type Output = ToolOutput;
    type Error = std::io::Error;

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::builder()
            .name("blocker")
            .description("blocks until cancelled")
            .input_schema(serde_json::json!({"type": "object"}))
            .build()
    }

    async fn call(&self, _: &ToolContext<'_>, _: Value) -> Result<ToolOutput, std::io::Error> {
        tokio::select! {
            _ = self.release.notified() => Ok(ToolOutput::builder().content("released").build()),
            _ = self.cancellation.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")),
        }
    }
}

#[tokio::test]
async fn cancellation_during_a_dispatch_wave_stops_the_script_quickly() {
    let provider = script(vec![code_call("t1", "print(await blocker())")]);
    let dir = tempfile::tempdir().unwrap();
    let cancellation = Cancellation::default();
    let mut registry = ToolRegistry::default();
    registry
        .register(BlockingTool {
            release: tokio::sync::Notify::new(),
            cancellation: cancellation.clone(),
        })
        .unwrap();
    let catalog = crate::codemode::catalog::Catalog::from_registry(&registry);
    registry
        .register(crate::codemode::Code::new(
            catalog,
            codemode_config(CodemodeMode::Alongside).codemode,
        ))
        .unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &codemode_config(CodemodeMode::Alongside),
        &dir.path().display().to_string(),
        registry,
        None,
    )
    .await;

    let running = conversation.clone();
    let run_cancel = cancellation.clone();
    let task =
        tokio::spawn(
            async move { crate::harness::run_turn(&running, "go", &[], run_cancel).await },
        );
    while let Some(event) = stream.next().await {
        if let Event::Agent(AgentEvent::ToolStarted { call, .. }) = &event
            && call.id.starts_with("code-")
        {
            cancellation.cancel();
            break;
        }
    }
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("cancellation resolves within 1s")
        .expect("run task joins");
    assert!(
        matches!(outcome, TurnOutcome::Cancelled | TurnOutcome::Failed(_)),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn successful_script_edits_roll_back_through_undo_snapshots() {
    let source = "print(await edit('a.txt', old_string='old', new_string='new'))";
    let provider = script(vec![
        code_call("t1", source),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;
    let snapshots = conversation.snapshots();

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        snapshots.undo_depth(),
        1,
        "the child edit captures its undo session"
    );
    snapshots.rollback().await.expect("rollback");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "old\n"
    );
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn plan_mode_blocks_a_child_edit_but_not_the_code_call() {
    let source = "try:\n    await edit('a.txt', old_string='old', new_string='new')\n    print('edited')\nexcept ToolError as error:\n    print(f'blocked: {error}')";
    let provider = script(vec![
        code_call("t1", source),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (conversation, mut stream) = conversation(&dir, &provider, CodemodeMode::Alongside).await;
    conversation.update_settings(|settings| {
        settings.mode = crate::harness::AgentMode::Plan(root.join("plans/alpha.md"));
    });

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "old\n",
        "the denied child edit never executes"
    );
    let results = tool_results(&conversation).await;
    assert!(
        result_text(&results[0]).contains("blocked:"),
        "{}",
        result_text(&results[0])
    );
    let _ = events_until_done(&mut stream).await;
}
