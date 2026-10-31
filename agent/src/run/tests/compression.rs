//! Request-view compression: recency tail, read lifecycle, pre-compression
//! skips and the opt-out.

use super::*;
use crate::history::{ContentBlock, Role, ext};

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
    let provider = script(vec![crate::testing::text_turn("ok")]);
    let dir = tempfile::tempdir().unwrap();
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        animist::agent::tool::ToolRegistry::default(),
        sink,
    )
    .recency(std::sync::Arc::new(Turns))
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let requests = provider.requests();
    let sent = &requests[0].messages;
    // The request layer numbers prepares from 1 within a turn.
    assert!(
        ext::text(&sent[0]).contains("<turn-context>\n\nturn 1"),
        "tail must ride the request"
    );
    let messages = stored(&conversation).await;
    assert!(
        !ext::message_text(&messages[0]).contains("turn-context"),
        "tail must not be committed to history"
    );
}

#[tokio::test]
async fn read_lifecycle_marks_request_view_only() {
    // read → write the same file, with enough later read turns that the
    // early read falls outside the working-set lookback. The model's
    // later requests must see the stale marker; history keeps the raw text.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f.txt"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "write",
            serde_json::json!({"path": "f.txt", "content": "changed\n"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path": "g.txt"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t4",
            "read",
            serde_json::json!({"path": "h.txt"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t5",
            "read",
            serde_json::json!({"path": "i.txt"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t6",
            "read",
            serde_json::json!({"path": "j.txt"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let long: String = (1..=40)
        .map(|i| format!("original line {i} of the file\n"))
        .collect();
    for name in ["f.txt", "g.txt", "h.txt", "i.txt", "j.txt"] {
        std::fs::write(dir.path().join(name), &long).unwrap();
    }
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    // Committed history keeps the raw read.
    let messages = stored(&conversation).await;
    let raw_message = messages[2].message();
    assert_eq!(raw_message.role, Role::User, "first read result message");
    let ContentBlock::ToolResult {
        content: raw_content,
        ..
    } = &raw_message.content[0]
    else {
        panic!("tool result block");
    };
    assert!(ext::tool_content_text(raw_content).contains("40: original line 40"));
    // The model's last request carried the stale marker for that read.
    let requests = provider.requests();
    let sent = &requests.last().unwrap().messages;
    let wire_message = &sent[2];
    assert_eq!(
        wire_message.role,
        Role::User,
        "tool result message on the wire"
    );
    let ContentBlock::ToolResult {
        content: result, ..
    } = &wire_message.content[0]
    else {
        panic!("tool result block on the wire");
    };
    let wire_text = ext::tool_content_text(result);
    assert!(
        wire_text.starts_with("[Stale read: "),
        "expected a stale marker, got: {wire_text}"
    );
    assert!(wire_text.contains("Re-read the file"));
}

#[tokio::test]
async fn read_output_stays_verbatim_on_wire() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "big.txt"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=100)
        .map(|i| format!("filler line number {i} for size\n"))
        .collect();
    std::fs::write(dir.path().join("big.txt"), body).unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    // Committed history and the finished-call event keep the raw result.
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    let raw_text = result_text(&results[0]);
    assert!(raw_text.len() >= crate::compression::MIN_COMPRESS_LEN);
    assert!(!raw_text.contains("lines omitted"));
    assert!(events_until_done(&mut stream).await.iter().any(|event| {
        matches!(
            event,
            Event::Agent(animist::agent::AgentEvent::ToolFinished { output, .. })
                if !ext::tool_content_text(&output.content).contains("lines omitted")
        )
    }));
    // `read` output is caller-selected and must reach the model verbatim,
    // even though its `N: ` numbering looks like code to the detector.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let sent = &requests[1].messages;
    let wire_message = &sent[2];
    assert_eq!(
        wire_message.role,
        Role::User,
        "tool result message on the wire"
    );
    let ContentBlock::ToolResult {
        content: result, ..
    } = &wire_message.content[0]
    else {
        panic!("tool result block on the wire");
    };
    let wire_text = ext::tool_content_text(result);
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
    let mut full = vec![ext::observation(vec![
        ext::tool_result("c1", Some("read"), raw_read.clone(), false),
        ext::tool_result("c2", Some("grep"), raw_grep.clone(), false),
        ext::tool_result("c3", Some("retrieve"), raw_retrieve.clone(), false),
        ext::tool_result("c4", Some("bash"), raw_bash.clone(), false),
    ])];
    crate::run::view::compress_request_view(
        &mut full,
        &crate::compression::CompressionConfig::default(),
    );
    let content = &full[0].content;
    let text = |i: usize| {
        let ContentBlock::ToolResult { content, .. } = &content[i] else {
            panic!("tool result {i}");
        };
        ext::tool_content_text(content)
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
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "big.txt"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=100)
        .map(|i| format!("filler line number {i} for size\n"))
        .collect();
    std::fs::write(dir.path().join("big.txt"), body).unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("compression { enabled = false }").unwrap(),
        &dir.path().display().to_string(),
        workspace.registry().unwrap(),
        None,
    )
    .await;

    let _ = run_turn(&conversation, "go").await;

    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    let raw_text = result_text(&results[0]);
    // With compression off even a non-verbatim tool's output goes raw; the
    // read here only proves the pipeline never touches its bytes.
    let requests = provider.requests();
    let sent = &requests[1].messages;
    let wire_message = &sent[2];
    assert_eq!(
        wire_message.role,
        Role::User,
        "tool result message on the wire"
    );
    let ContentBlock::ToolResult {
        content: result, ..
    } = &wire_message.content[0]
    else {
        panic!("tool result block on the wire");
    };
    assert_eq!(ext::tool_content_text(result), raw_text);
}
