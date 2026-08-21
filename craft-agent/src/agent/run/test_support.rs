#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use craft_providers::provider::Provider;
use craft_providers::{
    ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason, StreamResponse,
    TokenUsage,
};
use serde_json::Value;

use super::*;
use crate::permissions::PermissionManager;
use crate::tools::FileReadTracker;
use crate::{AdvisorSeverity, AgentMode, Envelope, EventSender, ExtractedCommand, InterruptSource};
use craft_config::ToolOutputLines;

pub(super) struct MockInterruptSource {
    commands: Mutex<VecDeque<ExtractedCommand>>,
}

impl MockInterruptSource {
    pub(super) fn new(commands: Vec<ExtractedCommand>) -> Arc<Self> {
        Arc::new(Self {
            commands: Mutex::new(commands.into()),
        })
    }
}

impl InterruptSource for MockInterruptSource {
    fn poll(&self) -> Option<ExtractedCommand> {
        self.commands.lock().unwrap().pop_front()
    }
}

pub(super) struct MockProvider {
    responses: Mutex<Vec<StreamResponse>>,
}

impl MockProvider {
    pub(super) fn new(responses: Vec<StreamResponse>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream_message(
        &self,
        _: &Model,
        _: &[Message],
        _: &str,
        _: &Value,
        _: &flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&craft_storage::id::SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        let mut responses = self.responses.lock().unwrap();
        assert!(!responses.is_empty(), "MockProvider: no more responses");
        Ok(responses.remove(0))
    }

    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        unimplemented!()
    }
}

pub(super) fn default_model() -> Model {
    Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
}

pub(super) fn text_response(stop_reason: StopReason) -> StreamResponse {
    text_reply("response", stop_reason)
}

pub(super) fn text_reply(text: &str, stop_reason: StopReason) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(stop_reason),
    }
}

pub(super) fn empty_response() -> StreamResponse {
    assistant_response(vec![])
}

pub(super) fn thinking_response() -> StreamResponse {
    assistant_response(vec![ContentBlock::Thinking {
        thinking: "stalled".into(),
        signature: None,
    }])
}

fn assistant_response(content: Vec<ContentBlock>) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(StopReason::EndTurn),
    }
}

pub(super) fn make_agent_params() -> AgentParams {
    AgentParams {
        provider: Arc::new(MockProvider::new(vec![])),
        model: default_model(),
        config: AgentConfig::default(),
        tool_output_lines: ToolOutputLines::default(),
        permissions: Arc::new(PermissionManager::new(
            craft_config::PermissionsConfig {
                default: craft_config::DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp"),
            Arc::default(),
        )),
        session_id: None,
        mailbox: None,
        timeouts: craft_providers::Timeouts::default(),
        file_tracker: FileReadTracker::fresh(),
        prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
        subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
        registry: Arc::new(crate::tools::ToolRegistry::with_natives()),
        compression: craft_config::CompressionConfig::default(),
        model_policy: Arc::new(craft_config::ModelPolicy::default()),
        findings_store: None,
        fs: Arc::new(crate::tools::LocalFs),
        doom: Arc::new(std::sync::Mutex::new(crate::agent::doom::DoomTracker::new())),
        flow_thread_history: None,
        flow_thread_manager: None,
        flow_advisor: None,
        flow_progress_tx: None,
    }
}

pub(super) fn make_run_params(
    history: &mut History,
) -> (AgentRunParams<'_>, flume::Receiver<Envelope>) {
    let (raw_tx, event_rx) = flume::unbounded();
    (
        AgentRunParams {
            history,
            system: "system".into(),
            event_tx: EventSender::new(raw_tx, 0),
            tools: serde_json::json!([]),
            promoted: crate::tools::PromotedTools::new(),
            tool_build: None,
            hooks: None,
        },
        event_rx,
    )
}

pub(super) fn default_input() -> AgentInput {
    AgentInput {
        message: "hello".into(),
        mode: AgentMode::Build,
        ..Default::default()
    }
}

pub(super) fn drain_events(rx: &flume::Receiver<Envelope>) -> Vec<Envelope> {
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    events
}

pub(super) async fn run_agent(provider: MockProvider, max_turns: Option<u32>) -> (u32, DoneReason) {
    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let mut params = make_agent_params();
    params.provider = Arc::new(provider);
    params.config.max_turns = max_turns;
    let agent = Agent::new(params, run_params);
    let _ = agent.run(default_input()).await;
    drain_events(&event_rx)
        .into_iter()
        .find_map(|e| match e.event {
            AgentEvent::Done {
                num_turns, reason, ..
            } => Some((num_turns, reason)),
            _ => None,
        })
        .expect("expected Done event")
}

pub(super) fn has_event(events: &[Envelope], predicate: impl Fn(&AgentEvent) -> bool) -> bool {
    events.iter().any(|e| predicate(&e.event))
}

pub(super) fn has_interrupt_in_history(history: &[Message]) -> bool {
    history.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { text } if text.contains("<user-interrupt>")))
    })
}

pub(super) fn tool_call_response(tool_name: &str, tool_id: &str) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                tool_id,
                tool_name,
                serde_json::json!({"pattern": "*.nonexistent_test_xyz", "path": "/tmp"}),
            )],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(StopReason::ToolUse),
    }
}

pub(super) fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
    let mut model = default_model();
    model.context_window = context_window;
    model.max_output_tokens = Some(max_output_tokens);
    model
}

#[track_caller]
pub(super) fn assert_ends_with_cancel_marker(history: &History) {
    let last = history.as_slice().last().unwrap();
    assert!(matches!(last.role, Role::User));
    assert!(
        matches!(&last.content[0], ContentBlock::Text { text } if text == "[Cancelled by user]")
    );
}

pub(super) struct PanickingProvider;
#[async_trait]
impl Provider for PanickingProvider {
    async fn stream_message(
        &self,
        _: &Model,
        _: &[Message],
        _: &str,
        _: &Value,
        _: &flume::Sender<ProviderEvent>,
        _: RequestOptions,
        _: Option<&craft_storage::id::SessionRef>,
    ) -> Result<StreamResponse, AgentError> {
        panic!("LLM should not be called when VCC compaction succeeds")
    }
    async fn list_models(&self) -> Result<Vec<String>, AgentError> {
        unimplemented!()
    }
}

pub(super) fn vcc_overflow_history() -> Vec<Message> {
    let mut msgs = Vec::new();
    for i in 0..6 {
        msgs.push(Message::user(format!("do task {i}")));
        msgs.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                format!("t{i}"),
                "bash",
                serde_json::json!({"command": format!("echo step{i}")}),
            )],
            ..Default::default()
        });
        msgs.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: format!("t{i}"),
                content: format!("output {i}"),
                images: vec![],
                is_error: false,
            }],
            ..Default::default()
        });
    }
    msgs.push(Message::user("final user message".into()));
    msgs
}

pub(super) async fn run_nudge(
    responses: Vec<StreamResponse>,
) -> (Vec<Envelope>, Option<u32>, History) {
    let mut history = History::new(Vec::new());
    let (run_params, event_rx) = make_run_params(&mut history);
    let mut params = make_agent_params();
    params.provider = Arc::new(MockProvider::new(responses));
    let agent = Agent::new(params, run_params);
    let _ = agent.run(default_input()).await;
    let events = drain_events(&event_rx);
    let done = events.iter().find_map(|e| match &e.event {
        AgentEvent::Done { num_turns, .. } => Some(*num_turns),
        _ => None,
    });
    (events, done, history)
}

pub(super) fn shift_tool_call(tool_id: &str, target: &str, rationale: &str) -> StreamResponse {
    StreamResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: format!("shifting to {target}"),
                },
                ContentBlock::ToolUse {
                    id: tool_id.into(),
                    name: "shift".into(),
                    input: serde_json::json!({
                        "target": target,
                        "rationale": rationale,
                    }),
                    thought_signature: None,
                },
            ],
            ..Default::default()
        },
        usage: TokenUsage::default(),
        stop_reason: Some(StopReason::ToolUse),
    }
}

pub(super) fn tmp_flow_store() -> (tempfile::TempDir, Arc<craft_storage::flow::FlowStore>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let store = Arc::new(craft_storage::flow::FlowStore::from_root(
        tmp.path().to_path_buf(),
    ));
    (tmp, store)
}

pub(super) fn flow_agent_params(
    store: Arc<craft_storage::flow::FlowStore>,
    progress_tx: flume::Sender<crate::agent::flow_loop::FlowProgress>,
) -> AgentParams {
    let (state, _progress_rx, _cancel_trigger) =
        crate::agent::flow_loop::FlowRunState::split(store, "test-project", "test-workstream");
    AgentParams {
        provider: Arc::new(MockProvider::new(vec![])),
        model: default_model(),
        config: AgentConfig::default(),
        tool_output_lines: ToolOutputLines::default(),
        permissions: Arc::new(PermissionManager::new(
            craft_config::PermissionsConfig {
                default: craft_config::DefaultEffect::Allow,
                rules: vec![],
                ..Default::default()
            },
            std::path::PathBuf::from("/tmp"),
            Arc::default(),
        )),
        session_id: None,
        mailbox: None,
        timeouts: craft_providers::Timeouts::default(),
        file_tracker: FileReadTracker::fresh(),
        prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
        subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
        registry: Arc::new(crate::tools::ToolRegistry::with_natives()),
        compression: craft_config::CompressionConfig::default(),
        model_policy: Arc::new(craft_config::ModelPolicy::default()),
        findings_store: None,
        fs: Arc::new(crate::tools::LocalFs),
        doom: Arc::new(std::sync::Mutex::new(crate::agent::doom::DoomTracker::new())),
        flow_thread_history: Some(state.thread_history),
        flow_thread_manager: Some(state.thread_manager),
        flow_advisor: Some(state.advisor),
        flow_progress_tx: Some(progress_tx),
    }
}

pub(super) fn flow_input() -> AgentInput {
    AgentInput {
        message: "please flow".into(),
        mode: AgentMode::Flow("test-workstream".into()),
        ..Default::default()
    }
}

pub(super) fn advisor_note(
    severity: AdvisorSeverity,
    message: &str,
) -> crate::agent::advisor::AdvisorNote {
    crate::agent::advisor::AdvisorNote {
        severity,
        message: message.into(),
    }
}

pub(super) fn advisor_cfg(
    auto_act: craft_config::AdvisorAutoAct,
    max_act_turns: u32,
) -> craft_config::AdvisorConfig {
    craft_config::AdvisorConfig {
        enabled: true,
        model: None,
        dedup_size: 16,
        auto_act,
        max_act_turns,
    }
}
