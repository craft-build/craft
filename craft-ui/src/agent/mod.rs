mod agent_loop;
mod cancel_map;
mod command_router;
pub(crate) mod shared_queue;

use std::mem;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use craft_agent::permissions::PermissionManager;
use craft_agent::{
    AgentConfig, CancelMap, CancelToken, Envelope, HistorySnapshot, McpCommand, McpConfigErrors,
    McpHandle, McpSnapshotReader, SessionMailbox, SharedMessages, ToolOutputLines,
};
use craft_config::ModelPolicy;
use craft_lua::EventHandle;
use craft_storage::id::SessionRef;

use self::cancel_map::new_run_cancel_map;
use craft_providers::provider::Provider;
use craft_providers::{Message, Model};
use tracing::{info, warn};

use crate::app::App;

use self::agent_loop::AgentLoop;
use self::command_router::spawn_command_router;
pub(crate) use self::shared_queue::{QueueSender, QueuedMessage};

pub(crate) struct ModelSlot {
    pub(crate) model: Model,
    pub(crate) provider: Arc<dyn Provider>,
}

pub(crate) enum AgentCommand {
    Cancel { run_id: u64 },
    CancelAll,
    CancelSubagent { tool_use_id: String },
}

/// Input channels (`cmd_tx`, `answer_tx`, `queue`) are per-agent, so an old
/// loop can never steal new input. The output channel (`agent_tx`/`agent_rx`)
/// is per-tab: `respawn` reuses it, so anyone still holding a sender (a Lua
/// restore reply, a click, an old agent winding down) can always deliver.
/// Stale events are filtered by `run_id`, not by killing the channel.
pub(crate) struct AgentHandles {
    pub(crate) cmd_tx: flume::Sender<AgentCommand>,
    pub(crate) agent_rx: flume::Receiver<Envelope>,
    agent_tx: flume::Sender<Envelope>,
    pub(crate) answer_tx: flume::Sender<String>,
    pub(crate) history: SharedMessages,
    pub(crate) mcp_handle: Option<McpHandle>,
    pub(crate) mcp_config_errors: McpConfigErrors,
    pub(crate) queue: QueueSender,
    pub(crate) timeouts: craft_providers::Timeouts,
    pub(crate) btw_system: Arc<ArcSwap<String>>,
    pub(crate) flow_progress_rx: flume::Receiver<craft_agent::FlowProgress>,
    pub(crate) repomap_enabled: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) goal_ready_flag: Arc<std::sync::atomic::AtomicBool>,
    model_policy: Arc<ModelPolicy>,
    mailbox: Option<SessionMailbox>,
    task: tokio::task::JoinHandle<()>,
}

impl AgentHandles {
    /// MCP is started once up front. The handle lives across agent respawns, only the agent
    /// loop task gets replaced.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        initial_history: Vec<Message>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        session_id: Option<SessionRef>,
        timeouts: craft_providers::Timeouts,
        lua_handle: EventHandle,
        mcp_handle: Option<McpHandle>,
        mcp_config_errors: McpConfigErrors,
        compression: craft_config::CompressionConfig,
        model_policy: Arc<ModelPolicy>,
        flow_store: std::sync::Arc<craft_storage::flow::FlowStore>,
        embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>,
    ) -> Self {
        spawn_agent_internal(
            flume::unbounded(),
            model_slot,
            initial_history,
            config,
            tool_output_lines,
            permissions,
            mcp_handle,
            mcp_config_errors,
            session_id,
            timeouts,
            lua_handle,
            compression,
            model_policy,
            flow_store,
            embed_rx,
        )
    }

    pub(crate) fn mcp_reader(&self) -> McpSnapshotReader {
        self.mcp_handle
            .as_ref()
            .map(McpHandle::reader)
            .unwrap_or_else(McpSnapshotReader::empty)
    }

    pub(crate) fn apply_to_app(&self, app: &mut App) {
        app.answer_tx = Some(self.answer_tx.clone());
        app.cmd_tx = Some(self.cmd_tx.clone());
        app.shared_history = Some(Arc::clone(&self.history));
        app.queue.set_shared(self.queue.clone());
        app.btw_system = Some(Arc::clone(&self.btw_system));
        app.repomap_enabled = Arc::clone(&self.repomap_enabled);
        app.goal_ready_flag = Arc::clone(&self.goal_ready_flag);

        let restore_tx =
            craft_agent::EventSender::new(self.agent_tx.clone(), crate::app::RESTORE_RUN_ID);
        app.restore_event_tx = Some(restore_tx);
    }

    pub(crate) fn send_cancel_all(&self) {
        let _ = self.cmd_tx.try_send(AgentCommand::CancelAll);
    }

    pub(crate) fn claim_mailbox_wake(&self) -> Vec<Message> {
        self.mailbox
            .as_ref()
            .map(SessionMailbox::claim_wake)
            .unwrap_or_default()
    }

    /// True if the agent task has exited (cleanly or panicked). Used by the
    /// supervisor to surface "agent stopped unexpectedly" — `agent_rx`
    /// disconnect is NOT a reliable signal because `App.restore_event_tx`
    /// holds a clone of `agent_tx` that keeps the channel connected after
    /// the task drops.
    pub(crate) fn task_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub(crate) fn cancel(self) {
        self.send_cancel_all();
    }

    pub(crate) fn send_mcp(&self, cmd: McpCommand) {
        if let Some(ref h) = self.mcp_handle {
            h.send(cmd);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn respawn(
        &mut self,
        history: Vec<Message>,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        compression: craft_config::CompressionConfig,
        tool_output_lines: ToolOutputLines,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
        lua_handle: EventHandle,
    ) {
        // The output channel survives the respawn, so this bump is the only
        // thing that makes the old loop's in-flight envelopes stale. It lives
        // here so no caller can respawn without it.
        app.run_id += 1;
        let slot = model_slot.load();
        let provider = Arc::clone(&slot.provider);
        tokio::spawn(async move {
            if let Err(e) = provider.reload_auth().await {
                warn!(error = %e, "failed to reload auth, continuing with existing credentials");
            }
        });
        let flow_store = std::sync::Arc::new(
            craft_storage::flow::FlowStore::new(&app.storage).unwrap_or_else(|_| {
                craft_storage::flow::FlowStore::from_root(app.storage.path().join("projects"))
            }),
        );
        let new = spawn_agent_internal(
            (self.agent_tx.clone(), self.agent_rx.clone()),
            model_slot,
            history,
            config,
            tool_output_lines,
            permissions,
            self.mcp_handle.clone(),
            self.mcp_config_errors.clone(),
            Some(app.state.session.id.clone()),
            self.timeouts,
            lua_handle,
            compression,
            Arc::clone(&self.model_policy),
            flow_store,
            None,
        );
        let old = mem::replace(self, new);
        // Repoint the app at the new queue before dropping `old`, otherwise the app keeps
        // the last old `QueueSender` alive and the old loop parks in `recv_notify` forever.
        self.apply_to_app(app);
        app.flush_restored_queue();
        old.cancel();
    }

    /// Tear down the agent task without touching the shared MCP handle.
    /// Use this in the multi-session loop where one `McpHandle` is shared
    /// across every runtime; the caller shuts MCP down exactly once after
    /// all per-runtime agents have stopped.
    pub(crate) async fn shutdown_no_mcp(self, timeout: Duration) {
        self.send_cancel_all();
        let mut task = self.task;
        drop((self.cmd_tx, self.agent_rx, self.answer_tx, self.queue));
        info!("waiting for agent to finish (timeout {timeout:?})");
        let finished = tokio::select! {
            _ = &mut task => true,
            _ = tokio::time::sleep(timeout) => false,
        };
        if !finished {
            warn!("agent did not finish within {timeout:?}, forcing shutdown");
        }
    }

    /// Hand back the agent task, dropping every channel so the loop can
    /// wind down. The caller sends `CancelAll` first and then awaits all
    /// tabs at once via [`join_all`] instead of paying a serial timeout
    /// per tab.
    pub(crate) fn into_task(self) -> tokio::task::JoinHandle<()> {
        self.task
    }
}

/// Wait for every agent task under one shared timeout, not one per task.
pub(crate) fn join_all(tasks: Vec<tokio::task::JoinHandle<()>>, timeout: Duration) {
    info!(
        count = tasks.len(),
        "waiting for agents to finish (timeout {timeout:?})"
    );
    tokio::runtime::Handle::current().block_on(async move {
        let mut set = tokio::task::JoinSet::from_iter(tasks);
        let drain = async { while set.join_next().await.is_some() {} };
        tokio::pin!(drain);
        let finished = tokio::select! {
            _ = &mut drain => true,
            _ = tokio::time::sleep(timeout) => false,
        };
        if !finished {
            warn!("agents did not finish within {timeout:?}, forcing shutdown");
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent_internal(
    (agent_tx, agent_rx): (flume::Sender<Envelope>, flume::Receiver<Envelope>),
    model_slot: &Arc<ArcSwap<ModelSlot>>,
    initial_history: Vec<Message>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    permissions: &Arc<PermissionManager>,
    mcp_handle: Option<McpHandle>,
    mcp_config_errors: McpConfigErrors,
    session_id: Option<SessionRef>,
    timeouts: craft_providers::Timeouts,
    lua_handle: EventHandle,
    compression: craft_config::CompressionConfig,
    model_policy: Arc<ModelPolicy>,
    flow_store: std::sync::Arc<craft_storage::flow::FlowStore>,
    embed_rx: Option<flume::Receiver<craft_agent::EmbedRequest>>,
) -> AgentHandles {
    let (cmd_tx, cmd_rx) = flume::unbounded::<AgentCommand>();
    let (answer_tx, answer_rx) = flume::unbounded::<String>();
    let (queue_tx, queue_rx) = shared_queue::queue();
    let queue_rx = Arc::new(queue_rx);
    // Seeded empty because `AgentLoop::new` below publishes the real snapshot
    // synchronously, before any handle escapes.
    let shared_history: SharedMessages =
        Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
    let (init_trigger, init_cancel) = CancelToken::new();
    let cancel_map = Arc::new(new_run_cancel_map(0, init_trigger));
    let subagent_cancels: Arc<CancelMap<String>> = Arc::new(CancelMap::new());

    let btw_system: Arc<ArcSwap<String>> = Arc::new(ArcSwap::from_pointee(String::new()));
    let repomap_enabled = Arc::new(std::sync::atomic::AtomicBool::new(config.repomap.enabled));
    let mailbox = session_id
        .as_ref()
        .map(|session_id| SessionMailbox::register(session_id.id()));

    spawn_command_router(
        cmd_rx,
        Arc::clone(&cancel_map),
        Arc::clone(&subagent_cancels),
    );

    let (flow_progress_tx, flow_progress_rx) = flume::unbounded::<craft_agent::FlowProgress>();
    let goal_ready_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let agent_loop = AgentLoop::new(
        Arc::clone(model_slot),
        config,
        tool_output_lines,
        initial_history,
        Arc::clone(&shared_history),
        mcp_handle.clone(),
        Arc::clone(permissions),
        agent_tx.clone(),
        answer_rx,
        queue_rx,
        cancel_map,
        init_cancel,
        session_id,
        mailbox.clone(),
        timeouts,
        lua_handle,
        Arc::clone(&btw_system),
        compression,
        Arc::clone(&model_policy),
        subagent_cancels,
        flow_store,
        flow_progress_tx,
        Arc::clone(&repomap_enabled),
        Arc::clone(&goal_ready_flag),
    );

    let task = tokio::spawn(agent_loop.run());

    if let Some(rx) = embed_rx {
        let service = craft_agent::EmbeddingService::new();
        tokio::spawn(async move {
            while let Ok((text, reply_tx)) = rx.recv_async().await {
                let result = service.embed(&text).await.map_err(|e| e.to_string());
                let _ = reply_tx.send(result);
            }
        });
    }

    AgentHandles {
        cmd_tx,
        agent_rx,
        agent_tx,
        answer_tx,
        history: shared_history,
        mcp_handle,
        mcp_config_errors,
        queue: queue_tx,
        timeouts,
        btw_system,
        flow_progress_rx,
        repomap_enabled,
        goal_ready_flag,
        model_policy,
        mailbox,
        task,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use craft_agent::AgentEvent;
    use craft_config::PermissionsConfig;
    use craft_providers::provider::Provider;
    use craft_providers::{
        AgentError, Message, Model, ProviderEvent, RequestOptions, StreamResponse,
    };
    use craft_storage::id::SessionRef;

    use super::*;

    const LONG_TIMEOUT: Duration = Duration::from_secs(60);
    const SHORT_TIMEOUT: Duration = Duration::from_millis(50);
    const PROBE_TEXT: &str = "probe-through-old-sender";
    const RESTORED_TEXT: &str = "restored-queued-message";
    const RESUMED_HISTORY_TEXT: &str = "resumed-conversation";

    struct StubProvider;

    #[async_trait]
    impl Provider for StubProvider {
        async fn stream_message(
            &self,
            _model: &Model,
            _messages: &[Message],
            _system: &str,
            _tools: &serde_json::Value,
            _event_tx: &flume::Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&SessionRef>,
        ) -> Result<StreamResponse, AgentError> {
            std::future::pending::<Result<StreamResponse, AgentError>>().await
        }

        async fn list_models(&self) -> Result<Vec<String>, AgentError> {
            Ok(Vec::new())
        }
    }

    fn stub_spawn() -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        stub_spawn_with(Vec::new())
    }

    fn stub_spawn_with(
        initial_history: Vec<Message>,
    ) -> (
        AgentHandles,
        Arc<ArcSwap<ModelSlot>>,
        Arc<PermissionManager>,
    ) {
        let model_slot = Arc::new(ArcSwap::from_pointee(ModelSlot {
            model: crate::components::test_model(),
            provider: Arc::new(StubProvider),
        }));
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            PathBuf::from("/tmp"),
        ));
        let flow_store = Arc::new(craft_storage::flow::FlowStore::from_root(PathBuf::from(
            "/tmp",
        )));
        let handles = AgentHandles::spawn(
            &model_slot,
            initial_history,
            AgentConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            None,
            craft_providers::Timeouts::default(),
            EventHandle::disconnected_for_test(),
            None,
            McpConfigErrors::new(PathBuf::new()),
            craft_config::CompressionConfig::default(),
            Arc::new(ModelPolicy::default()),
            flow_store,
            None,
        );
        (handles, model_slot, permissions)
    }

    fn respawn(
        handles: &mut AgentHandles,
        model_slot: &Arc<ArcSwap<ModelSlot>>,
        permissions: &Arc<PermissionManager>,
        app: &mut App,
    ) {
        handles.respawn(
            Vec::new(),
            model_slot,
            AgentConfig::default(),
            craft_config::CompressionConfig::default(),
            ToolOutputLines::default(),
            permissions,
            app,
            EventHandle::disconnected_for_test(),
        );
    }

    /// Senders captured before any respawn (Lua restore replies, clicks) must
    /// still reach the live receiver, and restored queue items must land in
    /// the freshly wired queue, not the one that just died.
    #[test]
    fn respawn_twice_keeps_channel_and_delivers_restored_queue() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let (mut handles, model_slot, permissions) = stub_spawn();
        let pre_gen1_sender =
            craft_agent::EventSender::new(handles.agent_tx.clone(), crate::app::RESTORE_RUN_ID);

        let mut app = crate::app::tests::test_app();
        let run_id_before = app.run_id;
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(app.run_id, run_id_before + 1);

        app.state.session_mut().meta.queued_messages = vec![RESTORED_TEXT.into()];
        respawn(&mut handles, &model_slot, &permissions, &mut app);
        assert_eq!(
            app.run_id,
            run_id_before + 2,
            "each respawn must bump run_id exactly once"
        );
        assert_eq!(
            app.queue.text_messages(),
            [RESTORED_TEXT],
            "the restored item lands in the new queue exactly once"
        );

        pre_gen1_sender
            .send(AgentEvent::TextDelta {
                text: PROBE_TEXT.into(),
            })
            .expect("pre-generation-1 sender must still deliver after two respawns");

        let mut probe_seen = false;
        let mut consumed_seen = false;
        while !(probe_seen && consumed_seen) {
            let envelope = handles
                .agent_rx
                .recv_timeout(LONG_TIMEOUT)
                .expect("probe or restored queue item never reached the tab channel");
            match envelope.event {
                AgentEvent::TextDelta { ref text } if text == PROBE_TEXT => probe_seen = true,
                AgentEvent::QueueItemConsumed { ref text, .. } => {
                    assert_eq!(text, RESTORED_TEXT);
                    assert_eq!(envelope.run_id, app.run_id);
                    consumed_seen = true;
                }
                _ => {}
            }
        }
    }

    /// If the seeded empty snapshot ever outlived `spawn`, the next checkpoint
    /// would adopt it and wipe a resumed conversation from disk.
    #[test]
    fn spawn_publishes_the_resumed_history_before_the_handles_escape() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let (handles, _model_slot, _permissions) =
            stub_spawn_with(vec![Message::user(RESUMED_HISTORY_TEXT.into())]);
        let snapshot = handles.history.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "the seeded empty snapshot must be replaced synchronously"
        );
        assert_eq!(snapshot.messages[0].user_text(), Some(RESUMED_HISTORY_TEXT));
    }

    #[test]
    fn respawn_publishes_the_new_history_into_the_app_mirror() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let (mut handles, model_slot, permissions) = stub_spawn();
        let mut app = crate::app::tests::test_app();
        handles.respawn(
            vec![Message::user(RESUMED_HISTORY_TEXT.into())],
            &model_slot,
            AgentConfig::default(),
            craft_config::CompressionConfig::default(),
            ToolOutputLines::default(),
            &permissions,
            &mut app,
            EventHandle::disconnected_for_test(),
        );

        let mirror = app
            .shared_history
            .as_ref()
            .expect("respawn wires the live mirror into the app");
        let snapshot = mirror.load();
        assert_eq!(
            snapshot.messages.len(),
            1,
            "a checkpoint right after respawn must not see the seeded empty snapshot"
        );
        assert_eq!(snapshot.messages[0].user_text(), Some(RESUMED_HISTORY_TEXT));
    }

    #[test]
    fn join_all_returns_when_all_tasks_complete() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        join_all(Vec::new(), LONG_TIMEOUT);
        join_all(
            (0..3).map(|_| tokio::spawn(async {})).collect(),
            LONG_TIMEOUT,
        );
    }

    #[test]
    fn join_all_stuck_task_returns_after_shared_timeout() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let start = Instant::now();
        join_all(
            vec![
                tokio::spawn(async {}),
                tokio::spawn(async { std::future::pending::<()>().await }),
            ],
            SHORT_TIMEOUT,
        );
        assert!(start.elapsed() >= SHORT_TIMEOUT);
    }
}
