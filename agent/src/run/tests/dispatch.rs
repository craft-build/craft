//! Tool dispatch: decisions and hooks, dedup, and parallel dispatch with
//! the write-conflict barrier.

use super::*;
use crate::history::{ContentBlock, Message, Role, StoredMessage, ToolContent, ext};
use crate::run::*;

/// Every tool result's model-visible text, in call order.
fn tool_texts<'a>(messages: impl Iterator<Item = &'a Message>) -> Vec<String> {
    messages
        .flat_map(ext::tool_results)
        .map(|block| {
            let ContentBlock::ToolResult { content, .. } = block else {
                panic!("tool result block");
            };
            ext::tool_content_text(content)
        })
        .collect()
}

/// Collect every tool result's first text block, in call order.
fn collect_tool_text(history: &[StoredMessage]) -> Vec<String> {
    tool_texts(history.iter().map(StoredMessage::message))
}

#[tokio::test]
async fn unknown_tool_surfaces_error_and_run_continues() {
    let (model, provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "no_such_tool",
            serde_json::json!({}),
        )]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    assert_eq!(
        provider.requests().len(),
        2,
        "the model gets a second chance"
    );
    let errors = collect_tool_text(&history);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("unknown tool: no_such_tool"),
        "got: {errors:?}"
    );
    // The retried request carries the error result, so the model sees what
    // went wrong and can correct the name.
    let requests = provider.requests();
    let replayed = &requests[1].messages;
    let replayed_errors = tool_texts(replayed.iter());
    assert!(
        replayed_errors[0].contains("unknown tool: no_such_tool"),
        "got: {replayed_errors:?}"
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
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("okay"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_before(std::sync::Arc::new(Deny));
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "okay"));
    let result_message = history[2].message();
    assert_eq!(result_message.role, Role::User, "tool result message");
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &result_message.content[0]
    else {
        panic!("tool result block");
    };
    assert_eq!(ext::tool_content_text(content), "not allowed");
    assert!(!is_error);
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
                        if let ToolContent::Text { text } = item {
                            *text = text.to_uppercase();
                        }
                    }
                }
                result
            })
        }
    }
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("ok"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_after(std::sync::Arc::new(Upper));
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let _ = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    let result_message = history[2].message();
    assert_eq!(result_message.role, Role::User, "tool result message");
    let ContentBlock::ToolResult { content, .. } = &result_message.content[0] else {
        panic!("tool result block");
    };
    assert_eq!(ext::tool_content_text(content), "1: BODY");
}

#[tokio::test]
async fn dedup_replays_identical_read_calls() {
    // Two identical read calls in one run: the second answers from cache.
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read()]),
        crate::testing::tool_turn(vec![read()]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache());
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let results = collect_tool_text(&history);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], "1: body");
    assert!(
        results[1].starts_with("[cached] 1: body"),
        "second identical read must replay from cache"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_write_to_same_path() {
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let write = || {
        tool_event(
            "t",
            "write",
            serde_json::json!({"path": "f", "content": "new\n"}),
        )
    };
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read()]),
        crate::testing::tool_turn(vec![write()]),
        crate::testing::tool_turn(vec![read()]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache());
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let _ = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    let results = collect_tool_text(&history);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], "1: old");
    assert!(!results[1].contains("cached"), "writes are never cached");
    assert_eq!(
        results[2], "1: new",
        "post-write read must re-execute against fresh contents"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_multiedit_top_level_path() {
    // The multiedit schema carries one top-level `path`; a stale extractor that
    // looked per-edit left the file's cached read in place.
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let multiedit = || {
        tool_event(
            "t",
            "multiedit",
            serde_json::json!({
                "path": "f",
                "edits": [{"old_string": "old", "new_string": "new"}]
            }),
        )
    };
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read()]),
        crate::testing::tool_turn(vec![multiedit()]),
        crate::testing::tool_turn(vec![read()]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache());
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let _ = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    let results = collect_tool_text(&history);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], "1: old");
    assert_eq!(
        results[2], "1: new",
        "post-multiedit read must re-execute, not replay the stale entry"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_batch_child_write() {
    // A batch child runs through the same dispatch table, so its write must
    // invalidate the read cache just like a top-level write.
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let batch = || {
        tool_event(
            "t",
            "batch",
            serde_json::json!({"tool_calls": [{"tool": "write", "parameters": {
                "path": "f", "content": "new\n"
            }}]}),
        )
    };
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read()]),
        crate::testing::tool_turn(vec![batch()]),
        crate::testing::tool_turn(vec![read()]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache());
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let _ = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    let results = collect_tool_text(&history);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], "1: old");
    assert_eq!(
        results[2], "1: new",
        "batch child write must invalidate the parent read cache"
    );
}

#[tokio::test]
async fn dedup_cleared_by_bash() {
    // bash can rewrite arbitrary files without naming them, so the whole
    // cache is cleared rather than a single path.
    // SAFETY: disable the sandbox like tools/bash.rs tests do — hosts that
    // already sandbox the test process deny nested sandbox-exec.
    unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let bash = || {
        tool_event(
            "t",
            "bash",
            serde_json::json!({"command": "printf 'new\\n' > f"}),
        )
    };
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read()]),
        crate::testing::tool_turn(vec![bash()]),
        crate::testing::tool_turn(vec![read()]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache());
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let _ = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    let results = collect_tool_text(&history);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], "1: old");
    assert_eq!(
        results[2], "1: new",
        "bash write must clear the read cache; bash said: {:?}",
        results[1]
    );
}

#[tokio::test]
async fn dedup_replays_through_after_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // The cache stores the raw result; a replay must re-run the
    // per-call AfterExecute transform instead of replaying its output.
    struct Counting {
        count: std::sync::Arc<AtomicUsize>,
    }
    impl AfterExecute for Counting {
        fn transform(&self, _call: ContentBlock, result: ContentBlock) -> BoxFuture<ContentBlock> {
            let count = self.count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::Relaxed);
                result
            })
        }
    }
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read()]),
        crate::testing::tool_turn(vec![read()]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_dedup(crate::run::shared_cache())
        .with_after(std::sync::Arc::new(Counting {
            count: count.clone(),
        }));
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let _ = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert_eq!(
        count.load(Ordering::Relaxed),
        2,
        "after hook must run on both the execution and the replay"
    );
    assert!(history.len() >= 5);
    let replayed = history[history.len() - 2].message();
    assert_eq!(replayed.role, Role::User, "tool result message");
    let ContentBlock::ToolResult { content, .. } = &replayed.content[0] else {
        panic!("tool result block");
    };
    assert!(ext::tool_content_text(content).starts_with("[cached]"));
}

// --- parallel dispatch with write-conflict barrier ---
use std::sync::Mutex as StdMutex;

use std::sync::atomic::{AtomicUsize, Ordering};

use rig_core::tool::{DynamicTool, ToolOutput};

use crate::run::dispatch_tool_calls;

fn call(id: &str, name: &str, args: serde_json::Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input: args,
        thought_signature: None,
    }
}

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

async fn dispatch(
    tools: &ToolDispatch,
    calls: Vec<ContentBlock>,
) -> (Option<RunOutcome>, Vec<StoredMessage>, Vec<ContentBlock>) {
    let (_, cancel) = cancel_channel();
    let mut turn = Vec::new();
    let done = std::sync::Arc::new(StdMutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&done);
    let outcome = dispatch_tool_calls(
        tools,
        &mut turn,
        calls,
        &mut doom::RecentCalls::default(),
        &cancel,
        &move |event| {
            if let Event::ToolDone { result, .. } = event {
                sink.lock().unwrap().push(result);
            }
        },
    )
    .await;
    (outcome.0, turn, done.lock().unwrap().clone())
}

/// The committed result blocks of a dispatched turn, in call order.
fn results_in_call_order(turn: &[StoredMessage]) -> Vec<ContentBlock> {
    turn.iter()
        .filter_map(|stored| {
            let message = stored.message();
            if message.role == Role::User {
                message.content.first().cloned()
            } else {
                None
            }
        })
        .collect()
}

/// The result block's call id.
fn result_id(result: &ContentBlock) -> &str {
    let ContentBlock::ToolResult { tool_use_id, .. } = result else {
        panic!("tool result block");
    };
    tool_use_id
}

/// Whether the result block reports an error.
fn result_is_error(result: &ContentBlock) -> bool {
    let ContentBlock::ToolResult { is_error, .. } = result else {
        panic!("tool result block");
    };
    *is_error
}

/// The result block's model-visible text.
fn result_text(result: &ContentBlock) -> String {
    let ContentBlock::ToolResult { content, .. } = result else {
        panic!("tool result block");
    };
    ext::tool_content_text(content)
}

/// Two independent read-only calls that each wait for the other to start:
/// sequential dispatch would deadlock both on the barrier, parallel
/// dispatch passes.
#[tokio::test]
async fn independent_calls_run_concurrently() {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mk_probe = |name: &'static str, barrier: &std::sync::Arc<tokio::sync::Barrier>| {
        let barrier = std::sync::Arc::clone(barrier);
        DynamicTool::new(
            crate::tools::tool_name(name),
            name,
            serde_json::json!({}),
            move |_: serde_json::Value| {
                let barrier = std::sync::Arc::clone(&barrier);
                Box::pin(async move {
                    barrier.wait().await;
                    Ok(ToolOutput::text("ok"))
                })
            },
        )
    };
    let tools = ToolDispatch::new([mk_probe("probe_a", &barrier), mk_probe("probe_b", &barrier)]);
    let dispatched = dispatch(
        &tools,
        vec![
            call("t1", "probe_a", serde_json::json!({})),
            call("t2", "probe_b", serde_json::json!({})),
        ],
    );
    // Sequential dispatch would deadlock both probes on the barrier.
    let (outcome, turn, done) =
        tokio::time::timeout(std::time::Duration::from_secs(10), dispatched)
            .await
            .expect("calls must overlap; dispatch looks sequential");
    assert!(outcome.is_none());
    assert_eq!(done.len(), 2);
    let results = results_in_call_order(&turn);
    assert_eq!(result_id(&results[0]), "t1");
    assert_eq!(result_id(&results[1]), "t2");
}

/// Three identical calls in a row: the first two execute, the third is
/// blocked as a doom loop (reference skips execution and errors), and
/// the cleared window lets an immediate identical retry through.
#[tokio::test]
async fn identical_calls_blocked_as_doom_loop() {
    let runs = std::sync::Arc::new(AtomicUsize::new(0));
    let runs2 = std::sync::Arc::clone(&runs);
    let tools = ToolDispatch::new([DynamicTool::new(
        crate::tools::tool_name("probe"),
        "probe",
        serde_json::json!({}),
        move |_: serde_json::Value| {
            let runs = std::sync::Arc::clone(&runs2);
            Box::pin(async move {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput::text("ok"))
            })
        },
    )]);
    let input = serde_json::json!({});
    let (outcome, turn, done) = dispatch(
        &tools,
        vec![
            call("t1", "probe", input.clone()),
            call("t2", "probe", input.clone()),
            call("t3", "probe", input.clone()),
            // Window was cleared by the block, so this identical retry
            // executes instead of being re-blocked forever.
            call("t4", "probe", input.clone()),
        ],
    )
    .await;
    assert!(outcome.is_none());
    assert_eq!(runs.load(Ordering::SeqCst), 3, "t3 is blocked, not run");
    let results = results_in_call_order(&turn);
    assert_eq!(results.len(), 4);
    // The blocked call's result is committed immediately, ahead of the
    // wave's results; the other three ran.
    let blocked = results
        .iter()
        .filter(|r| result_is_error(r))
        .collect::<Vec<_>>();
    assert_eq!(blocked.len(), 1);
    assert!(result_text(blocked[0]).contains("stuck in a loop"));
    assert!(
        results
            .iter()
            .any(|r| !result_is_error(r) && result_id(r) == "t1")
    );
    assert!(
        results
            .iter()
            .any(|r| !result_is_error(r) && result_id(r) == "t4")
    );
    assert_eq!(done.len(), 4);
}

/// Two writes to the same path must not overlap: each records start/end
/// markers around a sleep and the log must show one fully nested pair.
#[tokio::test]
async fn same_path_writes_serialize() {
    let log = std::sync::Arc::new(StdMutex::new(Vec::new()));
    let mk = |name: &'static str, log: &std::sync::Arc<StdMutex<Vec<&'static str>>>| {
        let log = std::sync::Arc::clone(log);
        tool(name, move || {
            log.lock().unwrap().push("start");
            std::thread::sleep(std::time::Duration::from_millis(20));
            log.lock().unwrap().push("end");
        })
    };
    let tools = ToolDispatch::new([mk("write", &log), mk("edit", &log)]);
    let (outcome, turn, _) = dispatch(
        &tools,
        vec![
            call(
                "t1",
                "write",
                serde_json::json!({"path": "same.txt", "content": "a"}),
            ),
            call(
                "t2",
                "edit",
                serde_json::json!({"path": "same.txt", "old_string": "a", "new_string": "b"}),
            ),
        ],
    )
    .await;
    assert!(outcome.is_none());
    assert_eq!(*log.lock().unwrap(), vec!["start", "end", "start", "end"]);
    assert_eq!(result_id(&results_in_call_order(&turn)[1]), "t2");
}

/// A never-parallel tool joins the wave barrier: everything spawned before
/// it has finished before it starts. The counter is incremented by the
/// earlier call at its end and read by the batch call at its start.
#[tokio::test]
async fn never_parallel_tool_waits_for_earlier_calls() {
    let started = std::sync::Arc::new(AtomicUsize::new(0));
    let finished = std::sync::Arc::new(AtomicUsize::new(0));
    let seen_before = std::sync::Arc::new(AtomicUsize::new(usize::MAX));
    let earlier = {
        let finished = std::sync::Arc::clone(&finished);
        tool("read", move || {
            started.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            finished.fetch_add(1, Ordering::SeqCst);
        })
    };
    let batch = {
        let finished = std::sync::Arc::clone(&finished);
        let seen = std::sync::Arc::clone(&seen_before);
        tool("batch", move || {
            seen.store(finished.load(Ordering::SeqCst), Ordering::SeqCst);
        })
    };
    let tools = ToolDispatch::new([earlier, batch]);
    let (outcome, _, _) = dispatch(
        &tools,
        vec![
            call("t1", "read", serde_json::json!({"path": "f"})),
            call("t2", "batch", serde_json::json!({})),
        ],
    )
    .await;
    assert!(outcome.is_none());
    assert_eq!(
        seen_before.load(Ordering::SeqCst),
        1,
        "batch must start only after the earlier call finished"
    );
}

/// A panicking tool future becomes an error result for that call; the
/// sibling call still succeeds and the run continues.
#[tokio::test]
async fn panicking_tool_becomes_error_result() {
    let tools = ToolDispatch::new([tool("boom", || panic!("kaboom")), tool("fine", || {})]);
    let (outcome, turn, _) = dispatch(
        &tools,
        vec![
            call("t1", "boom", serde_json::json!({})),
            call("t2", "fine", serde_json::json!({})),
        ],
    )
    .await;
    assert!(outcome.is_none(), "panic must not fail the run");
    let results = results_in_call_order(&turn);
    assert!(result_is_error(&results[0]));
    let text = result_text(&results[0]);
    assert!(text.contains("tool panicked"), "got: {text}");
    assert!(text.contains("kaboom"), "got: {text}");
    assert!(!result_is_error(&results[1]));
}

/// An unknown tool becomes a per-call error result; the run continues.
#[tokio::test]
async fn unknown_tool_becomes_recoverable_error_result() {
    let tools = ToolDispatch::new([tool("read", || {})]);
    let (outcome, turn, _) = dispatch(
        &tools,
        vec![call("t1", "nonexistent", serde_json::json!({}))],
    )
    .await;
    assert!(outcome.is_none(), "an unknown tool must not stop the run");
    let results = results_in_call_order(&turn);
    assert_eq!(results.len(), 1);
    assert!(result_is_error(&results[0]));
    let text = result_text(&results[0]);
    assert!(text.contains("unknown tool: nonexistent"), "got: {text}");
}

#[test]
fn never_parallel_classification() {
    assert!(crate::run::dispatch::is_never_parallel("batch"));
    assert!(crate::run::dispatch::is_never_parallel("question"));
    assert!(!crate::run::dispatch::is_never_parallel("read"));
    assert!(!crate::run::dispatch::is_never_parallel("write"));
}
