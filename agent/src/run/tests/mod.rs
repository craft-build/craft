//! Run-loop integration tests, split by subsystem. The shared scripted
//! harness lives here; the topic modules hold the tests.

mod advisor;
mod cli_policy;
mod compression;
mod dispatch;
mod overflow;
mod spec;
mod turns;

use animist::agent::Cancellation;
use animist::agent::tool::ToolRegistry;
use animist::history::{ContentBlock, History, HistoryEntry, StoredMessage};

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
    conversation_over(
        scripted,
        &Config::parse("").expect("default config"),
        &dir.path().display().to_string(),
        workspace.registry().expect("registry"),
        None,
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
    let (sink, stream) = EventSink::new();
    let handle = ModelHandle::scripted(crate::testing::scripted_model(
        scripted,
        crate::testing::model_info("scripted", 200_000, 8_192, false, false),
    ));
    let mut builder = ConversationBuilder::new(config, cwd, "", handle, tools, sink);
    if let Some(history) = history {
        builder = builder.history(history);
    }
    let conversation = builder.build().expect("conversation builds");
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

/// Drive one turn on a fresh cancellation.
async fn run_turn(conversation: &Conversation, prompt: &str) -> TurnOutcome {
    crate::harness::run_turn(conversation, prompt, &[], Cancellation::default()).await
}
