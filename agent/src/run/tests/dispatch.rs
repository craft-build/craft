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
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::*;
use crate::history::{ContentBlock, Role, ext};
use crate::run::{BeforeExecute, BoxFuture, Decision};

/// A native probe tool: sleeps `delay`, appends markers to the shared log,
/// optionally panics, and answers "ok". Claims come from `claim`.
#[derive(Clone)]
struct Probe {
    name: &'static str,
    delay: Duration,
    panic: Option<&'static str>,
    /// `start`/`end` markers appended to the shared log, when set.
    log: Option<Arc<StdMutex<Vec<&'static str>>>>,
    /// Runs at call start (after the delay, before `end`).
    on_run: Option<Arc<dyn Fn() + Send + Sync>>,
    claim: ProbeClaim,
}

#[derive(Clone, Copy)]
enum ProbeClaim {
    Exclusive,
    Independent,
    /// Claim reads of one opaque key.
    Reads(&'static str),
    /// Claim writes of explicit keys.
    Writes(&'static [&'static str]),
}

impl Probe {
    fn simple(name: &'static str) -> Self {
        Self {
            name,
            delay: Duration::ZERO,
            panic: None,
            log: None,
            on_run: None,
            claim: ProbeClaim::Exclusive,
        }
    }

    fn independent(name: &'static str) -> Self {
        Self {
            claim: ProbeClaim::Independent,
            ..Self::simple(name)
        }
    }

    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn panicking(mut self, message: &'static str) -> Self {
        self.panic = Some(message);
        self
    }

    fn logging(mut self, log: &Arc<StdMutex<Vec<&'static str>>>) -> Self {
        self.log = Some(Arc::clone(log));
        self
    }

    fn on_run(mut self, on_run: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_run = Some(Arc::new(on_run));
        self
    }

    fn claiming(mut self, claim: ProbeClaim) -> Self {
        self.claim = claim;
        self
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct ProbeArgs {
    #[allow(dead_code)]
    _unused: Option<Value>,
}

#[async_trait]
impl ToolImpl for Probe {
    type Args = ProbeArgs;
    type Output = ToolOutput;
    type Error = crate::tools::error::Error;

    fn definition(&self) -> ToolDefinition {
        crate::tools::error::definition::<ProbeArgs>(self.name, "test probe")
    }

    fn scheduling(&self, _input: &Value) -> ToolScheduling {
        match self.claim {
            ProbeClaim::Exclusive => ToolScheduling::Exclusive,
            ProbeClaim::Independent => ToolScheduling::Resources {
                reads: Vec::new(),
                writes: Vec::new(),
            },
            ProbeClaim::Reads(key) => ToolScheduling::Resources {
                reads: vec![key.to_owned()],
                writes: Vec::new(),
            },
            ProbeClaim::Writes(keys) => ToolScheduling::Resources {
                reads: Vec::new(),
                writes: keys.iter().map(|key| (*key).to_owned()).collect(),
            },
        }
    }

    async fn call(
        &self,
        _context: &ToolContext<'_>,
        _args: ProbeArgs,
    ) -> Result<ToolOutput, crate::tools::error::Error> {
        if let Some(log) = &self.log {
            log.lock().unwrap().push("start");
        }
        tokio::time::sleep(self.delay).await;
        if let Some(on_run) = &self.on_run {
            on_run();
        }
        if let Some(message) = self.panic {
            panic!("{message}");
        }
        if let Some(log) = &self.log {
            log.lock().unwrap().push("end");
        }
        Ok(crate::tools::error::text_output("ok"))
    }
}

fn registry(tools: impl IntoIterator<Item = Probe>) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    for tool in tools {
        registry.register(tool).expect("probe registers");
    }
    registry
}

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
async fn skip_decision_reports_reason_to_model() {
    struct Deny;
    impl BeforeExecute for Deny {
        fn decide(&self, _call: ContentBlock) -> BoxFuture<Decision> {
            Box::pin(async { Decision::Skip("not allowed".into()) })
        }
    }
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "read",
            serde_json::json!({"path":"f"}),
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
        workspace.registry().unwrap(),
        sink,
    )
    .approval(Arc::new(Deny))
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

/// The post-execution transform seam (the old `AfterExecute` hook) is the
/// layer chain's `post_tool_exec`: an output-transforming layer sees every
/// executed call's result.
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
        .tools(workspace.registry().unwrap())
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

/// An approval gate reaches batch children: the fan-out dispatches through
/// the same pipeline as top-level calls, so the gate's skip answers each
/// child. (Named replacement for the dispatch-table hook test.)
#[tokio::test]
async fn approval_gate_reaches_batch_children() {
    struct DenyReads;
    impl BeforeExecute for DenyReads {
        fn decide(&self, call: ContentBlock) -> BoxFuture<Decision> {
            // Allow the outer batch so the test reaches the children.
            let decision = match &call {
                ContentBlock::ToolUse { name, .. } if name == "batch" => Decision::Run,
                ContentBlock::ToolUse { name, .. } => Decision::Skip(format!("denied: {name}")),
                _ => Decision::Run,
            };
            Box::pin(std::future::ready(decision))
        }
    }
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
        workspace.registry().unwrap(),
        sink,
    )
    .approval(Arc::new(DenyReads))
    .workspace(workspace)
    .build()
    .unwrap();
    let outcome = run_turn(&conversation, "go").await;
    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert!(
        result_text(&results[0]).contains("denied: list"),
        "the gate must reach batch children: {}",
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
    let results = tool_results(&conversation).await;
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
    let texts: Vec<String> = tool_results(&conversation)
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
    let texts: Vec<String> = tool_results(&conversation)
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
    let texts: Vec<String> = tool_results(&conversation)
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
    let texts: Vec<String> = tool_results(&conversation)
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
#[ignore = "restored in 07"]
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
