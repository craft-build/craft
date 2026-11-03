//! Turn lifecycle: multi-turn commits, truncation and continuations, doom,
//! cancel boundaries, nudges, and snapshot commits.

use super::*;
use crate::history::{ContentBlock, Role, ext, marker};
use crate::harness::approval::{Approval, ApprovalAsk, ApprovalResponder};
use animist::agent::AgentEvent;
use std::sync::Arc;

#[tokio::test]
async fn multi_turn_tool_round_trip_commits_history() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"file.txt"}),
        )]),
        crate::testing::text_turn("all done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "content\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let snapshots = workspace.snapshots().clone();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
    )
    .await;

    let outcome = run_turn(&conversation, "read the file").await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "all done"),
        "{outcome:?}"
    );
    let messages = stored(&conversation).await;
    // user prompt, assistant tool call, tool result, final assistant text
    assert_eq!(messages.len(), 4);
    let assistant = messages[1].message();
    assert_eq!(
        assistant.role,
        Role::Assistant,
        "assistant tool-call message"
    );
    let ContentBlock::ToolUse { id, name, .. } = &assistant.content[0] else {
        panic!("tool-call block");
    };
    assert_eq!(name, "read");
    let result_message = messages[2].message();
    assert_eq!(result_message.role, Role::User, "tool result message");
    assert!(matches!(&result_message.content[0],
            ContentBlock::ToolResult { tool_use_id, name, is_error: false, .. }
                if tool_use_id == id && name.as_deref() == Some("read")));
    // The result the model sees on the next call is the committed one.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let replayed = &requests[1].messages;
    assert_eq!(replayed.len(), 3, "prompt, tool call, result");
    assert!(
        matches!(
            &replayed[2],
            animist::history::Message { role: Role::User, content, .. }
                if matches!(&content[0],
                    ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
        ),
        "the retried request carries the result: {replayed:?}"
    );
    // Events: the call started and finished, two steps completed, and the
    // terminal Done reports both turns.
    let events = events_until_done(&mut stream).await;
    let started = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Agent(AgentEvent::ToolStarted { call, .. }) if call.id == *id
            )
        })
        .count();
    let finished = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Agent(AgentEvent::ToolFinished { call, output, .. })
                    if call.id == *id && !output.is_error
            )
        })
        .count();
    let steps = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::StepFinished { .. })))
        .count();
    assert_eq!(started, 1);
    assert_eq!(finished, 1);
    assert_eq!(steps, 2);
    assert!(matches!(
        events.last(),
        Some(Event::Craft(crate::harness::CraftEvent::Done {
            reason: crate::harness::DoneReason::Stop,
            num_turns: 2,
            ..
        }))
    ));
    // Snapshot sessions stay empty: no write ran.
    assert_eq!(snapshots.undo_depth(), 0);
}

#[tokio::test]
async fn snapshots_commit_on_done_and_undo_restores_files() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "write",
            serde_json::json!({"path":"f.txt","content":"changed\n"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "original\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let snapshots = workspace.snapshots().clone();
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        sink,
    )
    .snapshots(snapshots.clone())
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "write it").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(snapshots.undo_depth(), 1, "session committed on Done");
    let message = snapshots.rollback().await.expect("session rolls back");
    assert!(message.contains("rolled back"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "original\n"
    );
}

#[tokio::test]
async fn truncated_reply_continues_then_finishes() {
    // Truncated, truncated, then a clean stop: the run continues the
    // truncated replies behind continuation observations and ends Done
    // with the final text.
    let provider = script(vec![
        crate::testing::max_tokens_turn("part one "),
        crate::testing::max_tokens_turn("part two "),
        crate::testing::text_turn("part three"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "tell me a long story").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "part three"),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 3);
    // prompt + three assistant replies + the two continuation
    // observations between them, all committed inside one run.
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 6);
    assert_eq!(ext::message_text(&messages[1]), "part one ");
    assert!(crate::history::marker::is(
        &messages[2],
        crate::history::marker::CONTINUATION
    ));
    assert!(crate::history::marker::is(
        &messages[4],
        crate::history::marker::CONTINUATION
    ));
}

#[tokio::test]
async fn truncation_gives_up_after_the_continuation_budget() {
    // Every reply truncates: after `max_continuation_turns` continuations
    // the run ends MaxTokens with the truncated tail committed.
    let provider = script(vec![
        crate::testing::max_tokens_turn("a"),
        crate::testing::max_tokens_turn("b"),
        crate::testing::max_tokens_turn("c"),
        crate::testing::max_tokens_turn("d"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::MaxTokens { reply } if reply == "d"),
        "{outcome:?}"
    );
    // 1 initial + 3 continuations (the default budget).
    assert_eq!(provider.requests().len(), 4);
    assert_eq!(
        stored(&conversation).await.len(),
        8,
        "prompt + four truncated replies + three continuation observations"
    );
}

#[tokio::test]
async fn continuation_budget_is_configurable_to_zero() {
    let provider = script(vec![crate::testing::max_tokens_turn("cut off")]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_tuned(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
        |builder| builder.max_continuation_turns(0),
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(outcome, TurnOutcome::MaxTokens { .. }),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 1);
}

/// Decision 12 sizes `max_steps` from `max_turns` plus the continuation
/// and nudge budgets, so a continuation never eats the productive budget;
/// a continuation with no step budget left ends the run `StepLimit`, which
/// the driver maps to today's exhausted-budget outcome — and the appended
/// marker stays durable.
#[tokio::test]
async fn continuation_with_no_step_budget_left_ends_step_limit() {
    // max_turns = 1, continuation budget = 1 ⇒ max_steps = 1 + 1 + 20 = 22.
    // Twenty-one varied productive steps exhaust the loop; the truncated
    // 22nd appends its continuation marker, and the run ends StepLimit.
    let dir = tempfile::tempdir().unwrap();
    let big = "line\n".repeat(64);
    std::fs::write(dir.path().join("f.txt"), big).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut script_turns = vec![];
    for i in 0..21u32 {
        script_turns.push(crate::testing::tool_turn(vec![tool_event(
            &format!("t{i}"),
            "read",
            serde_json::json!({"path":"f.txt","offset": i, "limit": 1}),
        )]));
    }
    script_turns.push(crate::testing::max_tokens_turn("cut off"));
    let provider = script(script_turns);
    let (conversation, _stream) = conversation_tuned(
        &provider,
        &Config::parse("agent { max_turns = 1 }").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
        |builder| builder.max_continuation_turns(1),
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::MaxTurns), "{outcome:?}");
    assert_eq!(provider.requests().len(), 22);
    let messages = stored(&conversation).await;
    let last = messages.last().unwrap();
    assert_eq!(ext::message_text(last), crate::harness::driver::END_MARKER);
    assert!(
        crate::history::marker::is(
            &messages[messages.len() - 2],
            crate::history::marker::CONTINUATION
        ),
        "the appended continuation observation stays durable past the step limit"
    );
}

#[tokio::test]
async fn empty_truncated_reply_is_committed_as_max_tokens() {
    // A truncation with zero visible text is not an empty-and-stalled
    // turn: the cut-off (empty) assistant message is committed and the
    // run ends MaxTokens instead of firing the nudge path. The
    // continuation budget is 0.
    let provider = script(vec![crate::testing::max_tokens_turn("")]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_tuned(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
        |builder| builder.max_continuation_turns(0),
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::MaxTokens { .. }),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 1, "no nudge was sent");
}

#[tokio::test]
async fn guardrails_warn_then_block_repeated_failures() {
    // Seven failing reads with varied arguments (identical arguments
    // would hit the doom-loop block first): the any-failure counters
    // warn at 3 and block at 6, so the fourth call carries the warning
    // and the seventh is blocked.
    let read = |n: usize| {
        tool_event(
            &format!("t{n}"),
            "read",
            serde_json::json!({ "path": format!("missing{n}") }),
        )
    };
    let provider = script(vec![
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
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let events = events_until_done(&mut stream).await;
    let infos: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::Info(text)) => Some(text.as_str()),
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
    assert!(
        warns >= 1,
        "a guardrail warn emits an Info event: {infos:?}"
    );
    assert_eq!(
        blocks, 1,
        "the guardrail block emits one Info event: {infos:?}"
    );
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 7);
    assert!(!texts[0].contains("guardrail"));
    assert!(
        texts[3].contains("[guardrail]"),
        "warn at the fourth call: {}",
        texts[3]
    );
    assert!(
        texts[3]
            .contains("read is repeating unproductively; consider a different approach or tool"),
        "the pre-warned fallback is the old guardrail_note wording: {}",
        texts[3]
    );
    assert!(
        texts[6].contains("blocked by guardrails"),
        "block at the seventh call: {}",
        texts[6]
    );
}

#[tokio::test]
async fn failed_run_preserves_prior_history_and_commits_the_turn() {
    // The failed turn itself is committed — with a failure note — because
    // the prompt was already admitted; the entries before it are untouched.
    let provider = script(vec![crate::testing::http_error(400, "boom")]);
    let dir = tempfile::tempdir().unwrap();
    let history = seeded_history(vec![ext::stored_user_text(
        crate::harness::doom::GRACE_CALL_PROMPT,
    )])
    .await;
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        Workspace::new(dir.path())
            .unwrap()
            .registry(&animist::history::History::new())
            .unwrap(),
        Some(history),
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(message) if message.contains("boom")),
        "{outcome:?}"
    );
    // Grace prompt + failed-turn prompt + failure note.
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 3);
    assert_eq!(
        ext::message_text(&messages[0]),
        crate::harness::doom::GRACE_CALL_PROMPT,
        "the prior history is untouched"
    );
    assert_eq!(
        ext::message_text(&messages[1]),
        "go",
        "the failed turn's prompt is committed"
    );
    let note = ext::message_text(&messages[2]);
    assert!(
        note.starts_with("[Run failed: ") && note.contains("boom"),
        "the turn and its failure mode are committed, got: {note}"
    );
}

#[tokio::test]
async fn doom_loop_scores_reach_grace_then_summarize() {
    // One batch of identical failing probe calls: two errors (+1 each) and
    // one blocked doom-loop call (+15, exactly) put the score at 17 — past
    // the grace threshold — so the next request carries the one-shot grace
    // prompt. The probe is exclusive-claimed, so the window ordering is
    // deterministic under concurrent dispatch. Distinct ids per call:
    // replay validation rejects duplicates.
    let call = |n: usize| {
        tool_event(
            &format!("t{n}"),
            "probe",
            serde_json::json!({"path": "missing"}),
        )
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![call(1), call(2), call(3)]),
        crate::testing::text_turn("summarized"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([Probe::simple("probe").panicking("nope")]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "summarized"),
        "{outcome:?}"
    );
    let messages = stored(&conversation).await;
    let grace_count = messages
        .iter()
        .filter(|m| ext::message_text(m) == crate::harness::doom::GRACE_CALL_PROMPT)
        .count();
    assert_eq!(grace_count, 1, "grace prompt fired exactly once");
    let events = events_until_done(&mut stream).await;
    let stagnation = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Craft(crate::harness::CraftEvent::StagnationDetected { .. })
            )
        })
        .count();
    assert_eq!(stagnation, 1, "grace posts exactly one StagnationDetected");
    // The score at grace is exactly 2 errors + one doom block: 17/25 (the
    // executable pin that a blocked call contributes exactly +15).
    assert!(
        events.iter().any(|event| matches!(event,
        Event::Craft(crate::harness::CraftEvent::StagnationDetected { similarity })
            if (similarity - 17.0 / 25.0).abs() < f32::EPSILON)),
        "similarity must be 17/25"
    );
    // The blocked call's result carries the doom-loop warning. The step's
    // results squash into one entry, so the denial is a block in it.
    assert!(messages.iter().any(|stored_message| {
        let message = stored_message.message();
        message.role == Role::User
            && message.content.iter().any(|block| {
                matches!(block,
                    ContentBlock::ToolResult { is_error: true, content, .. }
                        if ext::tool_content_text(content).contains("stuck in a loop"))
            })
    }));
    // The second request saw the grace prompt as its trailing message.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last().map(ext::text),
        Some(crate::harness::doom::GRACE_CALL_PROMPT.to_owned())
    );
}

#[tokio::test]
async fn doom_score_hard_stops_the_run() {
    // Two doom-loop batches push the score past the hard-stop threshold
    // (25); the run ends with sanitized partial history and no third
    // request. The probe is exclusive-claimed (deterministic window order
    // under concurrent dispatch); distinct ids per call because replay
    // validation rejects duplicates.
    let call = |n: usize| {
        tool_event(
            &format!("t{n}"),
            "probe",
            serde_json::json!({"path": "missing"}),
        )
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![call(4), call(5), call(6)]),
        crate::testing::tool_turn(vec![call(1), call(2), call(3)]),
        // Never requested: the hard stop preempts the third turn.
        crate::testing::text_turn("never"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([Probe::simple("probe").panicking("nope")]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        !matches!(&outcome, TurnOutcome::Done { reply } if reply == "never"),
        "the hard stop must preempt the third turn: {outcome:?}"
    );
    assert_eq!(
        provider.requests().len(),
        2,
        "no request after the hard stop"
    );
    let messages = stored(&conversation).await;
    assert_eq!(
        ext::message_text(messages.last().unwrap()),
        crate::harness::driver::END_MARKER
    );
}

#[tokio::test]
async fn failed_run_emits_error_then_done_with_error_reason() {
    let provider = script(vec![crate::testing::http_error(400, "boom")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(message) if message.contains("boom")),
        "{outcome:?}"
    );
    let events = events_until_done(&mut stream).await;
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Craft(crate::harness::CraftEvent::Error(message)) if message.contains("boom")
    )));
    assert!(matches!(
        events.last(),
        Some(Event::Craft(crate::harness::CraftEvent::Done {
            reason: crate::harness::DoneReason::Error,
            ..
        }))
    ));
}

#[tokio::test]
async fn turn_complete_reports_a_nonzero_context_size() {
    let provider = script(vec![crate::testing::text_turn("ok")]);
    let dir = tempfile::tempdir().unwrap();
    let history = seeded_history(vec![ext::stored_user_text(
        "a sufficiently long prompt to register a nonzero token estimate \
         beyond the estimator floor, padded out for good measure."
            .repeat(4),
    )])
    .await;
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        Workspace::new(dir.path())
            .unwrap()
            .registry(&animist::history::History::new())
            .unwrap(),
        Some(history),
    )
    .await;
    let _ = run_turn(&conversation, "go").await;
    let events = events_until_done(&mut stream).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Done { context_size, .. })
                if *context_size > 0
        )),
        "context estimate must be reported"
    );
}

#[tokio::test]
async fn max_turns_bound_commits_sanitized_partial_history() {
    // Decision 12 sizes the step budget at max_turns plus the continuation
    // and nudge budgets (2 + 3 + 20 = 25), so a tool-looping run stops at
    // the 25th model call — not the second — and commits its sanitized
    // partial history with the end marker.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "x\n".repeat(64)).unwrap();
    let turns: Vec<Attempt> = (0..25)
        .map(|i| {
            crate::testing::tool_turn(vec![tool_event(
                &format!("t{i}"),
                "read",
                serde_json::json!({"path":"f", "offset": i, "limit": 1}),
            )])
        })
        .collect();
    let provider = script(turns);
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("agent { max_turns = 2 }").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "loop").await;
    assert!(matches!(outcome, TurnOutcome::MaxTurns), "{outcome:?}");
    // Prompt + 25x(assistant+result) + end marker; the 26th call never ran.
    assert_eq!(provider.requests().len(), 25);
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 52);
    assert_eq!(
        ext::message_text(messages.last().unwrap()),
        crate::harness::driver::END_MARKER
    );
    // No dangling tool calls: every call has its result.
    let calls: Vec<String> = messages
        .iter()
        .flat_map(|m| ext::tool_calls(m.message()))
        .map(|block| {
            let ContentBlock::ToolUse { id, .. } = block else {
                panic!("tool call block");
            };
            id.clone()
        })
        .collect();
    let answered: Vec<String> = messages
        .iter()
        .flat_map(|m| ext::tool_results(m.message()))
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
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::http_error(400, "boom"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "x\n").unwrap();
    let history = seeded_history(vec![ext::stored_user_text("earlier")]).await;
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        Workspace::new(dir.path())
            .unwrap()
            .registry(&animist::history::History::new())
            .unwrap(),
        Some(history),
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed(message) if message.contains("boom")),
        "{outcome:?}"
    );
    // earlier + prompt + assistant + result + failure note.
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 5);
    // No dangling tool calls: the resolved call has its result.
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    let note = ext::message_text(messages.last().unwrap());
    assert!(
        note.starts_with("[Run failed: ") && note.contains("boom"),
        "the failure note ends the committed turn, got: {note}"
    );
}

#[tokio::test]
async fn stop_decision_at_the_dispatch_boundary_commits_the_interrupted_call() {
    // The responder answers the ask by stopping the run, standing in for a
    // cancellation arriving at the approval boundary. The turn is committed
    // sanitized: the prompt, the assistant's tool call, an error result
    // closing it, and a failure note.
    struct StopAll;
    impl ApprovalResponder for StopAll {
        fn ask(&self, _ask: ApprovalAsk<'_>) -> crate::harness::BoxFuture<Approval> {
            Box::pin(async {
                Approval::Stop {
                    reason: "cancelled".into(),
                }
            })
        }
    }
    // `bash` prompts under the default rules, so the responder decides it.
    let provider = script(vec![crate::testing::tool_turn(vec![tool_event(
        "t1",
        "bash",
        serde_json::json!({"command": "echo hi"}),
    )])]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        sink,
    )
    .policy(crate::harness::PolicyLayerInputs::new(
        Arc::new(crate::permissions::PermissionManager::new(
            crate::permissions::PermissionsConfig::default(),
            dir.path().to_path_buf(),
        )),
        Arc::new(StopAll),
    ))
    .build()
    .unwrap();
    let mut stream = stream;

    let outcome = run_turn(&conversation, "go").await;

    assert!(
        matches!(&outcome, TurnOutcome::Failed(message) if message.contains("cancelled")),
        "a stop at the gate halts the run: {outcome:?}"
    );
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 4, "prompt + tool call + closure + note");
    assert_eq!(ext::message_text(&messages[0]), "go");
    let closure = messages[2].message();
    assert_eq!(closure.role, Role::User, "tool result message");
    assert!(matches!(&closure.content[0],
            ContentBlock::ToolResult { is_error: true, name, .. }
                if name.as_deref() == Some("bash")));
    assert!(marker::is(&messages[3], marker::FAILED));
    let _ = events_until_done(&mut stream).await;
}

#[tokio::test]
async fn cancel_before_the_first_model_call_commits_the_marker_only() {
    let provider = script(Vec::new());
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    // The cancellation is already fired when the turn starts, so the input
    // is never admitted and only the marker is committed.
    let cancel = Cancellation::default();
    cancel.cancel();
    let outcome = crate::harness::run_turn(&conversation, "stop", &[], cancel).await;
    assert!(matches!(outcome, TurnOutcome::Cancelled), "{outcome:?}");
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 1);
    assert!(marker::is(&messages[0], marker::CANCEL));
}

#[tokio::test]
async fn argument_fragments_accumulate_into_one_call() {
    // The assembler owns fragment folding; the runtime must surface exactly
    // one tool call with the arguments parsed as JSON.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "call-1",
            "read",
            serde_json::json!({"path":"f","limit":1}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "a\nb\n").unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let events = events_until_done(&mut stream).await;
    let starts = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::ToolStarted { .. })))
        .count();
    assert_eq!(starts, 1);
    let messages = stored(&conversation).await;
    let assistant = messages[1].message();
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
    let provider = script(vec![
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
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "all better now"),
        "nudged model must recover: {outcome:?}"
    );
    // prompt, tool call, result, the committed empty reply, the empty
    // marker, the nudge prompt, final reply
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 7);
    assert!(crate::history::marker::is(
        &messages[4],
        crate::history::marker::NUDGE
    ));
    assert_eq!(ext::message_text(&messages[4]), "(empty)");
    assert_eq!(
        ext::message_text(&messages[5]),
        "You just executed tool calls but returned an empty response. \
         Please process the tool results above and continue with the task."
    );
    assert!(
        events_until_done(&mut stream)
            .await
            .iter()
            .any(|event| matches!(event, Event::Craft(crate::harness::CraftEvent::Nudge)))
    );
    // The nudged retry carried the marker and the prompt.
    assert_eq!(provider.requests().len(), 3);
}

#[tokio::test]
async fn empty_reply_without_recent_tool_results_ends_the_run() {
    let provider = script(vec![crate::testing::text_turn("")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply.is_empty()),
        "{outcome:?}"
    );
    // prompt + empty reply; no follow-up request
    assert_eq!(stored(&conversation).await.len(), 2);
    assert_eq!(provider.requests().len(), 1);
}

/// The nudge budget runs out: after `MAX_NUDGES` stalled replies the next
/// stall ends the turn empty-handed instead of nudging again.
#[tokio::test]
async fn nudge_budget_runs_out_after_max_nudges() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut script_turns = vec![crate::testing::tool_turn(vec![tool_event(
        "t1",
        "read",
        serde_json::json!({"path":"f"}),
    )])];
    // Twenty empty replies spend the nudge budget; the twenty-first stalls
    // with nothing left.
    for _ in 0..21 {
        script_turns.push(crate::testing::text_turn(""));
    }
    let provider = script(script_turns);
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply.is_empty()),
        "{outcome:?}"
    );
    // 1 tool round + 21 stalled replies: the last one fires no nudge.
    assert_eq!(provider.requests().len(), 22);
    let nudges = events_until_done(&mut stream)
        .await
        .into_iter()
        .filter(|event| matches!(event, Event::Craft(crate::harness::CraftEvent::Nudge)))
        .count();
    assert_eq!(nudges, 20);
}

#[tokio::test]
async fn empty_history_and_prompt_round_trip() {
    let provider = script(vec![crate::testing::text_turn("hello")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "hi").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "hello"),
        "{outcome:?}"
    );
    assert_eq!(stored(&conversation).await.len(), 2);
}
