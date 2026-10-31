//! Tool dispatch: decisions and hooks, dedup, and scheduling of one turn's
//! calls through the conversation.

use super::*;
use crate::history::{ContentBlock, Role, ext};
use crate::run::dispatch::{AfterExecute, BeforeExecute, BoxFuture, Decision, DispatchOutcome};
use animist::agent::AgentEvent;
use rig_core::tool::{DynamicTool, ToolOutput};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

/// One model-visible tool call block.
fn call(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input: args,
        thought_signature: None,
    }
}

/// A rig tool that runs `run` and answers "ok".
fn tool(name: &str, run: impl Fn() + Send + Sync + Clone + 'static) -> DynamicTool {
    let run2 = run.clone();
    DynamicTool::new(
        crate::tools::tool_name(name),
        name,
        serde_json::json!({}),
        move |_| {
            let run = run2.clone();
            Box::pin(async move {
                run();
                Ok(ToolOutput::text("ok"))
            })
        },
    )
}

/// Execute one call through the dispatch table's interception pipeline and
/// return its result block. This is the path batch children take.
async fn run_call(dispatch: &crate::run::ToolDispatch, call: ContentBlock) -> ContentBlock {
    match dispatch.execute(call).await.expect("dispatch") {
        DispatchOutcome::Ran(result) | DispatchOutcome::Skipped(result) => result,
        DispatchOutcome::Stopped(reason) => panic!("stopped: {reason}"),
    }
}

/// Whether a result block reports an error.
fn result_is_error(result: &ContentBlock) -> bool {
    let ContentBlock::ToolResult { is_error, .. } = result else {
        panic!("tool result block");
    };
    *is_error
}

/// A result block's model-visible text.
fn raw_text(result: &ContentBlock) -> String {
    let ContentBlock::ToolResult { content, .. } = result else {
        panic!("tool result block");
    };
    ext::tool_content_text(content)
}

#[tokio::test]
async fn unknown_tool_surfaces_error_and_run_continues() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "no_such_tool",
            serde_json::json!({}),
        )]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        provider.requests().len(),
        2,
        "the model gets a second chance"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        result_text(&results[0]),
        "[error] Unknown tool: no_such_tool",
        "the model-visible wording is the runtime's"
    );
    // The retried request carries the error result, so the model sees what
    // went wrong and can correct the name.
    let replayed = &provider.requests()[1].messages;
    assert!(
        replayed.iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, is_error: true, .. }
                        if ext::tool_content_text(content) == "Unknown tool: no_such_tool")
                })
        }),
        "got: {replayed:?}"
    );
}

/// An unknown tool becomes a per-call error result through the dispatch
/// table too (the path batch children take); the wording is ours.
#[tokio::test]
async fn unknown_tool_becomes_recoverable_error_result() {
    let dir = tempfile::tempdir().unwrap();
    let dispatch = Workspace::new(dir.path()).unwrap().register();
    let result = run_call(&dispatch, call("t1", "nonexistent", serde_json::json!({}))).await;
    assert!(result_is_error(&result), "must not stop the run");
    assert!(
        raw_text(&result).contains("unknown tool: nonexistent"),
        "got: {}",
        raw_text(&result)
    );
}

#[tokio::test]
async fn skip_decision_reports_reason_to_model() {
    struct Deny;
    impl BeforeExecute for Deny {
        fn decide(&self, _call: ContentBlock) -> BoxFuture<Decision> {
            Box::pin(async { Decision::Skip("not allowed".into()) })
        }
    }
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("okay"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace.register(),
        sink,
    )
    .approval(Arc::new(Deny))
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "go").await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "okay"),
        "{outcome:?}"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(result_text(&results[0]), "not allowed");
}

#[tokio::test]
async fn after_execute_transform_rewrites_results() {
    struct Upper;
    impl AfterExecute for Upper {
        fn transform(
            &self,
            _call: ContentBlock,
            mut result: ContentBlock,
        ) -> BoxFuture<ContentBlock> {
            Box::pin(async move {
                if let ContentBlock::ToolResult { content, .. } = &mut result {
                    for item in &mut content.0 {
                        if let animist::history::ToolContent::Text { text } = item {
                            *text = text.to_uppercase();
                        }
                    }
                }
                result
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let dispatch = Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_after(Arc::new(Upper));
    let result = run_call(
        &dispatch,
        call("t1", "read", serde_json::json!({"path": "f"})),
    )
    .await;
    assert_eq!(raw_text(&result), "1: BODY");
}

#[tokio::test]
async fn dedup_replays_identical_read_calls() {
    // Two identical read calls in one run: the second answers from cache
    // without executing.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert_eq!(result_text(&results[0]), "1: body");
    assert_eq!(
        result_text(&results[1]),
        "1: body",
        "the second identical read replays the recorded result"
    );
    // Only the first call executes; both finish.
    let events = events_until_done(&mut stream).await;
    let started = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::ToolStarted { .. })))
        .count();
    let finished = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::ToolFinished { .. })))
        .count();
    assert_eq!(started, 1, "only the first call executes");
    assert_eq!(finished, 2, "both calls finish");
}

#[tokio::test]
async fn dedup_invalidated_by_write_to_same_path() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "write",
            serde_json::json!({"path": "f", "content": "new\n"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert!(!texts[1].contains("cached"), "writes are never cached");
    assert_eq!(
        texts[2], "1: new",
        "post-write read must re-execute against fresh contents"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_multiedit_top_level_path() {
    // The multiedit schema carries one top-level `path`; a stale extractor
    // that looked per-edit left the file's cached read in place.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "multiedit",
            serde_json::json!({
                "path": "f",
                "edits": [{"old_string": "old", "new_string": "new"}]
            }),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert_eq!(
        texts[2], "1: new",
        "post-multiedit read must re-execute, not replay the stale entry"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_batch_child_write() {
    // A batch child runs through the same dispatch table, so its write must
    // invalidate the read cache just like a top-level write.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let dispatch = Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache());
    let read = || call("t", "read", serde_json::json!({"path": "f"}));
    let batch = || {
        call(
            "t",
            "batch",
            serde_json::json!({"tool_calls": [{"tool": "write", "parameters": {
                "path": "f", "content": "new\n"
            }}]}),
        )
    };
    let first = run_call(&dispatch, read()).await;
    assert_eq!(raw_text(&first), "1: old");
    let _ = run_call(&dispatch, batch()).await;
    let third = run_call(&dispatch, read()).await;
    assert_eq!(
        raw_text(&third),
        "1: new",
        "batch child write must invalidate the read cache"
    );
}

#[tokio::test]
async fn dedup_cleared_by_bash() {
    // bash can rewrite arbitrary files without naming them, so the whole
    // cache is cleared rather than a single path.
    // SAFETY: disable the sandbox like tools/bash.rs tests do — hosts that
    // already sandbox the test process deny nested sandbox-exec.
    unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "bash",
            serde_json::json!({"command": "printf 'new\\n' > f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert_eq!(
        texts[2], "1: new",
        "bash write must clear the read cache; bash said: {:?}",
        texts[1]
    );
}

#[tokio::test]
async fn dedup_replays_through_after_hook() {
    // The cache stores the raw result; a replay must re-run the per-call
    // AfterExecute transform instead of replaying its output.
    struct Counting {
        count: Arc<AtomicUsize>,
    }
    impl AfterExecute for Counting {
        fn transform(&self, _call: ContentBlock, result: ContentBlock) -> BoxFuture<ContentBlock> {
            let count = Arc::clone(&self.count);
            Box::pin(async move {
                count.fetch_add(1, Ordering::Relaxed);
                result
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let dispatch = Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache())
        .with_after(Arc::new(Counting {
            count: count.clone(),
        }));
    let read = |id: &str| call(id, "read", serde_json::json!({"path": "f"}));
    let first = run_call(&dispatch, read("t1")).await;
    assert_eq!(raw_text(&first), "1: body");
    let replayed = run_call(&dispatch, read("t2")).await;
    assert_eq!(
        raw_text(&replayed),
        "[cached] 1: body",
        "the replayed result carries the cache marker"
    );
    assert_eq!(
        count.load(Ordering::Relaxed),
        2,
        "after hook must run on both the execution and the replay"
    );
}

// --- scheduling of one turn's calls ---

/// Two writes to the same path must not overlap: each records start/end
/// markers around a sleep and the log must show one fully nested pair.
#[tokio::test]
async fn same_path_writes_serialize() {
    let log = Arc::new(StdMutex::new(Vec::new()));
    let mk = |name: &'static str, log: &Arc<StdMutex<Vec<&'static str>>>| {
        let log = Arc::clone(log);
        tool(name, move || {
            log.lock().unwrap().push("start");
            std::thread::sleep(std::time::Duration::from_millis(20));
            log.lock().unwrap().push("end");
        })
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event(
                "t1",
                "write",
                serde_json::json!({"path": "same.txt", "content": "a"}),
            ),
            tool_event(
                "t2",
                "edit",
                serde_json::json!({"path": "same.txt", "old_string": "a", "new_string": "b"}),
            ),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        crate::run::ToolDispatch::new([mk("write", &log), mk("edit", &log)]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(*log.lock().unwrap(), vec!["start", "end", "start", "end"]);
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert!(!result_is_error(&results[0]));
    assert!(!result_is_error(&results[1]));
}

/// A never-parallel tool joins the wave barrier: everything spawned before
/// it has finished before it starts. The counter is incremented by the
/// earlier call at its end and read by the batch call at its start.
#[tokio::test]
async fn never_parallel_tool_waits_for_earlier_calls() {
    let finished = Arc::new(AtomicUsize::new(0));
    let seen_before = Arc::new(AtomicUsize::new(usize::MAX));
    let earlier = {
        let finished = Arc::clone(&finished);
        tool("read", move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            finished.fetch_add(1, Ordering::SeqCst);
        })
    };
    let batch = {
        let finished = Arc::clone(&finished);
        let seen = Arc::clone(&seen_before);
        tool("batch", move || {
            seen.store(finished.load(Ordering::SeqCst), Ordering::SeqCst);
        })
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "read", serde_json::json!({"path": "f"})),
            tool_event("t2", "batch", serde_json::json!({})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        crate::run::ToolDispatch::new([earlier, batch]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        seen_before.load(Ordering::SeqCst),
        1,
        "batch must start only after the earlier call finished"
    );
}

/// Two independent read-only calls that each wait for the other to start:
/// sequential dispatch would deadlock both on the barrier, concurrent
/// dispatch passes. Bounded so the suite can never hang on it.
#[tokio::test]
#[ignore = "restored in 06"]
async fn independent_calls_run_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mk_probe = |name: &'static str, barrier: &Arc<tokio::sync::Barrier>| {
        let barrier = Arc::clone(barrier);
        DynamicTool::new(
            crate::tools::tool_name(name),
            name,
            serde_json::json!({}),
            move |_: serde_json::Value| {
                let barrier = Arc::clone(&barrier);
                Box::pin(async move {
                    barrier.wait().await;
                    Ok(ToolOutput::text("ok"))
                })
            },
        )
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe_a", serde_json::json!({})),
            tool_event("t2", "probe_b", serde_json::json!({})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        crate::run::ToolDispatch::new([
            mk_probe("probe_a", &barrier),
            mk_probe("probe_b", &barrier),
        ]),
        None,
    )
    .await;
    // Sequential dispatch would deadlock both probes on the barrier.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        run_turn(&conversation, "go").await
    })
    .await
    .expect("calls must overlap; dispatch looks sequential");
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
}

/// Three identical calls in a row: the first two execute, the third is
/// blocked as a doom loop, and the cleared window lets an immediate
/// identical retry through.
#[tokio::test]
#[ignore = "restored in 07"]
async fn identical_calls_blocked_as_doom_loop() {
    let runs = Arc::new(AtomicUsize::new(0));
    let runs2 = Arc::clone(&runs);
    let probe = DynamicTool::new(
        crate::tools::tool_name("probe"),
        "probe",
        serde_json::json!({}),
        move |_: serde_json::Value| {
            let runs = Arc::clone(&runs2);
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput::text("ok"))
            })
        },
    );
    let input = serde_json::json!({});
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe", input.clone()),
            tool_event("t2", "probe", input.clone()),
            tool_event("t3", "probe", input.clone()),
            // Window was cleared by the block, so this identical retry
            // executes instead of being re-blocked forever.
            tool_event("t4", "probe", input.clone()),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        crate::run::ToolDispatch::new([probe]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 3, "t3 is blocked, not run");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 4);
    let blocked: Vec<&ContentBlock> = results.iter().filter(|r| result_is_error(r)).collect();
    assert_eq!(blocked.len(), 1);
    assert!(
        raw_text(blocked[0]).contains("stuck in a loop"),
        "got: {}",
        raw_text(blocked[0])
    );
    assert!(results.iter().any(|r| !result_is_error(r)));
}

/// A panicking tool future becomes an error result for that call; the
/// sibling call still succeeds and the run continues.
#[tokio::test]
async fn panicking_tool_becomes_error_result() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "boom", serde_json::json!({})),
            tool_event("t2", "fine", serde_json::json!({})),
        ]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        crate::run::ToolDispatch::new([tool("boom", || panic!("kaboom")), tool("fine", || {})]),
        None,
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "panic must not fail the run: {outcome:?}"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert!(result_is_error(&results[0]), "{}", result_text(&results[0]));
    let text = result_text(&results[0]);
    assert!(text.contains("panicked"), "got: {text}");
    assert!(text.contains("kaboom"), "got: {text}");
    assert!(!result_is_error(&results[1]));
    assert!(
        events_until_done(&mut stream)
            .await
            .iter()
            .any(|event| matches!(event, Event::Agent(AgentEvent::ToolFailed { .. }))),
        "the structured failure is reported"
    );
}
