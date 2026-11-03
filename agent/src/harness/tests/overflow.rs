//! Stream-error recovery: overflow compaction and retry, rate limits,
//! fallback chains, reauth waits, and the cancel marker — 09's recovery
//! layer and the driver's stop-reason re-run.

use super::*;
use crate::harness::ReauthHook;
use crate::history::{ext, marker};
use animist::agent::{AgentEvent, Cancellation};
use animist::provider::ProviderEvent;

// --- Context-overflow recovery ---
// The in-run half (retry, compaction inside `recover`) is the recovery
// layer; the stop-reason half is the driver's re-run. A failed run KEEPS
// any compaction it did.
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

/// Window big enough that the driver's pre-turn fill check leaves the
/// seeded history alone (its estimate stays under every stage threshold),
/// so the overflow recovery inside the run is the pass that compacts.
fn overflow_window() -> u32 {
    let tokens = crate::compaction::estimate_tokens(&overflow_history());
    ((tokens as f64 * 2.2) as u32).max(1)
}

/// The recovery-stage list: one VCC stage, so overflow recovery compacts
/// without an LLM call eating the scripted attempts.
fn vcc_only_config() -> Config {
    let mut config = Config::parse("").unwrap();
    config.compaction = vec![crate::config::CompactionConfig {
        kind: crate::config::CompactionKind::Vcc,
        context: 0.6,
    }];
    config
}

/// A config with no compaction stage at all: recovery cannot run.
fn no_stage_config() -> Config {
    let mut config = Config::parse("").unwrap();
    config.compaction = Vec::new();
    config
}

/// A conversation over the oversized history, window-sized to overflow.
async fn overflow_conversation(
    provider: &crate::testing::ScriptedProvider,
) -> (Conversation, EventStream) {
    overflow_conversation_over(provider, &vcc_only_config()).await
}

/// [`overflow_conversation`] under `config` (a stage list of its own).
async fn overflow_conversation_over(
    provider: &crate::testing::ScriptedProvider,
    config: &Config,
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
        config,
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

/// A reauth responder for the tests: resolves immediately, either having
/// refreshed credentials in place (the provider reloads them), failing, or
/// never resolving at all.
enum TestReauth {
    Reload,
    Fail,
    Hang,
}

#[async_trait::async_trait]
impl ReauthHook for TestReauth {
    async fn reauth(&self, _attempt: u32) -> std::result::Result<Option<ModelHandle>, String> {
        match self {
            TestReauth::Reload => Ok(None),
            TestReauth::Fail => Err("reauth failed".to_owned()),
            TestReauth::Hang => std::future::pending().await,
        }
    }
}

/// A conversation whose reauth hook is `hook`.
async fn reauth_conversation(
    dir: &tempfile::TempDir,
    provider: &crate::testing::ScriptedProvider,
    hook: TestReauth,
) -> (Conversation, EventStream) {
    let workspace = Workspace::new(dir.path()).unwrap();
    let history = History::default();
    let tools = workspace.registry(&history).unwrap();
    conversation_tuned(
        provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        tools,
        Some(history),
        |builder| builder.reauth(std::sync::Arc::new(hook)),
    )
    .await
}

#[tokio::test]
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
    // Compaction events fired around the recovery, the retry event landed
    // after them and ahead of animist's own `Retrying`, and the estimator
    // recalibrated from the provider-reported prompt size in the error
    // body ("you requested 210744 tokens" → "resulted in 210744").
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
        events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Retry {
                attempt: 1,
                message,
                delay_ms: 0,
            }) if message.contains("compacted and retrying")
        )),
        "the overflow recovery announces its retry like every other kind"
    );
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

/// The overflow stop reason takes the driver's re-run path: the same
/// compact-and-retry, with history showing inactive sources plus the
/// summary and a continuation observation.
#[tokio::test]
async fn stop_reason_overflow_compacts_and_re_runs() {
    // The usage reports a prompt well over the estimate, so recovery
    // recalibrates the estimator from the provider's measurement (the
    // stop-reason form has no error body to parse).
    let measured = (crate::compaction::estimate_tokens(&overflow_history()).max(1) * 2) as u32;
    let provider = script(vec![
        crate::testing::context_overflow_measured(measured),
        done_turn("recovered"),
    ]);
    let (conversation, mut stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 2);
    assert!(
        conversation
            .compaction()
            .lock()
            .await
            .estimator
            .multiplier()
            > 1.0,
        "the provider-measured prompt size recalibrates the estimator"
    );
    let entries = entries(&conversation).await;
    assert!(
        entries.iter().any(|entry| !entry.is_active()),
        "compaction left inactive sources behind"
    );
    assert!(
        entries
            .iter()
            .filter_map(|entry| entry.current())
            .any(|stored| marker::is(&stored, marker::CONTINUATION)),
        "the re-run continued from a continuation observation"
    );
    let _ = events_until_done(&mut stream).await;
}

/// An error-form recovery followed by real progress rearms the shared
/// budget: a later stop-reason overflow still gets its own compact-and-
/// re-run. Only *consecutive* overflows share the budget.
#[tokio::test]
async fn stop_reason_overflow_after_an_error_recovery_still_recovers() {
    let provider = script(vec![
        overflow_error_turn(),
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        crate::testing::context_overflow(),
        done_turn("recovered"),
    ]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    assert_eq!(
        provider.requests().len(),
        4,
        "error recovery, tool turn, the re-run after the stop-reason recovery"
    );
}

/// A re-run that made real progress between stop-reason overflows earns a
/// fresh compact-and-retry: the follow-up's tool steps are successful
/// streams since the last charge, exactly like main's reset.
#[tokio::test]
async fn stop_reason_overflow_after_progress_still_recovers() {
    let provider = script(vec![
        crate::testing::context_overflow(),
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        crate::testing::context_overflow(),
        done_turn("recovered"),
    ]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    // run1's CWE, the re-run's two tool steps, its CWE, the second re-run.
    assert_eq!(provider.requests().len(), 5);
}

/// A second consecutive stop-reason overflow surfaces the failure: the
/// budget allowed exactly one compact-and-retry.
#[tokio::test]
async fn second_stop_reason_overflow_surfaces_the_error() {
    let provider = script(vec![
        crate::testing::context_overflow(),
        crate::testing::context_overflow(),
    ]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(m) if m.contains("context window")),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
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
    // Rate limits are retryable by design; a fatal error must
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

/// An overflow with no compaction stage configured is an honest failure:
/// the provider's error surfaces and nothing rewrites history.
#[tokio::test]
async fn overflow_without_compaction_stages_fails_as_before() {
    let provider = script(vec![overflow_error_turn()]);
    let (conversation, mut stream) =
        overflow_conversation_over(&provider, &no_stage_config()).await;
    let outcome = run_turn(&conversation, "continue").await;
    match outcome {
        TurnOutcome::Failed(message) => {
            assert!(message.contains("maximum context length"), "got: {message}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(provider.requests().len(), 1);
    let events = events_until_done(&mut stream).await;
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::AutoCompacting { .. })
    )));
}

// --- Streaming retry state machine ---
#[tokio::test(start_paused = true)]
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

/// The full rate-limit ladder, in order: key rotation, fallback hop, then
/// plain backoff on the last hop. Paused time keeps the waits instant.
#[tokio::test(start_paused = true)]
async fn rate_limit_rotates_then_fails_over_then_backs_off() {
    let provider = script(vec![
        rate_limit_turn(),
        rate_limit_turn(),
        rate_limit_turn(),
        rate_limit_turn(),
        done_turn("recovered"),
    ]);
    // The rotation hook succeeds exactly once, then reports exhaustion.
    let rotations = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let rotate: crate::harness::retry::RotateKey = {
        let rotations = std::sync::Arc::clone(&rotations);
        std::sync::Arc::new(move || {
            rotations.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0
        })
    };
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let fallback = ModelHandle::scripted(crate::testing::scripted_model(
        &provider,
        crate::testing::model_info("fallback-1", 200_000, 8_192, false, false),
    ));
    let (conversation, mut stream) = conversation_tuned(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
        |builder| {
            builder.retry(crate::harness::retry::RetryCtx {
                rotate: Some(rotate),
                fallbacks: vec![fallback],
            })
        },
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    // The primary served the first two attempts (rotation, then the failed
    // hop-over), the fallback served the rest — and keeps serving: a hop
    // now sticks for the rest of the run.
    let models: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| request.model.clone())
        .collect();
    assert_eq!(
        models,
        [
            "scripted",
            "scripted",
            "fallback-1",
            "fallback-1",
            "fallback-1"
        ]
    );
    let retries: Vec<(String, u64)> = events_until_done(&mut stream)
        .await
        .into_iter()
        .filter_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::Retry {
                message, delay_ms, ..
            }) => Some((message, delay_ms)),
            _ => None,
        })
        .collect();
    let messages: Vec<&str> = retries.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(
        messages,
        [
            "rotated API key; retrying",
            "key rotation exhausted; failing over to fallback-1",
            "Rate limited",
            "Rate limited",
        ],
        "rotation → fallback → backoff, in that order"
    );
    // The rotation paces briefly and the backoff honors the server's
    // "retry after 30s" as a floor; the failover hop carries no delay.
    assert!(retries[0].1 > 0, "rotation paces its retry");
    assert_eq!(retries[1].1, 0, "the failover hop is immediate");
    assert!(
        retries[2].1 >= 30_000 && retries[3].1 >= 30_000,
        "backoff honors the server-named wait: {retries:?}"
    );
}

/// A Bedrock 429 was already retried inside animist: recovery goes
/// straight to the fallback with no craft backoff sleep.
#[tokio::test(start_paused = true)]
async fn bedrock_429_goes_straight_to_fallback_without_a_backoff() {
    let provider = script(vec![
        crate::testing::bedrock_http_error(429),
        done_turn("ok"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let fallback = ModelHandle::scripted(crate::testing::scripted_model(
        &provider,
        crate::testing::model_info("fallback-1", 200_000, 8_192, false, false),
    ));
    let (conversation, mut stream) = conversation_tuned(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
        |builder| {
            builder.retry(crate::harness::retry::RetryCtx {
                rotate: None,
                fallbacks: vec![fallback],
            })
        },
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "ok"),
        "{outcome:?}"
    );
    let models: Vec<String> = provider
        .requests()
        .iter()
        .map(|request| request.model.clone())
        .collect();
    assert_eq!(models, ["scripted", "fallback-1"]);
    let retries: Vec<(String, u64)> = events_until_done(&mut stream)
        .await
        .into_iter()
        .filter_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::Retry {
                message, delay_ms, ..
            }) => Some((message, delay_ms)),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1, "no same-model backoff stacked on it");
    assert_eq!(retries[0].1, 0, "the failover hop is immediate");
    assert!(retries[0].0.contains("failing over to fallback-1"));
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

// --- Auth-error reauth wait ---
#[tokio::test]
async fn auth_error_without_responder_fails_as_before() {
    // Without a reauth hook, an auth error is an honest failure.
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
async fn auth_error_with_responder_waits_and_recovers() {
    let provider = script(vec![auth_error_turn(), done_turn("recovered")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = reauth_conversation(&dir, &provider, TestReauth::Reload).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "got {outcome:?}"
    );
    assert_eq!(provider.requests().len(), 2, "the turn was retried");
    // The reload path fired on the serving provider before the retry.
    assert_eq!(provider.reload_count(), 1);
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
async fn persistent_auth_errors_stop_after_max_attempts() {
    let provider = script(vec![
        auth_error_turn(),
        auth_error_turn(),
        auth_error_turn(),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = reauth_conversation(&dir, &provider, TestReauth::Reload).await;
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
    let (conversation, mut stream) = reauth_conversation(&dir, &provider, TestReauth::Reload).await;
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
async fn cancelling_during_reauth_wait_ends_cancelled() {
    // A responder that never resolves: only cancellation can end the wait.
    let provider = script(vec![auth_error_turn(), done_turn("never")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = reauth_conversation(&dir, &provider, TestReauth::Hang).await;
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
async fn failing_responder_fails_the_run() {
    let provider = script(vec![auth_error_turn(), done_turn("never")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = reauth_conversation(&dir, &provider, TestReauth::Fail).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(m) if m.contains("reauth failed")),
        "got {outcome:?}"
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn declined_compaction_fails_without_a_blind_retry() {
    // Overflow, then a scripted success waiting: with no compaction stage
    // able to run, the run must fail with the provider's error instead of
    // resubmitting the identical oversized prompt.
    let provider = script(vec![overflow_error_turn(), done_turn("never reached")]);
    let (conversation, _stream) = overflow_conversation_over(&provider, &no_stage_config()).await;
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

/// A failed run KEEPS the compaction it did — history
/// keeps sources, so nothing is lost — and the failed turn still commits.
#[tokio::test]
async fn failed_run_after_overflow_compaction_keeps_the_rewrite() {
    // Overflow → compact → retry overflows again → budget spent → the run
    // fails. The compaction stays in the history; the failed turn commits
    // on top of it.
    let provider = script(vec![overflow_error_turn(), overflow_error_turn()]);
    let (conversation, _stream) = overflow_conversation(&provider).await;
    let before = stored(&conversation).await;
    let outcome = run_turn(&conversation, "continue").await;
    assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(
        provider.requests().len(),
        2,
        "recovery compacted and retried once on the compacted history"
    );
    let after = stored(&conversation).await;
    assert!(
        after.len() < before.len() + 2,
        "the compaction stayed: the compacted history is far smaller than \
         the seeded one plus the failed turn"
    );
    assert!(
        conversation
            .compaction()
            .lock()
            .await
            .carry_from()
            .is_some(),
        "the failed turn's input stays protected"
    );
    assert!(
        !after.is_empty(),
        "the failed turn is committed, not dropped"
    );
}

#[tokio::test]
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
