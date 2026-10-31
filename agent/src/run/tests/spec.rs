//! Cost/billing accounting (the priced `by_model` ledger on the terminal
//! Done) and the output-token clamp.

use super::*;

/// The clamp mirrors the reference `clamped_output_tokens` table.
#[test]
fn clamps_output_tokens_to_the_remaining_window() {
    use crate::run::{MIN_OUTPUT_TOKENS, clamped_max_tokens};
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
async fn done_by_model_counts_the_served_model_without_a_price() {
    // The scripted provider's spec resolves to no priced model, so the
    // ledger counts its tokens and shows no cost — never "$0.000".
    let provider = script(vec![crate::testing::text_turn("hi")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "hello").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let done = events_until_done(&mut stream)
        .await
        .into_iter()
        .find_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::Done { by_model, cost, .. }) => {
                Some((by_model, cost))
            }
            _ => None,
        })
        .expect("a terminal Done");
    let (by_model, cost) = done;
    assert_eq!(cost, None, "an unpriced run shows no cost");
    let usage = by_model
        .get(&conversation.model_spec())
        .expect("the served model is recorded");
    assert_eq!(usage.cost, None);
    assert!(usage.input + usage.output > 0, "tokens still counted");
}

/// The output-cap clamp runs on the overhead-inclusive estimate: with the
/// tool schemas and system prompt riding along, the window must reserve
/// room for them, not just the messages.
#[tokio::test]
async fn output_cap_clamp_counts_request_overhead() {
    let provider = script(vec![crate::testing::text_turn("ok")]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let dispatch = workspace.register();
    let cwd = dir.path().display().to_string();
    // The system bytes the request layer will send, mirrored from its own
    // construction so the overhead below is the one the clamp sees.
    let system = crate::prompt::build_system_prompt(
        &crate::prompt::Vars::new()
            .set("{cwd}", cwd.as_str())
            .set("{platform}", std::env::consts::OS)
            .set("{date}", crate::prompt::today_utc()),
        "",
        &crate::prompt::ResolvedSlots::default(),
        None,
    );
    let overhead = crate::compaction::RequestOverhead::from_parts(
        Some(system.as_str()),
        &dispatch.definitions(),
    )
    .tokens();
    assert!(overhead > 0, "the builtin tool table carries schemas");
    // The prompt is the only history: its estimate is the message-structure
    // cost alone, far under the overhead.
    let prompt_tokens =
        crate::compaction::estimate_tokens(&[crate::history::ext::stored_user_text("hi")]);
    let margin: u64 = 50_000;
    let window = (prompt_tokens + overhead + margin) as u32;
    let (sink, _stream) = EventSink::new();
    let handle = ModelHandle::scripted(crate::testing::scripted_model(
        &provider,
        crate::testing::model_info("mock/x", window, 8_192, false, false),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("agent { max_tokens = 100000 }").unwrap(),
        &cwd,
        "",
        handle,
        dispatch,
        sink,
    )
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "hi").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].options.max_output_tokens,
        Some(margin as u32),
        "the clamp reserves the window for history + request overhead"
    );
}

/// Without a window the configured cap stands untouched and compaction
/// never triggers (pre-change behavior preserved).
#[tokio::test]
async fn unknown_window_leaves_the_cap_and_skips_compaction() {
    let provider = script(vec![crate::testing::text_turn("ok")]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(crate::testing::scripted_model(
        &provider,
        crate::testing::model_info_with("mock/x", None, None),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("agent { max_tokens = 100000 }").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        workspace.register(),
        sink,
    )
    .build()
    .unwrap();
    let mut stream = stream;

    let outcome = run_turn(&conversation, "hi").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let requests = provider.requests();
    assert_eq!(
        requests[0].options.max_output_tokens,
        Some(100_000),
        "no window: the configured cap is untouched"
    );
    assert!(
        !events_until_done(&mut stream)
            .await
            .iter()
            .any(|event| matches!(
                event,
                Event::Craft(crate::harness::CraftEvent::AutoCompacting { .. })
            )),
        "no window: compaction never triggers"
    );
}
