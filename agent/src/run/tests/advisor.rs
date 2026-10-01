/// C.12 advisor: the loop streams, the advisor completes — one mock per
/// channel, because `MockCompletionModel` scripts the two channels from
/// separate queues.
#[cfg(test)]
mod advisor_tests {
    use crate::config::{AdvisorAutoAct, AdvisorConfig};
    use crate::run::{self, Event, RunOutcome, ToolDispatch, cancel_channel};
    use rig_core::completion::{
        CompletionError, CompletionModel, CompletionRequest, CompletionResponse,
    };
    use rig_core::streaming::StreamingCompletionResponse;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent, MockTurn};

    #[derive(Clone)]
    struct SplitMock {
        stream: MockCompletionModel,
        completion: MockCompletionModel,
    }

    impl CompletionModel for SplitMock {
        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, CompletionError> {
            self.completion.completion(request).await
        }

        async fn stream(
            &self,
            request: CompletionRequest,
        ) -> Result<StreamingCompletionResponse, CompletionError> {
            self.stream.stream(request).await
        }
    }

    fn done_turn(text: &str) -> Vec<MockStreamEvent> {
        vec![
            MockStreamEvent::text(text),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]
    }

    fn advisor_params() -> run::RunParams {
        run::RunParams {
            advisor: AdvisorConfig {
                enabled: true,
                dedup_size: 8,
                auto_act: AdvisorAutoAct::Concern,
                max_act_turns: 2,
            },
            ..run::RunParams::default()
        }
    }

    async fn run_with(
        model: &SplitMock,
        params: &run::RunParams,
    ) -> (RunOutcome, Vec<Message>, Vec<Event>) {
        let (_, cancel) = cancel_channel();
        let mut history = Vec::new();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        let outcome = run::run(
            model,
            params,
            &ToolDispatch::default(),
            &mut history,
            "do the thing",
            &cancel,
            &move |event| sink.lock().unwrap().push(event),
        )
        .await;
        let events = std::sync::Arc::try_unwrap(events)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_default();
        (outcome, history, events)
    }

    fn notes(events: &[Event]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::AdvisorNote { severity, message } => {
                    Some((severity.clone(), message.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn note_at_threshold_continues_the_run() {
        let model = SplitMock {
            stream: MockCompletionModel::from_stream_turns([
                done_turn("first reply"),
                done_turn("fixed reply"),
            ]),
            completion: MockCompletionModel::new([
                MockTurn::text("concern: fix X"),
                MockTurn::text("ok"),
            ]),
        };
        let (outcome, history, events) = run_with(&model, &advisor_params()).await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "fixed reply"),
            "{outcome:?}"
        );
        assert_eq!(
            notes(&events),
            vec![("concern".to_string(), "fix x".to_string())]
        );
        assert_eq!(model.completion.request_count(), 2);
        assert!(
            history
                .iter()
                .any(|m| m.text().contains("<advisor-note>") && m.text().contains("fix x"),),
            "follow-up must be committed to history"
        );
        // The review sees the advisor preamble and no tools.
        let request = &model.completion.requests()[0];
        assert!(request.tools.is_empty());
    }

    #[tokio::test]
    async fn below_threshold_note_stops_but_surfaces() {
        let mut params = advisor_params();
        params.advisor.auto_act = AdvisorAutoAct::Concern;
        let model = SplitMock {
            stream: MockCompletionModel::from_stream_turns([done_turn("first reply")]),
            completion: MockCompletionModel::new([MockTurn::text("nit: tiny thing")]),
        };
        let (outcome, history, events) = run_with(&model, &params).await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "first reply"),
            "{outcome:?}"
        );
        assert_eq!(
            notes(&events),
            vec![("nit".to_string(), "tiny thing".to_string())]
        );
        assert_eq!(model.stream.request_count(), 1, "no follow-up turn");
        assert!(!history.iter().any(|m| m.text().contains("<advisor-note>")));
    }

    #[tokio::test]
    async fn disabled_advisor_makes_no_extra_calls() {
        let model = SplitMock {
            stream: MockCompletionModel::from_stream_turns([done_turn("done")]),
            completion: MockCompletionModel::new(Vec::<MockTurn>::new()),
        };
        let (outcome, _, events) = run_with(&model, &run::RunParams::default()).await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        assert_eq!(model.completion.request_count(), 0);
        assert!(notes(&events).is_empty());
    }

    #[tokio::test]
    async fn continuation_cap_stops_after_max_act_turns() {
        let mut params = advisor_params();
        params.advisor.max_act_turns = 1;
        let model = SplitMock {
            stream: MockCompletionModel::from_stream_turns([
                done_turn("first"),
                done_turn("second"),
            ]),
            completion: MockCompletionModel::new([
                MockTurn::text("concern: fix X"),
                MockTurn::text("concern: fix Y"),
            ]),
        };
        let (outcome, _, events) = run_with(&model, &params).await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "second"),
            "{outcome:?}"
        );
        assert_eq!(notes(&events).len(), 2, "both notes surface");
        assert_eq!(model.stream.request_count(), 2, "cap stops the loop");
    }

    #[tokio::test]
    async fn advisor_failure_never_fails_the_run() {
        let model = SplitMock {
            stream: MockCompletionModel::from_stream_turns([done_turn("done")]),
            completion: MockCompletionModel::new([MockTurn::error("rate limited")]),
        };
        let (outcome, _, events) = run_with(&model, &advisor_params()).await;
        assert!(matches!(outcome, RunOutcome::Done { .. }), "{outcome:?}");
        assert!(notes(&events).is_empty());
    }

    use crate::history::Message;
}
