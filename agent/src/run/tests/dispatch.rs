//! Tool dispatch: decisions and hooks, dedup, and parallel dispatch with
//! the write-conflict barrier.

use super::*;
use crate::history::UserContent;
use crate::run::*;
use rig_core::test_utils::MockStreamEvent;

/// Collect every tool result's first text block, in call order.
fn collect_tool_text(history: &[Message]) -> Vec<String> {
    history
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|b| match b {
                    UserContent::ToolResult(r) => Some(r.content[0].to_text()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

#[tokio::test]
async fn failed_dispatch_leaves_history_uncommitted() {
    let (model, _turns) = stream_turns(vec![vec![
        tool_event("t1", "no_such_tool", serde_json::json!({})),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = vec![Message::user("earlier")];
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
    assert!(matches!(outcome, RunOutcome::Failed(message) if message.contains("unknown tool")));
    assert_eq!(history.len(), 1, "failed run must not commit");
}

#[tokio::test]
async fn skip_decision_reports_reason_to_model() {
    struct Deny;
    impl BeforeExecute for Deny {
        fn decide(&self, _call: history::ToolCall) -> BoxFuture<Decision> {
            Box::pin(async { Decision::Skip("not allowed".into()) })
        }
    }
    let (model, _turns) = stream_turns(vec![
        vec![
            tool_event("t1", "read", serde_json::json!({"path":"f"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("okay"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let history::Message::User { content } = &history[2] else {
        panic!("tool result message");
    };
    let UserContent::ToolResult(result) = &content[0] else {
        panic!("tool result block");
    };
    assert_eq!(result.content[0].to_text(), "not allowed");
    assert!(!result.is_error);
}

#[tokio::test]
async fn after_execute_transform_rewrites_results() {
    struct Upper;
    impl AfterExecute for Upper {
        fn transform(
            &self,
            _call: history::ToolCall,
            mut result: history::ToolResult,
        ) -> BoxFuture<history::ToolResult> {
            Box::pin(async move {
                for item in &mut result.content {
                    if let history::ToolResultContent::Text(text) = item {
                        text.text = text.text.to_uppercase();
                    }
                }
                result
            })
        }
    }
    let (model, _turns) = stream_turns(vec![
        vec![
            tool_event("t1", "read", serde_json::json!({"path":"f"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("ok"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let history::Message::User { content } = &history[2] else {
        panic!("tool result message");
    };
    let UserContent::ToolResult(result) = &content[0] else {
        panic!("tool result block");
    };
    assert_eq!(result.content[0].to_text(), "1: BODY");
}

#[tokio::test]
async fn dedup_replays_identical_read_calls() {
    // Two identical read calls in one run: the second answers from cache.
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let (model, _turns) = stream_turns(vec![
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let results: Vec<&history::ToolResult> = history
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|b| match b {
                    UserContent::ToolResult(r) => Some(r),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].content[0].to_text(), "1: body");
    assert!(
        results[1].content[0]
            .to_text()
            .starts_with("[cached] 1: body"),
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
    let (model, _turns) = stream_turns(vec![
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            write(),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let results: Vec<String> = history
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|b| match b {
                    UserContent::ToolResult(r) => Some(r.content[0].to_text()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
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
    let (model, _turns) = stream_turns(vec![
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            multiedit(),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let (model, _turns) = stream_turns(vec![
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            batch(),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let (model, _turns) = stream_turns(vec![
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![bash(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
        fn transform(
            &self,
            _call: history::ToolCall,
            result: history::ToolResult,
        ) -> BoxFuture<history::ToolResult> {
            let count = self.count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::Relaxed);
                result
            })
        }
    }
    let read = || tool_event("t", "read", serde_json::json!({"path": "f"}));
    let (model, _turns) = stream_turns(vec![
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![read(), MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let replayed = &history[history.len() - 2];
    let Message::User { content } = replayed else {
        panic!("tool result message");
    };
    let UserContent::ToolResult(result) = &content[0] else {
        panic!("tool result block");
    };
    assert!(result.content[0].to_text().starts_with("[cached]"));
}

// --- parallel dispatch with write-conflict barrier ---
use std::sync::Mutex as StdMutex;

use std::sync::atomic::{AtomicUsize, Ordering};

use rig_core::tool::{PortableDynamicTool, ToolOutput};

use crate::run::dispatch_tool_calls;

fn call(id: &str, name: &str, args: serde_json::Value) -> crate::history::ToolCall {
    crate::history::ToolCall {
        id: id.into(),
        function: crate::history::ToolFunction {
            name: name.into(),
            arguments: args,
        },
    }
}

fn tool(name: &str, run: impl Fn() + Send + Sync + Clone + 'static) -> PortableDynamicTool {
    let run2 = run.clone();
    PortableDynamicTool::new(name, name, serde_json::json!({}), move |_| {
        let run = run2.clone();
        Box::pin(async move {
            run();
            Ok(ToolOutput::text("ok"))
        })
    })
}

async fn dispatch(
    tools: &ToolDispatch,
    calls: Vec<crate::history::ToolCall>,
) -> (
    Option<RunOutcome>,
    Vec<Message>,
    Vec<crate::history::ToolResult>,
) {
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

fn results_in_call_order(turn: &[Message]) -> Vec<crate::history::ToolResult> {
    turn.iter()
        .filter_map(|m| match m {
            Message::User { content } => match &content[0] {
                UserContent::ToolResult(r) => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Two independent read-only calls that each wait for the other to start:
/// sequential dispatch would deadlock both on the barrier, parallel
/// dispatch passes.
#[tokio::test]
async fn independent_calls_run_concurrently() {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let mk_probe = |name: &'static str, barrier: &std::sync::Arc<tokio::sync::Barrier>| {
        let barrier = std::sync::Arc::clone(barrier);
        PortableDynamicTool::new(
            name,
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
    assert_eq!(results[0].call, "t1");
    assert_eq!(results[1].call, "t2");
}

/// Three identical calls in a row: the first two execute, the third is
/// blocked as a doom loop (reference skips execution and errors), and
/// the cleared window lets an immediate identical retry through.
#[tokio::test]
async fn identical_calls_blocked_as_doom_loop() {
    let runs = std::sync::Arc::new(AtomicUsize::new(0));
    let runs2 = std::sync::Arc::clone(&runs);
    let tools = ToolDispatch::new([PortableDynamicTool::new(
        "probe",
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
    let blocked = results.iter().filter(|r| r.is_error).collect::<Vec<_>>();
    assert_eq!(blocked.len(), 1);
    assert!(blocked[0].content[0].to_text().contains("stuck in a loop"));
    assert!(results.iter().any(|r| !r.is_error && r.call == "t1"));
    assert!(results.iter().any(|r| !r.is_error && r.call == "t4"));
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
    assert_eq!(results_in_call_order(&turn)[1].call, "t2");
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
    assert!(results[0].is_error);
    let text = results[0].content[0].to_text();
    assert!(text.contains("tool panicked"), "got: {text}");
    assert!(text.contains("kaboom"), "got: {text}");
    assert!(!results[1].is_error);
}

/// Unknown tools still fail the run with the same message as before.
#[tokio::test]
async fn unknown_tool_still_fails_the_run() {
    let tools = ToolDispatch::new([tool("read", || {})]);
    let (outcome, _, _) = dispatch(
        &tools,
        vec![call("t1", "nonexistent", serde_json::json!({}))],
    )
    .await;
    match outcome {
        Some(RunOutcome::Failed(msg)) => {
            assert!(msg.contains("unknown tool"), "got: {msg}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn never_parallel_classification() {
    assert!(crate::run::dispatch::is_never_parallel("batch"));
    assert!(crate::run::dispatch::is_never_parallel("question"));
    assert!(!crate::run::dispatch::is_never_parallel("read"));
    assert!(!crate::run::dispatch::is_never_parallel("write"));
}
