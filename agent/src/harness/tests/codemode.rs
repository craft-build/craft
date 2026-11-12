//! Codemode end to end: provider visibility, script dispatch, and the
//! code-scoped guardrail/policy path.

use super::*;
use crate::config::CodemodeMode;

fn codemode_config(mode: CodemodeMode) -> Config {
    Config::parse(&format!(
        "codemode {{ mode = \"{}\" }}",
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
