//! Request-view compression: recency tail, read lifecycle, pre-compression
//! skips and the opt-out.

use super::*;
use crate::history::UserContent;
use crate::run::*;

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
    let (model, provider) = stream_turns(vec![crate::testing::text_turn("ok")]);
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
    let requests = provider.requests();
    let sent = crate::testing::replay_history(&requests[0].messages);
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
async fn read_lifecycle_marks_request_view_only() {
    // read → write the same file, with enough later read turns that the
    // early read falls outside the working-set lookback. The model's
    // later requests must see the stale marker; history keeps the raw text.
    let (model, provider) = stream_turns(vec![
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
    let requests = provider.requests();
    let sent = crate::testing::replay_history(&requests.last().unwrap().messages);
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
    let (model, provider) = stream_turns(vec![
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
    assert!(raw_text.len() >= crate::compression::MIN_COMPRESS_LEN);
    assert!(!raw_text.contains("lines omitted"));
    assert!(events.lock().unwrap().iter().any(
        |e| matches!(e, Event::ToolDone { result, .. } if !result.content[0]
            .to_text()
            .contains("lines omitted"))
    ));
    // `read` output is caller-selected and must reach the model verbatim,
    // even though its `N: ` numbering looks like code to the detector.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let sent = crate::testing::replay_history(&requests[1].messages);
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
    let (model, provider) = stream_turns(vec![
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
    let requests = provider.requests();
    let sent = crate::testing::replay_history(&requests[1].messages);
    let Message::User { content } = &sent[2] else {
        panic!("tool result message on the wire");
    };
    let UserContent::ToolResult(result) = &content[0] else {
        panic!("tool result block on the wire");
    };
    assert_eq!(result.content[0].to_text(), raw.content[0].to_text());
}
