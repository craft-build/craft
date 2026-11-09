//! Opt-in live provider smoke tests (animist 12): real model calls against
//! a local or explicitly approved endpoint. Never run by default — each is
//! `#[ignore]`d and additionally gated on `CRAFT_LIVE_BASE_URL`, so CI and
//! `cargo test` never make paid calls. Run explicitly:
//!
//! ```text
//! CRAFT_LIVE_BASE_URL=http://127.0.0.1:8000/v1 \
//! CRAFT_LIVE_API_KEY=…  CRAFT_LIVE_MODEL=orion-26b \
//!     cargo test -p craft --lib conformance::live -- --ignored --nocapture
//! ```
//!
//! `CRAFT_LIVE_KIND` picks the config kind (`openai-compatible` default;
//! `llama-cpp`, `omlx`, `ollama`, `anthropic`, …). The same environment
//! also serves the manual QA session described in `agent/README.md`.

use crate::config::Config;
use crate::harness::{Event, EventSink, TurnOutcome};
use crate::provider::ProviderHandle;
use crate::tools::Workspace;

/// The live wiring from the env gate, or `None` (skip) when unset.
fn live_bml() -> Option<(String, String)> {
    let base_url = std::env::var("CRAFT_LIVE_BASE_URL").ok()?;
    let kind = std::env::var("CRAFT_LIVE_KIND").unwrap_or_else(|_| "openai-compatible".to_owned());
    let model = std::env::var("CRAFT_LIVE_MODEL").ok()?;
    let key_line = match std::env::var("CRAFT_LIVE_API_KEY") {
        Ok(key) if !key.is_empty() => format!("  api_key = \"{key}\"\n"),
        _ => String::new(),
    };
    let bml = format!(
        "provider \"live\" {{\n  kind = \"{kind}\"\n  base_url = \"{base_url}\"\n{key_line}\
         \n  discover_models = false\n  model \"{model}\" {{ context_length = 100000 }}\n}}\n"
    );
    Some((bml, model))
}

/// One live turn through a real conversation; prints the reply.
async fn live_turn(bml: &str, prompt: &str, with_tools: bool) -> TurnOutcome {
    let config = Config::parse(bml).expect("live config parses");
    let provider_config = config.providers.get("live").expect("alias").clone();
    let catalog = crate::provider::load_catalog().await;
    let handle = ProviderHandle::from_config(&provider_config, Some(&catalog))
        .await
        .expect("live provider constructs");
    let model_id = provider_config
        .models()
        .keys()
        .next()
        .expect("one configured model")
        .clone();
    let model = handle
        .model(&provider_config, Some(&catalog), None, &model_id)
        .await
        .expect("live model builds");

    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(dir.path().join("note.txt"), "live smoke note\n").unwrap();
    let workspace = Workspace::new(dir.path()).expect("workspace");
    let (sink, stream) = EventSink::new();
    let history = animist::history::History::default();
    let registry = if with_tools {
        workspace.registry(&history).expect("registry")
    } else {
        animist::agent::tool::ToolRegistry::default()
    };
    let conversation = crate::harness::ConversationBuilder::new(
        &config,
        &dir.path().display().to_string(),
        "",
        model,
        registry,
        sink,
    )
    .history(history)
    .build()
    .expect("conversation builds");
    let mut stream = stream;

    let run = crate::harness::run_turn(
        &conversation,
        prompt,
        &[],
        animist::agent::Cancellation::default(),
    );
    tokio::pin!(run);
    // A stalled live endpoint must fail the test, not hang it.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(event) = stream.next() => {
                if let Event::Agent(animist::agent::AgentEvent::Provider {
                    event: animist::provider::ProviderEvent::TextDelta { text },
                    ..
                }) = &event
                {
                    print!("{text}");
                }
                    if matches!(&event, Event::Craft(crate::harness::CraftEvent::Done { .. })) {
                        break run.await;
                    }
                }
            }
        }
    });
    println!();
    outcome.await.expect("live turn finished within 120 s")
}

/// One live text round trip: any non-empty reply passes.
#[tokio::test]
#[ignore = "live model call: set CRAFT_LIVE_BASE_URL/_MODEL (see module docs) and run with --ignored"]
async fn live_text_round_trip() {
    let Some((bml, _)) = live_bml() else {
        eprintln!("skipping: CRAFT_LIVE_BASE_URL / CRAFT_LIVE_MODEL not set");
        return;
    };
    let outcome = live_turn(&bml, "Reply with exactly the word: pong", false).await;
    match outcome {
        TurnOutcome::Done { reply } => assert!(!reply.trim().is_empty(), "empty live reply"),
        other => panic!("live text turn failed: {other:?}"),
    }
}

/// One live tool round trip: the model must read a file and quote it.
#[tokio::test]
#[ignore = "live model call: set CRAFT_LIVE_BASE_URL/_MODEL (see module docs) and run with --ignored"]
async fn live_tool_round_trip() {
    let Some((bml, _)) = live_bml() else {
        eprintln!("skipping: CRAFT_LIVE_BASE_URL / CRAFT_LIVE_MODEL not set");
        return;
    };
    let outcome = live_turn(
        &bml,
        "Read the file note.txt with the read tool, then reply with its exact contents.",
        true,
    )
    .await;
    match outcome {
        TurnOutcome::Done { reply } => assert!(
            reply.contains("live smoke note"),
            "the tool result did not reach the reply: {reply}"
        ),
        other => panic!("live tool turn failed: {other:?}"),
    }
}
