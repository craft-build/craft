//! Turn lifecycle: multi-turn commits, truncation and continuations, doom,
//! cancel boundaries, nudges, and snapshot commits.

use super::*;
use crate::history::{ContentBlock, Role, ext, marker};
use crate::run::{BeforeExecute, BoxFuture, Decision};
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
        workspace.registry().unwrap(),
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
        workspace.registry().unwrap(),
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
#[ignore = "restored in 09"]
async fn truncated_reply_continues_then_finishes() {
    // Truncated, truncated, then a clean stop: the run continues the
    // truncated replies and ends Done with the final text.
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
    // prompt + three assistant messages, all committed
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 4);
    assert_eq!(ext::message_text(&messages[1]), "part one ");
}

#[tokio::test]
#[ignore = "restored in 09"]
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
        5,
        "truncated tail committed"
    );
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn continuation_budget_is_configurable_to_zero() {
    let provider = script(vec![crate::testing::max_tokens_turn("cut off")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(outcome, TurnOutcome::MaxTokens { .. }),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn continuations_respect_the_turn_budget() {
    // One clean turn budget, a truncated first reply: the continuation
    // counts against `max_turns` and the run ends MaxTurns sanitized.
    let provider = script(vec![
        crate::testing::max_tokens_turn("a"),
        crate::testing::max_tokens_turn("b"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("agent { max_turns = 1 }").unwrap(),
        &dir.path().display().to_string(),
        workspace.registry().unwrap(),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::MaxTurns), "{outcome:?}");
    assert_eq!(provider.requests().len(), 1);
    let messages = stored(&conversation).await;
    assert_eq!(
        ext::message_text(messages.last().unwrap()),
        crate::harness::driver::END_MARKER
    );
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn empty_truncated_reply_is_committed_as_max_tokens() {
    // A truncation with zero visible text is not an empty-and-stalled
    // turn: the cut-off (empty) assistant message is committed and the
    // run ends MaxTokens instead of firing the nudge path.
    let provider = script(vec![crate::testing::max_tokens_turn("")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(outcome, TurnOutcome::MaxTokens { .. }),
        "{outcome:?}"
    );
    assert_eq!(provider.requests().len(), 1, "no nudge was sent");
}

#[tokio::test]
#[ignore = "restored in 07"]
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
        crate::run::doom::GRACE_CALL_PROMPT,
    )])
    .await;
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        Workspace::new(dir.path()).unwrap().registry().unwrap(),
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
        crate::run::doom::GRACE_CALL_PROMPT,
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
#[ignore = "restored in 07"]
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
    let provider = script(vec![
        crate::testing::tool_turn(vec![read(1), read(2), read(3)]),
        crate::testing::text_turn("summarized"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "summarized"),
        "{outcome:?}"
    );
    let messages = stored(&conversation).await;
    let grace_count = messages
        .iter()
        .filter(|m| ext::message_text(m) == crate::run::doom::GRACE_CALL_PROMPT)
        .count();
    assert_eq!(grace_count, 1, "grace prompt fired exactly once");
    let stagnation_count = events_until_done(&mut stream)
        .await
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Craft(crate::harness::CraftEvent::StagnationDetected { .. })
            )
        })
        .count();
    assert_eq!(
        stagnation_count, 1,
        "grace posts exactly one StagnationDetected"
    );
    // The blocked call's result carries the doom-loop warning.
    assert!(messages.iter().any(|stored_message| {
        let message = stored_message.message();
        message.role == Role::User
            && matches!(&message.content[0],
                ContentBlock::ToolResult { is_error: true, content, .. }
                    if ext::tool_content_text(content).contains("stuck in a loop"))
    }));
    // The second request saw the grace prompt as its trailing message.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].messages.last().map(ext::text),
        Some(crate::run::doom::GRACE_CALL_PROMPT.to_owned())
    );
}

#[tokio::test]
#[ignore = "restored in 07"]
async fn doom_score_hard_stops_the_run() {
    // Two doom-loop batches push the score past the hard-stop threshold
    // (25); the run ends with sanitized partial history and no third
    // request. Distinct ids per call: replay validation rejects duplicates.
    let read = |n: usize| {
        tool_event(
            &format!("t{n}"),
            "read",
            serde_json::json!({"path": "missing"}),
        )
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![read(4), read(5), read(6)]),
        crate::testing::tool_turn(vec![read(1), read(2), read(3)]),
        // Never requested: the hard stop preempts the third turn.
        crate::testing::text_turn("never"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
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
        Workspace::new(dir.path()).unwrap().registry().unwrap(),
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
    let turns: Vec<Attempt> = (0..3)
        .map(|i| {
            crate::testing::tool_turn(vec![tool_event(
                &format!("t{i}"),
                "read",
                serde_json::json!({"path":"f"}),
            )])
        })
        .collect();
    let provider = script(turns);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "x\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("agent { max_turns = 2 }").unwrap(),
        &dir.path().display().to_string(),
        workspace.registry().unwrap(),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "loop").await;
    assert!(matches!(outcome, TurnOutcome::MaxTurns), "{outcome:?}");
    // Prompt + 2x(assistant+result) + end marker; the third call never ran.
    assert_eq!(provider.requests().len(), 2);
    let messages = stored(&conversation).await;
    assert_eq!(messages.len(), 6);
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
        Workspace::new(dir.path()).unwrap().registry().unwrap(),
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
    // The before-execution hook denies the call by stopping the run,
    // standing in for a cancellation arriving at the dispatch boundary.
    // The turn is committed sanitized: the prompt, the assistant's tool
    // call, an error result closing it, and a failure note.
    struct StopAll;
    impl BeforeExecute for StopAll {
        fn decide(&self, _call: ContentBlock) -> BoxFuture<Decision> {
            Box::pin(async { Decision::Stop("cancelled".into()) })
        }
    }
    let provider = script(vec![crate::testing::tool_turn(vec![tool_event(
        "t1",
        "read",
        serde_json::json!({"path": "f"}),
    )])]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "secret\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace.registry().unwrap(),
        sink,
    )
    .approval(Arc::new(StopAll))
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
                if name.as_deref() == Some("read")));
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
#[ignore = "restored in 09"]
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
    // prompt, tool call, result, empty marker, nudge prompt, final reply
    assert_eq!(stored(&conversation).await.len(), 6);
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

#[tokio::test]
#[ignore = "restored in 09"]
async fn empty_reply_counts_against_the_turn_budget() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn(""),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("agent { max_turns = 2 }").unwrap(),
        &dir.path().display().to_string(),
        workspace.registry().unwrap(),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::MaxTurns), "{outcome:?}");
    assert_eq!(provider.requests().len(), 2, "nudged retry still ran");
    // The sanitized tail closes with the end marker.
    let messages = stored(&conversation).await;
    assert_eq!(
        ext::message_text(messages.last().unwrap()),
        crate::harness::driver::END_MARKER
    );
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
