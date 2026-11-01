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
async fn read_lifecycle_markers_are_durable_with_raw_sources() {
    // read → write the same file, with enough later read turns that the
    // early read falls outside the working-set lookback. The model's later
    // requests see the stale marker in-run, and once the turn ends the
    // marker is a durable revision: the latest content carries it while the
    // entry source keeps the raw text for retrieval.
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
    // The model's last request carried the stale marker for that read
    // (the in-run request view rewrote it as before).
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
    // After the turn, the durable alteration revised the entry: the latest
    // content is the marker, tagged as a lifecycle alteration, while the
    // entry source keeps the raw read.
    let entries = entries(&conversation).await;
    let entry = &entries[2];
    let current = entry.current().expect("the read result stays active");
    assert_eq!(current.message().role, Role::User);
    let ContentBlock::ToolResult {
        content: marked, ..
    } = &current.message().content[0]
    else {
        panic!("tool result block");
    };
    assert!(
        ext::tool_content_text(marked).starts_with("[Stale read: "),
        "the durable revision is the marker: {}",
        ext::tool_content_text(marked)
    );
    assert_eq!(
        current
            .tags()
            .get(crate::compaction::alter::ALTER_TAG)
            .map(String::as_str),
        Some(crate::compaction::alter::ALTER_STALE)
    );
    // The pre-alteration revision keeps the raw read (the agent's squash
    // makes the entry source the interrupted placeholder, so the revision
    // under the marker is where the tool's return lives).
    let raw = raw_tool_results(&conversation).await;
    assert!(
        result_text(&raw[0]).contains("40: original line 40"),
        "the raw read stays recoverable: {}",
        result_text(&raw[0])
    );
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
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
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

/// The in-run request rewrites only cover results the current run
/// produced; everything older arrived pre-altered from history (08). On
/// the wire: an old seeded result is durably compressed, a seeded result
/// still inside the protected recent window stays raw, and a result this
/// run produced is compressed in the view as before.
#[tokio::test]
async fn request_view_scope_is_in_run_only() {
    // SAFETY: the bash call must run without the host's nested sandbox
    // (the pattern `dedup_cleared_by_bash` uses).
    unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t9",
            "bash",
            serde_json::json!({"command": "for i in $(seq 1 60); do echo \"$i: let x = $i;\"; done"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    // Three seeded bash rounds: the first leaves the two-entry recent
    // window before the turn starts.
    let mut seeded = Vec::new();
    for id in ["t1", "t2", "t3"] {
        seeded.push(crate::compaction::test_support::stored_assistant_tool_args(
            id,
            "bash",
            serde_json::json!({"command": "cat file"}),
        ));
        let output: String = (1..=60)
            .map(|i| format!("{i}: seeded line {i}\n"))
            .collect();
        seeded.push(crate::compaction::test_support::stored_tool_result_of(
            id, &output,
        ));
    }
    let workspace = Workspace::new(dir.path()).unwrap();
    let history = seeded_history(seeded).await;
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace.registry(&history).unwrap(),
        Some(history),
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let requests = provider.requests();
    let sent = &requests.last().unwrap().messages;
    let text_of = |message: &crate::history::Message| -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(ext::tool_content_text(content)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n--\n")
    };
    let wire_texts: Vec<String> = sent
        .iter()
        .filter(|message| ext::is_user(message))
        .map(text_of)
        .filter(|text| !text.is_empty())
        .collect();
    // Seeded t1 (outside the window) arrived durably compressed; t2/t3
    // (inside the window) raw; the in-run t9 result compressed in the view.
    assert!(
        wire_texts[0].contains("lines omitted"),
        "the old seeded result is compressed: {}",
        wire_texts[0]
    );
    assert!(
        wire_texts[1].contains("60: seeded line 60"),
        "a result still inside the protected window stays raw: {}",
        wire_texts[1]
    );
    assert!(
        wire_texts[2].contains("60: seeded line 60"),
        "the newest seeded result stays raw too: {}",
        wire_texts[2]
    );
    assert!(
        wire_texts[3].contains("lines omitted"),
        "the in-run result is compressed in the view: {}",
        wire_texts[3]
    );
}

/// 08's measurement, pinned: one scripted turn whose run edits a file a
/// *previous* turn read (the read lives in the request prefix). Compare the
/// old full-view rewrites (lifecycle + compression recomputed from raw
/// history every step) against the durable-alteration model (prefix
/// pre-altered between runs, suffix-only in run): the same request bytes
/// overall, but the old model mutates already-sent prefix bytes mid-run —
/// the provider-cache invalidation durable alterations remove — while the
/// new model changes the prefix exactly once, between runs.
#[tokio::test]
async fn durable_alters_match_old_view_bytes_without_mid_run_prefix_mutations() {
    let big = |n: usize| -> String {
        (1..=n)
            .map(|i| format!("{i}: file line {i} of the read\n"))
            .collect()
    };
    // The classifier only reads assistant tool calls, so the calls ride
    // assistant messages and the results user observations, the shapes the
    // agent commits.
    let assistant_call = |id: &str, name: &str, path: String| crate::history::Message {
        role: Role::Assistant,
        content: vec![crate::history::ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({"path": path}),
            thought_signature: None,
        }],
        display_text: None,
        kind: animist::history::MessageKind::Turn,
    };
    // The previous turn left a big read of f1 (the prefix); this turn
    // edits f1, so the read turns Stale while the run is in flight.
    let prefix = vec![
        ext::user_text("earlier task"),
        assistant_call("t0", "read", "f1".to_owned()),
        ext::observation(vec![ext::tool_result("t0", Some("read"), big(200), false)]),
    ];
    // This run's states, each step appending one exchange.
    let mut run_steps: Vec<Vec<crate::history::Message>> = vec![
        vec![ext::user_text("go")],
        vec![
            assistant_call("t2", "edit", "f1".to_owned()),
            ext::observation(vec![ext::tool_result("t2", Some("edit"), "ok", false)]),
        ],
        vec![
            assistant_call("t3", "read", "f2".to_owned()),
            ext::observation(vec![ext::tool_result("t3", Some("read"), big(200), false)]),
        ],
    ];
    // Later read rounds of other files, so the f1 edit leaves the
    // working-set lookback and the prefix's f1 read turns Stale mid-run.
    for gap in 0..crate::run::read_lifecycle::WORKING_SET_LOOKBACK + 1 {
        let id = format!("g{gap}");
        run_steps.push(vec![
            assistant_call(&id, "read", format!("gap{gap}.txt")),
            ext::observation(vec![ext::tool_result(&id, Some("read"), "gap", false)]),
        ]);
    }
    let config = crate::compression::CompressionConfig::default();
    let bytes = |messages: &[crate::history::Message]| -> usize {
        messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        ContentBlock::ToolResult { content, .. } => {
                            ext::tool_content_text(content).len()
                        }
                        ContentBlock::ToolUse { input, .. } => input.to_string().len(),
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum()
    };

    // Build the raw request state per step: prefix + the run so far.
    let states: Vec<Vec<crate::history::Message>> = {
        let mut acc = prefix.clone();
        let mut states = Vec::new();
        for step in &run_steps {
            acc.extend(step.iter().cloned());
            states.push(acc.clone());
        }
        states
    };
    let run_start = prefix.len();

    let score = |suffix_only: bool| -> (usize, usize) {
        let mut total = 0;
        let mut mid_run_mutations = 0;
        let mut previous: Option<Vec<crate::history::Message>> = None;
        for raw in &states {
            let mut view = raw.clone();
            if suffix_only {
                crate::run::read_lifecycle::apply_to_request_from(&mut view, None, run_start);
                crate::run::view::compress_request_view_from(&mut view, &config, run_start);
            } else {
                crate::run::read_lifecycle::apply_to_request(&mut view, None);
                crate::run::view::compress_request_view(&mut view, &config);
            }
            total += bytes(&view);
            // Cache stability: bytes already sent on an earlier request must
            // not change on a later one (the shared prefix of consecutive
            // requests is what a provider cache keys on).
            if let Some(previous) = &previous {
                let shared = previous.len().min(view.len());
                if view[..shared] != previous[..shared] {
                    mid_run_mutations += 1;
                }
            }
            previous = Some(view);
        }
        (total, mid_run_mutations)
    };

    let (old_total, old_mutations) = score(false);
    let (new_total, new_mutations) = score(true);
    eprintln!(
        "measurement: old total={old_total}B mid-run-prefix-changes={old_mutations}; \
         new total={new_total}B mid-run-prefix-changes={new_mutations}"
    );
    // The measured trade: the old model shrinks the stale read from the
    // step it turns stale (smaller totals, paid for with a mid-run rewrite
    // of bytes the provider already cached); the durable model keeps the
    // prefix untouched for the rest of the turn and pays the same shrink
    // once, at the boundary.
    assert!(
        old_total < new_total && (new_total - old_total) * 3 < old_total,
        "same order of request bytes: old={old_total} new={new_total}"
    );
    assert!(
        old_mutations >= 1,
        "premise: the old view rewrites already-sent prefix bytes mid-run"
    );
    assert_eq!(
        new_mutations, 0,
        "the durable-alteration model never mutates sent bytes mid-run"
    );
}

/// A failed turn leaves its input unanswered: the carry anchor must hold
/// it (and everything after) out of any summary the between-runs engine
/// runs, so a forced `/compact` right after a failure cannot summarize the
/// unanswered prompt away.
#[tokio::test]
async fn failed_turns_input_survives_a_forced_compaction() {
    // The 500 fails the turn; the extra turn serves the forced
    // compaction's summarizer.
    let provider = script(vec![
        crate::testing::http_error(500, "server exploded"),
        crate::testing::text_turn("summary"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    // A history long enough that a forced VCC pass has a boundary to cut.
    let mut seeded = Vec::new();
    for i in 0..8 {
        seeded.push(crate::compaction::test_support::stored_user(&format!(
            "do task {i} with a fairly long instruction"
        )));
        seeded.push(crate::compaction::test_support::stored_assistant_tool_args(
            &format!("t{i}"),
            "bash",
            serde_json::json!({"command": format!("echo {i}")}),
        ));
        seeded.push(crate::compaction::test_support::stored_tool_result_of(
            &format!("t{i}"),
            &"x".repeat(200),
        ));
    }
    let workspace = Workspace::new(dir.path()).unwrap();
    let history = seeded_history(seeded).await;
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace.registry(&history).unwrap(),
        Some(history),
    )
    .await;

    let outcome = run_turn(&conversation, "precious unanswered prompt").await;

    assert!(matches!(outcome, TurnOutcome::Failed(_)), "{outcome:?}");
    // The user forces a compaction before trying again.
    let ran = crate::harness::compact_between_runs(&conversation, true).await;
    assert!(ran, "the long history compacts under force");
    let view = stored(&conversation).await;
    assert!(
        view.iter()
            .any(|message| ext::message_text(message) == "precious unanswered prompt"),
        "the unanswered input stays verbatim in the active view"
    );
    // And the anchor says so.
    assert_eq!(
        conversation.compaction().lock().await.carry_from(),
        Some(
            conversation
                .history()
                .snapshot()
                .await
                .entries()
                .iter()
                .position(
                    |entry| ext::message_text(entry.verbatim()) == "precious unanswered prompt"
                )
                .expect("the input entry")
        )
    );
}
