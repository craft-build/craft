//! Turn lifecycle: multi-turn commits, truncation and continuations, doom,
//! cancel boundaries, nudges, and snapshot commits.

use super::*;
use crate::history::{ContentBlock, Role, ext};
use crate::run::*;

#[tokio::test]
async fn multi_turn_tool_round_trip_commits_history() {
    let (model, provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        crate::testing::text_turn("all done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "read the file",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    let guard = events.lock().unwrap();
    assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "all done"));
    // user prompt, assistant tool call, tool result, final assistant text
    assert_eq!(history.len(), 4);
    // The call id is the run-stable internal id the stream minted, not
    // the scripted "t1"; the result must answer whatever id was recorded.
    let assistant = history[1].message();
    assert_eq!(
        assistant.role,
        Role::Assistant,
        "assistant tool-call message"
    );
    let ContentBlock::ToolUse { id, name, .. } = &assistant.content[0] else {
        panic!("tool-call block");
    };
    assert_eq!(name, "read");
    let result_message = history[2].message();
    assert_eq!(result_message.role, Role::User, "tool result message");
    assert!(matches!(&result_message.content[0],
        ContentBlock::ToolResult { tool_use_id, name, .. }
            if tool_use_id == id && name.as_deref() == Some("read")));
    // The result the model sees on the next call is literal text.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let replayed = &requests[1].messages;
    assert_eq!(replayed[2], *history[2].message());
    // Events: tool start, tool done, usage per call, results submitted
    // per wave, one terminal Done.
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::ToolStart { name, .. } if name == "read"))
    );
    assert!(guard.iter().any(|e| matches!(
        e,
        Event::ToolDone {
            result: ContentBlock::ToolResult {
                is_error: false,
                ..
            },
            ..
        }
    )));
    assert_eq!(
        guard
            .iter()
            .filter(|e| matches!(e, Event::TurnComplete { .. }))
            .count(),
        2
    );
    assert!(guard.iter().any(|e| matches!(e,
            Event::ToolResultsSubmitted { message }
                if message.role == Role::User && matches!(
                    &message.content[0], ContentBlock::ToolResult { .. }))));
    assert!(matches!(
        guard.iter().last(),
        Some(Event::Done {
            reason: DoneReason::Stop,
            num_turns: 2,
            ..
        })
    ));
}

#[tokio::test]
async fn snapshots_commit_on_done_and_undo_restores_files() {
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "write",
            serde_json::json!({"path":"f.txt","content":"changed\n"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "original\n").unwrap();
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    let snapshots = workspace.snapshots().clone();
    let tools = workspace.register();
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "write it",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(snapshots.undo_depth(), 1, "session committed on Done");
    let message = snapshots.rollback().await.unwrap();
    assert!(message.contains("rolled back"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn truncated_reply_continues_then_finishes() {
    // Truncated, truncated, then a clean stop: the run continues the
    // truncated replies and ends Done with the final text.
    let (model, provider) = stream_turns(vec![
        crate::testing::max_tokens_turn("part one "),
        crate::testing::max_tokens_turn("part two "),
        crate::testing::text_turn("part three"),
    ]);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &ToolDispatch::default(),
        &mut history,
        "tell me a long story",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(
        matches!(outcome, RunOutcome::Done { ref reply } if reply == "part three"),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 3);
    // prompt + three assistant messages, all committed
    assert_eq!(history.len(), 4);
    assert_eq!(ext::message_text(&history[1]), "part one ");
}

#[tokio::test]
async fn truncation_gives_up_after_the_continuation_budget() {
    // Every reply truncates: after `max_continuation_turns` continuations
    // the run ends MaxTokens with the truncated tail committed.
    let (model, provider) = stream_turns(vec![
        crate::testing::max_tokens_turn("a"),
        crate::testing::max_tokens_turn("b"),
        crate::testing::max_tokens_turn("c"),
        crate::testing::max_tokens_turn("d"),
    ]);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &ToolDispatch::default(),
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(
        matches!(outcome, RunOutcome::MaxTokens { ref reply } if reply == "d"),
        "{outcome:?}"
    );
    // 1 initial + 3 continuations (the default budget).
    assert_eq!(provider.requests().len(), 4);
    assert_eq!(history.len(), 5, "truncated tail is committed");
}

#[tokio::test]
async fn continuation_budget_is_configurable_to_zero() {
    let (model, provider) = stream_turns(vec![crate::testing::max_tokens_turn("cut off")]);
    let (_, cancel) = cancel_channel();
    let params = RunParams {
        max_continuation_turns: 0,
        ..RunParams::default()
    };
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &params,
        &ToolDispatch::default(),
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::MaxTokens { .. }));
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn continuations_respect_the_turn_budget() {
    // One clean turn budget, a truncated first reply: the continuation
    // counts against `max_turns` and the run ends MaxTurns sanitized.
    let (model, provider) = stream_turns(vec![
        crate::testing::max_tokens_turn("a"),
        crate::testing::max_tokens_turn("b"),
    ]);
    let (_, cancel) = cancel_channel();
    let params = RunParams {
        max_turns: 1,
        ..RunParams::default()
    };
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &params,
        &ToolDispatch::default(),
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::MaxTurns));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(ext::message_text(history.last().unwrap()), END_MARKER);
}

#[tokio::test]
async fn empty_truncated_reply_is_committed_as_max_tokens() {
    // A truncation with zero visible text is not an empty-and-stalled
    // turn: the cut-off (empty) assistant message is committed and the
    // run ends MaxTokens instead of firing the nudge path.
    let (model, provider) = stream_turns(vec![crate::testing::max_tokens_turn("")]);
    let (_, cancel) = cancel_channel();
    let params = RunParams {
        max_continuation_turns: 0,
        ..RunParams::default()
    };
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &params,
        &ToolDispatch::default(),
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::MaxTokens { .. }));
    assert_eq!(provider.requests().len(), 1, "no nudge was sent");
    assert_eq!(history.len(), 2);
    // prompt + the truncated assistant message (empty, not replaced by
    // the empty-response marker)
    assert_ne!(ext::message_text(&history[1]), nudge::EMPTY_RESPONSE_MARKER);
}

#[tokio::test]
async fn guardrails_warn_then_block_repeated_failures() {
    // Seven failing reads with varied arguments (identical arguments
    // would hit the doom-loop block first): the any-failure counters
    // warn at 3 and block at 6, so the fourth call carries the warning
    // and the seventh is blocked.
    let read = |n: usize| {
        tool_event(
            "t",
            "read",
            serde_json::json!({ "path": format!("missing{n}") }),
        )
    };
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read(1)]),
        crate::testing::tool_turn(vec![read(2)]),
        crate::testing::tool_turn(vec![read(3)]),
        crate::testing::tool_turn(vec![read(4)]),
        crate::testing::tool_turn(vec![read(5)]),
        crate::testing::tool_turn(vec![read(6)]),
        crate::testing::tool_turn(vec![read(7)]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_guardrails(crate::run::shared_guardrails());
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let events = std::sync::Mutex::new(Vec::new());
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|ev| events.lock().expect("events lock").push(ev),
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let events = events.into_inner().expect("events lock");
    let infos: Vec<&str> = events
        .iter()
        .filter_map(|ev| match ev {
            Event::Info(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let warns = infos
        .iter()
        .filter(|text| text.starts_with("guardrail warning"))
        .count();
    let blocks = infos
        .iter()
        .filter(|text| text.starts_with("guardrail blocked"))
        .count();
    assert!(warns >= 1, "a guardrail warn emits Event::Info: {infos:?}");
    assert_eq!(
        blocks, 1,
        "the guardrail block emits one Event::Info: {infos:?}"
    );
    let results: Vec<String> = history
        .iter()
        .flat_map(|stored| ext::tool_results(stored.message()))
        .map(|block| {
            let ContentBlock::ToolResult { content, .. } = block else {
                panic!("tool result block");
            };
            ext::tool_content_text(content)
        })
        .collect();
    assert_eq!(results.len(), 7);
    assert!(!results[0].contains("guardrail"));
    assert!(
        results[3].starts_with("[guardrail]"),
        "warn at the fourth call: {}",
        results[3]
    );
    assert!(
        results[6].contains("blocked by guardrails"),
        "block at the seventh call: {}",
        results[6]
    );
}

#[tokio::test]
async fn failed_run_preserves_prior_history_and_commits_the_turn() {
    // A failed run restores the caller's pristine bytes (the strip and
    // any in-place compaction must not leak) but still commits the
    // failed turn itself — sanitized, with a failure note — because tool
    // calls that already ran have real workspace effects.
    let (model, _provider) = stream_turns(vec![crate::testing::http_error(400, "boom")]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = vec![ext::stored_user_text(doom::GRACE_CALL_PROMPT)];
    let before = history.clone();
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
    assert!(matches!(outcome, RunOutcome::Failed(_)));
    assert_eq!(
        &history[..before.len()],
        &before[..],
        "the prior history is restored byte-identically"
    );
    // Grace prompt + failed-turn prompt + end marker + failure note.
    assert_eq!(history.len(), 4);
    assert_eq!(
        ext::message_text(&history[1]),
        "go",
        "the failed turn's prompt is committed"
    );
    let note = ext::message_text(&history[3]);
    assert!(
        note.starts_with("[Run failed: ") && note.contains("boom"),
        "the turn and its failure mode are committed, got: {note}"
    );
}

#[tokio::test]
async fn doom_loop_scores_reach_grace_then_summarize() {
    // One batch of identical failing reads: two errors (+1 each) and one
    // blocked doom-loop call (+15) put the score at 17 — past the grace
    // threshold — so the next request carries the one-shot grace prompt.
    // Distinct ids per call: a provider streams each tool use with its own
    // id, and replay validation rejects duplicates.
    let read = |n: usize| {
        tool_event(
            &format!("t{n}"),
            "read",
            serde_json::json!({"path": "missing"}),
        )
    };
    let (model, provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read(1), read(2), read(3)]),
        crate::testing::text_turn("summarized"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let events = std::sync::Mutex::new(Vec::new());
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|ev| events.lock().expect("events lock").push(ev),
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "summarized"));
    let grace_count = history
        .iter()
        .filter(|m| ext::message_text(m) == doom::GRACE_CALL_PROMPT)
        .count();
    assert_eq!(grace_count, 1, "grace prompt fired exactly once");
    let stagnation_count = events
        .lock()
        .expect("events lock")
        .iter()
        .filter(|ev| matches!(ev, Event::StagnationDetected { .. }))
        .count();
    assert_eq!(
        stagnation_count, 1,
        "grace posts exactly one StagnationDetected"
    );
    // The blocked call's result carries the doom-loop warning.
    assert!(history.iter().any(|stored| {
        let message = stored.message();
        message.role == Role::User
            && matches!(&message.content[0],
                ContentBlock::ToolResult { is_error: true, content, .. }
                    if ext::tool_content_text(content).contains("stuck in a loop"))
    }));
    // The second request saw the grace prompt as its trailing message.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let replayed = &requests[1].messages;
    assert_eq!(
        replayed.last().map(ext::text),
        Some(doom::GRACE_CALL_PROMPT.to_owned())
    );
}

#[tokio::test]
async fn doom_score_hard_stops_the_run() {
    // Two doom-loop batches push the score past the hard-stop threshold
    // (25); the run ends DoomStop with sanitized partial history.
    // Distinct ids per call: a provider streams each tool use with its own
    // id, and replay validation rejects duplicates.
    let read = |n: usize| {
        tool_event(
            &format!("t{n}"),
            "read",
            serde_json::json!({"path": "missing"}),
        )
    };
    let (model, provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![read(4), read(5), read(6)]),
        crate::testing::tool_turn(vec![read(1), read(2), read(3)]),
        // Never requested: the hard stop preempts the third turn.
        crate::testing::text_turn("never"),
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
    assert!(matches!(outcome, RunOutcome::DoomStop));
    assert_eq!(
        provider.requests().len(),
        2,
        "no request after the hard stop"
    );
    assert_eq!(
        history.last().map(ext::message_text),
        Some(END_MARKER.to_owned())
    );
}

#[tokio::test]
async fn failed_run_emits_error_then_done_with_error_reason() {
    let (model, _provider) = stream_turns(vec![crate::testing::http_error(400, "boom")]);
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
    assert!(matches!(outcome, RunOutcome::Failed(_)));
    let guard = events.lock().unwrap();
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::Error(message) if message.contains("boom")))
    );
    assert!(matches!(
        guard.iter().last(),
        Some(Event::Done {
            reason: DoneReason::Error,
            ..
        })
    ));
}

#[tokio::test]
async fn turn_complete_reports_a_nonzero_context_size() {
    let (model, _provider) = stream_turns(vec![crate::testing::text_turn("ok")]);
    let (_, cancel) = cancel_channel();
    let mut history = vec![ext::stored_user_text(
        "a sufficiently long prompt to register a nonzero token estimate \
         beyond the estimator floor, padded out for good measure."
            .repeat(4),
    )];
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let _ = run(
        &model,
        &RunParams::default(),
        &ToolDispatch::default(),
        &mut history,
        "go",
        &cancel,
        &|event| events.lock().unwrap().push(event),
    )
    .await;
    assert!(
        events.lock().unwrap().iter().any(|e| matches!(
            e,
            Event::TurnComplete {
                context_size: size,
                ..
            } if *size > 0
        )),
        "context estimate must be reported"
    );
}

#[tokio::test]
async fn max_turns_bound_commits_sanitized_partial_history() {
    let turns: Vec<crate::testing::Attempt> = (0..3)
        .map(|i| {
            crate::testing::tool_turn(vec![tool_event(
                &format!("t{i}"),
                "read",
                serde_json::json!({"path":"f"}),
            )])
        })
        .collect();
    let (model, provider) = stream_turns(turns);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "x\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let params = RunParams {
        max_turns: 2,
        ..RunParams::default()
    };
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "loop",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::MaxTurns));
    // Prompt + 2x(assistant+result) + end marker; the third call never ran.
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(history.len(), 6);
    assert_eq!(ext::message_text(history.last().unwrap()), END_MARKER);
    // No dangling tool calls: every call has its result.
    let calls: Vec<String> = history
        .iter()
        .flat_map(|stored| ext::tool_calls(stored.message()))
        .map(|block| {
            let ContentBlock::ToolUse { id, .. } = block else {
                panic!("tool call block");
            };
            id.clone()
        })
        .collect();
    let answered: Vec<String> = history
        .iter()
        .flat_map(|stored| ext::tool_results(stored.message()))
        .map(|block| {
            let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                panic!("tool result block");
            };
            tool_use_id.clone()
        })
        .collect();
    assert!(calls.iter().all(|id| answered.contains(id)));
}

#[tokio::test]
async fn failed_run_commits_sanitized_partial_history() {
    // The turn that failed keeps what already happened: a resolved tool
    // result survives, dangling calls are closed, and a failure note
    // records why the run stopped — so the model (or a resumed session)
    // starts from what actually happened.
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::http_error(400, "boom"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "x\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = vec![ext::stored_user_text("earlier")];
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
    assert!(matches!(outcome, RunOutcome::Failed(_)));
    // earlier + prompt + assistant + result + end marker + failure note.
    assert_eq!(history.len(), 6);
    assert_eq!(ext::message_text(&history[4]), END_MARKER);
    let note = ext::message_text(&history[5]);
    assert!(
        note.starts_with("[Run failed: ") && note.contains("boom"),
        "the failure note ends the committed turn, got: {note}"
    );
}

#[tokio::test]
async fn cancel_mid_tool_skips_execution_and_commits_sanitized_partial() {
    // The before-execution hook denies the call by stopping the run,
    // standing in for a cancellation arriving at the dispatch boundary.
    struct StopAll;
    impl BeforeExecute for StopAll {
        fn decide(&self, _call: ContentBlock) -> BoxFuture<Decision> {
            Box::pin(async { Decision::Stop("cancelled".into()) })
        }
    }
    let (model, _provider) = stream_turns(vec![crate::testing::tool_turn(vec![tool_event(
        "t1",
        "read",
        serde_json::json!({"path": "f"}),
    )])]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "secret\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path())
        .unwrap()
        .register()
        .with_before(std::sync::Arc::new(StopAll));
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
    assert!(matches!(outcome, RunOutcome::Cancelled));
    // The cancelled turn is committed sanitized: the prompt, the
    // assistant's tool call, an error result closing it, and the marker.
    assert_eq!(history.len(), 4, "prompt + tool call + closure + marker");
    assert_eq!(ext::message_text(&history[0]), "go");
    let closure = history[2].message();
    assert_eq!(closure.role, Role::User, "tool result message");
    assert!(matches!(&closure.content[0],
        ContentBlock::ToolResult { is_error: true, name, .. }
            if name.as_deref() == Some("read")));
    assert_eq!(ext::message_text(&history[3]), CANCEL_MARKER);
}

#[tokio::test]
async fn cancel_before_the_first_model_call_commits_prompt_and_marker_only() {
    let (model, _provider) = stream_turns(vec![]);
    let (flag, cancel) = cancel_channel();
    flag.set(true);
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &ToolDispatch::default(),
        &mut history,
        "stop",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Cancelled));
    assert_eq!(history.len(), 2);
    assert_eq!(ext::message_text(&history[0]), "stop");
    assert_eq!(ext::message_text(&history[1]), CANCEL_MARKER);
}

#[tokio::test]
async fn tool_call_ids_stay_unique_across_model_streams() {
    // Every model response restarts the provider's own call numbering; the
    // internal ids the driver mints must not, or history carries duplicate
    // tool ids and the TUI merges the second call into the first card.
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "call-1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "call-1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "a\n").unwrap();
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
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let ids: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            Event::ToolStart { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert_ne!(ids[0], ids[1], "each call gets its own id: {ids:?}");
}

#[tokio::test]
async fn argument_fragments_accumulate_into_one_call() {
    // rig-core's assembler owns fragment folding; the driver must surface
    // exactly one ToolStart and one assistant tool-call block per call,
    // with the arguments parsed as JSON.
    let (model, _provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "call-1",
            "read",
            serde_json::json!({"path":"f","limit":1}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "a\nb\n").unwrap();
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
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let starts = events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, Event::ToolStart { .. }))
        .count();
    assert_eq!(starts, 1);
    let assistant = history[1].message();
    assert_eq!(assistant.role, Role::Assistant, "assistant message");
    let tool_calls: Vec<_> = ext::tool_calls(assistant).collect();
    assert_eq!(tool_calls.len(), 1);
    let ContentBlock::ToolUse { name, input, .. } = tool_calls[0] else {
        panic!("tool call block");
    };
    assert_eq!(name, "read");
    assert_eq!(input, &serde_json::json!({"path": "f", "limit": 1}));
}

#[tokio::test]
async fn empty_reply_after_tool_call_is_nudged_to_continue() {
    // Tool call, then a completely empty reply, then recovery.
    let (model, provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn(""),
        crate::testing::text_turn("all better now"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut history = Vec::new();
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
    assert!(
        matches!(outcome, RunOutcome::Done { ref reply } if reply == "all better now"),
        "nudged model must recover: {outcome:?}"
    );
    // prompt, tool call, result, empty marker, nudge prompt, final reply
    assert_eq!(history.len(), 6);
    assert_eq!(ext::message_text(&history[3]), nudge::EMPTY_RESPONSE_MARKER);
    assert!(
        ext::message_text(&history[4]).contains("returned an empty response"),
        "nudge prompt must be committed: {}",
        ext::message_text(&history[4])
    );
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::Nudge)),
        "a nudge event must be emitted"
    );
    // The nudged retry carried the marker and the prompt.
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let sent = &requests[2].messages;
    assert_eq!(ext::text(&sent[3]), nudge::EMPTY_RESPONSE_MARKER);
    assert!(ext::text(&sent[4]).contains("returned an empty response"));
}

#[tokio::test]
async fn empty_reply_without_recent_tool_results_ends_the_run() {
    let (model, provider) = stream_turns(vec![crate::testing::text_turn("")]);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &ToolDispatch::default(),
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply.is_empty()));
    // prompt + empty marker; no nudge prompt follows
    assert_eq!(history.len(), 2);
    assert_eq!(ext::message_text(&history[1]), nudge::EMPTY_RESPONSE_MARKER);
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn empty_reply_counts_against_the_turn_budget() {
    let (model, provider) = stream_turns(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn(""),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let params = RunParams {
        max_turns: 2,
        ..RunParams::default()
    };
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &params,
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::MaxTurns));
    assert_eq!(provider.requests().len(), 2, "nudged retry still ran");
    // The sanitized tail closes with the end marker.
    assert_eq!(ext::message_text(history.last().unwrap()), END_MARKER);
}

#[tokio::test]
async fn empty_history_and_prompt_round_trip() {
    let (model, _provider) = stream_turns(vec![crate::testing::text_turn("hello")]);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &ToolDispatch::default(),
        &mut history,
        "hi",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "hello"));
    assert_eq!(history.len(), 2);
}
