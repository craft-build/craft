//! Stream-error recovery: overflow compaction and retry, rate limits,
//! reauth waits, and the cancel marker. Recovery machinery returns with
//! the 08/09 layers; the failing paths below are live now.

use super::*;
use crate::history::{ext, marker};
use animist::agent::{AgentEvent, Cancellation};
use animist::provider::ProviderEvent;

// --- Context-overflow recovery (C.5, restored in 09) ---
// The in-run half (retry, compaction inside `recover`) is 09's
// `RecoveryLayer`; 08 built the compactors it will call, so these stay
// 09-tagged. One premise also flips with README decision 11: a failed run
// KEEPS any compaction it did.
fn overflow_error_turn() -> Attempt {
    // The body must name a size beyond the run's overhead-inclusive
    // estimate (history + tool schemas) for recalibration to trigger.
    crate::testing::context_overflow_http()
}

fn rate_limit_turn() -> Attempt {
    crate::testing::http_error(429, "rate limit exceeded, retry after 30s")
}

fn done_turn(text: &str) -> Attempt {
    crate::testing::text_turn(text)
}

fn auth_error_turn() -> Attempt {
    crate::testing::http_error(401, "401 unauthorized: invalid credentials")
}

fn overflow_history() -> Vec<StoredMessage> {
    use crate::compaction::test_support as ts;
    // Repeated 50x so the history dwarfs the fixed tool-schema overhead
    // (~14k tokens): these tests exercise a *history*-driven overflow that
    // compaction can actually cure, matching the production premise.
    let mut messages = Vec::new();
    for round in 0..8 * 50 {
        let i = round % 8;
        messages.push(ts::stored_user(&format!(
            "do task {i} with a fairly long instruction"
        )));
        // Wire tool-call ids must be unique across the whole history
        // (replay validation rejects duplicates), so each round mints its
        // own while the args keep the per-task variety.
        messages.push(ts::stored_assistant_tool_args(
            &format!("t{round}"),
            "bash",
            serde_json::json!({"command": format!("echo {i}")}),
        ));
        messages.push(ts::stored_tool_result_of(
            &format!("t{round}"),
            &"x".repeat(200),
        ));
    }
    messages
}

/// Window sized so the seeded history overflows the buffer-subtracted
/// window and a compaction stage is forced to run.
fn overflow_window() -> u32 {
    let tokens = crate::compaction::estimate_tokens(&overflow_history());
    ((tokens as f64 / 0.9) as u32).max(1)
}

/// A conversation over the oversized history, window-sized to overflow.
async fn overflow_conversation(
    provider: &crate::testing::ScriptedProvider,
) -> (Conversation, EventStream) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(crate::testing::scripted_model(
        provider,
        crate::testing::model_info("scripted", overflow_window(), 8_192, false, false),
    ));
    let history = seeded_history(overflow_history()).await;
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        workspace.registry(&history).unwrap(),
        sink,
    )
    .history(history)
    .build()
    .expect("conversation builds");
    (conversation, stream)
}

fn auth_event_count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Craft(crate::harness::CraftEvent::AuthRequired { .. })
            )
        })
        .count()
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn overflow_recovers_compacts_and_retries() {
    let provider = script(vec![overflow_error_turn(), done_turn("recovered")]);
    let (conversation, mut stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    // The retried request was rebuilt from the compacted history.
    assert_eq!(provider.requests().len(), 2);
    // Compaction events fired around the recovery, and the estimator
    // recalibrated from the provider-reported prompt size in the error
    // body ("you requested 8192 tokens").
    let events = events_until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::AutoCompacting { .. })
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::CompactionDone { .. })
    )));
    assert!(
        conversation
            .compaction()
            .lock()
            .await
            .estimator
            .multiplier()
            > 1.0,
        "calibration uses the provider-reported prompt size"
    );
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn second_consecutive_overflow_surfaces_the_error() {
    let provider = script(vec![overflow_error_turn(), overflow_error_turn()]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    match outcome {
        TurnOutcome::Failed(message) => {
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
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn success_between_overflows_rearms_recovery() {
    let provider = script(vec![
        overflow_error_turn(),
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        overflow_error_turn(),
        done_turn("done"),
    ]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    // Without the reset-on-success the second overflow would fail.
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "done"),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 4);
}

#[tokio::test]
async fn fatal_errors_fail_immediately() {
    // Rate limits are retryable by design since C.2; a fatal error must
    // still surface without a retry or compaction event.
    let provider = script(vec![crate::testing::http_error(400, "invalid api key")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    match outcome {
        TurnOutcome::Failed(message) => {
            assert!(message.contains("api key"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
    let events = events_until_done(&mut stream).await;
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::AutoCompacting { .. })
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::Retry { .. })
    )));
}

#[tokio::test]
async fn overflow_without_compaction_context_fails_as_before() {
    // Until 09 wires recovery into the turn, an overflow is an honest
    // failure: the provider's error surfaces unchanged (the between-runs
    // engine of 08 runs before the turn, not inside it).
    let provider = script(vec![overflow_error_turn()]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    match outcome {
        TurnOutcome::Failed(message) => {
            assert!(message.contains("maximum context length"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
}

// --- Streaming retry state machine (C.2, restored in 09) ---
#[tokio::test]
#[ignore = "restored in 09"]
async fn rate_limited_stream_is_retried_and_recovers() {
    let provider = script(vec![rate_limit_turn(), done_turn("recovered")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 2);
    let retries: Vec<u32> = events_until_done(&mut stream)
        .await
        .into_iter()
        .filter_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::Retry { attempt, .. }) => Some(attempt),
            _ => None,
        })
        .collect();
    assert_eq!(retries, vec![1]);
}

#[tokio::test]
async fn cancel_mid_stream_commits_the_partial_reply_with_marker() {
    let provider = script(vec![crate::testing::text_turn("partial answer").blocked()]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;

    let cancel = Cancellation::default();
    let turn = {
        let cancel = cancel.clone();
        let conversation = conversation.clone();
        tokio::spawn(
            async move { crate::harness::run_turn(&conversation, "go", &[], cancel).await },
        )
    };
    loop {
        match next_event(&mut stream).await {
            Event::Agent(AgentEvent::Provider {
                event: ProviderEvent::TextDelta { .. },
                ..
            }) => break,
            _ => continue,
        }
    }
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the cancelled turn finishes")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "{outcome:?}");
    // The committed history keeps the streamed partial the user saw,
    // then the cancel marker.
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 3, "prompt + partial reply + marker");
    assert_eq!(ext::message_text(&messages[0]), "go");
    assert_eq!(ext::message_text(&messages[1]), "partial answer");
    assert!(marker::is(&messages[2], marker::CANCEL));
    let _ = events_until_done(&mut stream).await;
}

// --- Auth-error reauth wait (E.10, restored in 09) ---
#[tokio::test]
async fn auth_error_without_responder_fails_as_before() {
    // Until 09 wires the reauth hook, an auth error is an honest failure.
    let provider = script(vec![auth_error_turn()]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    match outcome {
        TurnOutcome::Failed(message) => {
            assert!(message.contains("401"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(auth_event_count(&events_until_done(&mut stream).await), 0);
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn auth_error_with_responder_waits_and_recovers() {
    let provider = script(vec![auth_error_turn(), done_turn("recovered")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "got {outcome:?}"
    );
    assert_eq!(provider.requests().len(), 2, "the turn was retried");
    let events = events_until_done(&mut stream).await;
    assert_eq!(auth_event_count(&events), 1);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::AuthRequired { attempt: 1, .. })
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Craft(crate::harness::CraftEvent::Error(_))))
    );
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn persistent_auth_errors_stop_after_max_attempts() {
    let provider = script(vec![
        auth_error_turn(),
        auth_error_turn(),
        auth_error_turn(),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(m) if m.contains("401")),
        "got {outcome:?}"
    );
    // Two waits, then the third auth error surfaces without waiting.
    assert_eq!(provider.requests().len(), 3);
    assert_eq!(auth_event_count(&events_until_done(&mut stream).await), 2);
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn success_between_auth_errors_rearms_attempts() {
    let provider = script(vec![
        auth_error_turn(),
        auth_error_turn(),
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        auth_error_turn(),
        done_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "done"),
        "{outcome:?}"
    );
    // Without the reset-on-success the third auth error (fourth request)
    // would have exceeded the budget.
    assert_eq!(provider.requests().len(), 5);
    assert_eq!(auth_event_count(&events_until_done(&mut stream).await), 3);
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn cancelling_during_reauth_wait_ends_cancelled() {
    // A responder that never resolves: only cancellation can end the wait.
    let provider = script(vec![auth_error_turn(), done_turn("never")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let cancel = Cancellation::default();
    let turn = {
        let task_cancel = cancel.clone();
        let conversation = conversation.clone();
        tokio::spawn(async move {
            crate::harness::run_turn(&conversation, "continue", &[], task_cancel).await
        })
    };
    // Let the run reach the reauth wait, then cancel the hanging responder.
    loop {
        match next_event(&mut stream).await {
            Event::Craft(crate::harness::CraftEvent::AuthRequired { .. }) => break,
            Event::Craft(crate::harness::CraftEvent::Done { .. }) => {
                panic!("the run must park on the reauth wait, not finish")
            }
            _ => continue,
        }
    }
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("run task hung after cancellation")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "got {outcome:?}");
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn failing_responder_fails_the_run() {
    let provider = script(vec![auth_error_turn(), done_turn("never")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(m) if m == "reauth failed"),
        "got {outcome:?}"
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn declined_compaction_fails_without_a_blind_retry() {
    // Overflow, then a scripted success waiting: with no compaction stage
    // able to run, the run must fail with the provider's error instead of
    // resubmitting the identical oversized prompt.
    let provider = script(vec![overflow_error_turn(), done_turn("never reached")]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let before = stored(&conversation).await;
    let outcome = run_turn(&conversation, "continue").await;
    match outcome {
        TurnOutcome::Failed(message) => {
            assert!(
                message.contains("maximum context length"),
                "the overflow error is the honest answer, got: {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(
        provider.requests().len(),
        1,
        "no blind retry of the identical oversized prompt"
    );
    let after = stored(&conversation).await;
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "a declined compaction must not rewrite the history"
    );
    assert!(
        after.len() > before.len(),
        "the failed turn itself is still committed"
    );
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn failed_run_after_overflow_compaction_restores_prefix_and_commits_turn() {
    // Overflow → compact → retry overflows again → budget spent → the
    // run fails. The compaction ran on the history in place; a failed run
    // restores the prior entries, then commits the failed turn: compaction
    // must not leak, but what already happened stays.
    let provider = script(vec![overflow_error_turn(), overflow_error_turn()]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    conversation.compaction().lock().await.protect_from(3);
    let before = stored(&conversation).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(
        provider.requests().len(),
        2,
        "recovery compacted and retried once on the compacted history"
    );
    let after = stored(&conversation).await;
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "compaction must not leak into the history"
    );
    assert_eq!(
        conversation.compaction().lock().await.carry_from(),
        Some(3),
        "the unanswered-input anchor must not follow the rolled-back history"
    );
    assert!(
        after.len() > before.len(),
        "the failed turn is committed, not dropped"
    );
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn overflow_body_without_a_prompt_size_leaves_the_estimator_alone() {
    // Recovery still works when the body names no size; the estimator
    // simply does not recalibrate to a bogus value.
    let provider = script(vec![
        crate::testing::http_error(400, "context window"),
        done_turn("recovered"),
    ]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    assert_eq!(
        conversation
            .compaction()
            .lock()
            .await
            .estimator
            .multiplier(),
        1.0,
        "no prompt size parsed ⇒ no recalibration"
    );
}
