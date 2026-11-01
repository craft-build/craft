//! Run-loop integration tests, split by subsystem. The shared scripted
//! harness lives here; the topic modules hold the tests.

mod advisor;
mod cli_policy;
mod compression;
mod dispatch;
mod overflow;
mod spec;
mod turns;

use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use animist::agent::Cancellation;
use animist::agent::tool::{
    ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolRegistry, ToolScheduling,
};
use animist::history::{ContentBlock, History, HistoryEntry, StoredMessage};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::harness::{
    Conversation, ConversationBuilder, Event, EventSink, EventStream, TurnOutcome,
};
use crate::provider::ModelHandle;
use crate::testing::{Attempt, ScriptedProvider, ToolCallSpec};
use crate::tools::Workspace;

/// A scripted provider with `turns` queued, recording every request it
/// serves.
fn script(turns: Vec<Attempt>) -> ScriptedProvider {
    let provider = ScriptedProvider::new(Vec::new());
    for turn in turns {
        provider.push(turn);
    }
    provider
}

/// A run-loop model handle over a scripted provider.
fn model(provider: &ScriptedProvider) -> ModelHandle {
    ModelHandle::scripted(crate::testing::scripted_model(
        provider,
        crate::testing::model_info("scripted", 200_000, 8_192, false, false),
    ))
}

/// One tool-call spec for [`crate::testing::tool_turn`].
fn tool_event(id: &str, name: &str, args: serde_json::Value) -> ToolCallSpec {
    (id.to_owned(), name.to_owned(), args)
}

/// A conversation over the workspace's real tool registry.
async fn workspace_conversation(
    dir: &tempfile::TempDir,
    scripted: &ScriptedProvider,
) -> (Conversation, EventStream) {
    let workspace = Workspace::new(dir.path()).expect("workspace");
    // The registry's retrieve reads the same history the conversation runs
    // on, so retrieval works end to end in these tests.
    let history = History::default();
    conversation_over(
        scripted,
        &Config::parse("").expect("default config"),
        &dir.path().display().to_string(),
        workspace.registry(&history).expect("registry"),
        Some(history),
    )
    .await
}

/// A conversation over `tools`, optionally restoring a seeded history.
async fn conversation_over(
    scripted: &ScriptedProvider,
    config: &Config,
    cwd: &str,
    tools: ToolRegistry,
    history: Option<History>,
) -> (Conversation, EventStream) {
    conversation_tuned(scripted, config, cwd, tools, history, |builder| builder).await
}

/// [`conversation_over`] with extra builder wiring (reauth hook, retry
/// chain, continuation budget) for the recovery and continuation tests.
async fn conversation_tuned(
    scripted: &ScriptedProvider,
    config: &Config,
    cwd: &str,
    tools: ToolRegistry,
    history: Option<History>,
    edit: impl FnOnce(
        crate::harness::ConversationBuilder<'_>,
    ) -> crate::harness::ConversationBuilder<'_>,
) -> (Conversation, EventStream) {
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(crate::testing::scripted_model(
        scripted,
        crate::testing::model_info("scripted", 200_000, 8_192, false, false),
    ));
    let mut builder = ConversationBuilder::new(config, cwd, "", handle, tools, sink);
    if let Some(history) = history {
        builder = builder.history(history);
    }
    let conversation = edit(builder).build().expect("conversation builds");
    (conversation, stream)
}

/// A conversation over `tools` with a fake tool injected under `name`:
/// the classic way the scheduling tests pinned ordering.
#[allow(dead_code)]
async fn conversation_with_tools(
    scripted: &ScriptedProvider,
    config: &Config,
    cwd: &str,
    tools: ToolRegistry,
) -> (Conversation, EventStream) {
    conversation_over(scripted, config, cwd, tools, None).await
}
/// A history seeded with `messages`, as a resumed session restores it.
async fn seeded_history(messages: Vec<StoredMessage>) -> History {
    let history = History::default();
    for message in messages {
        history.add_stored(message).await;
    }
    history
}

/// Wait for the next event, failing (not hanging) when none arrives: the
/// conversation's stream stays open until the whole conversation drops, so
/// a missed event would otherwise wait forever.
async fn next_event(stream: &mut EventStream) -> Event {
    tokio::time::timeout(std::time::Duration::from_secs(30), stream.next())
        .await
        .expect("event before the deadline")
        .expect("stream closed unexpectedly")
}

/// The conversation's events up to and including the terminal `Done`.
async fn events_until_done(stream: &mut EventStream) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = next_event(stream).await;
        let done = matches!(event, Event::Craft(crate::harness::CraftEvent::Done { .. }));
        events.push(event);
        if done {
            return events;
        }
    }
}

/// The conversation's committed entries, current revisions included.
async fn entries(conversation: &Conversation) -> Vec<HistoryEntry> {
    conversation.history().snapshot().await.entries().to_vec()
}

/// The current revision of every committed entry.
async fn stored(conversation: &Conversation) -> Vec<StoredMessage> {
    entries(conversation)
        .await
        .iter()
        .filter_map(HistoryEntry::current)
        .cloned()
        .collect()
}

/// Every tool result committed to history as the tool returned it: the
/// latest revision craft's durable alterations have not touched (08 may
/// have marked or compressed the newest one; the agent's squash keeps the
/// interrupted placeholder as the source, so the verbatim entry is not the
/// tool's return).
async fn raw_tool_results(conversation: &Conversation) -> Vec<ContentBlock> {
    entries(conversation)
        .await
        .iter()
        .flat_map(|entry| {
            let stored = entry
                .revisions()
                .iter()
                .rev()
                .find(|stored| {
                    !stored
                        .tags()
                        .contains_key(crate::compaction::alter::ALTER_TAG)
                })
                .unwrap_or_else(|| entry.verbatim());
            crate::history::ext::tool_results(stored.message())
        })
        .cloned()
        .collect()
}

/// Every tool result committed to history, in call order.
async fn tool_results(conversation: &Conversation) -> Vec<ContentBlock> {
    stored(conversation)
        .await
        .iter()
        .flat_map(|message| crate::history::ext::tool_results(message.message()))
        .cloned()
        .collect()
}

/// The result block's model-visible text, `[error] `-prefixed on failure.
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

/// Drive one turn on a fresh cancellation.
async fn run_turn(conversation: &Conversation, prompt: &str) -> TurnOutcome {
    crate::harness::run_turn(conversation, prompt, &[], Cancellation::default()).await
}
