use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};

use craft_providers::provider::Provider;
use craft_providers::{Message, Model, RequestOptions, StopReason, StreamResponse, TokenUsage};

use super::doom::SharedDoomTracker;
use super::escalation::EscalationTracker;
use super::format::Formatter;
use super::guardrails::ToolGuardrails;
use super::history::{History, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::memory_extraction;
use super::snapshot::SnapshotManager;
use super::trust::TrustTracker;
use super::validation::Validator;
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpHandle;
use crate::permissions::PermissionManager;
use crate::tools::FileReadTracker;
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, DoneReason, EventSender,
    InterruptSource, SessionMailbox,
};
use craft_config::{ModelPolicy, ToolOutputLines};

pub(super) mod compaction;
pub(super) mod doom_state;
pub(super) mod flow;
pub(super) mod io;
pub(super) mod recency;
#[cfg(test)]
pub(super) mod test_support;
pub(super) mod tools;
pub(super) mod turn;

use compaction::AgentCompaction;
use doom_state::AgentDoom;
use flow::AgentFlow;
use io::AgentIo;
use recency::AgentRecency;
use tools::AgentTools;

pub use compaction::{estimate_message_tokens, resolve_compaction_model};

const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";
const HOOK_BEST_EFFORT_TIMEOUT: Duration = Duration::from_secs(5);
const GRACE_CALL_PROMPT: &str = "Your recent actions look like a doom-loop (repeated calls, errors, or stagnation). Summarize your progress so far and tell the user what still needs to be done. Do NOT call any tools.";

pub(super) enum TurnOutcome {
    Continue,
    Done(Option<StopReason>),
    Overflow,
    /// A Flow narrow turn type finished its work (EndTurn, no tool calls) but
    /// is not `general`. The loop shifts back to `general` and resumes.
    ShiftOut,
}

pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<craft_storage::id::SessionRef>,
    pub mailbox: Option<SessionMailbox>,
    pub timeouts: craft_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub registry: Arc<crate::tools::ToolRegistry>,
    pub compression: craft_config::CompressionConfig,
    pub model_policy: Arc<ModelPolicy>,
    pub findings_store: Option<super::findings_store::SharedFindingsStore>,
    pub fs: Arc<dyn crate::tools::FsBackend>,
    pub doom: SharedDoomTracker,
    /// Flow mode only: the per-workstream typed log. `None` in Build/Plan.
    pub flow_thread_history: Option<Arc<std::sync::Mutex<super::typed_log::ThreadHistory>>>,
    /// Flow mode only: the thread-tree manager. `None` in Build/Plan.
    pub flow_thread_manager: Option<Arc<std::sync::Mutex<super::threads::ThreadManager>>>,
    /// Flow mode only: the between-turn tree-watching advisor with override
    /// power. `None` in Build/Plan and when the advisor is disabled.
    pub flow_advisor: Option<Arc<dyn super::flow_loop::FlowAdvisor + Send + Sync>>,
    /// Flow mode only: channel for emitting `FlowProgress` events from inside
    /// `run_loop`/`turn`. `None` in Build/Plan.
    pub flow_progress_tx: Option<flume::Sender<super::flow_loop::FlowProgress>>,
}

pub struct AgentRunParams<'h> {
    pub history: &'h mut History,
    pub system: String,
    pub event_tx: EventSender,
    pub tools: Value,
    pub promoted: crate::tools::PromotedTools,
    pub tool_build: Option<crate::tools::ToolBuild>,
    pub hooks: Option<Arc<dyn crate::Hooks>>,
}

pub struct Agent<'h> {
    history: &'h mut History,
    system: String,
    tools: Value,
    loaded_instructions: LoadedInstructions,
    config: AgentConfig,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    findings_store: Option<super::findings_store::SharedFindingsStore>,
    total_usage: TokenUsage,
    context_size: u32,
    num_turns: u32,
    io: AgentIo,
    tool_state: AgentTools,
    compaction: AgentCompaction,
    doom: AgentDoom,
    model_policy: Arc<ModelPolicy>,
    flow: AgentFlow,
    recency: AgentRecency,
}

impl<'h> Agent<'h> {
    pub fn new(params: AgentParams, run: AgentRunParams<'h>) -> Self {
        let dynamic = crate::tools::DynamicContext::from_config(&params.config);
        let advisor_enabled = params.config.advisor.enabled;
        let advisor_state = advisor_enabled
            .then(|| super::advisor::AdvisorState::with_dedup(params.config.advisor.dedup_size));
        let ttsr = params
            .config
            .ttsr
            .enabled
            .then(|| {
                let m = super::ttsr::TtsrManager::load_from_discovery();
                m.enabled().then(|| Arc::new(m))
            })
            .flatten();
        Self {
            history: run.history,
            system: run.system,
            tools: run.tools,
            loaded_instructions: LoadedInstructions::new(),
            config: params.config,
            prompt_slots: params.prompt_slots,
            findings_store: params.findings_store,
            total_usage: TokenUsage::default(),
            context_size: 0,
            num_turns: 0,
            io: AgentIo {
                provider: params.provider,
                model: Arc::new(params.model),
                opts: RequestOptions::default(),
                timeouts: params.timeouts,
                fallback_chain: Vec::new(),
                event_tx: run.event_tx,
                cancel: CancelToken::none(),
                mailbox: params.mailbox,
                interrupt_source: None,
                user_response_rx: None,
                session_id: params.session_id,
                reauth_attempts: 0,
            },
            tool_state: AgentTools {
                permissions: params.permissions,
                registry: params.registry,
                tool_build: run.tool_build,
                hooks: run.hooks,
                fs: params.fs,
                snapshot_store: crate::tools::safety::SnapshotStore::fresh(),
                pending_edits: crate::tools::ast_edit::PendingEditStore::fresh(),
                promoted: run.promoted,
                dynamic,
                mcp: None,
                dedup_cache: super::dedup::ToolDedupCache::new(),
                snapshot: SnapshotManager::new(std::env::current_dir().unwrap_or_default()),
                validator: Validator::new(
                    std::env::current_dir().unwrap_or_default(),
                    craft_config::ValidationConfig::default(),
                ),
                formatter: Formatter::new(
                    std::env::current_dir().unwrap_or_default(),
                    craft_config::FormatConfig::default(),
                ),
                file_tracker: params.file_tracker,
                tool_output_lines: params.tool_output_lines,
                host_question_routing: false,
                subagent_cancels: params.subagent_cancels,
                guardrails: ToolGuardrails::new(),
                trust_tracker: TrustTracker::new(craft_config::TrustDecayConfig::default()),
            },
            compaction: AgentCompaction {
                auto_compact: super::compaction::auto_compact_enabled(),
                compression: params.compression.clone(),
                cache_tracker: super::cache::PrefixCacheTracker::new(),
                compression_store: super::compression_store::shared_store(),
                token_estimation_multiplier: 1.0,
                last_relevance_scores: None,
                ineffective_compaction_count: 0,
                rollback_len: 0,
            },
            model_policy: params.model_policy,
            doom: AgentDoom {
                doom: params.doom,
                escalation: EscalationTracker::new(Default::default()),
            },
            flow: AgentFlow {
                flow_search: None,
                thread_id: super::typed_log::ThreadId::new(""),
                thread_history: params.flow_thread_history,
                thread_manager: params.flow_thread_manager,
                flow_advisor: params.flow_advisor,
                flow_progress_tx: params.flow_progress_tx,
                goal: None,
                goal_criteria: Vec::new(),
                judge_continuations: 0,
                advisor_continuations: 0,
                advisor_state,
                ttsr,
                mode: AgentMode::default(),
                turn_type: crate::agent::turn_type::TurnType::General,
                pending_approval: false,
            },
            recency: AgentRecency {
                scorer: Some(super::semantic::RelevanceScorer::new()),
                recency_source: None,
                repo_map: None,
            },
        }
    }

    pub fn with_mcp(mut self, mcp: Option<McpHandle>) -> Self {
        self.tool_state.trust_tracker = TrustTracker::new(self.config.trust_decay);
        self.tool_state.validator = Validator::new(
            std::env::current_dir().unwrap_or_default(),
            self.config.validation.clone(),
        );
        self.tool_state.formatter = Formatter::new(
            std::env::current_dir().unwrap_or_default(),
            self.config.format.clone(),
        );
        self.tool_state.mcp = mcp;
        self
    }

    pub fn with_user_response_rx(
        mut self,
        rx: Arc<tokio::sync::Mutex<flume::Receiver<String>>>,
    ) -> Self {
        self.io.user_response_rx = Some(rx);
        self
    }

    /// Route `question` tool calls to the host over the event channel
    /// instead of the registry entry.
    pub fn with_host_question_routing(mut self, enabled: bool) -> Self {
        self.tool_state.host_question_routing = enabled;
        self
    }

    pub fn with_interrupt_source(mut self, source: Arc<dyn InterruptSource>) -> Self {
        self.io.interrupt_source = Some(source);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.io.cancel = cancel;
        self
    }

    pub fn with_fallback_chain(mut self, chain: Vec<craft_providers::roles::ChainHop>) -> Self {
        self.io.fallback_chain = chain;
        self
    }

    pub fn with_loaded_instructions(mut self, loaded: LoadedInstructions) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    pub fn with_flow_search(
        mut self,
        flow_search: crate::tools::flow_search::FlowSearchHandle,
    ) -> Self {
        self.flow.flow_search = flow_search;
        self
    }

    /// Flow mode only: override the root thread id with a child `ThreadId`.
    /// Used by the `task` tool's Flow integration so a child agent runs its
    /// shift-enabled loop against its own thread.
    pub fn with_flow_thread_id(mut self, id: super::typed_log::ThreadId) -> Self {
        self.flow.thread_id = id;
        self
    }

    pub fn with_repo_map(mut self, repo_map: Option<craft_repomap::RepoMap>) -> Self {
        self.recency.repo_map = repo_map;
        self
    }

    /// Provide a per-turn volatile-facts source. When set, the agent rebuilds
    /// a [`crate::prompt::RecencyFacts`] every turn and appends its rendered
    /// tail to the latest user message at request-build time.
    pub fn with_recency_source(
        mut self,
        source: Option<Arc<dyn crate::prompt::RecencySource>>,
    ) -> Self {
        self.recency.recency_source = source;
        self
    }

    /// Cancellation is an ending, not a failure: it comes back as
    /// `Ok(DoneReason::Cancelled)` so callers only report real errors.
    pub async fn run(mut self, mut input: AgentInput) -> Result<DoneReason, AgentError> {
        compaction::strip_trailing_grace_prompt(self.history, GRACE_CALL_PROMPT);
        self.doom
            .doom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset_for_new_user_input();
        self.compaction.rollback_len = self.history.len();
        let message = input.message.clone();
        let images = input.images.clone();
        let mode = input.mode.clone();
        let preamble = std::mem::take(&mut input.preamble);
        let thinking = input.thinking;
        let fast = input.fast;
        let workflow_mode = mode.clone();
        self.push_input_context(preamble);
        if !message.trim().is_empty() || !images.is_empty() {
            self.history
                .push(Message::user_with_images(message.clone(), images));
        }
        self.flow.mode = mode;
        self.flow.turn_type = crate::agent::turn_type::TurnType::General;
        if !matches!(self.flow.mode, AgentMode::Flow(_)) {
            self.tools = crate::tools::strip_flow_only_tools(&self.tools);
        } else {
            // Flow mode: set the current thread id to the root thread so
            // typed-log appends target the root. A child agent created via
            // `with_flow_thread_id` keeps its child thread id.
            if self.flow.thread_id.as_str().is_empty() {
                if let Some(mgr) = self.flow.thread_manager.as_ref() {
                    let root = mgr.lock().unwrap_or_else(|e| e.into_inner()).root.clone();
                    self.flow.thread_id = root;
                } else if let Some(hist) = self.flow.thread_history.as_ref() {
                    self.flow.thread_id = hist
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .root_thread_id()
                        .clone();
                }
            }
        }
        // Flow goal-approval resume: re-enter as the gate's target type.
        // `advance_turn_type` advances the ThreadManager, emits
        // `TurnTypeEntered`, and pushes the stage brief. No-op outside Flow
        // mode or when no resume stage is requested.
        if matches!(self.flow.mode, AgentMode::Flow(_))
            && let Some(target) = input.flow_resume_stage
        {
            self.advance_turn_type(target, crate::agent::turn_type::ThreadAction::Advance);
        }
        self.flow.goal = input.goal;
        self.flow.goal_criteria = input.goal_criteria.clone();
        self.io.opts = RequestOptions { thinking, fast };

        info!(
            model = %self.io.model.id,
            mode = ?workflow_mode,
            message_len = message.len(),
            "agent run started"
        );

        if self.config.hooks_enabled
            && let Some(hooks) = &self.tool_state.hooks
            && tokio::time::timeout(HOOK_BEST_EFFORT_TIMEOUT, hooks.session_start())
                .await
                .is_err()
        {
            warn!("session_start hook timed out");
        }

        let result = self.run_loop().await;

        let reason = match result {
            Ok(reason) => reason,
            Err(AgentError::Cancelled) => {
                sanitize_cancelled_history(self.history, self.compaction.rollback_len);
                DoneReason::Cancelled
            }
            Err(e) => return Err(e),
        };
        self.emit_done(reason)?;

        Ok(reason)
    }

    fn push_input_context(&mut self, preamble: Vec<Message>) {
        for message in preamble {
            self.history.push(message);
        }
        if let Some(mailbox) = &self.io.mailbox {
            for message in mailbox.drain() {
                self.history.push(message);
            }
        }
    }

    async fn run_loop(&mut self) -> Result<DoneReason, AgentError> {
        loop {
            if let Some(max) = self.config.max_turns
                && self.num_turns >= max
            {
                return Ok(DoneReason::MaxTurns);
            }
            let _spec = self.flow.turn_type.spec();
            if matches!(self.flow.mode, AgentMode::Flow(_)) && self.io.cancel.is_cancelled() {
                if let Some(tx) = self.flow.flow_progress_tx.as_ref() {
                    let _ = tx.send(super::flow_loop::FlowProgress::Cancelled);
                }
                self.tool_state.snapshot.commit();
                return Ok(DoneReason::Cancelled);
            }
            let (should_grace, should_hard_stop) = {
                let d = self.doom.doom.lock().unwrap_or_else(|e| e.into_inner());
                (d.should_grace(), d.should_hard_stop())
            };
            if should_hard_stop {
                let score = self
                    .doom
                    .doom
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .score();
                info!(
                    score,
                    turns = self.num_turns,
                    "doom hard-stop reached, ending run"
                );
                self.tool_state.snapshot.commit();
                return Ok(DoneReason::EndTurn);
            }
            if should_grace {
                self.doom
                    .doom
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mark_grace_called();
                self.history
                    .push(Message::user(GRACE_CALL_PROMPT.to_string()));
                let score = self
                    .doom
                    .doom
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .score();
                info!(
                    score,
                    turns = self.num_turns,
                    "doom grace threshold reached, issuing grace call"
                );
            }
            match self.turn().await? {
                TurnOutcome::Continue => {
                    self.apply_shift_if_requested().await?;
                    if self.flow.pending_approval {
                        self.tool_state.snapshot.commit();
                        return Ok(DoneReason::AwaitingGoalApproval);
                    }
                }
                TurnOutcome::Done(stop_reason) => {
                    let note = match self.run_advisor().await {
                        Ok(note) => note,
                        // The user cancelled while the advisor review was in
                        // flight; end the run cancelled like a stream cancel.
                        Err(_) => {
                            if let Some(tx) = self.flow.flow_progress_tx.as_ref() {
                                let _ = tx.send(super::flow_loop::FlowProgress::Cancelled);
                            }
                            self.tool_state.snapshot.commit();
                            return Ok(DoneReason::Cancelled);
                        }
                    };
                    match flow::advisor_turn_action(
                        note,
                        &self.config.advisor,
                        self.flow.pending_approval,
                        self.flow.advisor_continuations,
                    ) {
                        flow::AdvisorTurnAction::Continue(note) => {
                            self.flow.advisor_continuations += 1;
                            let _ = self.io.event_tx.send(AgentEvent::Info {
                                message: flow::advisor_continuation_info(
                                    &note,
                                    self.flow.advisor_continuations,
                                    self.config.advisor.max_act_turns,
                                ),
                            });
                            self.history.push(flow::advisor_followup_message(&note));
                            continue;
                        }
                        flow::AdvisorTurnAction::Stop => {}
                    }
                    if matches!(self.flow.mode, AgentMode::Flow(_)) {
                        self.commit_turn_write(self.flow.turn_type);
                    }
                    let reason = if self.flow.pending_approval {
                        DoneReason::AwaitingGoalApproval
                    } else {
                        stop_reason.into()
                    };

                    if let Some(ctx) = self.memory_extraction_ctx() {
                        tokio::spawn(async move {
                            memory_extraction::extract_and_store(ctx).await;
                        });
                    }

                    return Ok(reason);
                }
                TurnOutcome::ShiftOut => {
                    // A Flow narrow turn finished without shifting. Commit its
                    // write, hand control back to `general`, and resume.
                    self.commit_turn_write(self.flow.turn_type);
                    self.advance_turn_type(
                        crate::agent::turn_type::TurnType::General,
                        crate::agent::turn_type::ThreadAction::Advance,
                    );
                    self.history
                        .push(Message::synthetic(flow::SHIFT_OUT_TO_GENERAL_PROMPT.into()));
                }
                TurnOutcome::Overflow => {
                    info!("context overflow detected, attempting auto-compact and retry");
                    let usage = TokenUsage {
                        input: self.context_size,
                        ..Default::default()
                    };
                    self.try_auto_compact(&usage, true).await?;
                }
            }
        }
    }

    fn emit_turn_complete(&self, response: &StreamResponse) -> Result<(), AgentError> {
        self.io.event_tx.send(AgentEvent::TurnComplete(Box::new(
            crate::TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.io.model.id.clone(),
                cost: self
                    .io
                    .model
                    .billed_cost(&response.usage, self.io.opts.fast),
                context_size: Some(response.usage.context_tokens()),
                context_window: self.io.model.context_window,
            },
        )))
    }

    fn emit_done(&self, reason: DoneReason) -> Result<(), AgentError> {
        info!(
            self.num_turns,
            total_input = self.total_usage.input,
            total_output = self.total_usage.output,
            %reason,
            "agent run completed"
        );
        self.io.event_tx.send(AgentEvent::Done {
            usage: self.total_usage,
            num_turns: self.num_turns,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::ExtractedCommand;
    use crate::agent::history::History;
    use craft_providers::{Message, Role, StopReason};

    #[tokio::test]
    async fn run_ingests_preamble_then_mailbox_then_user_message() {
        let id = craft_storage::id::CraftId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "mailbox".into(), false).unwrap();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        params.mailbox = Some(mailbox);
        let agent = Agent::new(params, run_params);
        let mut input = default_input();
        input.preamble = vec![Message::observation("preamble".into())];

        agent.run(input).await.unwrap();

        assert_eq!(history.as_slice()[0].user_text(), Some("preamble"));
        assert_eq!(history.as_slice()[1].user_text(), Some("mailbox"));
        assert_eq!(history.as_slice()[2].user_text(), Some("hello"));
    }

    #[tokio::test]
    async fn queued_input_drains_preamble_and_mailbox() {
        let id = craft_storage::id::CraftId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "mailbox".into(), false).unwrap();
        let mut input = default_input();
        input.preamble = vec![Message::observation("preamble".into())];
        let source = MockInterruptSource::new(vec![ExtractedCommand::Interrupt(input, 0)]);
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.mailbox = Some(mailbox);
        let mut agent = Agent::new(params, run_params).with_interrupt_source(source);

        assert!(agent.handle_queued_command().await.unwrap());

        let text = history
            .as_slice()
            .iter()
            .map(Message::user_text)
            .collect::<Vec<_>>();
        assert_eq!(text, [Some("preamble"), Some("mailbox"), Some("hello")]);
        assert!(history.as_slice()[0].is_observation());
        assert!(history.as_slice()[1].is_observation());
    }

    #[tokio::test]
    async fn wake_only_run_does_not_insert_an_empty_user_turn() {
        let id = craft_storage::id::CraftId::generate();
        let mailbox = SessionMailbox::register(id);
        SessionMailbox::notify(id, "failed".into(), true).unwrap();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        params.mailbox = Some(mailbox);
        let agent = Agent::new(params, run_params);
        let mut input = default_input();
        input.message.clear();

        agent.run(input).await.unwrap();

        assert_eq!(history.as_slice().len(), 2);
        assert!(history.as_slice()[0].is_observation());
        assert!(matches!(history.as_slice()[1].role, Role::Assistant));
    }

    #[tokio::test]
    async fn memory_extraction_gated_off_in_tests_even_with_flag_on() {
        let mut history = History::new(Vec::new());
        history.push(Message::user("we're rebranding to acme".into()));
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.config.memory_extraction = true;
        let agent = Agent::new(params, run_params);

        assert!(agent.config.memory_extraction);
        assert!(agent.memory_extraction_ctx().is_none());
    }

    #[tokio::test]
    async fn memory_extraction_ctx_requires_user_message() {
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let params = make_agent_params();
        let agent = Agent::new(params, run_params);
        assert!(agent.memory_extraction_ctx().is_none());
    }
}
