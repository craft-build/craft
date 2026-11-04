//! Tool execution through the conversation: decisions, dedup, and
//! scheduling of one turn's calls.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use animist::agent::AgentEvent;
use animist::agent::tool::{
    ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolRegistry, ToolScheduling,
};
use async_trait::async_trait;
use serde_json::Value;

use super::*;
use crate::harness::approval::{Approval, ApprovalAsk, ApprovalResponder};
use crate::history::{ContentBlock, Role, ext};

/// Whether a result block reports an error.
fn result_is_error(result: &ContentBlock) -> bool {
    let ContentBlock::ToolResult { is_error, .. } = result else {
        panic!("tool result block");
    };
    *is_error
}

#[tokio::test]
async fn unknown_tool_surfaces_error_and_run_continues() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "no_such_tool",
            serde_json::json!({}),
        )]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        provider.requests().len(),
        2,
        "the model gets a second chance"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        result_text(&results[0]),
        "[error] Unknown tool: no_such_tool",
        "the model-visible wording is the runtime's"
    );
    // The retried request carries the error result, so the model sees what
    // went wrong and can correct the name.
    let replayed = &provider.requests()[1].messages;
    assert!(
        replayed.iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, is_error: true, .. }
                        if ext::tool_content_text(content) == "Unknown tool: no_such_tool")
                })
        }),
        "got: {replayed:?}"
    );
}

#[tokio::test]
async fn responder_denial_reports_reason_to_model() {
    struct Deny;
    impl ApprovalResponder for Deny {
        fn ask(&self, _ask: ApprovalAsk<'_>) -> crate::harness::BoxFuture<Approval> {
            Box::pin(async {
                Approval::Deny {
                    message: "not allowed".into(),
                    remember: None,
                }
            })
        }
    }
    // `bash` prompts under the default rules, so the responder decides it.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "bash",
            serde_json::json!({"command": "echo hi"}),
        )]),
        crate::testing::text_turn("okay"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        sink,
    )
    .policy(crate::harness::PolicyLayerInputs::new(
        Arc::new(crate::permissions::PermissionManager::new(
            crate::permissions::PermissionsConfig::default(),
            dir.path().to_path_buf(),
        )),
        Arc::new(Deny),
    ))
    .workspace(workspace)
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "go").await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "okay"),
        "{outcome:?}"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(result_text(&results[0]), "not allowed");
}

/// The post-execution transform seam is the layer chain's `post_tool_exec`:
/// an output-transforming layer sees every executed call's result.
#[tokio::test]
async fn post_exec_layer_transforms_results() {
    use animist::agent::{Layer, LayerImpl};

    struct Upper;
    #[async_trait]
    impl LayerImpl for Upper {
        async fn post_tool_exec(
            &self,
            _agent: &animist::agent::Agent,
            _context: &ToolContext<'_>,
            output: ToolOutput,
        ) -> animist::agent::Result<ToolOutput> {
            Ok(ToolOutput::builder()
                .content(ext::tool_content_text(&output.content).to_uppercase())
                .is_error(output.is_error)
                .build())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let mut harness = animist::agent::testing::ToolHarness::builder()
        .tools(
            workspace
                .registry(&animist::history::History::new())
                .unwrap(),
        )
        .layers(
            animist::agent::AgentLayer::builder()
                .layer(Layer::new(Upper))
                .build(),
        )
        .build()
        .unwrap();
    // Root harness calls bypass layers by design, so the read rides a batch
    // child: children run through the agent pipeline.
    let output = harness
        .call(
            crate::tools::Batch,
            serde_json::json!({"tool_calls": [{"tool": "read", "parameters": {"path": "f"}}]}),
        )
        .await
        .expect("batch outcome");
    let text = ext::tool_content_text(&output.content);
    assert!(
        text.contains("1: BODY"),
        "layer must transform the child result: {text}"
    );
    assert!(!text.contains("1: body"), "{text}");
}

/// The policy layer reaches batch children: the fan-out dispatches through
/// the same pipeline as top-level calls, so the rules decide each child.
/// (Named replacement for the dispatch-table hook test.)
#[tokio::test]
async fn policy_reaches_batch_children() {
    // The outer `batch` is allowed by rule so the test reaches the
    // children, whose `list` call is denied by another rule.
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir(),
    ));
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: crate::permissions::ToolKey::native("batch"),
        scope: None,
        effect: crate::permissions::Effect::Allow,
    });
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: crate::permissions::ToolKey::native("list"),
        scope: None,
        effect: crate::permissions::Effect::Deny,
    });
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "batch",
            serde_json::json!({"tool_calls": [{"tool": "list", "parameters": {"path": "."}}]}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        sink,
    )
    .policy(crate::harness::PolicyLayerInputs::new(
        permissions,
        Arc::new(crate::headless::HeadlessGate),
    ))
    .workspace(workspace)
    .build()
    .unwrap();
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert!(
        result_text(&results[0]).contains("Permission denied for `list`"),
        "the policy must reach batch children: {}",
        result_text(&results[0])
    );
}

#[tokio::test]
async fn dedup_replays_identical_read_calls() {
    // Two identical read calls in one run: the second answers from cache
    // without executing.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let (conversation, mut stream) = workspace_conversation(&dir, &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = raw_tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert_eq!(result_text(&results[0]), "1: body");
    assert_eq!(
        result_text(&results[1]),
        "1: body",
        "the second identical read replays the recorded result"
    );
    // Only the first call executes; both finish.
    let events = events_until_done(&mut stream).await;
    let started = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::ToolStarted { .. })))
        .count();
    let finished = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::ToolFinished { .. })))
        .count();
    assert_eq!(started, 1, "only the first call executes");
    assert_eq!(finished, 2, "both calls finish");
}

#[tokio::test]
async fn dedup_invalidated_by_write_to_same_path() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "write",
            serde_json::json!({"path": "f", "content": "new\n"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = raw_tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert!(!texts[1].contains("cached"), "writes are never cached");
    assert_eq!(
        texts[2], "1: new",
        "post-write read must re-execute against fresh contents"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_multiedit_top_level_path() {
    // The multiedit schema carries one top-level `path`; a stale extractor
    // that looked per-edit left the file's cached read in place.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "multiedit",
            serde_json::json!({
                "path": "f",
                "edits": [{"old_string": "old", "new_string": "new"}]
            }),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = raw_tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert_eq!(
        texts[2], "1: new",
        "post-multiedit read must re-execute, not replay the stale entry"
    );
}

#[tokio::test]
async fn dedup_invalidated_by_batch_child_write() {
    // A batch child dispatches through the same registry, so its write must
    // invalidate the read cache just like a top-level write.
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "batch",
            serde_json::json!({"tool_calls": [{"tool": "write", "parameters": {
                "path": "f", "content": "new\n"
            }}]}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = raw_tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert!(!texts[1].contains("[error]"), "{}", texts[1]);
    assert_eq!(
        texts[2], "1: new",
        "batch child write must invalidate the read cache"
    );
}

#[tokio::test]
async fn dedup_cleared_by_bash() {
    // bash can rewrite arbitrary files without naming them, so the whole
    // cache is cleared rather than a single path.
    // SAFETY: disable the sandbox like tools/bash.rs tests do — hosts that
    // already sandbox the test process deny nested sandbox-exec.
    unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "bash",
            serde_json::json!({"command": "printf 'new\\n' > f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t3",
            "read",
            serde_json::json!({"path":"f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "old\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let _ = run_turn(&conversation, "go").await;
    let texts: Vec<String> = raw_tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    assert_eq!(texts[0], "1: old");
    assert_eq!(
        texts[2], "1: new",
        "bash write must clear the read cache; bash said: {:?}",
        texts[1]
    );
}

// --- scheduling of one turn's calls ---

/// Two writes to the same path must not overlap: each records start/end
/// markers around a delay and the log must show one fully nested pair.
#[tokio::test]
async fn same_path_writes_serialize() {
    let log = Arc::new(StdMutex::new(Vec::new()));
    let writer = |name: &'static str| {
        Probe::simple(name)
            .claiming(ProbeClaim::Writes(&["same.txt"]))
            .with_delay(Duration::from_millis(20))
            .logging(&log)
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event(
                "t1",
                "write",
                serde_json::json!({"path": "same.txt", "content": "a"}),
            ),
            tool_event(
                "t2",
                "edit",
                serde_json::json!({"path": "same.txt", "old_string": "a", "new_string": "b"}),
            ),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([writer("write"), writer("edit")]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(*log.lock().unwrap(), vec!["start", "end", "start", "end"]);
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert!(!result_is_error(&results[0]));
    assert!(!result_is_error(&results[1]));
}

/// An exclusive tool is a barrier: everything spawned before it has
/// finished before it starts. The counter is incremented by the earlier
/// call at its end and read by the exclusive call at its start.
#[tokio::test]
async fn exclusive_tool_waits_for_earlier_calls() {
    let finished = Arc::new(AtomicUsize::new(0));
    let seen_before = Arc::new(AtomicUsize::new(usize::MAX));
    let earlier = {
        let finished = Arc::clone(&finished);
        Probe::independent("read")
            .with_delay(Duration::from_millis(20))
            .on_run(move || {
                finished.fetch_add(1, Ordering::SeqCst);
            })
    };
    let exclusive = {
        let finished = Arc::clone(&finished);
        let seen = Arc::clone(&seen_before);
        Probe::simple("question").on_run(move || {
            seen.store(finished.load(Ordering::SeqCst), Ordering::SeqCst);
        })
    };
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "read", serde_json::json!({"path": "f"})),
            tool_event("t2", "question", serde_json::json!({})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([earlier, exclusive]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        seen_before.load(Ordering::SeqCst),
        1,
        "the exclusive call must start only after the earlier call finished"
    );
}

/// Two independent read-only calls that each wait for the other to start:
/// sequential dispatch would deadlock both on the barrier, concurrent
/// dispatch passes. Bounded so the suite can never hang on it.
#[tokio::test]
async fn independent_calls_run_concurrently() {
    /// A probe that awaits a shared barrier in its body.
    struct BarrierProbe {
        name: &'static str,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl ToolImpl for BarrierProbe {
        type Args = ProbeArgs;
        type Output = ToolOutput;
        type Error = crate::tools::error::Error;

        fn definition(&self) -> ToolDefinition {
            crate::tools::error::definition::<ProbeArgs>(self.name, "barrier probe")
        }

        fn scheduling(&self, _input: &Value) -> ToolScheduling {
            ToolScheduling::Resources {
                reads: Vec::new(),
                writes: Vec::new(),
            }
        }

        async fn call(
            &self,
            _context: &ToolContext<'_>,
            _args: ProbeArgs,
        ) -> Result<ToolOutput, crate::tools::error::Error> {
            let _ = self.barrier.wait().await;
            Ok(crate::tools::error::text_output("ok"))
        }
    }

    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe_a", serde_json::json!({})),
            tool_event("t2", "probe_b", serde_json::json!({})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut registry = ToolRegistry::default();
    registry
        .register(BarrierProbe {
            name: "probe_a",
            barrier: Arc::clone(&barrier),
        })
        .unwrap();
    registry
        .register(BarrierProbe {
            name: "probe_b",
            barrier: Arc::clone(&barrier),
        })
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry,
        None,
    )
    .await;
    // Sequential dispatch would deadlock both probes on the barrier.
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        run_turn(&conversation, "go").await
    })
    .await
    .expect("calls must overlap; dispatch looks sequential");
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
}

/// Two reads of one file overlap: both must start before either finishes,
/// or the barrier they meet on never releases.
#[tokio::test]
async fn two_reads_of_one_file_overlap() {
    /// A read-claiming probe that awaits a shared barrier in its body.
    struct ReadProbe {
        name: &'static str,
        key: &'static str,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl ToolImpl for ReadProbe {
        type Args = ProbeArgs;
        type Output = ToolOutput;
        type Error = crate::tools::error::Error;

        fn definition(&self) -> ToolDefinition {
            crate::tools::error::definition::<ProbeArgs>(self.name, "read probe")
        }

        fn scheduling(&self, _input: &Value) -> ToolScheduling {
            ToolScheduling::Resources {
                reads: vec![self.key.to_owned()],
                writes: Vec::new(),
            }
        }

        async fn call(
            &self,
            _context: &ToolContext<'_>,
            _args: ProbeArgs,
        ) -> Result<ToolOutput, crate::tools::error::Error> {
            let _ = self.barrier.wait().await;
            Ok(crate::tools::error::text_output("ok"))
        }
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "read_one", serde_json::json!({})),
            tool_event("t2", "read_two", serde_json::json!({})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let mut registry = ToolRegistry::default();
    for name in ["read_one", "read_two"] {
        registry
            .register(ReadProbe {
                name,
                key: "f",
                barrier: Arc::clone(&barrier),
            })
            .unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry,
        None,
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        run_turn(&conversation, "go").await
    })
    .await
    .expect("two reads of one file must overlap");
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
}

/// A write conflicts with a read of the same path: the read observes the
/// write's effect deterministically (a conflicting call is a barrier).
#[tokio::test]
async fn write_then_read_of_the_same_path_serialize() {
    let log = Arc::new(StdMutex::new(Vec::new()));
    let writer = Probe::simple("write")
        .claiming(ProbeClaim::Writes(&["f"]))
        .with_delay(Duration::from_millis(20))
        .on_run({
            let log = Arc::clone(&log);
            move || log.lock().unwrap().push("wrote")
        });
    let reader = Probe::simple("read")
        .claiming(ProbeClaim::Reads("f"))
        .on_run({
            let log = Arc::clone(&log);
            move || log.lock().unwrap().push("read")
        });
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "write", serde_json::json!({})),
            tool_event("t2", "read", serde_json::json!({})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([writer, reader]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        *log.lock().unwrap(),
        vec!["wrote", "read"],
        "the read must start only after the conflicting write finished"
    );
}

/// A directory `list` conflicts with a write beneath it, through the real
/// tools' claims: the listing always observes the written file.
#[tokio::test]
async fn directory_list_conflicts_with_a_write_beneath_it() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event(
                "t1",
                "write",
                serde_json::json!({"path": "src/new.rs", "content": "fn f() {}\n"}),
            ),
            tool_event("t2", "list", serde_json::json!({"path": "src"})),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/old.rs"), "x\n").unwrap();
    let (conversation, _stream) = workspace_conversation(&dir, &provider).await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert!(!result_is_error(&results[0]));
    assert!(
        result_text(&results[1]).contains("new.rs"),
        "the listing must observe the conflicting write: {}",
        result_text(&results[1])
    );
}

/// Three identical calls in a row: the first two execute, the third is
/// blocked as a doom loop, and the cleared window lets an immediate
/// identical retry through.
#[tokio::test]
async fn identical_calls_blocked_as_doom_loop() {
    let runs = Arc::new(AtomicUsize::new(0));
    let probe = {
        let runs = Arc::clone(&runs);
        Probe::simple("probe").on_run(move || {
            runs.fetch_add(1, Ordering::SeqCst);
        })
    };
    let input = serde_json::json!({});
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe", input.clone()),
            tool_event("t2", "probe", input.clone()),
            tool_event("t3", "probe", input.clone()),
            // Window was cleared by the block, so this identical retry
            // executes instead of being re-blocked forever.
            tool_event("t4", "probe", input.clone()),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([probe]),
        None,
    )
    .await;
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 3, "t3 is blocked, not run");
    // The cleared window lets t4 through, whose success decays the score to
    // 14 — under a block worth 16 the grace prompt would fire here. This is
    // the executable pin for the block's exact score contribution.
    assert!(
        !matches!(outcome, TurnOutcome::MaxTurns | TurnOutcome::DoomStop),
        "the run settles without grace or a hard stop: {outcome:?}"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 4);
    let blocked: Vec<&ContentBlock> = results.iter().filter(|r| result_is_error(r)).collect();
    assert_eq!(blocked.len(), 1);
    assert!(
        result_text(blocked[0]).contains("stuck in a loop"),
        "got: {}",
        result_text(blocked[0])
    );
    assert!(results.iter().any(|r| !result_is_error(r)));
}

/// A panicking tool future becomes an error result for that call; the
/// sibling call still succeeds and the run continues.
#[tokio::test]
async fn panicking_tool_becomes_error_result() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "boom", serde_json::json!({})),
            tool_event("t2", "fine", serde_json::json!({})),
        ]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([
            Probe::simple("boom").panicking("kaboom"),
            Probe::simple("fine"),
        ]),
        None,
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "panic must not fail the run: {outcome:?}"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert!(result_is_error(&results[0]), "{}", result_text(&results[0]));
    let text = result_text(&results[0]);
    assert!(text.contains("panicked"), "got: {text}");
    assert!(text.contains("kaboom"), "got: {text}");
    assert!(!result_is_error(&results[1]));
    assert!(
        events_until_done(&mut stream)
            .await
            .iter()
            .any(|event| matches!(event, Event::Agent(AgentEvent::ToolFailed { .. }))),
        "the structured failure is reported"
    );
}

// --- 07: the policy and guardrail layers in the chain ---

/// An always-allowing responder that counts its asks: the policy layer
/// consults it only where the rules cannot decide.
#[derive(Clone, Default)]
struct AllowAll {
    asks: Arc<StdMutex<usize>>,
}

impl crate::harness::ApprovalResponder for AllowAll {
    fn ask(
        &self,
        _ask: crate::harness::ApprovalAsk<'_>,
    ) -> crate::harness::BoxFuture<crate::harness::Approval> {
        *self.asks.lock().unwrap() += 1;
        Box::pin(std::future::ready(crate::harness::Approval::Allow {
            remember: None,
        }))
    }
}

/// A conversation over the workspace with the full policy seam wired:
/// rules from `permissions`, an always-allowing responder that counts.
async fn policy_conversation(
    dir: &tempfile::TempDir,
    permissions: Arc<crate::permissions::PermissionManager>,
    provider: &ScriptedProvider,
) -> (Conversation, Arc<AllowAll>) {
    let responder = Arc::new(AllowAll::default());
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(provider),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        sink,
    )
    .policy(crate::harness::PolicyLayerInputs::new(
        permissions,
        responder.clone(),
    ))
    .workspace(workspace)
    .build()
    .unwrap();
    (conversation, responder)
}

/// The layer order from the builder: a rules denial short-circuits the
/// chain, so the doom window never even sees the repeated call — the third
/// identical call reports the permission denial, not the doom-loop message.
#[tokio::test]
async fn policy_denials_precede_the_doom_window() {
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir(),
    ));
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: crate::permissions::ToolKey::native("read"),
        scope: None,
        effect: crate::permissions::Effect::Deny,
    });
    let input = serde_json::json!({"path": "f"});
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "read", input.clone()),
            tool_event("t2", "read", input.clone()),
            tool_event("t3", "read", input.clone()),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, responder) = policy_conversation(&dir, permissions, &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    for text in &texts {
        assert!(
            text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
            "the rules denial must win over every later layer: {text}"
        );
    }
    assert_eq!(
        *responder.asks.lock().unwrap(),
        0,
        "a rules denial never reaches the responder"
    );
}

/// The plan gate runs after the responder: an interaction that allows the
/// call still cannot write outside the plan file in plan mode.
#[tokio::test]
async fn plan_mode_blocks_a_responder_allowed_write() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    // Rules anchor at the workspace dir, so the sibling tempdir's file is
    // out-of-project: the write prompts (and the responder allows) before
    // the plan gate denies it.
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        dir.path().to_path_buf(),
    ));
    let target = outside.path().join("src/main.rs");
    let root = dir.path().canonicalize().unwrap();
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "write",
            serde_json::json!({"path": target.display().to_string(), "content": "hi\n"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let provider = &provider;
    let (conversation, responder) = policy_conversation(&dir, permissions, provider).await;
    conversation.update_settings(|settings| {
        settings.mode = crate::harness::AgentMode::Plan(root.join("plans/alpha.md"));
    });

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert!(!target.exists(), "the denied write must not touch disk");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        result_text(&results[0]),
        format!("[error] {}", crate::harness::PLAN_WRITE_RESTRICTED),
        "the plan gate has the final word after the responder allows"
    );
    assert!(
        *responder.asks.lock().unwrap() >= 1,
        "the write is an off-project ask, so the responder allowed it first"
    );
}

/// A guardrail block lands after the policy pass: a call the responder
/// allowed is still denied once its failure counters trip the block.
#[tokio::test]
async fn guardrail_block_survives_an_allowed_call() {
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir(),
    ));
    // Six distinct failing inputs trip the same-tool block; the seventh
    // call is denied before execution.
    let mut turns = Vec::new();
    for i in 0..7 {
        turns.push(crate::testing::tool_turn(vec![tool_event(
            &format!("t{i}"),
            "probe",
            serde_json::json!({ "n": i }),
        )]));
    }
    turns.push(crate::testing::text_turn("done"));
    let provider = script(turns);
    let dir = tempfile::tempdir().unwrap();
    let failing = || Probe::simple("probe").panicking("nope");
    let responder = Arc::new(AllowAll::default());
    let (sink, mut stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        {
            let mut registry = ToolRegistry::default();
            registry.register(failing()).unwrap();
            registry
        },
        sink,
    )
    .policy(crate::harness::PolicyLayerInputs::new(
        permissions,
        responder.clone(),
    ))
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 7);
    assert!(
        result_is_error(&tool_results(&conversation).await[6]),
        "the seventh call is the guardrail block"
    );
    assert!(
        texts[6].contains("blocked by guardrails"),
        "got: {}",
        texts[6]
    );
    assert!(
        *responder.asks.lock().unwrap() >= 7,
        "every call was allowed by the responder first"
    );
    assert!(
        events_until_done(&mut stream)
            .await
            .iter()
            .any(|event| matches!(event,
                Event::Craft(crate::harness::CraftEvent::Info(text))
                    if text.starts_with("guardrail blocked probe:"))),
        "the block rides an Info event"
    );
}

/// A repeated identical failure carries the guardrail warning prefix on its
/// result and the matching `Info` event, byte-identical to the old
/// `commit_wave` texts.
#[tokio::test]
async fn guardrail_warning_prefixes_the_second_identical_failure() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "probe",
            serde_json::json!({ "n": 1 }),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "probe",
            serde_json::json!({ "n": 1 }),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([Probe::simple("probe").panicking("nope")]),
        None,
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 2);
    assert!(
        texts[1].contains("[guardrail] same tool+input failed 2 times"),
        "the warning rides the model-visible result: {}",
        texts[1]
    );
    assert!(
        events_until_done(&mut stream).await.iter().any(|event| matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Info(text))
                if text == "guardrail warning on probe: same tool+input failed 2 times, consider a different approach"
        )),
        "the Info event matches the old commit_wave wording"
    );
}

/// A dedup hit inside the chain is served through `Respond` after the
/// policy and guardrail passes and never reaches the executor.
#[tokio::test]
async fn dedup_hit_serves_after_the_policy_pass_without_executing() {
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir(),
    ));
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::tool_turn(vec![tool_event(
            "t2",
            "read",
            serde_json::json!({"path": "f"}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();
    let (conversation, responder) = policy_conversation(&dir, permissions, &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = raw_tool_results(&conversation).await;
    assert_eq!(result_text(&results[0]), "1: body");
    assert_eq!(result_text(&results[1]), "1: body");
    assert_eq!(
        *responder.asks.lock().unwrap(),
        0,
        "reads are rule-allowed; the hit only proves dedup sits in the chain"
    );
}

/// The doom hard stop: two doom blocks push the score past the hard-stop
/// threshold, so the next call stops the run — `DoomStop` with an end
/// marker, partial history committed, and the next turn replays cleanly.
#[tokio::test]
async fn doom_hard_stop_ends_the_run_with_an_end_marker() {
    let runs = Arc::new(AtomicUsize::new(0));
    let probe = {
        let runs = Arc::clone(&runs);
        Probe::simple("probe").on_run(move || {
            runs.fetch_add(1, Ordering::SeqCst);
        })
    };
    let input = serde_json::json!({});
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe", input.clone()),
            tool_event("t2", "probe", input.clone()),
            tool_event("t3", "probe", input.clone()),
            tool_event("t4", "probe", input.clone()),
            tool_event("t5", "probe", input.clone()),
        ]),
        crate::testing::tool_turn(vec![tool_event("t6", "probe", input.clone())]),
        // The replay turn after the stopped run.
        crate::testing::text_turn("after"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([probe]),
        None,
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::DoomStop), "{outcome:?}");
    let messages = stored(&conversation).await;
    let last = messages.last().expect("entries");
    assert!(
        crate::history::marker::is(last, crate::history::marker::END),
        "the doom stop ends with the end marker: {}",
        crate::history::ext::message_text(last)
    );
    assert!(
        messages
            .iter()
            .all(|message| !crate::history::ext::message_text(message).contains("Run failed")),
        "a doom stop is a controlled end, not a failure"
    );
    assert!(
        events_until_done(&mut stream)
            .await
            .iter()
            .any(|event| matches!(
                event,
                Event::Craft(crate::harness::CraftEvent::Done {
                    reason: crate::harness::DoneReason::DoomStop,
                    ..
                })
            )),
        "the terminal Done reports the doom stop"
    );

    // History replays: the next turn runs to completion.
    let outcome = run_turn(&conversation, "again").await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "after"),
        "{outcome:?}"
    );
}

/// The doom grace: one doom block reaches the grace threshold, so the
/// step's boundary appends the grace prompt as a durable observation —
/// once, tagged, carried into the next request, and surfaced as a
/// `StagnationDetected` plus a `MessagesAppended` event.
#[tokio::test]
async fn grace_prompt_fires_once_and_rides_the_next_request() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe", serde_json::json!({})),
            tool_event("t2", "probe", serde_json::json!({})),
            tool_event("t3", "probe", serde_json::json!({})),
        ]),
        crate::testing::text_turn("summarized"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (conversation, mut stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        registry([Probe::simple("probe")]),
        None,
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "summarized"),
        "{outcome:?}"
    );
    let events = events_until_done(&mut stream).await;
    let stagnation = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::Craft(crate::harness::CraftEvent::StagnationDetected { .. })
            )
        })
        .count();
    assert_eq!(stagnation, 1, "grace fires exactly once");
    let appended = events
        .iter()
        .filter(|event| matches!(event, Event::Agent(AgentEvent::MessagesAppended { .. })))
        .count();
    assert_eq!(
        appended, 1,
        "the grace append rides a MessagesAppended event"
    );
    // The grace prompt is durable history: a tagged observation after the
    // step's results, before the model's summary reply.
    let messages = stored(&conversation).await;
    let grace_index = messages
        .iter()
        .position(|message| crate::history::marker::is(message, crate::history::marker::GRACE))
        .expect("the grace observation is committed");
    assert_eq!(
        crate::history::ext::message_text(&messages[grace_index]),
        crate::harness::doom::GRACE_CALL_PROMPT
    );
    assert!(
        messages[grace_index].message().role == Role::User,
        "the model must see it"
    );
    let summary_index = messages
        .iter()
        .position(|message| crate::history::ext::message_text(message) == "summarized")
        .expect("the summary reply");
    assert!(grace_index < summary_index);
    let last_result = messages
        .iter()
        .rposition(|message| {
            crate::history::ext::tool_results(message.message())
                .next()
                .is_some()
        })
        .expect("a tool-result message");
    assert!(
        grace_index > last_result,
        "the grace lands after the step's committed results"
    );
    // Step 2's request carried the grace prompt.
    let requests = provider.requests();
    assert!(requests.len() >= 2, "the run continued after the grace");
    assert!(
        requests[1].messages.iter().any(|message| {
            message.role == Role::User
                && message.content.iter().any(|block| {
                    matches!(block,
                    ContentBlock::Text { text } if text == crate::harness::doom::GRACE_CALL_PROMPT)
                })
        }),
        "the next request carries the grace prompt"
    );
}

/// One conversation from the real builder, three turns, one ordering claim:
/// the chain the builder assembles runs the policy layer before the
/// guardrail layer before the dedup cache — a denial from an earlier link
/// decides the call, and only calls every earlier link allowed reach the
/// cache (and the executor).
#[tokio::test]
async fn the_assembled_chain_orders_policy_guardrail_and_dedup() {
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir(),
    ));
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: crate::permissions::ToolKey::native("read"),
        scope: Some("*".to_owned()),
        effect: crate::permissions::Effect::Deny,
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f"), "body\n").unwrap();

    // Turn 1: two identical `list` calls — the second is served from the
    // dedup cache, at the chain's tail.
    let dedup_script = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("d1", "list", serde_json::json!({"path": "."})),
            tool_event("d2", "list", serde_json::json!({"path": "."})),
        ]),
        crate::testing::text_turn("dedup done"),
    ]);
    let (dedup_conversation, _stream) =
        policy_conversation(&dir, Arc::clone(&permissions), &dedup_script).await;
    let outcome = run_turn(&dedup_conversation, "dedup").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let dedup_results = tool_results(&dedup_conversation).await;
    assert_eq!(dedup_results.len(), 2, "both calls close");
    assert_eq!(
        result_text(&dedup_results[0]),
        result_text(&dedup_results[1])
    );

    // Turn 2 (same conversation): `read` is rule-denied, so three identical
    // calls report the permission denial — the doom window behind the
    // policy layer never sees them.
    let provider = crate::testing::ScriptedProvider::new(Vec::new());
    for turns in [
        vec![
            crate::testing::tool_turn(vec![
                tool_event("t1", "read", serde_json::json!({"path": "f"})),
                tool_event("t2", "read", serde_json::json!({"path": "f"})),
                tool_event("t3", "read", serde_json::json!({"path": "f"})),
            ]),
            crate::testing::text_turn("policy done"),
        ],
        vec![
            crate::testing::text_turn("never reached"),
            crate::testing::text_turn("never reached"),
        ],
    ] {
        for turn in turns {
            provider.push(turn);
        }
    }
    let (conversation, _stream) = policy_conversation(&dir, permissions, &provider).await;
    let outcome = run_turn(&conversation, "policy").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let texts: Vec<String> = tool_results(&conversation)
        .await
        .iter()
        .map(result_text)
        .collect();
    assert_eq!(texts.len(), 3);
    for text in &texts {
        assert!(
            text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
            "the rules denial must win over the doom window behind it: {text}"
        );
    }
}

/// A permission denial is a closed result like any other, so it decays the
/// doom score by one.
/// Discriminating pin: two rule-denied calls pull 2 errors + one doom block
/// down to exactly the grace threshold — 15, not 17 — and the grace
/// event carries the difference in its similarity.
#[tokio::test]
async fn permission_denials_decay_the_doom_score() {
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        std::env::temp_dir(),
    ));
    permissions.add_session_rule(crate::permissions::PermissionRule {
        tool: crate::permissions::ToolKey::native("other"),
        scope: None,
        effect: crate::permissions::Effect::Deny,
    });
    let input = serde_json::json!({"path": "missing"});
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event("t1", "probe", input.clone()),
            tool_event("t2", "probe", input.clone()),
            tool_event("t3", "probe", input.clone()),
            tool_event("t4", "other", serde_json::json!({})),
            tool_event("t5", "other", serde_json::json!({})),
        ]),
        crate::testing::text_turn("ok"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let (sink, mut stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        {
            let mut registry = ToolRegistry::default();
            registry
                .register(Probe::simple("probe").panicking("nope"))
                .unwrap();
            registry.register(Probe::simple("other")).unwrap();
            registry
        },
        sink,
    )
    .policy(crate::harness::PolicyLayerInputs::new(
        permissions,
        // The responder allows the probes; only  is rule-denied.
        Arc::new(AllowAll::default()),
    ))
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let events = events_until_done(&mut stream).await;
    assert!(
        events.iter().any(|event| matches!(event,
            Event::Craft(crate::harness::CraftEvent::StagnationDetected { similarity })
                if (similarity - 0.6).abs() < f32::EPSILON)),
        "grace fires at exactly 15 (0.6); without the denial decay it would be 17 (0.68)"
    );
}

/// Two `task` calls in one reply overlap: both subagents park on a shared
/// barrier, so serialized waves would exhaust the bounded wait and report
/// `serialized` instead of meeting. Pins the task tool's wave-compatible
/// scheduling claim; file safety across agents is PathLocks', not the
/// parent wave's.
#[tokio::test]
async fn subagent_calls_run_concurrently() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![
            tool_event(
                "t1",
                "task",
                serde_json::json!({"description": "first", "prompt": "a"}),
            ),
            tool_event(
                "t2",
                "task",
                serde_json::json!({"description": "second", "prompt": "b"}),
            ),
        ]),
        crate::testing::text_turn("done"),
    ]);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path())
        .unwrap()
        .with_subagents(Arc::new(crate::testing::BarrierSpawn(Arc::clone(&barrier))));
    let (conversation, _stream) = conversation_over(
        &provider,
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        workspace
            .registry(&animist::history::History::new())
            .unwrap(),
        None,
    )
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(20), async {
        run_turn(&conversation, "go").await
    })
    .await
    .expect("the turn must finish");
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    let texts: Vec<String> = results.iter().map(result_text).collect();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts.iter().all(|text| text == "overlapped"),
        "both subagents must be in flight at once: {texts:?}"
    );
}
