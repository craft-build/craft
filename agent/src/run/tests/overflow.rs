//! Stream-error recovery: overflow compaction and retry, rate limits,
//! reauth waits, and the cancel marker.

use super::*;
use crate::run::*;
use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

// --- Context-overflow recovery (C.5) ---
fn overflow_error_turn() -> Vec<MockStreamEvent> {
    // The requested size must exceed the run's overhead-inclusive estimate
    // (history + tool schemas) for recalibration to trigger.
    vec![MockStreamEvent::Error(
        rig_core::test_utils::MockError::provider(
            "This model's maximum context length is 131072 tokens. However, you requested 131072 tokens.",
        ),
    )]
}

fn rate_limit_turn() -> Vec<MockStreamEvent> {
    vec![MockStreamEvent::Error(
        rig_core::test_utils::MockError::provider("rate limit exceeded, retry after 30s"),
    )]
}

fn done_turn(text: &str) -> Vec<MockStreamEvent> {
    vec![
        MockStreamEvent::text(text),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]
}

fn overflow_history() -> Vec<Message> {
    use crate::compaction::test_support as ts;
    // Repeated 50x so the history dwarfs the fixed tool-schema overhead
    // (~14k tokens): these tests exercise a *history*-driven overflow that
    // compaction can actually cure, matching the production premise.
    let mut messages = Vec::new();
    for round in 0..8 * 50 {
        let i = round % 8;
        messages.push(ts::user(&format!(
            "do task {i} with a fairly long instruction"
        )));
        messages.push(ts::assistant_tool_args(
            &format!("t{i}"),
            "bash",
            serde_json::json!({"command": format!("echo {i}")}),
        ));
        messages.push(ts::tool_result_of(&format!("t{i}"), &"x".repeat(200)));
    }
    messages
}

fn recovery_setup() -> (RunParams, SharedCompactionState) {
    let shared: SharedCompactionState = std::sync::Arc::new(std::sync::Mutex::new(
        crate::compaction::CompactionState::default(),
    ));
    let tokens = crate::compaction::estimate_tokens(&overflow_history());
    // Window sized like the engine tests so the estimate (history + the
    // overhead the run loop publishes) overflows the buffer-subtracted
    // window and the VCC stage is forced to run.
    let context_length = ((tokens as f64 / 0.9) as u32).max(1);
    let params = RunParams::new(None).with_compaction(CompactionCtx {
        state: shared.clone(),
        stages: vec![crate::config::CompactionConfig {
            kind: crate::config::CompactionKind::Vcc,
            context: 0.6,
        }],
        buffer: crate::config::CompactionBuffer::Percent(20),
        context_length: Some(context_length),
    });
    (params, shared)
}

#[tokio::test]
async fn overflow_recovers_compacts_and_retries() {
    let (model, _turns) = stream_turns(vec![overflow_error_turn(), done_turn("recovered")]);
    let (params, shared) = recovery_setup();
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = overflow_history();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    assert!(matches!(&outcome, RunOutcome::Done { reply } if reply == "recovered"));
    // The retried request was rebuilt from the compacted history.
    assert_eq!(model.requests().len(), 2);
    // Compaction events fired around the recovery.
    let guard = events.lock().unwrap();
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::AutoCompacting { .. }))
    );
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::CompactionDone { .. }))
    );
    // No usage is reported on a failed request, but the overflow error
    // body names the requested prompt size ("you requested 8192 tokens"),
    // so the estimator recalibrates from it and the estimate shrinks for
    // real on the next call.
    assert!(
        shared.lock().unwrap().estimator.multiplier() > 1.0,
        "calibration uses the provider-reported prompt size"
    );
}

#[tokio::test]
async fn second_consecutive_overflow_surfaces_the_error() {
    let (model, _turns) = stream_turns(vec![overflow_error_turn(), overflow_error_turn()]);
    let (params, _shared) = recovery_setup();
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = overflow_history();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    match outcome {
        RunOutcome::Failed(message) => {
            assert!(
                message.contains("maximum context length"),
                "surfaced the original overflow error, got: {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // Both requests failed; the budget allowed exactly one recovery
    // attempt (the second overflow surfaces the error without
    // compacting again).
    assert_eq!(model.requests().len(), 2);
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::AutoCompacting { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn success_between_overflows_rearms_recovery() {
    let (model, _turns) = stream_turns(vec![
        overflow_error_turn(),
        vec![
            tool_event("t1", "read", serde_json::json!({"path":"file.txt"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        overflow_error_turn(),
        done_turn("done"),
    ]);
    let (params, _shared) = recovery_setup();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = overflow_history();
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|_| {},
    )
    .await;
    // Without the reset-on-success the second overflow would fail.
    assert!(matches!(&outcome, RunOutcome::Done { reply } if reply == "done"));
    assert_eq!(model.requests().len(), 4);
}

#[tokio::test]
async fn fatal_errors_fail_immediately() {
    // Rate limits are retryable by design since C.2; a fatal error must
    // still surface without a retry or compaction event.
    let (model, _turns) = stream_turns(vec![vec![MockStreamEvent::Error(
        rig_core::test_utils::MockError::provider("invalid api key"),
    )]]);
    let (params, _shared) = recovery_setup();
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = overflow_history();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    match outcome {
        RunOutcome::Failed(message) => {
            assert!(message.contains("api key"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(model.requests().len(), 1);
    let guard = events.lock().unwrap();
    assert!(
        !guard
            .iter()
            .any(|e| matches!(e, Event::AutoCompacting { .. }))
    );
    assert!(!guard.iter().any(|e| matches!(e, Event::Retry { .. })));
}

#[tokio::test]
async fn overflow_without_compaction_context_fails_as_before() {
    let (model, _turns) = stream_turns(vec![overflow_error_turn()]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = overflow_history();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|_| {},
    )
    .await;
    match outcome {
        RunOutcome::Failed(message) => {
            assert!(message.contains("maximum context length"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

// --- Streaming retry state machine (C.2) ---
#[tokio::test]
async fn rate_limited_stream_is_retried_and_recovers() {
    let (model, _turns) = stream_turns(vec![rate_limit_turn(), done_turn("recovered")]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    assert!(matches!(&outcome, RunOutcome::Done { reply } if reply == "recovered"));
    assert_eq!(model.requests().len(), 2);
    let retries: Vec<_> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Event::Retry {
                attempt,
                message,
                delay_ms,
            } => Some((*attempt, message.clone(), *delay_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].0, 1);
    assert_eq!(retries[0].1, "Rate limited");
}

#[tokio::test]
async fn cancel_mid_stream_commits_the_partial_reply_with_marker() {
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("partial answer"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (flag, cancel) = cancel_channel();
    let emit_flag = flag.clone();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|event| {
            // Cancel as soon as the first text delta reaches the view.
            if matches!(event, Event::TextDelta(_)) {
                emit_flag.set(true);
            }
        },
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Cancelled));
    // The committed history keeps the streamed partial the user saw,
    // then the cancel marker.
    assert_eq!(history.len(), 3, "prompt + partial reply + marker");
    assert_eq!(history[0].text(), "go");
    assert_eq!(history[1].text(), "partial answer");
    assert_eq!(history[2].text(), CANCEL_MARKER);
}

// --- Auth-error reauth wait (E.10) ---
fn auth_error_turn() -> Vec<MockStreamEvent> {
    vec![MockStreamEvent::Error(
        rig_core::test_utils::MockError::provider("401 unauthorized: invalid credentials"),
    )]
}

fn ok_reauth(attempts: std::sync::Arc<std::sync::atomic::AtomicU32>) -> ReauthHook {
    std::sync::Arc::new(move |attempt| {
        attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = attempt;
        Box::pin(std::future::ready(Ok(None)))
    })
}

/// The shared body of the reauth tests: a fresh workspace, a run over
/// `turns`, and the emitted events.
async fn reauth_run(
    turns: Vec<Vec<MockStreamEvent>>,
    params: &RunParams,
    cancel: &CancelToken,
) -> (
    MockCompletionModel,
    RunOutcome,
    std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
) {
    let (model, _turns) = stream_turns(turns);
    let dir = tempfile::tempdir().unwrap();
    // Some tests use a read tool call; keep a file for it.
    std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let mut history = Vec::new();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = events.clone();
    let outcome = run(
        &model,
        params,
        &tools,
        &mut history,
        "continue",
        cancel,
        &move |event| recorded.lock().unwrap().push(event),
    )
    .await;
    (model, outcome, events)
}

fn auth_event_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::AuthRequired { .. }))
        .count()
}

#[tokio::test]
async fn auth_error_without_responder_fails_as_before() {
    let (model, outcome, events) = reauth_run(
        vec![auth_error_turn()],
        &RunParams::default(),
        &cancel_channel().1,
    )
    .await;
    match outcome {
        RunOutcome::Failed(message) => {
            assert!(message.contains("401"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(model.requests().len(), 1);
    assert_eq!(auth_event_count(&events.lock().unwrap()), 0);
}

#[tokio::test]
async fn auth_error_with_responder_waits_and_recovers() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let params = RunParams {
        reauth: Some(ok_reauth(attempts.clone())),
        ..RunParams::default()
    };
    let (model, outcome, events) = reauth_run(
        vec![auth_error_turn(), done_turn("recovered")],
        &params,
        &cancel_channel().1,
    )
    .await;
    assert!(
        matches!(&outcome, RunOutcome::Done { reply } if reply == "recovered"),
        "got {outcome:?}"
    );
    assert_eq!(model.requests().len(), 2, "the turn was retried");
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let guard = events.lock().unwrap();
    assert_eq!(auth_event_count(&guard), 1);
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::AuthRequired { attempt: 1, .. }))
    );
    assert!(!guard.iter().any(|e| matches!(e, Event::Error(_))));
}

#[tokio::test]
async fn persistent_auth_errors_stop_after_max_attempts() {
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let params = RunParams {
        reauth: Some(ok_reauth(attempts.clone())),
        ..RunParams::default()
    };
    let (model, outcome, events) = reauth_run(
        vec![auth_error_turn(), auth_error_turn(), auth_error_turn()],
        &params,
        &cancel_channel().1,
    )
    .await;
    assert!(
        matches!(&outcome, RunOutcome::Failed(m) if m.contains("401")),
        "got {outcome:?}"
    );
    // Two waits, then the third auth error surfaces without waiting.
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        crate::run::MAX_REAUTH_ATTEMPTS
    );
    assert_eq!(model.requests().len(), 3);
    assert_eq!(auth_event_count(&events.lock().unwrap()), 2);
}

#[tokio::test]
async fn success_between_auth_errors_rearms_attempts() {
    let params = RunParams {
        reauth: Some(ok_reauth(std::sync::Arc::new(
            std::sync::atomic::AtomicU32::new(0),
        ))),
        ..RunParams::default()
    };
    let (model, _outcome, events) = reauth_run(
        vec![
            auth_error_turn(),
            auth_error_turn(),
            // A tool call keeps the run alive past this turn (a plain
            // reply would end it in Done).
            vec![
                tool_event("t1", "read", serde_json::json!({"path":"file.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            auth_error_turn(),
            done_turn("done"),
        ],
        &params,
        &cancel_channel().1,
    )
    .await;
    // Without the reset-on-success the third auth error (fourth request)
    // would have exceeded the budget.
    assert_eq!(model.requests().len(), 5);
    assert_eq!(auth_event_count(&events.lock().unwrap()), 3);
}

// `wait_for` blocks a worker thread while polling, so the run needs a
// second thread to make progress toward the reauth wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_during_reauth_wait_ends_cancelled() {
    let (flag, cancel) = cancel_channel();
    // A responder that never resolves: only cancellation can end it.
    let params = RunParams {
        reauth: Some(std::sync::Arc::new(|_attempt| {
            Box::pin(futures::future::pending::<
                Result<Option<crate::providers::DynamicModel>, String>,
            >())
        })),
        ..RunParams::default()
    };
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (model, _turns) = stream_turns(vec![auth_error_turn(), done_turn("never")]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let mut history = Vec::new();
    let recorded = events.clone();
    // The run must make progress while we poll for the AuthRequired
    // event, so it runs on its own task instead of an un-polled future.
    let cancel_spawn = cancel.clone();
    let task = tokio::spawn(async move {
        let emit = move |event: Event| recorded.lock().unwrap().push(event);
        run(
            &model,
            &params,
            &tools,
            &mut history,
            "continue",
            &cancel_spawn,
            &emit,
        )
        .await
    });
    // Let the run reach the reauth wait, then cancel the hanging responder.
    let events_wait = events.clone();
    wait_for(|| {
        events_wait
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::AuthRequired { .. }))
    });
    flag.set(true);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("run task hung after cancellation")
        .expect("run task panicked");
    assert!(matches!(outcome, RunOutcome::Cancelled), "got {outcome:?}");
}

#[tokio::test]
async fn failing_responder_fails_the_run() {
    let params = RunParams {
        reauth: Some(std::sync::Arc::new(|_attempt| {
            Box::pin(std::future::ready(Err("reauth failed".to_owned())))
        })),
        ..RunParams::default()
    };
    let (model, outcome, _) = reauth_run(
        vec![auth_error_turn(), done_turn("never")],
        &params,
        &cancel_channel().1,
    )
    .await;
    assert!(
        matches!(&outcome, RunOutcome::Failed(m) if m == "reauth failed"),
        "got {outcome:?}"
    );
    assert_eq!(model.requests().len(), 1);
}

/// Poll until `condition` holds, bounded; avoids sleeping on a guess.
fn wait_for(condition: impl Fn() -> bool) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("condition never held");
}

#[tokio::test]
async fn declined_compaction_fails_without_a_blind_retry() {
    // Overflow, then a scripted success waiting: with no compaction
    // stage able to run, the run must fail with the provider's error
    // instead of resubmitting the identical oversized prompt.
    let (model, _turns) = stream_turns(vec![overflow_error_turn(), done_turn("never reached")]);
    let (mut params, _shared) = recovery_setup();
    params
        .compaction
        .as_mut()
        .expect("compaction ctx")
        .stages
        .clear();
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let before = overflow_history();
    let mut history = before.clone();
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|_| {},
    )
    .await;
    match outcome {
        RunOutcome::Failed(message) => {
            assert!(
                message.contains("maximum context length"),
                "the overflow error is the honest answer, got: {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        model.requests().len(),
        1,
        "no blind retry of the identical oversized prompt"
    );
    assert_eq!(
        &history[..before.len()],
        &before[..],
        "a declined compaction must not rewrite the caller's history"
    );
    assert!(
        history.len() > before.len(),
        "the failed turn itself is still committed"
    );
}

#[tokio::test]
async fn failed_run_after_overflow_compaction_restores_prefix_and_commits_turn() {
    // Overflow → compact → retry overflows again → budget spent → the
    // run fails. The compaction ran on the caller's history in place; a
    // failed run restores the prior bytes, then commits the failed turn:
    // compaction must not leak, but what already happened stays.
    let (model, _turns) = stream_turns(vec![overflow_error_turn(), overflow_error_turn()]);
    let (params, shared) = recovery_setup();
    shared.lock().unwrap().protect_from(3);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let before = overflow_history();
    let mut history = before.clone();
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Failed(_)));
    assert_eq!(
        model.requests().len(),
        2,
        "recovery compacted and retried once on the compacted history"
    );
    assert_eq!(
        &history[..before.len()],
        &before[..],
        "compaction must not leak into the caller's history"
    );
    assert_eq!(
        shared.lock().unwrap().carry_from(),
        Some(3),
        "the unanswered-input anchor must not follow the rolled-back history"
    );
    assert!(
        history.len() > before.len(),
        "the failed turn is committed, not dropped"
    );
}

#[tokio::test]
async fn overflow_body_without_a_prompt_size_leaves_the_estimator_alone() {
    // Recovery still works when the body names no size; the estimator
    // simply does not recalibrate to a bogus value.
    let (model, _turns) = stream_turns(vec![
        vec![MockStreamEvent::Error(
            rig_core::test_utils::MockError::provider("context window"),
        )],
        done_turn("recovered"),
    ]);
    let (params, shared) = recovery_setup();
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = overflow_history();
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "continue",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(&outcome, RunOutcome::Done { reply } if reply == "recovered"));
    assert_eq!(
        shared.lock().unwrap().estimator.multiplier(),
        1.0,
        "no prompt size parsed ⇒ no recalibration"
    );
}
