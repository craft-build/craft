use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use craft_agent::agent;
use craft_agent::mcp::McpHandle;
use craft_agent::mcp::config::McpServerStatus;
use craft_agent::permissions::PermissionManager;
use craft_agent::template;
use craft_agent::template::Vars;
use craft_agent::tools::FileReadTracker;
use craft_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, CancelMap,
    CancelToken, CancelTrigger, DoomTracker, Envelope, EventSender, FindingsStore, History,
    Instructions, McpCommand, PromptRole, SessionMailbox, SharedDoomTracker, SharedFindingsStore,
    SharedMessages, ToolOutputLines, TurnType,
};
use craft_lua::EventHandle;
use craft_providers::{AgentError, Message, Model, StopReason, TokenUsage};
use craft_storage::flow::FlowStore;
use craft_storage::id::SessionRef;
use serde_json::Value;
use tracing::error;

use super::ModelSlot;
use super::cancel_map::RunCancelMap;
use super::shared_queue::{QueueItem, QueueReceiver};

pub(super) struct AgentLoop {
    model_slot: Arc<ArcSwap<ModelSlot>>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    vars: Vars,
    instructions: Instructions,
    tools: Value,
    promoted: craft_agent::tools::PromotedTools,
    mcp_handle: Option<McpHandle>,
    history: History,
    cancel_map: Arc<RunCancelMap>,
    init_cancel: CancelToken,
    permissions: Arc<PermissionManager>,
    file_tracker: Arc<FileReadTracker>,
    findings_store: SharedFindingsStore,
    min_run_id: u64,
    agent_tx: flume::Sender<Envelope>,
    answer_rx: Arc<tokio::sync::Mutex<flume::Receiver<String>>>,
    queue: Arc<QueueReceiver>,
    session_id: Option<SessionRef>,
    mailbox: Option<SessionMailbox>,
    timeouts: craft_providers::Timeouts,
    lua_handle: EventHandle,
    btw_system: Arc<ArcSwap<String>>,
    compression: craft_config::CompressionConfig,
    model_policy: Arc<craft_config::ModelPolicy>,
    doom: SharedDoomTracker,
    subagent_cancels: Arc<CancelMap<String>>,
    /// Phase 2 persists the typed log through this store; Phase 1 keeps it
    /// wired so the shell is ready without a cascade of signature changes.
    #[allow(dead_code)]
    flow_store: Arc<FlowStore>,
    flow_progress_tx: flume::Sender<craft_agent::FlowProgress>,
    repomap_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Set by the App (via `handle_flow_progress`) when `GoalReady` fires so
    /// `do_flow_run` can break out of `agent.run`, re-prompt for approval, and
    /// resume on approve/revise.
    goal_ready_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        initial_history: Vec<Message>,
        shared_history: SharedMessages,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancel_map: Arc<RunCancelMap>,
        init_cancel: CancelToken,
        session_id: Option<SessionRef>,
        mailbox: Option<SessionMailbox>,
        timeouts: craft_providers::Timeouts,
        lua_handle: EventHandle,
        btw_system: Arc<ArcSwap<String>>,
        compression: craft_config::CompressionConfig,
        model_policy: Arc<craft_config::ModelPolicy>,
        subagent_cancels: Arc<CancelMap<String>>,
        flow_store: Arc<FlowStore>,
        flow_progress_tx: flume::Sender<craft_agent::FlowProgress>,
        repomap_enabled: Arc<std::sync::atomic::AtomicBool>,
        goal_ready_flag: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            model_slot,
            config,
            tool_output_lines,
            vars: Vars::default(),
            instructions: Instructions::default(),
            tools: Value::Null,
            promoted: craft_agent::tools::PromotedTools::new(),
            mcp_handle,
            history: History::restored(initial_history).with_mirror(shared_history),
            cancel_map,
            init_cancel,
            permissions,
            file_tracker: FileReadTracker::fresh(),
            findings_store: FindingsStore::new_shared(),
            min_run_id: 0,
            agent_tx,
            answer_rx: Arc::new(tokio::sync::Mutex::new(answer_rx)),
            queue,
            session_id,
            mailbox,
            timeouts,
            lua_handle,
            btw_system,
            compression,
            model_policy,
            doom: Arc::new(Mutex::new(DoomTracker::new())),
            subagent_cancels,
            flow_store,
            flow_progress_tx,
            repomap_enabled,
            goal_ready_flag,
        }
    }

    pub(super) async fn run(mut self) {
        if !self.initialize().await {
            return;
        }

        while let Ok(()) = self.queue.recv_notify().await {
            while let Some(entry) = self.queue.pop() {
                if entry.run_id() < self.min_run_id {
                    continue;
                }
                self.process_entry(entry).await;
            }
        }
    }

    async fn process_entry(&mut self, entry: QueueItem) {
        let run_id = entry.run_id();
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);

        let result = match entry {
            QueueItem::Message {
                text,
                image_count,
                input,
                displayed,
                ..
            } => {
                if !displayed {
                    let _ = event_tx.send(AgentEvent::QueueItemConsumed { text, image_count });
                }
                self.do_agent_run(input, event_tx, run_id).await
            }
            QueueItem::Compact { .. } => self.do_compact(&event_tx).await,
        };

        if let Err(e) = result {
            self.emit_error(run_id, e);
        }
    }

    async fn initialize(&mut self) -> bool {
        self.vars = template::env_vars();
        self.reload_instructions().await;
        self.publish_btw_system(&craft_agent::prompt::ResolvedSlots::default());
        if self.init_cancel.is_cancelled() {
            return false;
        }

        let slot = self.model_slot.load();
        self.rebuild_tools(&slot.model);
        if let Some(ref mcp) = self.mcp_handle {
            // The queue is drained right after this, and a prompt typed during
            // startup must still carry the MCP tools.
            if self.init_cancel.race(mcp.ready()).await.is_err() {
                return false;
            }
            spawn_oauth_for_needs_auth(mcp);
        }
        !self.init_cancel.is_cancelled()
    }

    async fn do_compact(&mut self, event_tx: &EventSender) -> Result<(), AgentError> {
        let slot = self.model_slot.load();
        let (provider, model) = agent::resolve_compaction_model(
            &slot.provider,
            &slot.model,
            self.timeouts,
            &self.model_policy,
        )
        .await;
        agent::compact(
            &*provider,
            &model,
            &mut self.history,
            event_tx,
            &self.config,
        )
        .await
    }

    async fn do_agent_run(
        &mut self,
        mut input: AgentInput,
        event_tx: EventSender,
        run_id: u64,
    ) -> Result<(), AgentError> {
        let slot = self.model_slot.load();

        let old_cwd = self.vars.apply("{cwd}").into_owned();
        self.vars = template::env_vars();
        if *self.vars.apply("{cwd}") != old_cwd {
            self.reload_instructions().await;
        }
        self.rebuild_tools(&slot.model);

        if input.mode.flow_workstream().is_some() {
            return self.do_flow_run(input, event_tx, run_id).await;
        }

        for msg in std::mem::take(&mut input.preamble) {
            self.history.push(msg);
        }

        if let Some(ref prompt_ref) = input.prompt {
            let Some(ref mcp) = self.mcp_handle else {
                return Err(AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: "MCP not available".into(),
                });
            };
            let messages = mcp
                .get_prompt(&prompt_ref.qualified_name, &prompt_ref.arguments)
                .await
                .map_err(|e| AgentError::Tool {
                    tool: "mcp_prompt".into(),
                    message: e.to_string(),
                })?;
            for pm in messages {
                let text = pm.content.text.unwrap_or_default();
                let msg = match pm.role {
                    PromptRole::Assistant => Message {
                        role: craft_providers::Role::Assistant,
                        content: vec![craft_providers::ContentBlock::Text { text }],
                        ..Default::default()
                    },
                    PromptRole::User => Message::user(text),
                };
                self.history.push(msg);
            }
        }

        let prompt_slots = self.lua_handle.collect_prompt_slots_async().await;
        let model = self.model_slot.load();
        let compact = self
            .config
            .small_model
            .should_activate(model.model.context_window)
            && self.config.small_model.compact_prompt;
        let system = agent::build_system_prompt(
            &self.vars,
            &input.mode,
            &self.instructions.text,
            &prompt_slots,
            &model.model,
            compact,
        );

        self.publish_btw_system(&prompt_slots);
        let (trigger, cancel) = CancelToken::new();
        self.set_cancel_trigger(run_id, trigger);

        while self.answer_rx.lock().await.try_recv().is_ok() {}

        let agent = Agent::new(
            AgentParams {
                provider: Arc::clone(&slot.provider),
                model: slot.model.clone(),
                config: self.config.clone(),
                tool_output_lines: self.tool_output_lines,
                permissions: Arc::clone(&self.permissions),
                session_id: self.session_id.clone(),
                mailbox: self.mailbox.clone(),
                timeouts: self.timeouts,
                file_tracker: Arc::clone(&self.file_tracker),
                prompt_slots: std::sync::Arc::new(prompt_slots),
                subagent_cancels: Arc::clone(&self.subagent_cancels),
                registry: Arc::clone(craft_agent::tools::ToolRegistry::native_arc()),
                compression: self.compression.clone(),
                model_policy: Arc::clone(&self.model_policy),
                findings_store: Some(Arc::clone(&self.findings_store)),
                fs: Arc::new(craft_agent::tools::LocalFs),
                doom: Arc::clone(&self.doom),
                flow_thread_history: None,
                flow_thread_manager: None,
                flow_advisor: None,
                flow_progress_tx: None,
            },
            AgentRunParams {
                history: &mut self.history,
                system,
                event_tx,
                tools: self.tools.clone(),
                promoted: self.promoted.clone(),
                tool_build: Some(craft_agent::tools::ToolBuild {
                    vars: self.vars.clone(),
                    excluded: Vec::new(),
                    mcp: self.mcp_handle.clone(),
                }),
                hooks: Some(craft_lua::LuaHooks::new(self.lua_handle.clone())
                    as Arc<dyn craft_agent::Hooks>),
            },
        )
        .with_loaded_instructions(self.instructions.loaded.clone())
        .with_user_response_rx(Arc::clone(&self.answer_rx))
        .with_interrupt_source(Arc::clone(&self.queue) as Arc<dyn craft_agent::InterruptSource>)
        .with_cancel(cancel)
        .with_mcp(self.mcp_handle.clone())
        .with_repo_map(
            self.repomap_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
                .then(craft_repomap::RepoMap::try_from_cwd)
                .flatten()
                .map(|rm| rm.with_max_tokens(self.config.repomap.max_tokens)),
        )
        .with_recency_source(Some(Arc::new(craft_lua::LuaRecencySource::new(
            self.lua_handle.clone(),
        ))
            as Arc<dyn craft_agent::prompt::RecencySource>));

        let agent = {
            let role = craft_providers::roles::resolve_role(
                craft_config::model_roles::ModelRole::Default,
                slot.model.clone(),
                Arc::clone(&slot.provider),
                self.timeouts,
            )
            .await;
            agent.with_fallback_chain(role.fallbacks)
        };

        let result = agent.run(input).await;

        self.clear_cancel_trigger(run_id);

        if matches!(result, Err(AgentError::Cancelled)) {
            self.min_run_id = run_id + 1;
        }

        result
    }

    /// Drive Flow mode through the normal `Agent::run` path. Each prompt
    /// builds an `Agent` with the per-workstream typed log, thread manager,
    /// no-op advisor, and progress channel attached, then runs one
    /// `agent.run(input)` to completion. `FlowProgress` events flow to the
    /// TUI's FlowPanel through `flow_progress_tx`. The goal-approval gate is
    /// a terminal `Done { stop_reason: AwaitingGoalApproval }`; the TUI
    /// re-prompts on `answer_rx` and re-enters with the approval text. The
    /// agent reads its persisted typed log on resume to re-derive the goal
    /// and the next shift (plan §7).
    /// Drive Flow mode through the normal `Agent::run` path, looping across
    /// the goal-approval gate. Each iteration builds an `Agent` with the
    /// per-workstream typed log, thread manager, advisor, and progress channel
    /// attached, then runs it to completion. When the `Tpm -> Plan` gate fires
    /// (`FlowProgress::GoalReady`), the agent ends the run with
    /// `AwaitingGoalApproval` and the App sets `goal_ready_flag`; this loop
    /// then awaits the user's approve/revise/cancel answer (routed through
    /// `answer_rx` by the goal-approval form or a typed reply) and resumes on
    /// approve/revise or cancels on cancel. The agent reads its persisted
    /// typed log on each
    /// resume to re-derive the goal and the next shift (plan §7).
    async fn do_flow_run(
        &mut self,
        mut input: AgentInput,
        event_tx: EventSender,
        run_id: u64,
    ) -> Result<(), AgentError> {
        let workstream_id = input
            .mode
            .flow_workstream()
            .map(str::to_owned)
            .ok_or_else(|| AgentError::Tool {
                tool: "flow".into(),
                message: "flow mode without a workstream id".into(),
            })?;
        let cwd = self.vars.apply("{cwd}").into_owned();
        let project_id = craft_storage::flow::project_id(std::path::Path::new(&cwd));

        let (cancel_trigger, cancel_token) = CancelToken::new();
        self.set_cancel_trigger(run_id, cancel_trigger);

        if cancel_token.is_cancelled() {
            let result = Self::cancel_flow(&self.flow_progress_tx, &event_tx);
            self.clear_cancel_trigger(run_id);
            return result;
        }

        // Reset any stale goal-ready signal from a prior run.
        self.goal_ready_flag
            .store(false, std::sync::atomic::Ordering::SeqCst);

        loop {
            let prompt_slots = self.lua_handle.collect_prompt_slots_async().await;
            let slot = self.model_slot.load();
            let compact = self
                .config
                .small_model
                .should_activate(slot.model.context_window)
                && self.config.small_model.compact_prompt;
            let system = agent::build_system_prompt(
                &self.vars,
                &input.mode,
                &self.instructions.text,
                &prompt_slots,
                &slot.model,
                compact,
            );
            // Re-open the typed log each iteration: it reloads the persisted
            // log from disk, so a resume after the goal gate re-derives the
            // goal and the next shift from the prior Tpm write.
            let (state, _progress_rx, _state_cancel_trigger) = craft_agent::FlowRunState::split(
                Arc::clone(&self.flow_store),
                project_id.clone(),
                workstream_id.clone(),
            );

            while self.answer_rx.lock().await.try_recv().is_ok() {}

            let agent = Agent::new(
                AgentParams {
                    provider: Arc::clone(&slot.provider),
                    model: slot.model.clone(),
                    config: self.config.clone(),
                    tool_output_lines: self.tool_output_lines,
                    permissions: Arc::clone(&self.permissions),
                    session_id: self.session_id.clone(),
                    mailbox: self.mailbox.clone(),
                    timeouts: self.timeouts,
                    file_tracker: Arc::clone(&self.file_tracker),
                    prompt_slots: std::sync::Arc::new(prompt_slots),
                    subagent_cancels: Arc::clone(&self.subagent_cancels),
                    registry: Arc::clone(craft_agent::tools::ToolRegistry::native_arc()),
                    compression: self.compression.clone(),
                    model_policy: Arc::clone(&self.model_policy),
                    findings_store: Some(Arc::clone(&self.findings_store)),
                    fs: Arc::new(craft_agent::tools::LocalFs),
                    doom: Arc::clone(&self.doom),
                    flow_thread_history: Some(state.thread_history),
                    flow_thread_manager: Some(state.thread_manager),
                    flow_advisor: Some(state.advisor),
                    flow_progress_tx: Some(self.flow_progress_tx.clone()),
                },
                AgentRunParams {
                    history: &mut self.history,
                    system,
                    event_tx: event_tx.clone(),
                    tools: self.tools.clone(),
                    promoted: self.promoted.clone(),
                    tool_build: Some(craft_agent::tools::ToolBuild {
                        vars: self.vars.clone(),
                        excluded: Vec::new(),
                        mcp: self.mcp_handle.clone(),
                    }),
                    hooks: Some(craft_lua::LuaHooks::new(self.lua_handle.clone())
                        as Arc<dyn craft_agent::Hooks>),
                },
            )
            .with_loaded_instructions(self.instructions.loaded.clone())
            .with_user_response_rx(Arc::clone(&self.answer_rx))
            .with_cancel(cancel_token.clone())
            .with_mcp(self.mcp_handle.clone())
            .with_repo_map(
                self.repomap_enabled
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .then(craft_repomap::RepoMap::try_from_cwd)
                    .flatten()
                    .map(|rm| rm.with_max_tokens(self.config.repomap.max_tokens)),
            )
            .with_recency_source(Some(Arc::new(craft_lua::LuaRecencySource::new(
                self.lua_handle.clone(),
            ))
                as Arc<dyn craft_agent::prompt::RecencySource>));

            let cancelled = tokio::select! {
                biased;
                _ = cancel_token.cancelled() => true,
                r = agent.run(input.clone()) => match r {
                    Ok(()) => false,
                    Err(AgentError::Cancelled) => true,
                    Err(e) => {
                        let _ = event_tx.send(AgentEvent::Error {
                            message: e.user_message(),
                        });
                        self.clear_cancel_trigger(run_id);
                        return Ok(());
                    }
                },
            };

            if cancelled {
                let result = Self::cancel_flow(&self.flow_progress_tx, &event_tx);
                self.clear_cancel_trigger(run_id);
                return result;
            }

            // Goal gate: if GoalReady fired, await the user's answer and loop.
            if self
                .goal_ready_flag
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                let answer = self.answer_rx.lock().await.recv_async().await;
                match answer {
                    Ok(a) if a == craft_agent::FLOW_CANCEL_ANSWER => {
                        let result = Self::cancel_flow(&self.flow_progress_tx, &event_tx);
                        self.clear_cancel_trigger(run_id);
                        return result;
                    }
                    Ok(answer_text) => {
                        // Approve/revise: re-enter as `plan` (the gate's
                        // target) with the answer text as the resume message.
                        // Seeding `plan` instead of `general` keeps the
                        // pipeline on track: the stage brief tells the model
                        // to write the plan, and `TurnTypeEntered(plan)`
                        // updates the host's stage display.
                        input = input.with_flow_resume(answer_text, TurnType::Plan);
                        continue;
                    }
                    Err(_) => {
                        // Channel closed without an answer — treat as cancel.
                        let result = Self::cancel_flow(&self.flow_progress_tx, &event_tx);
                        self.clear_cancel_trigger(run_id);
                        return result;
                    }
                }
            }

            // No goal gate: run completed normally.
            self.clear_cancel_trigger(run_id);
            return Ok(());
        }
    }

    fn cancel_flow(
        flow_progress_tx: &flume::Sender<craft_agent::FlowProgress>,
        event_tx: &EventSender,
    ) -> Result<(), AgentError> {
        let _ = flow_progress_tx.send(craft_agent::FlowProgress::Cancelled);
        // Emit a clean `Done { Cancelled }` rather than `Error`. A user
        // initiated the cancel, so the chat should settle to idle, not a
        // lingering error status. `FlowProgress::Cancelled` already flashed
        // the cancel message.
        let _ = event_tx.send(AgentEvent::Done {
            usage: TokenUsage::default(),
            num_turns: 0,
            stop_reason: Some(StopReason::Cancelled),
        });
        Ok(())
    }

    fn rebuild_tools(&mut self, model: &Model) {
        let tool_build = craft_agent::tools::ToolBuild {
            vars: self.vars.clone(),
            excluded: Vec::new(),
            mcp: self.mcp_handle.clone(),
        };
        let dynamic = craft_agent::tools::DynamicContext::from_config(&self.config);
        self.tools = craft_agent::tools::build_active_tools(
            &tool_build,
            model,
            &self.config,
            &dynamic,
            &self.promoted,
        );
    }

    async fn reload_instructions(&mut self) {
        let cwd = self.vars.apply("{cwd}").into_owned();
        self.instructions = tokio::task::spawn_blocking(move || agent::load_instructions(&cwd))
            .await
            .unwrap_or_else(|e| {
                error!(error = %e, "spawn_blocking panicked");
                Instructions::default()
            });
    }

    fn set_cancel_trigger(&self, run_id: u64, trigger: CancelTrigger) {
        // One trigger per run, and `clear_cancel_trigger` drops the whole
        // key, so the slot is not worth carrying around.
        let _ = self.cancel_map.insert(run_id, trigger);
    }

    fn clear_cancel_trigger(&self, run_id: u64) {
        self.cancel_map.remove(&run_id);
    }

    fn publish_btw_system(&self, slots: &craft_agent::prompt::ResolvedSlots) {
        let slot = self.model_slot.load();
        let system = agent::build_system_prompt(
            &self.vars,
            &AgentMode::Build,
            &self.instructions.text,
            slots,
            &slot.model,
            false,
        );
        self.btw_system.store(Arc::new(system));
    }

    fn emit_error(&self, run_id: u64, error: AgentError) {
        let event_tx = EventSender::new(self.agent_tx.clone(), run_id);
        match error {
            AgentError::Cancelled => {
                let _ = event_tx.send(AgentEvent::Done {
                    usage: TokenUsage::default(),
                    num_turns: 0,
                    stop_reason: Some(StopReason::Cancelled),
                });
            }
            e => {
                error!(error = %e, "agent error");
                let _ = event_tx.send(AgentEvent::Error {
                    message: e.user_message(),
                });
            }
        }
    }
}

fn spawn_oauth_for_needs_auth(handle: &McpHandle) {
    let snapshot = handle.reader().load().clone();
    for info in snapshot.infos.iter() {
        let McpServerStatus::NeedsAuth { ref url } = info.status else {
            continue;
        };
        let Some(ref server_url) = info.url else {
            continue;
        };
        let handle = handle.clone();
        let server_name = info.name.clone();
        let server_url = server_url.clone();
        let www_auth = url.clone();
        let oauth = info.oauth.clone();
        tokio::spawn(async move {
            let storage = match craft_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            if let Err(e) = craft_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
                craft_agent::mcp::oauth::Interaction::Background,
                oauth,
            )
            .await
            {
                tracing::warn!(server = %server_name, error = %e, "background OAuth failed");
                return;
            }
            handle.send(McpCommand::Reconnect {
                server: server_name.clone(),
            });
            tracing::info!(server = %server_name, "MCP server authenticated via OAuth");
        });
    }
}
