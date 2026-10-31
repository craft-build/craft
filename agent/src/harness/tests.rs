//! Phase-A harness tests: the conversation driven end to end over the
//! scripted provider, pinning the behaviours the animist cutover changes.

use std::sync::Arc;

use animist::agent::{AgentEvent, Cancellation};
use animist::history::{ContentBlock, History, HistoryEntry, Role};
use animist::provider::ProviderEvent;

use crate::config::Config;
use crate::harness::{Conversation, ConversationBuilder, Event, EventSink, TurnOutcome};
use crate::provider::ModelHandle;
use crate::run::recency::{RecencyCtx, RecencyFacts, RecencySource};
use crate::testing::{self, ScriptedProvider};
use crate::tools::Workspace;
use animist::agent::tool::ToolRegistry;

struct FixedRecency;
impl RecencySource for FixedRecency {
    fn collect(&self, ctx: &RecencyCtx) -> RecencyFacts {
        let mut facts = RecencyFacts::new();
        facts.push(format!("turn {}", ctx.turn));
        facts
    }
}

/// A conversation over the workspace's real dispatch table.
async fn workspace_conversation(
    dir: &tempfile::TempDir,
    scripted: &ScriptedProvider,
) -> (Conversation, crate::harness::EventStream) {
    let workspace = Workspace::new(dir.path()).expect("workspace");
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("").expect("default config"),
        &dir.path().display().to_string(),
        "",
        handle,
        workspace.registry().expect("registry"),
        sink,
    )
    .workspace(workspace)
    .build()
    .expect("conversation builds");
    (conversation, stream)
}

async fn entries(history: &History) -> Vec<HistoryEntry> {
    history.snapshot().await.entries().to_vec()
}

/// The last committed entry, tagged with the marker it must carry.
async fn last_marker_entry(history: &History) -> animist::history::StoredMessage {
    let all = entries(history).await;
    all.last()
        .expect("at least one entry")
        .current()
        .expect("active")
        .clone()
}

async fn active_texts(conversation: &Conversation) -> Vec<String> {
    entries(&conversation.history())
        .await
        .iter()
        .filter_map(HistoryEntry::current)
        .map(|stored| {
            stored
                .message()
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        })
        .collect()
}

async fn tool_results(conversation: &Conversation) -> Vec<ContentBlock> {
    entries(&conversation.history())
        .await
        .iter()
        .filter_map(HistoryEntry::current)
        .flat_map(|stored| stored.message().content.iter())
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .cloned()
        .collect()
}

fn result_text(block: &ContentBlock) -> String {
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = block
    else {
        panic!("tool result block, got {block:?}");
    };
    let text = crate::history::ext::tool_content_text(content);
    if *is_error {
        format!("[error] {text}")
    } else {
        text
    }
}

/// Wait for the next event, failing (not hanging) when none arrives: the
/// conversation's stream stays open until the whole conversation drops, so
/// a missed event would otherwise wait forever.
async fn next_event(stream: &mut crate::harness::EventStream) -> Event {
    tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
        .await
        .expect("event before the deadline")
        .expect("stream closed unexpectedly")
}

#[tokio::test]
async fn tool_round_trip_completes_and_commits_history() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "contents").unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![(
            "t1".into(),
            "read".into(),
            serde_json::json!({ "path": "f.txt" }),
        )]))
        .push(testing::text_turn("done"));
    let (conversation, mut stream) = workspace_conversation(&dir, &scripted).await;

    let outcome =
        crate::harness::run_turn(&conversation, "read it", &[], Cancellation::default()).await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "done"),
        "{outcome:?}"
    );
    let texts = active_texts(&conversation).await;
    // The tool-use assistant message and the result observation carry no
    // plain text; the empty slots pin the entry sequence.
    assert_eq!(texts, vec!["read it", "", "", "done"]);
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(result_text(&results[0]), "1: contents");
    // The result entry squashed to its placeholder source plus one final
    // revision.
    let result_entry = entries(&conversation.history())
        .await
        .into_iter()
        .find(|entry| {
            entry.current().is_some_and(|stored| {
                stored.message().role == Role::User
                    && stored.message().content.iter().any(|block| {
                        matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1")
                    })
            })
        })
        .expect("result entry");
    assert_eq!(result_entry.revisions().len(), 1);
    // Events: the call started and finished, and the run's Done is last.
    let mut saw_started = false;
    let mut saw_finished = false;
    while let Some(event) = stream.next().await {
        match event {
            Event::Agent(AgentEvent::ToolStarted { call, .. }) if call.id == "t1" => {
                saw_started = true;
            }
            Event::Agent(AgentEvent::ToolFinished { call, .. }) if call.id == "t1" => {
                saw_finished = true;
            }
            Event::Craft(crate::harness::CraftEvent::Done {
                reason: crate::harness::DoneReason::Stop,
                ..
            }) => break,
            _ => {}
        }
    }
    assert!(saw_started, "the executed call must start");
    assert!(saw_finished, "the executed call must finish");
}

/// A native test tool: `boom` panics, `stall` waits for release. The old
/// rig DynamicTool fakes for panic and cancellation coverage.
#[derive(Clone)]
struct HarnessProbe {
    name: &'static str,
    release: Option<tokio::sync::watch::Receiver<bool>>,
}

#[derive(Debug, Default, schemars::JsonSchema, serde::Deserialize)]
struct HarnessProbeArgs {}

#[async_trait::async_trait]
impl animist::agent::tool::ToolImpl for HarnessProbe {
    type Args = HarnessProbeArgs;
    type Output = animist::agent::tool::ToolOutput;
    type Error = crate::tools::error::Error;

    fn definition(&self) -> animist::agent::tool::ToolDefinition {
        crate::tools::error::definition::<HarnessProbeArgs>(self.name, "harness probe")
    }

    async fn call(
        &self,
        _context: &animist::agent::tool::ToolContext<'_>,
        _args: HarnessProbeArgs,
    ) -> Result<animist::agent::tool::ToolOutput, crate::tools::error::Error> {
        if let Some(mut release) = self.release.clone() {
            let _ = release.changed().await;
            return Ok(crate::tools::error::text_output("late"));
        }
        panic!("boom");
    }
}

fn probe_registry(probe: HarnessProbe) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(probe).expect("probe registers");
    registry
}

#[tokio::test]
async fn panicking_tool_yields_panicked_error_result_and_the_run_continues() {
    let dir = tempfile::tempdir().unwrap();
    let boom = HarnessProbe {
        name: "boom",
        release: None,
    };
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![(
            "t1".into(),
            "boom".into(),
            serde_json::json!({}),
        )]))
        .push(testing::text_turn("recovered"));
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        &scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        probe_registry(boom),
        sink,
    )
    .build()
    .unwrap();
    let mut stream = stream;

    let outcome = crate::harness::run_turn(&conversation, "go", &[], Cancellation::default()).await;

    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "recovered"),
        "{outcome:?}"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        result_text(&results[0]),
        "[error] Tool execution panicked: boom",
        "the model-visible panic text is animist's"
    );
    let mut saw_failed = false;
    loop {
        match next_event(&mut stream).await {
            Event::Agent(AgentEvent::ToolFailed { .. }) => saw_failed = true,
            Event::Craft(crate::harness::CraftEvent::Done { .. }) => break,
            _ => {}
        }
    }
    assert!(saw_failed, "the structured failure is reported");
}

#[tokio::test]
async fn cancel_mid_stream_commits_partial_text_marker_and_replays_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted.push(testing::text_turn("partial").blocked());
    let (conversation, mut stream) = workspace_conversation(&dir, &scripted).await;

    let cancel = Cancellation::default();
    let turn = {
        let cancel = cancel.clone();
        let conversation = conversation.clone();
        tokio::spawn(
            async move { crate::harness::run_turn(&conversation, "go", &[], cancel).await },
        )
    };
    loop {
        match next_event(&mut stream).await {
            Event::Agent(AgentEvent::Provider {
                event: ProviderEvent::TextDelta { .. },
                ..
            }) => break,
            _ => continue,
        }
    }
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the cancelled turn finishes")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "{outcome:?}");

    // History carries the prompt, the interrupted attempt's validated text
    // prefix, and the cancel marker.
    let texts = active_texts(&conversation).await;
    assert_eq!(texts, vec!["go", "partial", "[Cancelled by user]"]);
    let marker_tagged = last_marker_entry(&conversation.history()).await;
    assert!(crate::history::marker::is(
        &marker_tagged,
        crate::history::marker::CANCEL
    ));

    // The same conversation replays cleanly on the next turn.
    scripted.release();
    scripted.push(testing::text_turn("after"));
    let outcome =
        crate::harness::run_turn(&conversation, "again", &[], Cancellation::default()).await;
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "after"),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn cancel_mid_tool_leaves_the_interrupted_result_committed() {
    let dir = tempfile::tempdir().unwrap();
    let (release, _receiver) = tokio::sync::watch::channel(false);
    let blocker = HarnessProbe {
        name: "stall",
        release: Some(release.subscribe()),
    };
    let scripted = ScriptedProvider::new(Vec::new());
    scripted.push(testing::tool_turn(vec![(
        "t1".into(),
        "stall".into(),
        serde_json::json!({}),
    )]));
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        &scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        probe_registry(blocker),
        sink,
    )
    .build()
    .unwrap();
    let mut stream = stream;
    let cancel = Cancellation::default();
    let turn = {
        let cancel = cancel.clone();
        let conversation = conversation.clone();
        tokio::spawn(
            async move { crate::harness::run_turn(&conversation, "go", &[], cancel).await },
        )
    };
    loop {
        match next_event(&mut stream).await {
            Event::Agent(AgentEvent::ToolStarted { call, .. }) if call.id == "t1" => break,
            _ => continue,
        }
    }
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), turn)
        .await
        .expect("the cancelled turn finishes")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "{outcome:?}");

    // The call's result entry is committed with an explicit error result
    // for the interrupted call, so replay stays valid.
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1, "one result for the interrupted call");
    assert!(
        result_text(&results[0]).starts_with("[error]"),
        "{}",
        result_text(&results[0])
    );
    let marker_tagged = last_marker_entry(&conversation.history()).await;
    assert!(crate::history::marker::is(
        &marker_tagged,
        crate::history::marker::CANCEL
    ));
}

#[tokio::test]
async fn dedup_hit_is_served_without_executing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "contents").unwrap();
    let call = |id: &str| {
        (
            id.to_string(),
            "read".to_string(),
            serde_json::json!({ "path": "f.txt" }),
        )
    };
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![call("t1")]))
        .push(testing::tool_turn(vec![call("t2")]))
        .push(testing::text_turn("done"));
    let (conversation, mut stream) = workspace_conversation(&dir, &scripted).await;

    let outcome =
        crate::harness::run_turn(&conversation, "read twice", &[], Cancellation::default()).await;

    assert!(matches!(&outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 2);
    assert_eq!(result_text(&results[0]), "1: contents");
    assert_eq!(
        result_text(&results[1]),
        "1: contents",
        "the cache hit replays the recorded result"
    );
    // The executed call started; the cache-served call finished without
    // ever starting — the surface adapter derives its start/done pair from
    // this shape.
    let mut started = Vec::new();
    let mut finished = Vec::new();
    while let Some(event) = stream.next().await {
        match event {
            Event::Agent(AgentEvent::ToolStarted { call, .. }) => started.push(call.id),
            Event::Agent(AgentEvent::ToolFinished { call, .. }) => finished.push(call.id),
            Event::Craft(crate::harness::CraftEvent::Done { .. }) => break,
            _ => {}
        }
    }
    assert_eq!(
        started.len(),
        1,
        "only the first call executes: {started:?}"
    );
    assert_eq!(finished.len(), 2, "both calls finish: {finished:?}");
}

#[tokio::test]
async fn snapshot_layer_captures_nothing_for_a_denied_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let plan = root.join("plans/alpha.md");
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![(
            "t1".into(),
            "write".into(),
            serde_json::json!({ "path": "src/main.rs", "content": "hi\n" }),
        )]))
        .push(testing::text_turn("done"));
    let (conversation, _stream) = workspace_conversation(&dir, &scripted).await;
    conversation.update_settings(|settings| {
        settings.mode = crate::run::mode::AgentMode::Plan(plan.clone());
    });

    let outcome =
        crate::harness::run_turn(&conversation, "write it", &[], Cancellation::default()).await;

    assert!(matches!(&outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert!(
        !root.join("src/main.rs").exists(),
        "the denied write must not touch disk"
    );
    assert_eq!(
        conversation.snapshots().undo_depth(),
        0,
        "a denied call captures no undo session"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert_eq!(
        result_text(&results[0]),
        format!("[error] {}", crate::run::mode::PLAN_WRITE_RESTRICTED),
        "the denial is the plan-gate message"
    );

    // The same call in build mode runs and, overwriting an existing file,
    // captures an undo session. (Creating a new file captures nothing: undo
    // restores prior content, and there is none.)
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "original\n").unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![(
            "t2".into(),
            "write".into(),
            serde_json::json!({ "path": "src/main.rs", "content": "hi\n" }),
        )]))
        .push(testing::text_turn("done"));
    let (conversation, _stream) = workspace_conversation(&dir, &scripted).await;
    let outcome =
        crate::harness::run_turn(&conversation, "write it", &[], Cancellation::default()).await;
    assert!(matches!(&outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        conversation.snapshots().undo_depth(),
        1,
        "an executed write captures its undo session"
    );
}

#[tokio::test]
async fn model_switch_between_turns_keeps_history_and_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let first = ScriptedProvider::new(Vec::new());
    first.push(testing::text_turn("one"));
    let second = ScriptedProvider::new(Vec::new());
    second.push(testing::text_turn("two"));
    let (conversation, mut stream) = workspace_conversation(&dir, &first).await;

    let outcome =
        crate::harness::run_turn(&conversation, "first", &[], Cancellation::default()).await;
    assert!(matches!(&outcome, TurnOutcome::Done { reply } if reply == "one"));

    let replacement = ModelHandle::scripted(testing::scripted_model(
        &second,
        testing::model_info("other", 128_000, 4_096, false, false),
    ));
    conversation.set_model(replacement).unwrap();
    let outcome =
        crate::harness::run_turn(&conversation, "second", &[], Cancellation::default()).await;
    assert!(matches!(&outcome, TurnOutcome::Done { reply } if reply == "two"));

    let texts = active_texts(&conversation).await;
    assert_eq!(texts, vec!["first", "one", "second", "two"]);
    // The same subscription saw both runs.
    let mut dones = 0;
    while let Some(event) = stream.next().await {
        if matches!(
            event,
            Event::Craft(crate::harness::CraftEvent::Done {
                reason: crate::harness::DoneReason::Stop,
                ..
            })
        ) {
            dones += 1;
            if dones == 2 {
                break;
            }
        }
    }
    assert_eq!(dones, 2, "both runs reported on one subscription");
}

#[tokio::test]
async fn recency_tail_rides_the_request_view_but_not_stored_history() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "contents").unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![(
            "t1".into(),
            "read".into(),
            serde_json::json!({ "path": "f.txt" }),
        )]))
        .push(testing::text_turn("done"));
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, _stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        &scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        workspace.registry().unwrap(),
        sink,
    )
    .recency(Arc::new(FixedRecency))
    .build()
    .unwrap();

    let outcome =
        crate::harness::run_turn(&conversation, "read it", &[], Cancellation::default()).await;
    assert!(matches!(&outcome, TurnOutcome::Done { .. }), "{outcome:?}");

    let requests = scripted.requests();
    assert!(requests.len() >= 2);
    for (index, request) in requests.iter().enumerate() {
        let tail = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .expect("a user message")
            .content
            .iter()
            .any(|block| {
                matches!(block, ContentBlock::Text { text } if text.starts_with("<turn-context>"))
            });
        assert!(tail, "request {index} carries the recency tail");
    }
    let mut stored_carries_tail = false;
    for entry in entries(&conversation.history()).await {
        if let Some(stored) = entry.current() {
            for block in &stored.message().content {
                if matches!(block, ContentBlock::Text { text } if text.starts_with("<turn-context>"))
                {
                    stored_carries_tail = true;
                }
            }
        }
    }
    assert!(
        !stored_carries_tail,
        "stored history never carries the tail"
    );
}

#[tokio::test]
async fn shutdown_is_idempotent_and_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted.push(testing::text_turn("done"));
    let (conversation, _stream) = workspace_conversation(&dir, &scripted).await;

    conversation.shutdown().await.unwrap();
    conversation.shutdown().await.unwrap();

    let outcome = crate::harness::run_turn(&conversation, "go", &[], Cancellation::default()).await;
    match outcome {
        TurnOutcome::Failed(message) => assert!(message.contains("shut down"), "{message}"),
        other => panic!("a shut-down conversation must fail its turns: {other:?}"),
    }
}

#[tokio::test]
async fn max_turns_budget_ends_with_the_end_marker() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "x").unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    for id in ["t1", "t2"] {
        scripted.push(testing::tool_turn(vec![(
            id.into(),
            "read".into(),
            serde_json::json!({ "path": "f.txt" }),
        )]));
    }
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, _stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        &scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let conversation = ConversationBuilder::new(
        &Config::parse("agent { max_turns = 2 }").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        workspace.registry().unwrap(),
        sink,
    )
    .build()
    .unwrap();

    let outcome = crate::harness::run_turn(&conversation, "go", &[], Cancellation::default()).await;

    assert!(matches!(outcome, TurnOutcome::MaxTurns), "{outcome:?}");
    let marker_tagged = last_marker_entry(&conversation.history()).await;
    assert!(crate::history::marker::is(
        &marker_tagged,
        crate::history::marker::END
    ));
}

// --- Phase B: the change-feed persister ---

fn state_dir(tmp: &tempfile::TempDir) -> crate::storage::StateDir {
    crate::storage::StateDir::from_path(tmp.path().to_path_buf())
}

fn session_ref() -> crate::id::SessionRef {
    crate::id::SessionRef::from_id("01965087-4c71-7f00-8000-000000000001".parse().unwrap())
}

/// A persisted conversation over the workspace's real tools.
async fn persisted_conversation(
    tmp: &tempfile::TempDir,
    scripted: &ScriptedProvider,
) -> Conversation {
    let persister = crate::harness::Persister::open_in(
        state_dir(tmp),
        session_ref(),
        "/project",
        "scripted/main",
    )
    .await
    .expect("persister opens");
    let workspace = Workspace::new(tmp.path()).expect("workspace");
    let (sink, _stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let history = persister.history();
    ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &tmp.path().display().to_string(),
        "",
        handle,
        workspace.registry().unwrap(),
        sink,
    )
    .history(history)
    .persist(Some(Arc::new(tokio::sync::Mutex::new(persister))))
    .build()
    .expect("conversation builds")
}

fn load_session(tmp: &tempfile::TempDir) -> crate::storage::sessions::StoredSession {
    crate::storage::sessions::StoredSession::load(session_ref().id(), &state_dir(tmp))
        .expect("session loads back")
}

fn jsonl_line_count(tmp: &tempfile::TempDir) -> usize {
    let path = state_dir(tmp)
        .ensure_subdir(crate::storage::sessions::SESSIONS_DIR)
        .expect("sessions dir");
    let name = format!("{}.jsonl", session_ref().id());
    let data = std::fs::read(path.join(name)).expect("session file");
    data.split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .count()
}

#[tokio::test]
async fn persisted_turns_replay_into_an_equal_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "contents").unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted
        .push(testing::tool_turn(vec![(
            "t1".into(),
            "read".into(),
            serde_json::json!({ "path": "f.txt" }),
        )]))
        .push(testing::text_turn("done"));
    let conversation = persisted_conversation(&dir, &scripted).await;

    let outcome =
        crate::harness::run_turn(&conversation, "read it", &[], Cancellation::default()).await;
    assert!(matches!(&outcome, TurnOutcome::Done { .. }), "{outcome:?}");

    // Mid-run checkpoints (step_end) recorded the placeholder revisions and
    // the squash; replaying the written records must equal the live
    // snapshot, revisions and all.
    let loaded = load_session(&dir);
    let live = entries(&conversation.history()).await;
    assert_eq!(
        loaded.messages().to_vec(),
        live,
        "replaying the written records reproduces the snapshot"
    );
}

#[tokio::test]
async fn one_message_turn_on_a_long_session_appends_a_small_delta() {
    let dir = tempfile::tempdir().unwrap();
    // Seed a long session and persist it, then reopen: the reopen's full
    // write is the baseline the delta is measured against.
    {
        let persister = crate::harness::Persister::open_in(
            state_dir(&dir),
            session_ref(),
            "/project",
            "scripted/main",
        )
        .await
        .unwrap();
        let history = persister.history();
        for i in 0..200 {
            history
                .add_stored(crate::history::ext::stored_user_text(format!("seed {i}")))
                .await;
        }
        let mut persister = persister;
        persister.checkpoint().await.unwrap();
    }
    let scripted = ScriptedProvider::new(Vec::new());
    scripted.push(testing::text_turn("ok"));
    let conversation = persisted_conversation(&dir, &scripted).await;
    let before = jsonl_line_count(&dir);

    let outcome = crate::harness::run_turn(&conversation, "go", &[], Cancellation::default()).await;
    assert!(matches!(&outcome, TurnOutcome::Done { .. }), "{outcome:?}");

    let after = jsonl_line_count(&dir);
    assert!(
        after - before <= 6,
        "one message appended {before} -> {after} lines; a full rewrite would add ~203"
    );
    let loaded = load_session(&dir);
    assert_eq!(loaded.messages().len(), 202, "prompt and reply persisted");
}

#[tokio::test]
async fn a_stale_cursor_falls_back_to_a_full_rewrite_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let persister = crate::harness::Persister::open_in(
        state_dir(&dir),
        session_ref(),
        "/project",
        "scripted/main",
    )
    .await
    .unwrap();
    // Outgrow the feed's retention (1024 events) without checkpointing.
    let history = persister.history();
    for i in 0..1030 {
        history
            .add_stored(crate::history::ext::stored_user_text(format!("flood {i}")))
            .await;
    }
    let mut persister = persister;
    persister.checkpoint().await.expect("rewrite recovers");
    let loaded = load_session(&dir);
    assert_eq!(
        loaded.messages().len(),
        1030,
        "the full rewrite carries every entry"
    );
    let after_rewrite = jsonl_line_count(&dir);

    // The feed resumes from the rewrite's cursor: the next change is a
    // small delta, not another rewrite.
    history
        .add_stored(crate::history::ext::stored_user_text("tail"))
        .await;
    persister.checkpoint().await.expect("delta appends");
    assert_eq!(
        jsonl_line_count(&dir),
        after_rewrite + 2,
        "one entry record plus the meta record"
    );
    assert_eq!(load_session(&dir).messages().len(), 1031);
}

#[tokio::test]
async fn a_checkpoint_failure_surfaces_an_event_and_never_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "contents").unwrap();
    let scripted = ScriptedProvider::new(Vec::new());
    scripted.push(testing::text_turn("done"));
    let persister = crate::harness::Persister::open_in(
        state_dir(&dir),
        session_ref(),
        "/project",
        "scripted/main",
    )
    .await
    .unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(testing::scripted_model(
        &scripted,
        testing::model_info("main", 128_000, 4_096, false, false),
    ));
    let history = persister.history();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        handle,
        workspace.registry().unwrap(),
        sink,
    )
    .history(history)
    .persist(Some(Arc::new(tokio::sync::Mutex::new(persister))))
    .build()
    .unwrap();
    let mut stream = stream;

    // Outgrow the feed so the next checkpoint must full-rewrite, then make
    // the sessions directory unwritable so that rewrite fails. (The append
    // path holds an open file handle and would survive the chmod.)
    let conversation_history = conversation.history();
    for i in 0..1030 {
        conversation_history
            .add_stored(crate::history::ext::stored_user_text(format!("flood {i}")))
            .await;
    }
    let sessions_dir = state_dir(&dir)
        .ensure_subdir(crate::storage::sessions::SESSIONS_DIR)
        .unwrap();
    let permissions = std::fs::metadata(&sessions_dir).unwrap().permissions();
    std::fs::set_permissions(
        &sessions_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o500),
    )
    .unwrap();

    let outcome = crate::harness::run_turn(&conversation, "go", &[], Cancellation::default()).await;
    let mut surfaced = false;
    loop {
        match next_event(&mut stream).await {
            Event::Craft(crate::harness::CraftEvent::PersistenceFailed(_)) => {
                surfaced = true;
            }
            Event::Craft(crate::harness::CraftEvent::Done { .. }) => break,
            _ => {}
        }
    }
    std::fs::set_permissions(&sessions_dir, permissions).unwrap();
    assert!(surfaced, "the checkpoint failure surfaces as a craft event");
    assert!(
        matches!(&outcome, TurnOutcome::Done { reply } if reply == "done"),
        "the run completes despite the failing disk: {outcome:?}"
    );

    // Recovery: with the disk writable again, the next checkpoint must
    // land exactly the live history — a retry after a failed one must
    // never duplicate the batch it already mirrored.
    let conversation_history = conversation.history();
    conversation_history
        .add_stored(crate::history::ext::stored_user_text("after recovery"))
        .await;
    let persister = conversation.persister().expect("persisted");
    persister
        .lock()
        .await
        .checkpoint()
        .await
        .expect("the retry checkpoint succeeds");
    let reloaded =
        crate::storage::sessions::StoredSession::load(session_ref().id(), &state_dir(&dir))
            .unwrap();
    let live = conversation_history.snapshot().await;
    let texts = |entries: &[animist::history::HistoryEntry]| {
        entries
            .iter()
            .filter_map(animist::history::HistoryEntry::current)
            .map(crate::history::ext::message_text)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(reloaded.messages()),
        texts(live.entries()),
        "the recovered file equals the live history, nothing duplicated"
    );
}
