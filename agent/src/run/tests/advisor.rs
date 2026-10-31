//! C.12 advisor: the post-turn review that may continue the run with a
//! committed note. The review machinery returns with 09's recovery layers;
//! until then the advisor never runs, which the first test pins.

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
#[ignore = "restored in 09"]
async fn note_at_threshold_continues_the_run() {
    let provider = script(vec![
        done_turn("first reply"),
        done_turn("concern: fix X"),
        done_turn("fixed reply"),
        done_turn("ok"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
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
    // review, the follow-up, the next review.
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
#[ignore = "restored in 09"]
async fn below_threshold_note_stops_but_surfaces() {
    let provider = script(vec![done_turn("first reply"), done_turn("nit: tiny thing")]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
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
#[ignore = "restored in 09"]
async fn continuation_cap_stops_after_max_act_turns() {
    let provider = script(vec![
        done_turn("first"),
        done_turn("concern: fix X"),
        done_turn("second"),
        done_turn("concern: fix Y"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
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
    // 2 stream turns + 2 advisor reviews, then the cap stops the loop.
    assert_eq!(provider.requests().len(), 4, "cap stops the loop");
}

#[tokio::test]
#[ignore = "restored in 09"]
async fn advisor_failure_never_fails_the_run() {
    let provider = script(vec![
        done_turn("done"),
        crate::testing::http_error(429, "rate limited"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "do the thing").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert!(notes(&events_until_done(&mut stream).await).is_empty());
}
