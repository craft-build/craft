use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use craft_agent::agent;
use craft_agent::mcp::McpHandle;
use craft_agent::mcp::config::McpServerStatus;
use craft_agent::permissions::PermissionManager;
use craft_agent::template;
use craft_agent::template::Vars;
use craft_agent::tools::{FileReadTracker, FlowRunnerEnv};
use craft_agent::{
    Agent, AgentConfig, AgentEvent, AgentInput, AgentMode, AgentParams, AgentRunParams, CancelMap,
    CancelToken, CancelTrigger, DoomTracker, Envelope, EventSender, FindingsStore, History,
    Instructions, McpCommand, PromptRole, SharedDoomTracker, SharedFindingsStore, ToolOutputLines,
};
use craft_flow::{ApprovalPayload, FlowOutcome, FlowParams, FlowProgress, TaskStageRunner};
use craft_lua::EventHandle;
use craft_providers::{AgentError, Message, Model, StopReason, TokenUsage};
use craft_storage::flow::FlowStore;
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
    session_id: Option<String>,
    timeouts: craft_providers::Timeouts,
    lua_handle: Option<EventHandle>,
    btw_system: Arc<ArcSwap<String>>,
    compression: craft_config::CompressionConfig,
    doom: SharedDoomTracker,
    subagent_cancels: Arc<CancelMap<String>>,
    flow_store: Arc<FlowStore>,
    flow_progress_tx: flume::Sender<FlowProgress>,
    repomap_enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentLoop {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        model_slot: Arc<ArcSwap<ModelSlot>>,
        config: AgentConfig,
        tool_output_lines: ToolOutputLines,
        initial_history: Vec<Message>,
        shared_history: Arc<ArcSwap<Vec<Message>>>,
        mcp_handle: Option<McpHandle>,
        permissions: Arc<PermissionManager>,
        agent_tx: flume::Sender<Envelope>,
        answer_rx: flume::Receiver<String>,
        queue: Arc<QueueReceiver>,
        cancel_map: Arc<RunCancelMap>,
        init_cancel: CancelToken,
        session_id: Option<String>,
        timeouts: craft_providers::Timeouts,
        lua_handle: Option<EventHandle>,
        btw_system: Arc<ArcSwap<String>>,
        compression: craft_config::CompressionConfig,
        subagent_cancels: Arc<CancelMap<String>>,
        flow_store: Arc<FlowStore>,
        flow_progress_tx: flume::Sender<FlowProgress>,
        repomap_enabled: Arc<std::sync::atomic::AtomicBool>,
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
            timeouts,
            lua_handle,
            btw_system,
            compression,
            doom: Arc::new(Mutex::new(DoomTracker::new())),
            subagent_cancels,
            flow_store,
            flow_progress_tx,
            repomap_enabled,
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
            spawn_oauth_for_needs_auth(mcp);
        }
        !self.init_cancel.is_cancelled()
    }

    async fn do_compact(&mut self, event_tx: &EventSender) -> Result<(), AgentError> {
        let slot = self.model_slot.load();
        let (provider, model) =
            agent::resolve_compaction_model(&slot.provider, &slot.model, self.timeouts).await;
        agent::compact(&*provider, &model, &mut self.history, event_tx).await
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

        let prompt_slots = match self.lua_handle.as_ref() {
            Some(h) => h.collect_prompt_slots_async().await,
            None => craft_agent::prompt::ResolvedSlots::default(),
        };
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
                timeouts: self.timeouts,
                file_tracker: Arc::clone(&self.file_tracker),
                prompt_slots: std::sync::Arc::new(prompt_slots),
                subagent_cancels: Arc::clone(&self.subagent_cancels),
                registry: Arc::clone(craft_agent::tools::ToolRegistry::native_arc()),
                compression: self.compression.clone(),
                findings_store: Some(Arc::clone(&self.findings_store)),
                fs: Arc::new(craft_agent::tools::LocalFs),
                doom: Arc::clone(&self.doom),
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
                hooks: self
                    .lua_handle
                    .as_ref()
                    .map(|h| craft_lua::LuaHooks::new(h.clone()) as Arc<dyn craft_agent::Hooks>),
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
        );

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

    /// Drive the Flow pipeline (`craft_flow::run`) instead of a single agent
    /// turn. Stage subagents are launched by `TaskStageRunner` against the live
    /// provider/model, and each stage's document persists to the `FlowStore` so
    /// resume works and nothing leaks to scratch paths. The goal-approval gate
    /// blocks on the user's answer (the same channel the `question` tool uses),
    /// then resumes with an `ApprovalPayload`. Progress events flow to the TUI
    /// through `flow_progress_tx` so the FlowPanel reflects live state.
    async fn do_flow_run(
        &mut self,
        input: AgentInput,
        event_tx: EventSender,
        run_id: u64,
    ) -> Result<(), AgentError> {
        let resume = input.flow_resume;
        let slot = self.model_slot.load();
        let workstream_id = input
            .mode
            .flow_workstream()
            .map(str::to_owned)
            .ok_or_else(|| AgentError::Tool {
                tool: "flow".into(),
                message: "flow mode without a workstream id".into(),
            })?;
        let prompt_slots = match self.lua_handle.as_ref() {
            Some(h) => h.collect_prompt_slots_async().await,
            None => craft_agent::prompt::ResolvedSlots::default(),
        };
        let cwd = self.vars.apply("{cwd}").into_owned();
        let project_id = craft_flow::project_id(std::path::Path::new(&cwd));
        // Build the embedder once for the whole run: it feeds both the
        // pipeline's reindex step (`params.embedder`) and the `flow_search`
        // backend injected into each stage subagent's ToolContext.
        let embedder: Arc<dyn craft_flow::search::Embedder> = Arc::new(
            craft_flow::search::OnnxEmbedder::new(craft_agent::EmbeddingService::new()),
        );
        let flow_search: craft_agent::tools::flow_search::FlowSearchHandle =
            Some(Arc::new(craft_flow::search::FlowSearchBackendImpl::new(
                Arc::clone(&self.flow_store),
                Arc::clone(&embedder),
                &project_id,
                &workstream_id,
            )));
        let env = Arc::new(FlowRunnerEnv {
            provider: Arc::clone(&slot.provider),
            model: Arc::new(slot.model.clone()),
            config: self.config.clone(),
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            compression: self.compression.clone(),
            prompt_slots: Arc::new(prompt_slots),
            event_tx: event_tx.clone(),
            flow_search,
        });

        let (cancel_trigger, cancel_token) = CancelToken::new();
        self.set_cancel_trigger(run_id, cancel_trigger);

        let mut approval: Option<ApprovalPayload> = None;
        let result = loop {
            if cancel_token.is_cancelled() {
                break Self::cancel_flow(&self.flow_progress_tx, &event_tx);
            }
            let mut params = FlowParams::new(
                project_id.clone(),
                workstream_id.clone(),
                input.message.clone(),
                self.config.flow.clone(),
                Arc::clone(&self.flow_store),
            );
            params.approval = approval.take();
            params.runner = Some(Arc::new(TaskStageRunner::new(
                Arc::clone(&env),
                workstream_id.clone(),
            )));
            params.progress = Some(self.flow_progress_tx.clone());
            params.embedder = Some(Arc::clone(&embedder));
            params.resume = resume;

            let outcome = tokio::select! {
                biased;
                _ = cancel_token.cancelled() => FlowOutcome::Cancelled,
                o = craft_flow::run(params) => o,
            };
            match outcome {
                FlowOutcome::Cancelled => {
                    break Self::cancel_flow(&self.flow_progress_tx, &event_tx);
                }
                FlowOutcome::AwaitingGoalApproval { goal_doc } => {
                    let _ = event_tx.send(AgentEvent::TextDelta {
                        text: format!("## Flow goal\n\n{goal_doc}"),
                    });
                    let _ = self.flow_progress_tx.send(FlowProgress::GoalReady {
                        goal_doc: goal_doc.clone(),
                    });
                    while self.answer_rx.lock().await.try_recv().is_ok() {}
                    let answer = self.answer_rx.lock().await.recv_async().await.ok();
                    approval = match answer.as_deref() {
                        Some(t) if t == craft_flow::FLOW_APPROVE_ANSWER => {
                            Some(ApprovalPayload::Approved)
                        }
                        Some(c) if c == craft_flow::FLOW_CANCEL_ANSWER => {
                            let _ = event_tx.send(AgentEvent::TextDelta {
                                text: "Flow run cancelled at the goal-approval gate.".into(),
                            });
                            let _ = event_tx.send(AgentEvent::Done {
                                usage: TokenUsage::default(),
                                num_turns: 0,
                                stop_reason: Some(StopReason::EndTurn),
                            });
                            break Ok(());
                        }
                        Some(rev) => Some(ApprovalPayload::Revised(rev.to_owned())),
                        None => {
                            let _ = event_tx.send(AgentEvent::Error {
                                message: "flow approval channel closed".into(),
                            });
                            break Ok(());
                        }
                    };
                }
                FlowOutcome::Done {
                    verification_report,
                } => {
                    let _ = event_tx.send(AgentEvent::TextDelta {
                        text: verification_report,
                    });
                    let _ = event_tx.send(AgentEvent::Done {
                        usage: TokenUsage::default(),
                        num_turns: 0,
                        stop_reason: Some(StopReason::EndTurn),
                    });
                    break Ok(());
                }
                FlowOutcome::NeedsReview {
                    verification_report,
                } => {
                    let _ = event_tx.send(AgentEvent::TextDelta {
                        text: format!("## Flow verification needs review\n\n{verification_report}"),
                    });
                    let _ = event_tx.send(AgentEvent::Done {
                        usage: TokenUsage::default(),
                        num_turns: 0,
                        stop_reason: Some(StopReason::EndTurn),
                    });
                    break Ok(());
                }
                FlowOutcome::Failed { stage, reason } => {
                    let _ = event_tx.send(AgentEvent::Error {
                        message: format!("flow {stage:?} failed: {reason}"),
                    });
                    break Ok(());
                }
            }
        };
        self.clear_cancel_trigger(run_id);
        result
    }

    /// Emit the cancellation signal for the flow panel and the chat, returning
    /// the terminal `Ok(())` so the agent loop treats cancellation as a clean
    /// stop rather than an error.
    fn cancel_flow(
        flow_progress_tx: &flume::Sender<FlowProgress>,
        event_tx: &EventSender,
    ) -> Result<(), AgentError> {
        let _ = flow_progress_tx.send(FlowProgress::Cancelled);
        let _ = event_tx.send(AgentEvent::Error {
            message: "flow run cancelled".into(),
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
        self.cancel_map.insert(run_id, trigger);
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
        tokio::spawn(async move {
            let storage = match craft_storage::StateDir::resolve() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "cannot resolve storage for OAuth");
                    return;
                }
            };
            let auth_data = match craft_agent::mcp::oauth::authenticate(
                &server_name,
                &server_url,
                www_auth.as_deref(),
                &storage,
                craft_agent::mcp::oauth::Interaction::Background,
            )
            .await
            {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(server = %server_name, error = %e, "background OAuth failed");
                    return;
                }
            };
            let Some(ref tokens) = auth_data.tokens else {
                return;
            };
            handle.send(McpCommand::Reconnect {
                server: server_name.clone(),
                url: server_url,
                token: tokens.access.clone(),
            });
            tracing::info!(server = %server_name, "MCP server authenticated via OAuth");
        });
    }
}
