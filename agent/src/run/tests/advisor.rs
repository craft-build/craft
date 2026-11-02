//! The post-turn advisor: a review rides the same script after a
//! terminal reply, its note may continue the run as a driver follow-up,
//! and a failure never fails the run.

use super::*;
use crate::history::ext;

fn done_turn(text: &str) -> Attempt {
    crate::testing::text_turn(text)
}

fn notes(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Craft(crate::harness::CraftEvent::AdvisorNote { severity, message }) => {
                Some((severity.clone(), message.clone()))
            }
            _ => None,
        })
        .collect()
}

/// A conversation with the advisor configured from `config`.
async fn advisor_conversation(
    dir: &tempfile::TempDir,
    provider: &crate::testing::ScriptedProvider,
    config: &Config,
) -> (Conversation, EventStream) {
    let workspace = Workspace::new(dir.path()).unwrap();
    let history = animist::history::History::default();
    conversation_over(
        provider,
        config,
        &dir.path().display().to_string(),
        workspace.registry(&history).unwrap(),
        Some(history),
    )
    .await
}

/// Without the advisor wired, a plain turn makes exactly one request and
/// surfaces no notes.
#[tokio::test]
async fn disabled_advisor_makes_no_extra_calls() {
    let provider = script(vec![done_turn("done")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "do the thing").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(provider.requests().len(), 1);
    assert!(notes(&events_until_done(&mut stream).await).is_empty());
}

#[tokio::test]
async fn advisor_note_continues_the_run_with_a_followup() {
    let provider = script(vec![
        done_turn("first reply"),
        done_turn("concern: fix X"),
        done_turn("fixed reply"),
        done_turn("ok"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = advisor_conversation(
        &dir,
        &provider,
        &Config::parse("agent { advisor { enabled = true } }").unwrap(),
    )
    .await;
    let outcome = run_turn(&conversation, "do the thing").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "fixed reply"),
        "{outcome:?}"
    );
    assert_eq!(
        notes(&events_until_done(&mut stream).await),
        vec![("concern".to_string(), "fix x".to_string())]
    );
    // Two advisor reviews rode the same script: the run's reply, the
    // review, the follow-up run, the next review.
    assert_eq!(provider.requests().len(), 4);
    assert!(
        stored(&conversation).await.iter().any(|m| {
            let text = ext::message_text(m);
            text.contains("<advisor-note>") && text.contains("fix x")
        }),
        "follow-up must be committed to history"
    );
}

#[tokio::test]
async fn below_threshold_note_stops_but_surfaces() {
    let provider = script(vec![done_turn("first reply"), done_turn("nit: tiny thing")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = advisor_conversation(
        &dir,
        &provider,
        &Config::parse("agent { advisor { enabled = true } }").unwrap(),
    )
    .await;
    let outcome = run_turn(&conversation, "do the thing").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "first reply"),
        "{outcome:?}"
    );
    assert_eq!(
        notes(&events_until_done(&mut stream).await),
        vec![("nit".to_string(), "tiny thing".to_string())]
    );
    // The review rode the script; no follow-up turn ran.
    assert_eq!(provider.requests().len(), 2, "no follow-up turn");
    assert!(
        !stored(&conversation)
            .await
            .iter()
            .any(|m| { ext::message_text(m).contains("<advisor-note>") })
    );
}

#[tokio::test]
async fn continuation_cap_stops_after_max_act_turns() {
    let provider = script(vec![
        done_turn("first"),
        done_turn("concern: fix X"),
        done_turn("second"),
        done_turn("concern: fix Y"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = advisor_conversation(
        &dir,
        &provider,
        &Config::parse("agent { advisor { enabled = true\n max_act_turns = 1 } }").unwrap(),
    )
    .await;
    let outcome = run_turn(&conversation, "do the thing").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "second"),
        "{outcome:?}"
    );
    assert_eq!(
        notes(&events_until_done(&mut stream).await).len(),
        2,
        "both notes surface"
    );
    // 2 run turns + 2 advisor reviews, then the cap stops the loop.
    assert_eq!(provider.requests().len(), 4, "cap stops the loop");
}

#[tokio::test]
async fn advisor_failure_never_fails_the_run() {
    let provider = script(vec![
        done_turn("done"),
        crate::testing::http_error(429, "rate limited"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = advisor_conversation(
        &dir,
        &provider,
        &Config::parse("agent { advisor { enabled = true } }").unwrap(),
    )
    .await;
    let outcome = run_turn(&conversation, "do the thing").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert!(notes(&events_until_done(&mut stream).await).is_empty());
}
