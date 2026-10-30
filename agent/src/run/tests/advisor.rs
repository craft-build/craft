/// C.12 advisor: the run's stream and the advisor's side completion draw
/// from one scripted queue in call order — stream reply, advisor review,
/// follow-up stream reply, next review — so the interleaving each test
/// scripts is exactly the call sequence.
#[cfg(test)]
mod advisor_tests {
    use crate::config::{AdvisorAutoAct, AdvisorConfig};
    use crate::provider::ModelHandle;
    use crate::run::{self, Event, RunOutcome, ToolDispatch, cancel_channel};
    use crate::testing::{Attempt, ScriptedProvider};

    fn done_turn(text: &str) -> Attempt {
        crate::testing::text_turn(text)
    }

    /// A model over the scripted attempts, and the provider recording
    /// every request. The advisor's reviews carry the advisor preamble;
    /// the run's streams carry the run's (empty in these tests).
    fn model(turns: Vec<Attempt>) -> (ModelHandle, ScriptedProvider) {
        let provider = ScriptedProvider::new(Vec::new());
        for turn in turns {
            provider.push(turn);
        }
        (
            ModelHandle::scripted(crate::testing::scripted_model(
                &provider,
                crate::testing::model_info("scripted", 200_000, 8_192, false, false),
            )),
            provider,
        )
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
        model: &ModelHandle,
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

    /// The advisor's side completions among the recorded requests: the
    /// ones carrying the advisor preamble.
    fn advisor_reviews(provider: &ScriptedProvider) -> Vec<crate::testing::RecordedRequest> {
        provider
            .requests()
            .into_iter()
            .filter(|request| !request.system.is_empty())
            .collect()
    }

    #[tokio::test]
    async fn note_at_threshold_continues_the_run() {
        let (model, provider) = model(vec![
            done_turn("first reply"),
            done_turn("concern: fix X"),
            done_turn("fixed reply"),
            done_turn("ok"),
        ]);
        let (outcome, history, events) = run_with(&model, &advisor_params()).await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "fixed reply"),
            "{outcome:?}"
        );
        assert_eq!(
            notes(&events),
            vec![("concern".to_string(), "fix x".to_string())]
        );
        assert_eq!(advisor_reviews(&provider).len(), 2);
        assert!(
            history
                .iter()
                .any(|m| m.text().contains("<advisor-note>") && m.text().contains("fix x"),),
            "follow-up must be committed to history"
        );
        // The review sees the advisor preamble and no tools.
        let request = &advisor_reviews(&provider)[0];
        assert!(request.tools.is_empty());
    }

    #[tokio::test]
    async fn below_threshold_note_stops_but_surfaces() {
        let mut params = advisor_params();
        params.advisor.auto_act = AdvisorAutoAct::Concern;
        let (model, provider) = model(vec![done_turn("first reply"), done_turn("nit: tiny thing")]);
        let (outcome, history, events) = run_with(&model, &params).await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "first reply"),
            "{outcome:?}"
        );
        assert_eq!(
            notes(&events),
            vec![("nit".to_string(), "tiny thing".to_string())]
        );
        assert_eq!(
            provider.requests().len() - advisor_reviews(&provider).len(),
            1,
            "no follow-up turn"
        );
        assert!(!history.iter().any(|m| m.text().contains("<advisor-note>")));
    }

    #[tokio::test]
    async fn disabled_advisor_makes_no_extra_calls() {
        let (model, provider) = model(vec![done_turn("done")]);
        let (outcome, _, events) = run_with(&model, &run::RunParams::default()).await;
        assert!(matches!(outcome, RunOutcome::Done { .. }));
        assert!(advisor_reviews(&provider).is_empty());
        assert_eq!(provider.requests().len(), 1);
        assert!(notes(&events).is_empty());
    }

    #[tokio::test]
    async fn continuation_cap_stops_after_max_act_turns() {
        let mut params = advisor_params();
        params.advisor.max_act_turns = 1;
        let (model, provider) = model(vec![
            done_turn("first"),
            done_turn("concern: fix X"),
            done_turn("second"),
            done_turn("concern: fix Y"),
        ]);
        let (outcome, _, events) = run_with(&model, &params).await;
        assert!(
            matches!(outcome, RunOutcome::Done { ref reply } if reply == "second"),
            "{outcome:?}"
        );
        assert_eq!(notes(&events).len(), 2, "both notes surface");
        // 2 stream turns + 2 advisor reviews, then the cap stops the loop.
        assert_eq!(provider.requests().len(), 4, "cap stops the loop");
    }

    #[tokio::test]
    async fn advisor_failure_never_fails_the_run() {
        let (model, _provider) = model(vec![
            done_turn("done"),
            crate::testing::http_error(429, "rate limited"),
        ]);
        let (outcome, _, events) = run_with(&model, &advisor_params()).await;
        assert!(matches!(outcome, RunOutcome::Done { .. }), "{outcome:?}");
        assert!(notes(&events).is_empty());
    }

    use crate::history::Message;
}
