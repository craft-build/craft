//! Cost/billing accounting (`served_spec`, the priced `by_model` ledger)
//! and the output-token clamp.

use super::*;
use crate::run::*;
use rig_core::test_utils::MockStreamEvent;
use std::collections::HashMap;

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

/// The output-cap clamp runs on the overhead-inclusive estimate: with the
/// tool schemas riding along, the window must reserve room for them, not
/// just the messages.
#[tokio::test]
async fn output_cap_clamp_counts_request_overhead() {
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("ok"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let overhead =
        crate::compaction::RequestOverhead::from_parts(None, &tools.definitions()).tokens();
    assert!(overhead > 0, "the builtin tool table carries schemas");
    let shared: SharedCompactionState = std::sync::Arc::new(std::sync::Mutex::new(
        crate::compaction::CompactionState::default(),
    ));
    let params = |context_length: Option<u32>| {
        RunParams {
            max_tokens: Some(100_000),
            model_spec: Some("mock/x".into()),
            ..RunParams::default()
        }
        .with_compaction(CompactionCtx {
            state: shared.clone(),
            stages: Vec::new(),
            buffer: crate::config::CompactionBuffer::Percent(20),
            context_length,
        })
    };
    let (_, cancel) = cancel_channel();
    // The prompt is the only history: its estimate is the message-structure
    // cost alone, far under the overhead.
    let prompt_tokens = crate::compaction::estimate_tokens(&[Message::user("hi")]);
    let margin: u64 = 50_000;
    let window = (prompt_tokens + overhead + margin) as u32;
    // A history-only estimate would leave margin + overhead of window;
    // counting the overhead must clamp to exactly `margin`.
    let mut history = Vec::new();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&events);
    run(
        &model,
        &params(Some(window)),
        &tools,
        &mut history,
        "hi",
        &cancel,
        &|event| sink.lock().unwrap().push(event),
    )
    .await;
    assert_eq!(model.requests().len(), 1);
    assert_eq!(
        model.requests()[0].max_tokens,
        Some(margin),
        "the clamp reserves the window for history + request overhead"
    );
}

/// Without a window the configured cap stands untouched and compaction
/// never triggers (pre-change behavior preserved).
#[tokio::test]
async fn unknown_window_leaves_the_cap_and_skips_compaction() {
    let (model, _turns) = stream_turns(vec![vec![
        MockStreamEvent::text("ok"),
        MockStreamEvent::final_response_with_total_tokens(1),
    ]]);
    let dir = tempfile::tempdir().unwrap();
    let tools = crate::tools::Workspace::new(dir.path()).unwrap().register();
    let shared: SharedCompactionState = std::sync::Arc::new(std::sync::Mutex::new(
        crate::compaction::CompactionState::default(),
    ));
    let params = RunParams {
        max_tokens: Some(100_000),
        model_spec: Some("mock/x".into()),
        ..RunParams::default()
    }
    .with_compaction(CompactionCtx {
        state: shared.clone(),
        stages: vec![crate::config::CompactionConfig {
            kind: crate::config::CompactionKind::Vcc,
            context: 0.1,
        }],
        buffer: crate::config::CompactionBuffer::Percent(20),
        context_length: None,
    });
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&events);
    run(
        &model,
        &params,
        &tools,
        &mut history,
        "hi",
        &cancel,
        &|event| sink.lock().unwrap().push(event),
    )
    .await;
    assert_eq!(
        model.requests()[0].max_tokens,
        Some(100_000),
        "no window: the configured cap is untouched"
    );
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, Event::AutoCompacting { .. })),
        "no window: compaction never triggers"
    );
}
