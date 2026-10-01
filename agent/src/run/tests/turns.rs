//! Turn lifecycle: multi-turn commits, truncation and continuations, doom,
//! cancel boundaries, nudges, and snapshot commits.

use super::*;
use crate::history::UserContent;
use crate::run::*;
use rig_core::test_utils::MockStreamEvent;

#[tokio::test]
async fn multi_turn_tool_round_trip_commits_history() {
    let (model, _turns) = stream_turns(vec![
        vec![
            tool_event("t1", "read", serde_json::json!({"path":"file.txt"})),
            MockStreamEvent::final_response_with_total_tokens(3),
        ],
        vec![
            MockStreamEvent::text("all done"),
            MockStreamEvent::final_response_with_total_tokens(5),
        ],
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
    let Message::Assistant { content } = &history[1] else {
        panic!("assistant tool-call message");
    };
    let history::AssistantContent::ToolCall(call) = &content[0] else {
        panic!("tool-call block");
    };
    assert_eq!(call.function.name, "read");
    assert!(matches!(&history[2],
        Message::User { content } if matches!(&content[0], UserContent::ToolResult(r)
            if r.call == call.id && r.name == "read")));
    // The result the model sees on the next call is literal text.
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let replayed = crate::edge::rig_to_own(&requests[1].chat_history);
    assert_eq!(replayed[2], history[2]);
    // Events: tool start, tool done, usage per call, results submitted
    // per wave, one terminal Done.
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::ToolStart { name, .. } if name == "read"))
    );
    assert!(
        guard
            .iter()
            .any(|e| matches!(e, Event::ToolDone { result, .. } if !result.is_error))
    );
    assert_eq!(
        guard
            .iter()
            .filter(|e| matches!(e, Event::TurnComplete { .. }))
            .count(),
        2
    );
    assert!(guard.iter().any(|e| matches!(e,
            Event::ToolResultsSubmitted { message } if matches!(
                &message, Message::User { content } if matches!(
                    &content[0], history::UserContent::ToolResult(_))))));
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
    let (model, _turns) = stream_turns(vec![
        vec![
            tool_event(
                "t1",
                "write",
                serde_json::json!({"path":"f.txt","content":"changed\n"}),
            ),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let (model, _turns) = stream_turns(vec![
        vec![MockStreamEvent::text("part one "), length_final(1)],
        vec![MockStreamEvent::text("part two "), length_final(1)],
        vec![
            MockStreamEvent::text("part three"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    assert_eq!(model.request_count(), 3);
    // prompt + three assistant messages, all committed
    assert_eq!(history.len(), 4);
    assert_eq!(history[1].text(), "part one ");
}

#[tokio::test]
async fn truncation_gives_up_after_the_continuation_budget() {
    // Every reply truncates: after `max_continuation_turns` continuations
    // the run ends MaxTokens with the truncated tail committed.
    let (model, _turns) = stream_turns(vec![
        vec![MockStreamEvent::text("a"), length_final(1)],
        vec![MockStreamEvent::text("b"), length_final(1)],
        vec![MockStreamEvent::text("c"), length_final(1)],
        vec![MockStreamEvent::text("d"), length_final(1)],
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
    assert_eq!(model.request_count(), 4);
    assert_eq!(history.len(), 5, "truncated tail is committed");
}

#[tokio::test]
async fn continuation_budget_is_configurable_to_zero() {
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("cut off"),
        length_final(1),
    ]]);
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
    assert_eq!(model.request_count(), 1);
}

#[tokio::test]
async fn continuations_respect_the_turn_budget() {
    // One clean turn budget, a truncated first reply: the continuation
    // counts against `max_turns` and the run ends MaxTurns sanitized.
    let (model, _turns) = stream_turns(vec![
        vec![MockStreamEvent::text("a"), length_final(1)],
        vec![MockStreamEvent::text("b"), length_final(1)],
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
    assert_eq!(model.request_count(), 1);
    assert_eq!(history.last().unwrap().text(), END_MARKER);
}

#[tokio::test]
async fn empty_truncated_reply_is_committed_as_max_tokens() {
    // A truncation with zero visible text is not an empty-and-stalled
    // turn: the cut-off (empty) assistant message is committed and the
    // run ends MaxTokens instead of firing the nudge path.
    let (model, _turns) = stream_turns(vec![vec![length_final(1)]]);
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
    assert_eq!(model.request_count(), 1, "no nudge was sent");
    assert_eq!(history.len(), 2);
    // prompt + the truncated assistant message (empty, not replaced by
    // the empty-response marker)
    assert_ne!(history[1].text(), nudge::EMPTY_RESPONSE_MARKER);
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
    let (model, _turns) = stream_turns(vec![
        vec![
            read(1),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(2),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(3),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(4),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(5),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(6),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(7),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
async fn failed_run_leaves_trailing_grace_prompt_intact() {
    // A trailing grace prompt must not replay as user text, but a run
    // that fails commits nothing: the caller's history stays
    // byte-identical, and a retry strips it again identically.
    let (model, _turns) = stream_turns(vec![vec![MockStreamEvent::Error(
        rig_core::test_utils::MockError::provider("boom"),
    )]]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let (_, cancel) = cancel_channel();
    let mut history = vec![Message::user(doom::GRACE_CALL_PROMPT)];
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
    assert_eq!(history, before, "failed run must not drop the grace prompt");

    // The retry strips it again: the request view omits it, and a
    // successful run never commits it back.
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("hi"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
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
    let requests = model.requests();
    let replayed = crate::edge::rig_to_own(&requests[0].chat_history);
    assert_eq!(
        replayed
            .iter()
            .filter(|m| m.text() == doom::GRACE_CALL_PROMPT)
            .count(),
        0,
        "request must not carry the grace prompt"
    );
    assert!(
        !history.iter().any(|m| m.text() == doom::GRACE_CALL_PROMPT),
        "committed history drops the grace prompt"
    );
}

#[tokio::test]
async fn doom_loop_scores_reach_grace_then_summarize() {
    // One batch of identical failing reads: two errors (+1 each) and one
    // blocked doom-loop call (+15) put the score at 17 — past the grace
    // threshold — so the next request carries the one-shot grace prompt.
    let read = || tool_event("t", "read", serde_json::json!({"path": "missing"}));
    let (model, _turns) = stream_turns(vec![
        vec![
            read(),
            read(),
            read(),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("summarized"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
        .filter(|m| m.text() == doom::GRACE_CALL_PROMPT)
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
    assert!(history.iter().any(|m| {
        matches!(m, Message::User { content } if matches!(&content[0],
            UserContent::ToolResult(r) if r.is_error
                && r.content[0].to_text().contains("stuck in a loop")))
    }));
    // The second request saw the grace prompt as its trailing message.
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let replayed = crate::edge::rig_to_own(&requests[1].chat_history);
    assert_eq!(
        replayed.last().map(Message::text),
        Some(doom::GRACE_CALL_PROMPT.to_owned())
    );
}

#[tokio::test]
async fn doom_score_hard_stops_the_run() {
    // Two doom-loop batches push the score past the hard-stop threshold
    // (25); the run ends DoomStop with sanitized partial history.
    let read = || tool_event("t", "read", serde_json::json!({"path": "missing"}));
    let (model, _turns) = stream_turns(vec![
        vec![
            read(),
            read(),
            read(),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            read(),
            read(),
            read(),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        // Never requested: the hard stop preempts the third turn.
        vec![
            MockStreamEvent::text("never"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    assert_eq!(model.requests().len(), 2, "no request after the hard stop");
    assert_eq!(
        history.last().map(Message::text),
        Some(END_MARKER.to_owned())
    );
}

#[tokio::test]
async fn failed_run_emits_error_then_done_with_error_reason() {
    let (model, _turns) = stream_turns(vec![vec![
        tool_event("t1", "no_such_tool", serde_json::json!({})),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
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
            .any(|e| matches!(e, Event::Error(message) if message.contains("unknown tool")))
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
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("ok"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
    let (_, cancel) = cancel_channel();
    let mut history = vec![Message::user(
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
    let turns: Vec<Vec<MockStreamEvent>> = (0..3)
        .map(|i| {
            vec![
                tool_event(&format!("t{i}"), "read", serde_json::json!({"path":"f"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ]
        })
        .collect();
    let (model, _t) = stream_turns(turns);
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
    assert_eq!(model.request_count(), 2);
    assert_eq!(history.len(), 6);
    assert_eq!(history.last().unwrap().text(), END_MARKER);
    // No dangling tool calls: every call has its result.
    let calls: Vec<String> = history
        .iter()
        .flat_map(|m| match m {
            Message::Assistant { content } => content
                .iter()
                .filter_map(|b| match b {
                    history::AssistantContent::ToolCall(c) => Some(c.id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    let answered: Vec<String> = history
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|b| match b {
                    UserContent::ToolResult(r) => Some(r.call.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert!(calls.iter().all(|id| answered.contains(id)));
}

#[tokio::test]
async fn cancel_mid_tool_skips_execution_and_commits_sanitized_partial() {
    // The before-execution hook denies the call by stopping the run,
    // standing in for a cancellation arriving at the dispatch boundary.
    struct StopAll;
    impl BeforeExecute for StopAll {
        fn decide(&self, _call: history::ToolCall) -> BoxFuture<Decision> {
            Box::pin(async { Decision::Stop("cancelled".into()) })
        }
    }
    let (model, _turns) = stream_turns(vec![vec![
        tool_event("t1", "read", serde_json::json!({"path":"f"})),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
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
    assert_eq!(history[0].text(), "go");
    assert!(matches!(&history[2], Message::User { content }
        if matches!(&content[0], UserContent::ToolResult(r)
            if r.is_error && r.name == "read")));
    assert_eq!(history[3].text(), CANCEL_MARKER);
}

#[tokio::test]
async fn cancel_before_the_first_model_call_commits_prompt_and_marker_only() {
    let (model, _turns) = stream_turns(vec![]);
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
    assert_eq!(history[0].text(), "stop");
    assert_eq!(history[1].text(), CANCEL_MARKER);
}

#[tokio::test]
async fn argument_fragments_accumulate_into_one_call() {
    // rig-core's assembler owns fragment folding; the driver must surface
    // exactly one ToolStart and one assistant tool-call block per call,
    // with the arguments parsed as JSON.
    let (model, _turns) = stream_turns(vec![
        vec![
            MockStreamEvent::tool_call("call-1", "read", serde_json::json!({"path":"f","limit":1})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    let history::Message::Assistant { content } = &history[1] else {
        panic!("assistant message");
    };
    let tool_calls: Vec<_> = content
        .iter()
        .filter_map(|b| match b {
            history::AssistantContent::ToolCall(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].function.name, "read");
    assert_eq!(
        tool_calls[0].function.arguments,
        serde_json::json!({"path": "f", "limit": 1})
    );
}

#[tokio::test]
async fn empty_reply_after_tool_call_is_nudged_to_continue() {
    // Tool call, then a completely empty reply, then recovery.
    let (model, _turns) = stream_turns(vec![
        vec![
            tool_event("t1", "read", serde_json::json!({"path":"f"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![MockStreamEvent::final_response_with_total_tokens(1)],
        vec![
            MockStreamEvent::text("all better now"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
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
    assert_eq!(history[3].text(), nudge::EMPTY_RESPONSE_MARKER);
    assert!(
        history[4].text().contains("returned an empty response"),
        "nudge prompt must be committed: {}",
        history[4].text()
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
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    let sent = crate::edge::rig_to_own(&requests[2].chat_history);
    assert_eq!(sent[3].text(), nudge::EMPTY_RESPONSE_MARKER);
    assert!(sent[4].text().contains("returned an empty response"));
}

#[tokio::test]
async fn empty_reply_without_recent_tool_results_ends_the_run() {
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
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
    assert_eq!(history[1].text(), nudge::EMPTY_RESPONSE_MARKER);
    assert_eq!(model.request_count(), 1);
}

#[tokio::test]
async fn empty_reply_counts_against_the_turn_budget() {
    let (model, _turns) = stream_turns(vec![
        vec![
            tool_event("t1", "read", serde_json::json!({"path":"f"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![MockStreamEvent::final_response_with_total_tokens(1)],
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
    assert_eq!(model.request_count(), 2, "nudged retry still ran");
    // The sanitized tail closes with the end marker.
    assert_eq!(history.last().unwrap().text(), END_MARKER);
}

#[tokio::test]
async fn empty_history_and_prompt_round_trip() {
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("hello"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
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
