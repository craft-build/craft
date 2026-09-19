#[cfg(test)]
mod served_spec_tests {
    use crate::run::served_spec;

    #[test]
    fn bare_ids_borrow_the_primary_provider_full_specs_do_not() {
        let primary = "anthropic/claude-sonnet-5";
        assert_eq!(
            served_spec(Some(primary), Some("claude-opus-5"), None),
            Some("anthropic/claude-opus-5".into())
        );
        assert_eq!(
            served_spec(Some(primary), Some("openai/gpt-5.6-sol"), None),
            Some("openai/gpt-5.6-sol".into())
        );
        assert_eq!(served_spec(None, Some("claude-opus-5"), None), None);
        assert_eq!(
            served_spec(Some(primary), None, None),
            Some("anthropic/claude-sonnet-5".into())
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::history::UserContent;
    use crate::run::*;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    fn stream_turns(
        turns: Vec<Vec<MockStreamEvent>>,
    ) -> (MockCompletionModel, Vec<Vec<MockStreamEvent>>) {
        let model = MockCompletionModel::from_stream_turns(turns.clone());
        (model, turns)
    }

    fn tool_event(id: &str, name: &str, args: serde_json::Value) -> MockStreamEvent {
        MockStreamEvent::tool_call(id, name, args)
    }

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

    fn length_final(total_tokens: u64) -> MockStreamEvent {
        use rig_core::streaming::StreamFinal;
        MockStreamEvent::FinalResponse(
            StreamFinal::new(
                "mock",
                rig_core::completion::Usage {
                    total_tokens,
                    ..Default::default()
                },
            )
            .with_finish_reason(rig_core::completion::FinishReason::Length),
        )
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
        assert!(matches!(outcome, RunOutcome::Done { ref reply } if reply == "summarized"));
        let grace_count = history
            .iter()
            .filter(|m| m.text() == doom::GRACE_CALL_PROMPT)
            .count();
        assert_eq!(grace_count, 1, "grace prompt fired exactly once");
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
    async fn argument_fragments_accumulate_into_one_call() {
        // rig-core's assembler owns fragment folding; the driver must surface
        // exactly one ToolStart and one assistant tool-call block per call,
        // with the arguments parsed as JSON.
        let (model, _turns) = stream_turns(vec![
            vec![
                MockStreamEvent::tool_call(
                    "call-1",
                    "read",
                    serde_json::json!({"path":"f","limit":1}),
                ),
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
    async fn recency_tail_rides_the_request_but_not_history() {
        struct Turns;
        impl crate::run::RecencySource for Turns {
            fn collect(&self, ctx: &crate::run::RecencyCtx) -> crate::run::RecencyFacts {
                let mut facts = crate::run::RecencyFacts::new();
                facts.push(format!("turn {}", ctx.turn));
                facts
            }
        }
        let (model, _turns) = stream_turns(vec![vec![
            MockStreamEvent::text("ok"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            recency: Some(std::sync::Arc::new(Turns)),
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
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        let requests = model.requests();
        let sent = crate::edge::rig_to_own(&requests[0].chat_history);
        assert!(
            sent[0].text().contains("<turn-context>\n\nturn 0"),
            "tail must ride the request"
        );
        assert!(
            !history[0].text().contains("turn-context"),
            "tail must not be committed to history"
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

    #[tokio::test]
    async fn read_lifecycle_marks_request_view_only() {
        // read → write the same file, with enough later read turns that the
        // early read falls outside the working-set lookback. The model's
        // later requests must see the stale marker; history keeps the raw text.
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path": "f.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event(
                    "t2",
                    "write",
                    serde_json::json!({"path": "f.txt", "content": "changed\n"}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t3", "read", serde_json::json!({"path": "g.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t4", "read", serde_json::json!({"path": "h.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t5", "read", serde_json::json!({"path": "i.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                tool_event("t6", "read", serde_json::json!({"path": "j.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let long: String = (1..=40)
            .map(|i| format!("original line {i} of the file\n"))
            .collect();
        for name in ["f.txt", "g.txt", "h.txt", "i.txt", "j.txt"] {
            std::fs::write(dir.path().join(name), &long).unwrap();
        }
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
        // Committed history keeps the raw read.
        let Message::User { content } = &history[2] else {
            panic!("first read result message");
        };
        let UserContent::ToolResult(raw) = &content[0] else {
            panic!("tool result block");
        };
        assert!(raw.content[0].to_text().contains("40: original line 40"));
        // The model's last request carried the stale marker for that read.
        let requests = model.requests();
        let sent = crate::edge::rig_to_own(&requests.last().unwrap().chat_history);
        let Message::User { content } = &sent[2] else {
            panic!("tool result message on the wire");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block on the wire");
        };
        let wire_text = result.content[0].to_text();
        assert!(
            wire_text.starts_with("[Stale read: "),
            "expected a stale marker, got: {wire_text}"
        );
        assert!(wire_text.contains("Re-read the file"));
    }

    #[tokio::test]
    async fn read_output_stays_verbatim_on_wire() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path": "big.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=100)
            .map(|i| format!("filler line number {i} for size\n"))
            .collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
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
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        // Committed history and the ToolDone event keep the raw result.
        let Message::User { content } = &history[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(raw) = &content[0] else {
            panic!("tool result block");
        };
        let raw_text = raw.content[0].to_text();
        assert!(raw_text.len() >= compression::MIN_COMPRESS_LEN);
        assert!(!raw_text.contains("lines omitted"));
        assert!(events.lock().unwrap().iter().any(
            |e| matches!(e, Event::ToolDone { result, .. } if !result.content[0]
                .to_text()
                .contains("lines omitted"))
        ));
        // `read` output is caller-selected and must reach the model verbatim,
        // even though its `N: ` numbering looks like code to the detector.
        let requests = model.requests();
        assert_eq!(requests.len(), 2);
        let sent = crate::edge::rig_to_own(&requests[1].chat_history);
        let Message::User { content } = &sent[2] else {
            panic!("tool result message on the wire");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block on the wire");
        };
        let wire_text = result.content[0].to_text();
        assert!(
            !wire_text.contains("lines omitted"),
            "read output must not be pre-compressed"
        );
        assert_eq!(wire_text, raw_text);
    }

    #[test]
    fn compress_request_view_skips_verbatim_tools_but_compresses_the_rest() {
        let numbered = |n: usize| {
            (1..=n)
                .map(|i| format!("{i}: let x = {i};\n"))
                .collect::<String>()
        };
        let raw_read = numbered(60);
        let raw_grep = numbered(60);
        let raw_retrieve = numbered(60);
        let raw_bash = numbered(60);
        let mut full = vec![Message::User {
            content: vec![
                UserContent::ToolResult(crate::history::ToolResult::text(
                    "c1",
                    "read",
                    raw_read.clone(),
                )),
                UserContent::ToolResult(crate::history::ToolResult::text(
                    "c2",
                    "grep",
                    raw_grep.clone(),
                )),
                UserContent::ToolResult(crate::history::ToolResult::text(
                    "c3",
                    "retrieve",
                    raw_retrieve.clone(),
                )),
                UserContent::ToolResult(crate::history::ToolResult::text(
                    "c4",
                    "bash",
                    raw_bash.clone(),
                )),
            ],
        }];
        compress_request_view(&mut full, &CompressionConfig::default());
        let Message::User { content } = &full[0] else {
            panic!("user message");
        };
        let text = |i: usize| {
            let UserContent::ToolResult(result) = &content[i] else {
                panic!("tool result {i}");
            };
            result.content[0].to_text()
        };
        assert_eq!(text(0), raw_read, "read stays verbatim");
        assert_eq!(text(1), raw_grep, "grep stays verbatim");
        assert_eq!(text(2), raw_retrieve, "retrieve stays verbatim");
        assert!(
            text(3).contains("lines omitted"),
            "non-verbatim tools are still pre-compressed"
        );
    }

    #[tokio::test]
    async fn compression_disabled_sends_raw_output() {
        let (model, _turns) = stream_turns(vec![
            vec![
                tool_event("t1", "read", serde_json::json!({"path": "big.txt"})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=100)
            .map(|i| format!("filler line number {i} for size\n"))
            .collect();
        std::fs::write(dir.path().join("big.txt"), body).unwrap();
        let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
        let (_, cancel) = cancel_channel();
        let params = RunParams {
            compression: CompressionConfig {
                enabled: false,
                ..CompressionConfig::default()
            },
            ..RunParams::default()
        };
        let mut history = Vec::new();
        let _ = run(
            &model,
            &params,
            &tools,
            &mut history,
            "go",
            &cancel,
            &|_| {},
        )
        .await;
        let Message::User { content } = &history[2] else {
            panic!("tool result message");
        };
        let UserContent::ToolResult(raw) = &content[0] else {
            panic!("tool result block");
        };
        let requests = model.requests();
        let sent = crate::edge::rig_to_own(&requests[1].chat_history);
        let Message::User { content } = &sent[2] else {
            panic!("tool result message on the wire");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result block on the wire");
        };
        assert_eq!(result.content[0].to_text(), raw.content[0].to_text());
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
        let tools =
            ToolDispatch::new([mk_probe("probe_a", &barrier), mk_probe("probe_b", &barrier)]);
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

    // --- Context-overflow recovery (C.5) ---

    fn overflow_error_turn() -> Vec<MockStreamEvent> {
        vec![MockStreamEvent::Error(
            rig_core::test_utils::MockError::provider(
                "This model's maximum context length is 4096 tokens. However, you requested 8192 tokens.",
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
        let mut messages = Vec::new();
        for i in 0..8 {
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
        // Window sized like the engine tests so the estimate overflows the
        // buffer-subtracted window and the VCC stage is forced to run.
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
        // No usage is reported on a failed request, so recalibration is a
        // safe no-op: the multiplier never moved.
        assert_eq!(
            shared.lock().unwrap().estimator.multiplier(),
            1.0,
            "calibration must no-op without reported input tokens"
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

    /// The clamp mirrors the reference `clamped_output_tokens` table.
    #[test]
    fn clamps_output_tokens_to_the_remaining_window() {
        const WINDOW: u32 = 262_144;
        let big_cap = 100_000u64;
        let small_cap = 2_048u64;
        let crowding = WINDOW as u64 - big_cap + 1;
        // Cap fits inside the remaining window: unchanged.
        assert_eq!(
            clamped_max_tokens(Some(WINDOW), 1_000, Some(big_cap)),
            Some(big_cap)
        );
        // Cap exceeds the remaining window: reduced to what remains.
        assert_eq!(
            clamped_max_tokens(Some(WINDOW), crowding, Some(big_cap)),
            Some(WINDOW as u64 - crowding)
        );
        // Prompt over the window: floored at the minimum.
        assert_eq!(
            clamped_max_tokens(Some(WINDOW), WINDOW as u64 + 1, Some(big_cap)),
            Some(MIN_OUTPUT_TOKENS)
        );
        // The floor never raises the cap above what was configured.
        assert_eq!(
            clamped_max_tokens(Some(WINDOW), WINDOW as u64 + 1, Some(small_cap)),
            Some(small_cap)
        );
        // No window or no configured cap: the provider picks its own.
        assert_eq!(
            clamped_max_tokens(None, 1_000, Some(big_cap)),
            Some(big_cap)
        );
        assert_eq!(clamped_max_tokens(Some(WINDOW), 1_000, None), None);
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
        let (model, outcome, events) = reauth_run(
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

    #[tokio::test]
    async fn done_by_model_carries_priced_and_unpriced_models() {
        async fn collect_by_model(spec: &str) -> HashMap<String, crate::usage::StoredTokenUsage> {
            let (model, _turns) = stream_turns(vec![vec![
                MockStreamEvent::text("hi"),
                MockStreamEvent::final_response(rig_core::completion::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..rig_core::completion::Usage::new()
                }),
            ]]);
            let tools = crate::tools::Workspace::new(std::env::temp_dir())
                .unwrap()
                .register();
            let (_, cancel) = cancel_channel();
            let mut history = Vec::new();
            let params = RunParams {
                model_spec: Some(spec.into()),
                ..RunParams::default()
            };
            let by_model = std::sync::Arc::new(std::sync::Mutex::new(None));
            let sink = std::sync::Arc::clone(&by_model);
            run(
                &model,
                &params,
                &tools,
                &mut history,
                "hello",
                &cancel,
                &|event| {
                    if let Event::Done { by_model, .. } = event {
                        *sink.lock().unwrap() = Some(by_model);
                    }
                },
            )
            .await;
            sink.lock().unwrap().take().unwrap()
        }

        let priced = collect_by_model("anthropic/claude-sonnet-5").await;
        let usage = priced
            .get("anthropic/claude-sonnet-5")
            .expect("priced model recorded");
        assert!(usage.cost.is_some_and(|c| c > 0.0));
        assert!(usage.input + usage.output > 0);

        let unpriced = collect_by_model("mock/no-such-model").await;
        let usage = unpriced
            .get("mock/no-such-model")
            .expect("unpriced model still counts tokens");
        assert_eq!(usage.cost, None);
        assert!(usage.input + usage.output > 0);
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
}
