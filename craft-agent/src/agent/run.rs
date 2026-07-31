use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{error, info, warn};

use craft_providers::provider::Provider;
use craft_providers::{
    ContentBlock, Message, Model, ModelTier, RequestOptions, Role, StopReason, StreamResponse,
    TokenUsage,
};

use super::compaction::{self, CONTINUE_AFTER_COMPACT};
use super::dedup::ToolDedupCache;
use super::doom::SharedDoomTracker;
use super::escalation::EscalationTracker;
use super::format::Formatter;
use super::guardrails::ToolGuardrails;
use super::history::{History, sanitize_cancelled_history};
use super::instructions::LoadedInstructions;
use super::snapshot::SnapshotManager;
use super::streaming::stream_with_retry;
use super::tool_dispatch::{self, ToolBatchOutcome};
use super::trust::TrustTracker;
use super::validation::Validator;
use crate::cancel::{CancelMap, CancelToken};
use crate::mcp::McpHandle;
use crate::permissions::PermissionManager;
use crate::tools::{Deadline, FileReadTracker, ToolContext};
use crate::{
    AgentConfig, AgentError, AgentEvent, AgentInput, AgentMode, EventSender, ExtractedCommand,
    InterruptSource, TurnCompleteEvent,
};
use craft_config::ToolOutputLines;
use craft_storage::id::SessionRef;

const MAX_REAUTH_ATTEMPTS: u32 = 2;
const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";
const HOOK_BEST_EFFORT_TIMEOUT: Duration = Duration::from_secs(5);
const GRACE_CALL_PROMPT: &str = "Your recent actions look like a doom-loop (repeated calls, errors, or stagnation). Summarize your progress so far and tell the user what still needs to be done. Do NOT call any tools.";
const DEFAULT_SMALL_MODEL_RATIO: f64 = 0.60;
const INEFFECTIVE_COMPACTION_THRESHOLD: f32 = 0.1;
const MANDATORY_RECENT_MESSAGES: usize = 6;
const STAGNATION_WINDOW_SIZE: usize = 5;
const STAGNATION_SIMILARITY_THRESHOLD: f32 = 0.85;
const ADVISOR_FOLLOWUP_PROMPT: &str = "<advisor-note>\nA lightweight advisor reviewed your last turn and flagged a {severity}:\n{note}\n\nAddress this concern before finishing. Do not simply acknowledge it; make the change or explain concretely why it does not apply.\n</advisor-note>";

pub async fn resolve_compaction_model(
    provider: &Arc<dyn Provider>,
    model: &Model,
    timeouts: craft_providers::Timeouts,
) -> (Arc<dyn Provider>, Model) {
    let compact_spec = craft_providers::model_registry::model_registry()
        .read()
        .unwrap()
        .spec_for_tier_any(ModelTier::Compaction);
    if let Some(spec) = compact_spec
        && let Ok(mut m) = Model::from_spec(&spec)
        && let Ok(p) = craft_providers::provider::from_model(&mut m, timeouts).await
    {
        return (Arc::from(p), m);
    }
    (Arc::clone(provider), model.clone())
}

enum TurnOutcome {
    Continue,
    Done(Option<StopReason>),
    Overflow,
    /// A Flow narrow turn type finished its work (EndTurn, no tool calls) but
    /// is not `general`. Instead of ending the run, the loop shifts back to
    /// `general` and resumes — the root owner re-derives the next step. Only
    /// `general` ending without a shift ends a Flow run (via `Done`).
    ShiftOut,
}

pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<SessionRef>,
    pub timeouts: craft_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub registry: Arc<crate::tools::ToolRegistry>,
    pub compression: craft_config::CompressionConfig,
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
    provider: Arc<dyn Provider>,
    model: Arc<Model>,
    history: &'h mut History,
    system: String,
    event_tx: EventSender,
    tools: Value,
    mode: AgentMode,
    turn_type: crate::agent::turn_type::TurnType,
    user_response_rx: Option<Arc<tokio::sync::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    total_usage: TokenUsage,
    context_size: u32,
    num_turns: u32,
    doom: SharedDoomTracker,
    guardrails: ToolGuardrails,
    ineffective_compaction_count: u8,
    auto_compact: bool,
    loaded_instructions: LoadedInstructions,
    rollback_len: usize,
    mcp: Option<McpHandle>,
    config: AgentConfig,
    tool_output_lines: ToolOutputLines,
    reauth_attempts: u32,
    post_tool_empty_retried: bool,
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    session_id: Option<SessionRef>,
    timeouts: craft_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    subagent_cancels: Arc<CancelMap<String>>,
    registry: Arc<crate::tools::ToolRegistry>,
    compression: craft_config::CompressionConfig,
    findings_store: Option<super::findings_store::SharedFindingsStore>,
    cache_tracker: super::cache::PrefixCacheTracker,
    compression_store: super::compression_store::SharedCompressionStore,
    dedup_cache: ToolDedupCache,
    trust_tracker: TrustTracker,
    snapshot: SnapshotManager,
    validator: Validator,
    formatter: Formatter,
    escalation: EscalationTracker,
    promoted: crate::tools::PromotedTools,
    dynamic: crate::tools::DynamicContext,
    tool_build: Option<crate::tools::ToolBuild>,
    hooks: Option<Arc<dyn crate::Hooks>>,
    scorer: Option<super::semantic::RelevanceScorer>,
    last_relevance_scores: Option<Vec<(usize, f32)>>,
    fs: Arc<dyn crate::tools::FsBackend>,
    goal: Option<String>,
    goal_criteria: Vec<String>,
    judge_continuations: u8,
    advisor_continuations: u32,
    snapshot_store: Arc<crate::tools::safety::SnapshotStore>,
    pending_edits: Arc<crate::tools::ast_edit::PendingEditStore>,
    fallback_chain: Vec<craft_providers::roles::ChainHop>,
    advisor_state: Option<super::advisor::AdvisorState>,
    ttsr: Option<Arc<super::ttsr::TtsrManager>>,
    flow_search: crate::tools::flow_search::FlowSearchHandle,
    host_question_routing: bool,
    token_estimation_multiplier: f64,
    repo_map: Option<craft_repomap::RepoMap>,
    thread_history: Option<Arc<std::sync::Mutex<super::typed_log::ThreadHistory>>>,
    thread_manager: Option<Arc<std::sync::Mutex<super::threads::ThreadManager>>>,
    flow_advisor: Option<Arc<dyn super::flow_loop::FlowAdvisor + Send + Sync>>,
    thread_id: super::typed_log::ThreadId,
    flow_progress_tx: Option<flume::Sender<super::flow_loop::FlowProgress>>,
    /// Flow mode only: set by the `Tpm -> Plan` goal-approval gate. When set,
    /// the terminal `Done` arm of `run_loop` emits this stop reason instead of
    /// the natural one, so the host can re-prompt for goal approval. Cleared
    /// after the run ends.
    pending_approval_stop: Option<StopReason>,
}

const MAX_JUDGE_CONTINUATIONS: u8 = 5;

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
            provider: params.provider,
            model: Arc::new(params.model),
            config: params.config,
            tool_output_lines: params.tool_output_lines,
            permissions: params.permissions,
            timeouts: params.timeouts,
            history: run.history,
            system: run.system,
            event_tx: run.event_tx,
            tools: run.tools,
            mode: AgentMode::default(),
            turn_type: crate::agent::turn_type::TurnType::General,
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
            total_usage: TokenUsage::default(),
            context_size: 0,
            num_turns: 0,
            doom: params.doom,
            guardrails: ToolGuardrails::new(),
            ineffective_compaction_count: 0,
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            post_tool_empty_retried: false,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            file_tracker: params.file_tracker,
            prompt_slots: params.prompt_slots,
            subagent_cancels: params.subagent_cancels,
            registry: params.registry,
            compression: params.compression.clone(),
            findings_store: params.findings_store,
            cache_tracker: super::cache::PrefixCacheTracker::new(),
            compression_store: super::compression_store::shared_store(),
            dedup_cache: ToolDedupCache::new(),
            trust_tracker: TrustTracker::new(craft_config::TrustDecayConfig::default()),
            snapshot: SnapshotManager::new(std::env::current_dir().unwrap_or_default()),
            validator: Validator::new(
                std::env::current_dir().unwrap_or_default(),
                craft_config::ValidationConfig::default(),
            ),
            formatter: Formatter::new(
                std::env::current_dir().unwrap_or_default(),
                craft_config::FormatConfig::default(),
            ),
            escalation: EscalationTracker::new(Default::default()),
            promoted: run.promoted,
            dynamic,
            tool_build: run.tool_build,
            hooks: run.hooks,
            scorer: Some(super::semantic::RelevanceScorer::new()),
            last_relevance_scores: None,
            fs: params.fs,
            goal: None,
            goal_criteria: Vec::new(),
            judge_continuations: 0,
            advisor_continuations: 0,
            snapshot_store: crate::tools::safety::SnapshotStore::fresh(),
            pending_edits: crate::tools::ast_edit::PendingEditStore::fresh(),
            fallback_chain: Vec::new(),
            advisor_state,
            ttsr,
            flow_search: None,
            host_question_routing: false,
            token_estimation_multiplier: 1.0,
            repo_map: None,
            thread_history: params.flow_thread_history,
            thread_manager: params.flow_thread_manager,
            flow_advisor: params.flow_advisor,
            thread_id: super::typed_log::ThreadId::new(""),
            flow_progress_tx: params.flow_progress_tx,
            pending_approval_stop: None,
        }
    }

    pub fn with_mcp(mut self, mcp: Option<McpHandle>) -> Self {
        self.trust_tracker = TrustTracker::new(self.config.trust_decay);
        self.validator = Validator::new(
            std::env::current_dir().unwrap_or_default(),
            self.config.validation.clone(),
        );
        self.formatter = Formatter::new(
            std::env::current_dir().unwrap_or_default(),
            self.config.format.clone(),
        );
        self.mcp = mcp;
        self
    }

    pub fn with_user_response_rx(
        mut self,
        rx: Arc<tokio::sync::Mutex<flume::Receiver<String>>>,
    ) -> Self {
        self.user_response_rx = Some(rx);
        self
    }

    /// Route `question` tool calls to the host over the event channel instead
    /// of the registry entry. Set by the headless/ACP/desktop path, where the
    /// Lua question form can't run. The TUI leaves this off so its form wins.
    pub fn with_host_question_routing(mut self, enabled: bool) -> Self {
        self.host_question_routing = enabled;
        self
    }

    pub fn with_interrupt_source(mut self, source: Arc<dyn InterruptSource>) -> Self {
        self.interrupt_source = Some(source);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_fallback_chain(mut self, chain: Vec<craft_providers::roles::ChainHop>) -> Self {
        self.fallback_chain = chain;
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
        self.flow_search = flow_search;
        self
    }

    /// Flow mode only: override the root thread id with a child `ThreadId`.
    /// Used by the `task` tool's Flow integration so a child agent runs its
    /// shift-enabled loop against its own thread. `Agent::run`'s root-default
    /// logic only fires when the thread id was not set via this builder.
    pub fn with_flow_thread_id(mut self, id: super::typed_log::ThreadId) -> Self {
        self.thread_id = id;
        self
    }

    pub fn with_repo_map(mut self, repo_map: Option<craft_repomap::RepoMap>) -> Self {
        self.repo_map = repo_map;
        self
    }

    async fn build_intent(&self) -> Option<Vec<f32>> {
        let scorer = self.scorer.as_ref()?;
        scorer.build_intent(self.history.as_slice()).await.ok()
    }

    async fn build_semantic_view(&self, intent: &[f32]) -> Option<Vec<Message>> {
        let scorer = self.scorer.as_ref()?;
        let scores = scorer
            .score_messages(self.history.as_slice(), intent)
            .await
            .ok()?;
        let token_budget = self.model.context_window.saturating_sub(
            self.config
                .resolve_compaction_buffer(self.model.context_window),
        );
        let selected = super::semantic::select_messages(
            &scores,
            self.history.len(),
            token_budget,
            MANDATORY_RECENT_MESSAGES,
            self.cache_tracker.frozen_count(),
            &|idx| self.history.message_token_estimate(&self.model, idx),
        );
        if selected.len() < self.history.len() {
            info!(
                total = self.history.len(),
                selected = selected.len(),
                "semantic context curation applied"
            );
            Some(self.history.select_view(&selected, self.history.len()))
        } else {
            None
        }
    }

    pub async fn run(mut self, input: AgentInput) -> Result<(), AgentError> {
        strip_trailing_grace_prompt(self.history);
        self.doom
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .reset_for_new_user_input();
        self.rollback_len = self.history.len();
        let msg = Message::user_with_images(input.message.clone(), input.images);
        self.history.push(msg);
        self.mode = input.mode;
        // Phase 1: every mode (Build/Plan/Flow) resolves to General, so
        // behavior is byte-identical to the pre-turn-type loop. Phase 2 routes
        // Flow mode through the turn-typed loop and sets narrow turn types.
        self.turn_type = crate::agent::turn_type::TurnType::General;
        // The `shift` tool is Flow-mode-only; strip it from the toolset in
        // Build/Plan so the model cannot call it (keeps Build/Plan
        // byte-identical to the pre-shift-tool behavior).
        if !matches!(self.mode, AgentMode::Flow(_)) {
            self.tools = crate::tools::strip_flow_only_tools(&self.tools);
        } else {
            // Flow mode: set the current thread id to the root thread (the
            // workstream id) so typed-log appends and ThreadManager calls
            // target the root. A child agent created via `with_flow_thread_id`
            // keeps its child thread id; the root default only fires when the
            // id was not overridden.
            if self.thread_id.as_str().is_empty() {
                if let Some(mgr) = self.thread_manager.as_ref() {
                    let root = mgr.lock().unwrap_or_else(|e| e.into_inner()).root.clone();
                    self.thread_id = root;
                } else if let Some(hist) = self.thread_history.as_ref() {
                    self.thread_id = hist
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .root_thread_id()
                        .clone();
                }
            }
        }
        // Flow goal-approval resume: re-enter as the gate's target type instead
        // of `general`. `advance_turn_type` advances the ThreadManager, emits
        // `TurnTypeEntered` (so the host's stage display updates), and pushes
        // the stage brief (so the model knows the pipeline resumed here, e.g.
        // "write the plan" rather than re-deriving from scratch). No-op outside
        // Flow mode or when no resume stage is requested.
        if matches!(self.mode, AgentMode::Flow(_))
            && let Some(target) = input.flow_resume_stage
        {
            self.advance_turn_type(target, crate::agent::turn_type::ThreadAction::Advance);
        }
        self.goal = input.goal;
        self.goal_criteria = input.goal_criteria.clone();
        self.opts = RequestOptions {
            thinking: input.thinking,
            fast: input.fast,
        };

        info!(
            model = %self.model.id,
            mode = ?self.mode,
            message_len = input.message.len(),
            "agent run started"
        );

        if self.config.hooks_enabled
            && let Some(hooks) = &self.hooks
            && tokio::time::timeout(HOOK_BEST_EFFORT_TIMEOUT, hooks.session_start())
                .await
                .is_err()
        {
            warn!("session_start hook timed out");
        }

        let result = self.run_loop().await;

        if matches!(result, Err(AgentError::Cancelled)) {
            sanitize_cancelled_history(self.history, self.rollback_len);
        }

        result
    }

    async fn run_loop(&mut self) -> Result<(), AgentError> {
        loop {
            if let Some(max) = self.config.max_turns
                && self.num_turns >= max
            {
                self.emit_done(None)?;
                return Ok(());
            }
            // Turn-type dispatch seed (design §1, plan Phase 1). Phase 1 only
            // enables General, so this resolves the General spec and runs
            // today's `turn()` body unchanged. The Flow shift path consults
            // the same spec's `.transitions` at the turn boundary (see
            // `apply_shift_if_requested`).
            let _spec = self.turn_type.spec();
            // Flow mode only: honor a cancel request as a turn boundary.
            // Build/Plan surface cancellation through `turn()`'s existing
            // `self.cancel.is_cancelled()` check (returns `AgentError::Cancelled`).
            if matches!(self.mode, AgentMode::Flow(_)) && self.cancel.is_cancelled() {
                if let Some(tx) = self.flow_progress_tx.as_ref() {
                    let _ = tx.send(super::flow_loop::FlowProgress::Cancelled);
                }
                self.snapshot.commit();
                self.emit_done(Some(StopReason::Cancelled))?;
                return Ok(());
            }
            let (should_grace, should_hard_stop) = {
                let d = self.doom.lock().unwrap_or_else(|e| e.into_inner());
                (d.should_grace(), d.should_hard_stop())
            };
            if should_hard_stop {
                let score = self.doom.lock().unwrap_or_else(|e| e.into_inner()).score();
                info!(
                    score,
                    turns = self.num_turns,
                    "doom hard-stop reached, ending run"
                );
                self.snapshot.commit();
                self.emit_done(None)?;
                return Ok(());
            }
            if should_grace {
                self.doom
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mark_grace_called();
                self.history
                    .push(Message::user(GRACE_CALL_PROMPT.to_string()));
                let score = self.doom.lock().unwrap_or_else(|e| e.into_inner()).score();
                info!(
                    score,
                    turns = self.num_turns,
                    "doom grace threshold reached, issuing grace call"
                );
            }
            match self.turn().await? {
                TurnOutcome::Continue => {
                    // Flow mode only: drain the last `shift` request from this
                    // turn and run it through `transitions::resolve`. No-op
                    // (and cheap) in Build/Plan and when no shift was made.
                    self.apply_shift_if_requested().await?;
                    // Goal-approval gate fired on `Tpm -> Plan`: end the run
                    // now so the host can re-prompt, before another turn runs.
                    if let Some(stop) = self.pending_approval_stop.take() {
                        self.snapshot.commit();
                        self.emit_done(Some(stop))?;
                        return Ok(());
                    }
                }
                TurnOutcome::Done(stop_reason) => {
                    self.snapshot.commit();
                    let note = self.run_advisor().await;
                    match advisor_turn_action(
                        note,
                        &self.config.advisor,
                        self.pending_approval_stop.is_some(),
                        self.advisor_continuations,
                    ) {
                        AdvisorTurnAction::Continue(note) => {
                            self.advisor_continuations += 1;
                            self.history.push(advisor_followup_message(&note));
                            continue;
                        }
                        AdvisorTurnAction::Stop => {}
                    }
                    // Flow mode: commit the final turn's write before exiting.
                    if matches!(self.mode, AgentMode::Flow(_)) {
                        self.commit_turn_write(self.turn_type);
                    }
                    // Goal-approval gate: the `Tpm -> Plan` shift set this;
                    // override the natural stop reason so the host re-prompts.
                    let stop_reason = self.pending_approval_stop.take().or(stop_reason);
                    self.emit_done(stop_reason)?;
                    return Ok(());
                }
                TurnOutcome::ShiftOut => {
                    // A Flow narrow turn finished without shifting. Commit its
                    // write, hand control back to `general`, and resume the
                    // loop so the root owner re-derives the next step.
                    self.commit_turn_write(self.turn_type);
                    self.advance_turn_type(
                        crate::agent::turn_type::TurnType::General,
                        crate::agent::turn_type::ThreadAction::Advance,
                    );
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

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if let Some(ttsr) = self.ttsr.as_ref() {
            ttsr.reset_turn();
        }

        if let Some(build) = &self.tool_build {
            self.tools = crate::tools::build_active_tools(
                build,
                &self.model,
                &self.config,
                &self.dynamic,
                &self.promoted,
            );
            if !matches!(self.mode, AgentMode::Flow(_)) {
                self.tools = crate::tools::strip_flow_only_tools(&self.tools);
            }
        }

        let intent = self.build_intent().await;

        if let Some(intent_vec) = &intent
            && let Some(scorer) = &self.scorer
        {
            let restored = super::semantic::auto_retrieve(
                scorer,
                &self.compression_store,
                intent_vec,
                self.history,
            )
            .await;
            if restored > 0 {
                info!(restored, "auto-retrieve restored compressed content");
            }
        }

        let semantic_view: Option<Vec<Message>> = match &intent {
            Some(intent_vec) => self.build_semantic_view(intent_vec).await,
            None => None,
        };

        let base_messages: &[Message] = semantic_view
            .as_deref()
            .unwrap_or_else(|| self.history.as_slice());

        let last_user_text = base_messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User))
            .and_then(|m| m.first_text_content())
            .unwrap_or("");

        let repo_map_msg = if let Some(rm) = &self.repo_map {
            let context_files: Vec<String> = self
                .file_tracker
                .read_paths()
                .into_iter()
                .filter_map(|p| p.to_str().map(String::from))
                .collect();
            let map_text = rm.get_repo_map(&[], &context_files, last_user_text);
            if map_text.is_empty() {
                None
            } else {
                Some(Message::synthetic(format!(
                    "Repo map (ranked symbols, may be stale):\n\n{map_text}"
                )))
            }
        } else {
            None
        };

        let owned_messages: Vec<Message>;
        let messages: &[Message] = if let Some(rm_msg) = repo_map_msg {
            owned_messages = std::iter::once(rm_msg)
                .chain(base_messages.iter().cloned())
                .collect();
            &owned_messages
        } else {
            base_messages
        };

        let response = match stream_with_retry(
            &*self.provider,
            &self.model,
            messages,
            &self.system,
            &self.tools,
            &self.event_tx,
            &self.cancel,
            self.opts,
            self.session_id.as_ref(),
            &self.fallback_chain,
            self.ttsr.clone(),
            self.num_turns,
        )
        .await
        {
            Ok((r, injection)) => {
                self.reauth_attempts = 0;
                if let Some(reminder) = injection {
                    self.history.push(Message::synthetic(reminder));
                }
                r
            }
            Err(e) if e.is_auth_error() => {
                return self.wait_for_reauth(e).await;
            }
            Err(e) if e.is_overflow() => {
                info!("context overflow detected, will attempt auto-compact");
                return Ok(TurnOutcome::Overflow);
            }
            Err(e) => {
                let (kind, action) = super::recovery::classify(&e);
                error!(
                    error = %e,
                    model = %self.model.id,
                    self.num_turns,
                    recovery_kind = ?kind,
                    recovery_action = ?action,
                    "stream_message failed",
                );
                if matches!(action, super::recovery::RecoveryAction::Escalate) {
                    let _ = self.event_tx.send(AgentEvent::Info {
                        message: format!("{kind:?}: {e}"),
                    });
                }
                return Err(e);
            }
        };
        self.num_turns += 1;

        let has_tools = response.message.has_tool_calls();
        let stop_reason = response.stop_reason;
        info!(
            input_tokens = response.usage.input,
            output_tokens = response.usage.output,
            cache_creation = response.usage.cache_creation,
            cache_read = response.usage.cache_read,
            has_tools,
            self.num_turns,
            model = %self.model.id,
            stop_reason = stop_reason.map_or("none", Into::into),
            "API response received"
        );

        self.emit_turn_complete(&response)?;
        let usage = response.usage;
        self.total_usage += usage;
        self.context_size = usage.total_input();
        self.cache_tracker.update(&usage, self.history.len());

        if let Some(scorer) = &self.scorer {
            let turn_summary = super::semantic::intent_summary(self.history.as_slice());
            if !turn_summary.is_empty()
                && let Ok(emb) = scorer.embed_text(&turn_summary).await
            {
                let mut doom = self.doom.lock().unwrap_or_else(|e| e.into_inner());
                doom.turn_embeddings.push_back(emb);
                if doom.turn_embeddings.len() > STAGNATION_WINDOW_SIZE {
                    doom.turn_embeddings.pop_front();
                }
                let embeddings = doom.turn_embeddings.make_contiguous();
                if super::semantic::detect_stagnation(embeddings, STAGNATION_SIMILARITY_THRESHOLD) {
                    let n = embeddings.len();
                    let sim = super::semantic::RelevanceScorer::similarity(
                        &embeddings[n - 2],
                        &embeddings[n - 1],
                    );
                    info!(sim, "stagnation detected");
                    doom.note_stagnation();
                    let _ = self
                        .event_tx
                        .send(AgentEvent::StagnationDetected { similarity: sim });
                }
            }
        }

        if has_tools {
            let history_len_before = self.history.len();
            let batch = self.process_tool_calls(response).await?;
            self.context_size +=
                estimate_message_tokens(&self.history.as_slice()[history_len_before..]);
            {
                let mut doom = self.doom.lock().unwrap_or_else(|e| e.into_inner());
                for _ in 0..batch.doom_loops {
                    doom.note_doom_loop();
                }
                for _ in 0..batch.errors {
                    doom.note_tool_error();
                }
                for _ in 0..batch.successes {
                    doom.note_tool_success();
                }
                for _ in 0..batch.validation_rejections {
                    doom.note_validator_rejection();
                }
            }
            self.escalation.record(&self.model.id, batch.had_errors());
            self.escalation.check_and_emit(
                &self.model.id,
                super::escalation::ModelTier::from_model_id(&self.model.id),
                &self.event_tx,
            );
        } else {
            let has_text = response.message.first_text_content().is_some();

            if !has_text && !self.post_tool_empty_retried && self.history.has_recent_tool_results(5)
            {
                self.post_tool_empty_retried = true;
                warn!("empty response after tool calls, nudging model to continue");
                self.event_tx.send(AgentEvent::Nudge)?;
                self.history.push(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "(empty)".into(),
                    }],
                    ..Default::default()
                });
                self.history.push(Message::synthetic(NUDGE_PROMPT.into()));
                return Ok(TurnOutcome::Continue);
            }

            self.history.push(response.message);

            if stop_reason == Some(StopReason::MaxTokens)
                && self.num_turns <= self.config.max_continuation_turns
            {
                warn!(
                    self.num_turns,
                    "response truncated (max_tokens), re-prompting"
                );
                return Ok(TurnOutcome::Continue);
            }
        }

        let cumulative_usage = TokenUsage {
            input: self.context_size,
            ..Default::default()
        };
        if self.try_auto_compact(&cumulative_usage, false).await?
            || self.handle_queued_command().await?
        {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            Ok(TurnOutcome::Continue)
        } else if let Some(ref goal) = self.goal.clone() {
            let criteria = self.goal_criteria.clone();
            self.run_goal_judge(goal, &criteria, stop_reason).await
        } else if matches!(self.mode, AgentMode::Flow(_))
            && self.turn_type != crate::agent::turn_type::TurnType::General
        {
            // A Flow narrow turn ended (EndTurn, no tool calls). Don't end the
            // run — hand control back to `general` so the root owner re-derives
            // the next step. Only `general` ending without a shift ends a run.
            Ok(TurnOutcome::ShiftOut)
        } else {
            Ok(TurnOutcome::Done(stop_reason))
        }
    }

    async fn run_advisor(&mut self) -> Option<super::advisor::AdvisorNote> {
        let state = self.advisor_state.as_mut()?;
        let result = super::advisor::review(
            state,
            self.history.as_slice(),
            &self.config.advisor,
            &self.provider,
            &self.model,
            self.timeouts,
            self.session_id.as_ref(),
        )
        .await;
        match result {
            Ok(Some(note)) => {
                let _ = self.event_tx.send(AgentEvent::AdvisorNote {
                    severity: note.severity.as_str().to_string(),
                    message: note.message.clone(),
                });
                Some(note)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(error = %e, "advisor review failed");
                None
            }
        }
    }

    /// Flow mode only: scan the most recent assistant turn in the chat history
    /// for the last `shift` tool call and recover the requested target +
    /// rationale from the matching `ToolResult`. Returns `None` outside Flow
    /// mode, when no `shift` call was made, or when the result cannot be
    /// parsed (the orchestrator-equivalent of "no shift this turn, stay
    /// cheap"). Implements Option A of plan §5.
    fn last_shift_request(&self) -> Option<crate::types::ToolOutput> {
        if !matches!(self.mode, AgentMode::Flow(_)) {
            return None;
        }
        let assistant = self
            .history
            .as_slice()
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant) && m.has_tool_calls())?;
        let mut shift_ids: Vec<&str> = assistant
            .tool_uses()
            .filter(|(_, name, _)| *name == crate::tools::SHIFT_TOOL_NAME)
            .map(|(id, _, _)| id)
            .collect();
        let last_id = shift_ids.pop()?;
        let result_text = self.history.as_slice().iter().rev().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } if tool_use_id == last_id => Some(content.as_str()),
                _ => None,
            })
        })?;
        parse_shift_output(result_text)
    }

    /// Flow mode only: commit one distilled typed-log entry for the turn that
    /// just completed. Mapping is `TurnType::spec().write.entry`; content is
    /// the assistant's final text for the turn (verbatim — plan §4a; a real
    /// distillation pass is a later refinement). No-op outside Flow mode or
    /// when no `ThreadHistory` is attached.
    fn commit_turn_write(&mut self, turn_type: crate::agent::turn_type::TurnType) {
        let Some(hist) = self.thread_history.as_ref() else {
            return;
        };
        let entry_type = turn_type.spec().write.entry;
        let content = self.last_turn_text();
        hist.lock().unwrap_or_else(|e| e.into_inner()).append(
            self.thread_id.clone(),
            entry_type,
            content,
        );
    }

    /// The just-completed turn's final assistant text (verbatim). Used for the
    /// typed-log write and to extract the goal doc at the `Tpm -> Plan` gate.
    /// Empty when no assistant text was produced.
    fn last_turn_text(&self) -> String {
        self.history
            .as_slice()
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::Assistant))
            .and_then(|m| m.first_text_content())
            .unwrap_or("")
            .to_string()
    }

    /// Flow mode only: run the between-turn `FlowAdvisor` (the tree watcher
    /// with override power, distinct from the cheaper per-turn `advisor::review`).
    /// Returns the Advisor's forced transition expressed as a `TurnProposal`
    /// (target always `General`, plan §12) to feed into `resolve` as
    /// `advisor_override`. Records any addressed note in the typed log and
    /// emits `FlowProgress::AdvisorNote`. `None` when there is no advisor, no
    /// override, or outside Flow mode.
    async fn run_flow_advisor(&mut self) -> Option<super::transitions::TurnProposal> {
        if !matches!(self.mode, AgentMode::Flow(_)) {
            return None;
        }
        let advisor = self.flow_advisor.clone()?;
        let hist = self.thread_history.clone()?;
        let mgr_snapshot = self
            .thread_manager
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let thread_id = self.thread_id.clone();
        let turn_type = self.turn_type;
        let forced = advisor
            .review(hist, mgr_snapshot, thread_id.clone(), turn_type)
            .await;
        let forced = forced?;
        let note = super::flow_loop::record_advisor_note(
            self.thread_history.as_ref().unwrap(),
            &thread_id,
            &forced,
        );
        if let Some(tx) = self.flow_progress_tx.as_ref() {
            let _ = tx.send(super::flow_loop::FlowProgress::AdvisorNote {
                thread_id: thread_id.to_string(),
                addressed_to: thread_id.to_string(),
                severity: forced.severity,
                message: note,
            });
        }
        Some(super::transitions::TurnProposal::self_report(
            crate::agent::turn_type::TurnType::General,
            crate::agent::turn_type::ThreadAction::Advance,
            forced.note,
        ))
    }

    /// Flow mode only: the turn-boundary shift logic (plan §3, §11). Drains
    /// the last `shift` request from the just-completed turn, runs the
    /// current type's `TransitionRule` set through `transitions::resolve`
    /// with the Advisor's forced transition as `advisor_override`, and either
    /// shifts (applying the typed-log write + emitting
    /// `FlowProgress::TurnTypeEntered`) or pushes a soft `Illegal` user
    /// message and stays. Cheap no-op outside Flow mode or when no shift was
    /// requested and no advisor override fires.
    async fn apply_shift_if_requested(&mut self) -> Result<(), AgentError> {
        if !matches!(self.mode, AgentMode::Flow(_)) {
            return Ok(());
        }
        let shift = self.last_shift_request();
        let advisor_override = self.run_flow_advisor().await;
        if shift.is_none() && advisor_override.is_none() {
            return Ok(());
        }
        let rules = self.turn_type.spec().transitions;
        let proposal = shift.as_ref().map(|s| {
            let crate::types::ToolOutput::ShiftTurnType { target, rationale } = s else {
                unreachable!("last_shift_request returns only ShiftTurnType");
            };
            super::transitions::TurnProposal::self_report(
                *target,
                crate::agent::turn_type::ThreadAction::Advance,
                rationale.clone(),
            )
        });
        let resolved = super::transitions::resolve(
            &rules,
            proposal
                .as_ref()
                .unwrap_or(&super::transitions::TurnProposal::self_report(
                    self.turn_type,
                    crate::agent::turn_type::ThreadAction::Advance,
                    String::new(),
                )),
            advisor_override.as_ref(),
        );
        match resolved {
            super::transitions::ResolvedTransition::Accepted { target, action } => {
                let from = self.turn_type;
                // Goal-approval gate (plan §7): the `Tpm -> Plan` transition
                // ends the run with `AwaitingGoalApproval` after emitting the
                // goal doc. The host re-prompts; on resume the agent re-derives
                // the shift from the persisted goal. `General -> Plan` (skipped
                // Tpm) does not pause.
                if from == crate::agent::turn_type::TurnType::Tpm
                    && target == crate::agent::turn_type::TurnType::Plan
                {
                    let goal_doc = self.last_turn_text();
                    self.commit_turn_write(from);
                    if let Some(tx) = self.flow_progress_tx.as_ref() {
                        let _ = tx.send(super::flow_loop::FlowProgress::GoalReady { goal_doc });
                    }
                    self.pending_approval_stop = Some(StopReason::AwaitingGoalApproval);
                    return Ok(());
                }
                self.commit_turn_write(from);
                self.advance_turn_type(target, action);
            }
            super::transitions::ResolvedTransition::Illegal { proposed } => {
                self.history.push(Message::user(format!(
                    "Illegal shift from {} to {}; staying.",
                    self.turn_type.as_str(),
                    proposed.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Flow mode only: mutate `turn_type`, advance the `ThreadManager` (if
    /// attached), emit `FlowProgress::TurnTypeEntered`, and (for narrow types)
    /// push a stage brief so the model knows what the new type does, what to
    /// read from the typed log, and where it may shift next. This is the single
    /// place `turn_type` changes after `Agent::run` seeds it to `General`.
    fn advance_turn_type(
        &mut self,
        target: crate::agent::turn_type::TurnType,
        _action: crate::agent::turn_type::ThreadAction,
    ) {
        if let Some(mgr) = self.thread_manager.as_ref() {
            mgr.lock()
                .unwrap_or_else(|e| e.into_inner())
                .advance(&self.thread_id, target);
        }
        self.turn_type = target;
        if let Some(tx) = self.flow_progress_tx.as_ref() {
            let _ = tx.send(super::flow_loop::FlowProgress::TurnTypeEntered {
                thread_id: self.thread_id.to_string(),
                turn_type: target,
            });
        }
        if target != crate::agent::turn_type::TurnType::General
            && let Some(brief) = self.stage_brief(target)
        {
            self.history.push(Message::synthetic(brief));
        }
    }

    /// Render the stage brief for a narrow `target` type from its `TurnTypeSpec`:
    /// the write commitment (with the JSON Schema inlined when the type has
    /// one), the resolved core-read entries inlined from the typed log, and the
    /// legal next shifts (flagging objective gates). Returns `None` only when
    /// no `ThreadHistory` is attached (Build/Plan); an attached-but-empty log
    /// still yields a brief, just without inlined reads.
    fn stage_brief(&self, target: crate::agent::turn_type::TurnType) -> Option<String> {
        let hist = self.thread_history.as_ref()?;
        let spec = target.spec();
        let mut out = String::new();
        out.push_str(&format!(
            "You are now in the `{}` turn type of Flow workstream `{}`.\n\n",
            target.as_str(),
            self.thread_id
        ));
        out.push_str("Begin this turn's work now. Do not just acknowledge the shift; produce or gather the artifact this turn type is responsible for, then either shift to the next type that the work needs, or back to `general` if the immediate question is answered.\n\n");
        out.push_str("## Write\n");
        out.push_str(&format!(
            "Commit one `{}` entry as your final reply this turn (prose or markdown, not JSON)",
            spec.write.entry.as_str()
        ));
        if let Some(guidance) = spec.write.guidance {
            out.push_str(". ");
            out.push_str(guidance);
        } else {
            out.push('.');
        }
        out.push('\n');

        let parent_id = self.parent_thread_id();
        let root_id = hist
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .root_thread_id()
            .clone();
        let mut inlined = false;
        for read in &spec.read.core {
            let scope_id = match read.level {
                crate::agent::turn_type::ThreadLevel::Own => Some(self.thread_id.clone()),
                crate::agent::turn_type::ThreadLevel::Parent => parent_id.clone(),
                crate::agent::turn_type::ThreadLevel::Root => Some(root_id.clone()),
            };
            let Some(scope) = scope_id else {
                continue;
            };
            let entry = hist
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .projection(read.entry, &scope)
                .cloned();
            if let Some(entry) = entry {
                if !inlined {
                    out.push_str("\n## Context (from the typed log)\n");
                    inlined = true;
                }
                out.push_str(&format!(
                    "### {} ({} @ {})\n{}\n\n",
                    read.entry.as_str(),
                    match read.level {
                        crate::agent::turn_type::ThreadLevel::Own => "this thread",
                        crate::agent::turn_type::ThreadLevel::Parent => "parent thread",
                        crate::agent::turn_type::ThreadLevel::Root => "root thread",
                    },
                    scope,
                    entry.content.trim()
                ));
            }
        }
        if inlined {
            out.push_str("Fetch more with `flow_search` or `read path=\"flow://<path>\"`.\n");
        }

        if !spec.transitions.is_empty() {
            out.push_str("\n## Legal next shifts\n");
            out.push_str(
                "Pick the one the work needs. Returning to `general` is always fine when the \
                 narrow role has done its job; you are not forced down a fixed pipeline.\n",
            );
            for rule in &spec.transitions {
                out.push_str(&format!(
                    "- `{}`{}\n",
                    rule.target.as_str(),
                    match rule.action {
                        crate::agent::turn_type::ThreadAction::Spawn => " (spawn child thread)",
                        crate::agent::turn_type::ThreadAction::Exit => " (exit this thread)",
                        crate::agent::turn_type::ThreadAction::Advance => "",
                    },
                ));
            }
        }
        Some(out)
    }

    /// The parent thread id for the current thread, from the `ThreadManager`.
    /// `None` for the root thread (it has no parent) or when no manager is
    /// attached.
    fn parent_thread_id(&self) -> Option<crate::agent::typed_log::ThreadId> {
        let mgr = self.thread_manager.as_ref()?;
        mgr.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&self.thread_id)
            .and_then(|t| t.parent.clone())
    }

    async fn run_goal_judge(
        &mut self,
        goal: &str,
        goal_criteria: &[String],
        stop_reason: Option<StopReason>,
    ) -> Result<TurnOutcome, AgentError> {
        if self.judge_continuations >= MAX_JUDGE_CONTINUATIONS {
            warn!(
                continuations = self.judge_continuations,
                "judge continuation cap reached, allowing stop"
            );
            return Ok(TurnOutcome::Done(stop_reason));
        }
        if !goal_criteria.is_empty() {
            return self
                .run_criteria_judge(goal, goal_criteria, stop_reason)
                .await;
        }
        let outcome = super::judge::evaluate(
            goal,
            self.history.as_slice(),
            &self.provider,
            &self.model,
            self.config.judge_model.as_deref(),
            self.timeouts,
            self.session_id.as_ref(),
        )
        .await;
        match outcome {
            Ok(super::judge::JudgeOutcome::Done) => {
                self.event_tx.send(AgentEvent::Info {
                    message: "Goal met (verified by judge)".into(),
                })?;
                Ok(TurnOutcome::Done(stop_reason))
            }
            Ok(super::judge::JudgeOutcome::NotDone(reason)) => {
                self.judge_continuations += 1;
                let note = format!(
                    "The judge evaluated that the goal is not yet fully met: {reason}. \
                     Continue working toward the goal: {goal}. Do not stop until it is done."
                );
                self.history.push(Message::synthetic(note));
                Ok(TurnOutcome::Continue)
            }
            Ok(super::judge::JudgeOutcome::Criteria { .. }) => {
                // Only returned by evaluate_criteria, not evaluate.
                Ok(TurnOutcome::Done(stop_reason))
            }
            Err(e) => {
                warn!(error = %e, "judge evaluation failed, allowing stop (fail-open)");
                Ok(TurnOutcome::Done(stop_reason))
            }
        }
    }

    async fn run_criteria_judge(
        &mut self,
        goal: &str,
        criteria: &[String],
        stop_reason: Option<StopReason>,
    ) -> Result<TurnOutcome, AgentError> {
        let outcome = super::judge::evaluate_criteria(
            criteria,
            self.history.as_slice(),
            &self.provider,
            &self.model,
            self.config.judge_model.as_deref(),
            self.timeouts,
            self.session_id.as_ref(),
        )
        .await;
        match outcome {
            Ok(super::judge::JudgeOutcome::Criteria { met, unmet }) => {
                if unmet.is_empty() {
                    self.event_tx.send(AgentEvent::Info {
                        message: format!("Goal met — all {} criteria verified", met.len()),
                    })?;
                    Ok(TurnOutcome::Done(stop_reason))
                } else {
                    self.judge_continuations += 1;
                    let list = unmet
                        .iter()
                        .map(|c| format!("- {c}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let note = format!(
                        "Unmet criteria:\n{list}\n\nContinue working toward the goal: {goal}. \
                         Do not stop until all criteria are met."
                    );
                    self.history.push(Message::synthetic(note));
                    Ok(TurnOutcome::Continue)
                }
            }
            Ok(_) => Ok(TurnOutcome::Done(stop_reason)),
            Err(e) => {
                warn!(error = %e, "criteria judge evaluation failed, allowing stop (fail-open)");
                Ok(TurnOutcome::Done(stop_reason))
            }
        }
    }

    async fn wait_for_reauth(&mut self, err: AgentError) -> Result<TurnOutcome, AgentError> {
        if self.reauth_attempts >= MAX_REAUTH_ATTEMPTS {
            error!(error = %err, attempts = self.reauth_attempts, "max re-auth attempts reached");
            return Err(err);
        }
        let Some(rx) = &self.user_response_rx else {
            error!(error = %err, model = %self.model.id, self.num_turns, "stream_message failed");
            return Err(err);
        };
        self.reauth_attempts += 1;
        warn!(error = %err, attempt = self.reauth_attempts, "auth error, waiting for re-authentication");
        self.event_tx.send(AgentEvent::AuthRequired)?;
        let rx = rx.lock().await;
        match tokio::select! {
            r = rx.recv_async() => r.map_err(|_| flume::RecvError::Disconnected),
            _ = self.cancel.cancelled() => Err(flume::RecvError::Disconnected),
        } {
            Ok(_) => {
                self.provider.refresh_auth().await?;
                Ok(TurnOutcome::Continue)
            }
            Err(_) => Err(AgentError::Cancelled),
        }
    }

    fn emit_turn_complete(&self, response: &StreamResponse) -> Result<(), AgentError> {
        self.event_tx
            .send(AgentEvent::TurnComplete(Box::new(TurnCompleteEvent {
                message: response.message.clone(),
                usage: response.usage,
                model: self.model.id.clone(),
                cost: self.model.cost_of(&response.usage, self.opts.fast),
                context_size: Some(response.usage.context_tokens()),
                context_window: self.model.context_window,
            })))
    }

    fn emit_done(&self, stop_reason: Option<StopReason>) -> Result<(), AgentError> {
        info!(
            self.num_turns,
            total_input = self.total_usage.input,
            total_output = self.total_usage.output,
            "agent run completed"
        );
        self.event_tx.send(AgentEvent::Done {
            usage: self.total_usage,
            num_turns: self.num_turns,
            stop_reason,
        })
    }

    async fn process_tool_calls(
        &mut self,
        response: StreamResponse,
    ) -> Result<ToolBatchOutcome, AgentError> {
        self.post_tool_empty_retried = false;
        let ctx = self.tool_context();
        let mut recent = {
            let mut d = self.doom.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut d.recent_calls)
        };
        let result = tool_dispatch::process_tool_calls(
            response,
            &mut recent,
            &mut self.guardrails,
            self.mcp.as_ref(),
            self.history,
            &self.event_tx,
            &ctx,
            &mut self.dedup_cache,
            &mut self.trust_tracker,
            &self.snapshot,
            &self.validator,
            &self.formatter,
        )
        .await;
        {
            let mut d = self.doom.lock().unwrap_or_else(|e| e.into_inner());
            d.recent_calls = recent;
        }
        result
    }

    fn small_model_ratio(&self) -> f64 {
        if self
            .config
            .small_model
            .should_activate(self.model.context_window)
            && self.config.small_model.aggressive_truncation
        {
            self.config.small_model.compaction_threshold
        } else {
            DEFAULT_SMALL_MODEL_RATIO
        }
    }
    /// Proactive compaction ratio: fraction of context window at which to
    /// compact. Per-model `compact_percent`/`reserve_tokens` overrides the
    /// small-model ratio when configured.
    fn proactive_ratio(&self) -> f64 {
        let provider_model_id = self.provider_model_id();
        if let Some(t) = craft_config::resolve_threshold(
            &self.config.compaction,
            provider_model_id.as_deref(),
            &self.model.id,
        ) {
            if let Some(pct) = t.compact_percent {
                return (pct as f64).max(0.01) / 100.0;
            }
            if let Some(reserve) = t.reserve_tokens
                && self.model.context_window > 0
            {
                return 1.0 - (reserve as f64 / self.model.context_window as f64);
            }
        }
        self.small_model_ratio()
    }

    /// Effective overflow buffer: per-model `reserve_tokens` overrides
    /// `compaction_buffer` when configured.
    fn effective_compaction_buffer(&self) -> u32 {
        let provider_model_id = self.provider_model_id();
        if let Some(t) = craft_config::resolve_threshold(
            &self.config.compaction,
            provider_model_id.as_deref(),
            &self.model.id,
        ) && let Some(reserve) =
            craft_config::resolve_reserve_tokens(t, self.model.context_window)
        {
            return reserve;
        }
        self.config
            .resolve_compaction_buffer(self.model.context_window)
    }

    fn provider_model_id(&self) -> Option<String> {
        Some(format!("{}/{}", self.model.provider, self.model.id))
    }

    fn tool_context(&self) -> ToolContext {
        let flow_search = if let Some(handle) = self.flow_search.clone() {
            Some(handle)
        } else if let Some(hist) = self.thread_history.clone() {
            let (project_id, workstream_id, root) = {
                let h = hist.lock().unwrap_or_else(|e| e.into_inner());
                (
                    h.project_id().to_string(),
                    h.root_thread_id().as_str().to_string(),
                    h.root_thread_id().clone(),
                )
            };
            Some(Arc::new(
                crate::tools::flow_search_backend::HistorySearchBackend::new(
                    hist,
                    project_id,
                    workstream_id,
                    root,
                ),
            )
                as Arc<dyn crate::tools::flow_search::FlowSearchBackend>)
        } else {
            None
        };
        ToolContext {
            provider: Arc::clone(&self.provider),
            model: Arc::clone(&self.model),
            event_tx: self.event_tx.clone(),
            mode: self.mode.clone(),
            tool_use_id: None,
            user_response_rx: self.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.cancel.clone(),
            mcp: self.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_output_lines: self.tool_output_lines,
            permissions: Arc::clone(&self.permissions),
            timeouts: self.timeouts,
            file_tracker: Arc::clone(&self.file_tracker),
            prompt_slots: Arc::clone(&self.prompt_slots),
            subagent_cancels: Arc::clone(&self.subagent_cancels),
            opts: self.opts,
            compression: self.compression.clone(),
            registry: Arc::clone(&self.registry),
            compression_store: Arc::clone(&self.compression_store),
            findings_store: self.findings_store.clone(),
            fs: Arc::clone(&self.fs),
            parent_messages: Arc::from(self.history.as_slice()),
            promoted: self.promoted.clone(),
            dynamic: self.dynamic.clone(),
            hooks: self.hooks.clone(),
            snapshot_store: Arc::clone(&self.snapshot_store),
            pending_edits: Arc::clone(&self.pending_edits),
            session_id: self.session_id.as_ref().map(|s| s.as_str().to_string()),
            flow_search,
            host_question_routing: self.host_question_routing,
            flow_thread_manager: self.thread_manager.clone(),
            flow_thread_id: Some(self.thread_id.clone()),
            flow_thread_history: self.thread_history.clone(),
            flow_progress_tx: self.flow_progress_tx.clone(),
        }
    }

    async fn try_auto_compact(
        &mut self,
        usage: &TokenUsage,
        force_full: bool,
    ) -> Result<bool, AgentError> {
        if !self.auto_compact {
            return Ok(false);
        }

        if self.ineffective_compaction_count >= 2 {
            info!("skipping auto-compaction: last 2 attempts were ineffective");
            return Ok(false);
        }

        let overflow = force_full
            || compaction::is_overflow(usage, &self.model, self.effective_compaction_buffer());

        if overflow {
            let estimated = self.history.estimate_tokens(&self.model) as f64;
            if estimated > 0.0 && self.context_size > 0 {
                let ratio = self.context_size as f64 / estimated;
                if ratio > self.token_estimation_multiplier {
                    self.token_estimation_multiplier =
                        (ratio * 1.1).min(compaction::MAX_TOKEN_ESTIMATION_MULTIPLIER);
                    info!(
                        ratio,
                        new_multiplier = self.token_estimation_multiplier,
                        "calibrated token estimation multiplier after overflow"
                    );
                }
            }
        }

        let proactive = !overflow
            && compaction::is_proactive_threshold(
                self.history,
                &self.model,
                self.proactive_ratio(),
                self.token_estimation_multiplier,
            );

        if !overflow && !proactive {
            return Ok(false);
        }

        self.dedup_cache.clear();

        if let Some(scorer) = &self.scorer
            && let Ok(intent) = scorer.build_intent(self.history.as_slice()).await
            && let Ok(scores) = scorer
                .score_messages(self.history.as_slice(), &intent)
                .await
        {
            self.last_relevance_scores = Some(scores);
        }

        let ctx = compaction::CompactContext {
            usage,
            model: &self.model,
            compaction_buffer: self
                .config
                .resolve_compaction_buffer(self.model.context_window),
            cache_tracker: Some(&self.cache_tracker),
            compression_store: Some(&self.compression_store),
            relevance_scores: self
                .scorer
                .as_ref()
                .and(self.last_relevance_scores.as_deref()),
            scorer: self.scorer.as_ref(),
        };
        let removed = compaction::progressive_compact(
            self.history,
            self.compression.protect_recent_tool_outputs,
            &ctx,
        )
        .await;

        if overflow
            && removed > 0
            && !compaction::is_overflow(usage, &self.model, self.effective_compaction_buffer())
        {
            info!(
                chars_removed = removed,
                "progressive compaction avoided full compaction"
            );
            return Ok(true);
        }

        if !overflow {
            return Ok(removed > 0);
        }

        info!(total_input = usage.total_input(), "auto-compacting (full)");
        self.event_tx.send(AgentEvent::AutoCompacting)?;
        let chars_before: usize = self
            .history
            .as_slice()
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        craft_providers::ContentBlock::Text { text } => text.len(),
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum();
        self.do_compact().await?;
        let chars_after: usize = self
            .history
            .as_slice()
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        craft_providers::ContentBlock::Text { text } => text.len(),
                        _ => 0,
                    })
                    .sum::<usize>()
            })
            .sum();
        let savings = if chars_before > 0 {
            1.0 - (chars_after as f32 / chars_before as f32)
        } else {
            0.0
        };
        if savings < INEFFECTIVE_COMPACTION_THRESHOLD {
            self.ineffective_compaction_count += 1;
            info!(
                savings_pct = format!("{:.0}%", savings * 100.0),
                "compaction was ineffective"
            );
            self.doom
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .note_ineffective_compaction();
        } else {
            self.ineffective_compaction_count = 0;
            self.doom
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .note_effective_compaction();
        }
        Ok(true)
    }

    async fn do_compact(&mut self) -> Result<(), AgentError> {
        let vcc_ok = compaction::vcc_compact(
            self.history,
            &self.model,
            self.config
                .resolve_compaction_buffer(self.model.context_window),
            self.token_estimation_multiplier,
        )?;
        if !vcc_ok {
            let (compact_provider, compact_model) =
                resolve_compaction_model(&self.provider, &self.model, self.timeouts).await;
            self.total_usage += compaction::compact_history(
                &*compact_provider,
                &compact_model,
                self.history,
                &self.event_tx,
                &self.cancel,
                self.last_relevance_scores.as_deref(),
            )
            .await?;
        }
        self.rollback_len = self.history.len();
        self.event_tx.send(AgentEvent::CompactionDone)?;
        self.history
            .push(Message::synthetic(CONTINUE_AFTER_COMPACT.into()));
        if let Some(state) = self.advisor_state.as_mut() {
            state.reset(&self.config.advisor);
        }
        if let Some(ttsr) = self.ttsr.as_ref() {
            ttsr.reset();
        }
        self.pending_edits.clear();
        Ok(())
    }

    async fn handle_queued_command(&mut self) -> Result<bool, AgentError> {
        let Some(ref source) = self.interrupt_source else {
            return Ok(false);
        };
        let Some(cmd) = source.poll() else {
            return Ok(false);
        };
        match cmd {
            ExtractedCommand::Interrupt(mut input, _) => {
                self.event_tx.send(AgentEvent::QueueItemConsumed {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                })?;
                for msg in std::mem::take(&mut input.preamble) {
                    self.history.push(msg);
                }
                self.mode = input.mode.clone();
                let display = input.message.clone();
                let wrapped = format!(
                    "<user-interrupt>\nThe user sent a new message while you were working. Address it and continue.\n\n{display}\n</user-interrupt>"
                );
                self.history.push(Message::user_display(wrapped, display));
            }
            ExtractedCommand::Compact(_) => {
                self.do_compact().await?;
            }
            ExtractedCommand::Undo(_) => {
                if let Some(msg) = self.snapshot.rollback().await {
                    self.event_tx.send(AgentEvent::Info { message: msg })?;
                }
            }
        }
        Ok(true)
    }
}

const CHARS_PER_TOKEN: usize = 4;

pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    if messages.is_empty() {
        return 0;
    }
    let total_bytes: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.len()),
            ContentBlock::ToolResult { content, .. } => Some(content.len()),
            ContentBlock::ToolUse { input, .. } => Some(input.to_string().len()),
            _ => None,
        })
        .sum();
    (total_bytes.max(CHARS_PER_TOKEN) / CHARS_PER_TOKEN) as u32
}

/// Removes a trailing GRACE_CALL_PROMPT user message and any synthetic
/// assistant reply that follows it. Called when a fresh user message is
/// about to be appended so the "Do NOT call any tools" instruction does
/// not shadow the new request.
fn strip_trailing_grace_prompt(history: &mut History) {
    loop {
        let msgs = history.as_slice();
        let n = msgs.len();
        if n == 0 {
            break;
        }
        let last = &msgs[n - 1];
        let last_is_grace = matches!(last.role, craft_providers::Role::User)
            && last.content.iter().any(|b| {
                matches!(b, craft_providers::ContentBlock::Text { text } if text == GRACE_CALL_PROMPT)
            });
        if last_is_grace {
            history.truncate(n - 1);
            continue;
        }
        if matches!(last.role, craft_providers::Role::Assistant) && n >= 2 {
            let prev = &msgs[n - 2];
            let prev_is_grace = matches!(prev.role, craft_providers::Role::User)
                && prev.content.iter().any(|b| {
                    matches!(b, craft_providers::ContentBlock::Text { text } if text == GRACE_CALL_PROMPT)
                });
            if prev_is_grace {
                history.truncate(n - 2);
                continue;
            }
        }
        break;
    }
}

/// Parse a `shift` tool's `ToolResult` content text back into the
/// `ShiftTurnType` sentinel. The wire shape is produced by
/// `ToolOutput::ShiftTurnType::as_display_text`:
/// `{"shift":{"target":"scout","rationale":"..."}}`. Returns `None` on any
/// parse failure (the boundary logic then treats it as "no shift").
fn parse_shift_output(text: &str) -> Option<crate::types::ToolOutput> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let shift = value.get("shift")?;
    let target_str = shift.get("target")?.as_str()?;
    let target = crate::agent::turn_type::TurnType::parse(target_str)?;
    let rationale = shift
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(crate::types::ToolOutput::ShiftTurnType { target, rationale })
}

/// What the run should do with an advisor note at a natural stop.
enum AdvisorTurnAction {
    /// Inject the note and run a follow-up turn.
    Continue(super::advisor::AdvisorNote),
    /// Stop the run: no note, below the act threshold, budget exhausted, or a
    /// goal-approval gate is pending.
    Stop,
}

/// Decide whether an actionable advisor note drives a follow-up turn. Pure so
/// the branch logic (severity threshold, budget, approval gate) is testable
/// without a live provider.
fn advisor_turn_action(
    note: Option<super::advisor::AdvisorNote>,
    cfg: &craft_config::AdvisorConfig,
    pending_approval: bool,
    continuations: u32,
) -> AdvisorTurnAction {
    let Some(note) = note else {
        return AdvisorTurnAction::Stop;
    };
    if pending_approval
        || continuations >= cfg.max_act_turns
        || !super::advisor::should_act(note.severity, cfg.auto_act)
    {
        return AdvisorTurnAction::Stop;
    }
    AdvisorTurnAction::Continue(note)
}

/// Build the synthetic user message that hands an actionable advisor note to the
/// agent so its next turn sees it. `display_text` is empty, matching the
/// compaction-continuation precedent, so the note is hidden from the chat view
/// (the visible flag still comes from `AgentEvent::AdvisorNote`).
fn advisor_followup_message(note: &super::advisor::AdvisorNote) -> Message {
    Message::synthetic(
        ADVISOR_FOLLOWUP_PROMPT
            .replace("{severity}", note.severity.as_str())
            .replace("{note}", &note.message),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use craft_providers::provider::Provider;
    use craft_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
    use crate::AdvisorSeverity;
    use crate::Envelope;
    use crate::permissions::PermissionManager;

    struct MockInterruptSource {
        commands: Mutex<VecDeque<ExtractedCommand>>,
    }

    impl MockInterruptSource {
        fn new(commands: Vec<ExtractedCommand>) -> Arc<Self> {
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

    struct MockProvider {
        responses: Mutex<Vec<StreamResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<StreamResponse>) -> Self {
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

    fn default_model() -> Model {
        Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap()
    }

    fn text_response(stop_reason: StopReason) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "response".into(),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(stop_reason),
        }
    }

    fn empty_response() -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::EndTurn),
        }
    }

    fn make_agent_params() -> AgentParams {
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
            )),
            session_id: None,
            timeouts: craft_providers::Timeouts::default(),
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
            subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
            registry: Arc::new(crate::tools::ToolRegistry::with_natives()),
            compression: craft_config::CompressionConfig::default(),
            findings_store: None,
            fs: Arc::new(crate::tools::LocalFs),
            doom: Arc::new(std::sync::Mutex::new(crate::agent::doom::DoomTracker::new())),
            flow_thread_history: None,
            flow_thread_manager: None,
            flow_advisor: None,
            flow_progress_tx: None,
        }
    }

    fn make_run_params(history: &mut History) -> (AgentRunParams<'_>, flume::Receiver<Envelope>) {
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

    fn default_input() -> AgentInput {
        AgentInput {
            message: "hello".into(),
            mode: AgentMode::Build,
            ..Default::default()
        }
    }

    fn drain_events(rx: &flume::Receiver<Envelope>) -> Vec<Envelope> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    async fn run_agent(provider: MockProvider) -> (u32, Option<StopReason>) {
        let mut history = History::new(Vec::new());
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(provider);
        let agent = Agent::new(params, run_params);
        let _ = agent.run(default_input()).await;
        drain_events(&event_rx)
            .into_iter()
            .find_map(|e| match e.event {
                AgentEvent::Done {
                    num_turns,
                    stop_reason,
                    ..
                } => Some((num_turns, stop_reason)),
                _ => None,
            })
            .expect("expected Done event")
    }

    fn has_event(events: &[Envelope], predicate: impl Fn(&AgentEvent) -> bool) -> bool {
        events.iter().any(|e| predicate(&e.event))
    }

    fn has_interrupt_in_history(history: &[Message]) -> bool {
        history.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.contains("<user-interrupt>")),
            )
        })
    }

    fn tool_call_response(tool_name: &str, tool_id: &str) -> StreamResponse {
        StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: tool_id.into(),
                    name: tool_name.into(),
                    input: serde_json::json!({"pattern": "*.nonexistent_test_xyz", "path": "/tmp"}),
                }],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        }
    }

    fn small_context_model(context_window: u32, max_output_tokens: u32) -> Model {
        let mut model = default_model();
        model.context_window = context_window;
        model.max_output_tokens = Some(max_output_tokens);
        model
    }

    #[track_caller]
    fn assert_ends_with_cancel_marker(history: &History) {
        let last = history.as_slice().last().unwrap();
        assert!(matches!(last.role, Role::User));
        assert!(
            matches!(&last.content[0], ContentBlock::Text { text } if text == "[Cancelled by user]")
        );
    }

    #[test_case(&[StopReason::EndTurn],                                                     1, Some(StopReason::EndTurn)  ; "end_turn_completes")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn],                                 2, Some(StopReason::EndTurn)  ; "max_tokens_continues")]
    #[test_case(&[StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens, StopReason::MaxTokens], 4, Some(StopReason::MaxTokens) ; "max_tokens_gives_up_after_limit")]
    #[tokio::test]
    async fn turn_counting(
        stops: &[StopReason],
        expected_turns: u32,
        expected_stop: Option<StopReason>,
    ) {
        let responses: Vec<_> = stops.iter().map(|s| text_response(*s)).collect();
        let provider = MockProvider::new(responses);
        let (turns, stop_reason) = run_agent(provider).await;
        assert_eq!(turns, expected_turns);
        assert_eq!(stop_reason, expected_stop);
    }

    #[test_case(Some(true),  true,  true  ; "after_tool_use_turn")]
    #[test_case(Some(false), true,  true  ; "after_text_only_turn")]
    #[test_case(None,        false, false ; "channel_empty")]
    #[tokio::test]
    async fn interrupt_handling(
        queued: Option<bool>,
        expect_consumed: bool,
        expect_injected: bool,
    ) {
        let source = if queued.is_some() {
            Some(MockInterruptSource::new(vec![ExtractedCommand::Interrupt(
                default_input(),
                0,
            )]))
        } else {
            None
        };

        let tool_use = queued.unwrap_or(true);
        let responses = if tool_use {
            vec![
                tool_call_response("glob", "t1"),
                text_response(StopReason::EndTurn),
            ]
        } else {
            vec![
                text_response(StopReason::EndTurn),
                text_response(StopReason::EndTurn),
            ]
        };

        let mut history = History::new(Vec::new());
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(responses));
        let agent = Agent::new(params, run_params);
        let agent = match source {
            Some(s) => agent.with_interrupt_source(s),
            None => agent,
        };
        let result = agent.run(default_input()).await;

        let events = drain_events(&event_rx);

        assert_eq!(
            has_event(&events, |e| matches!(
                e,
                AgentEvent::QueueItemConsumed { .. }
            )),
            expect_consumed,
        );
        assert_eq!(
            has_interrupt_in_history(history.as_slice()),
            expect_injected
        );
        let _ = result;
    }

    #[test_case(
        (0..10).map(|i| Message::user(format!("msg {i}"))).collect(),
        vec![ExtractedCommand::Compact(0)],
        vec![tool_call_response("glob", "t1"), text_response(StopReason::EndTurn), text_response(StopReason::EndTurn)]
        ; "compaction_via_interrupt_source"
    )]
    #[tokio::test]
    async fn compaction_through_interrupt(
        prior: Vec<Message>,
        commands: Vec<ExtractedCommand>,
        responses: Vec<StreamResponse>,
    ) {
        let source = MockInterruptSource::new(commands);

        let mut history = History::new(prior);
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(responses));
        let agent = Agent::new(params, run_params).with_interrupt_source(source);
        let result = agent.run(default_input()).await;

        assert!(result.is_ok());
    }

    #[test_case(true,  170_000, true  ; "enabled_and_over_threshold")]
    #[test_case(true,  150_000, false ; "enabled_but_below_threshold")]
    #[test_case(false, 170_000, false ; "disabled_even_over_threshold")]
    #[tokio::test]
    async fn try_auto_compact_behavior(enabled: bool, context_size: u32, expected: bool) {
        let responses = if expected {
            vec![text_response(StopReason::EndTurn)]
        } else {
            vec![]
        };
        let mut history = History::new(vec![Message::user("go".into())]);
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(responses));
        let mut agent = Agent::new(params, run_params);
        agent.model = Arc::new(small_context_model(200_000, 8_192));
        agent.auto_compact = enabled;
        agent.context_size = context_size;

        let usage = TokenUsage {
            input: context_size,
            ..Default::default()
        };
        let result = agent.try_auto_compact(&usage, false).await.unwrap();

        assert_eq!(result, expected);
        drop(agent);
        assert_eq!(
            has_event(&drain_events(&event_rx), |e| matches!(
                e,
                AgentEvent::AutoCompacting
            )),
            expected,
        );
    }

    #[tokio::test]
    async fn cancel_token_aborts_during_api_call() {
        struct HangingProvider;
        #[async_trait]
        impl Provider for HangingProvider {
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
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn list_models(&self) -> Result<Vec<String>, AgentError> {
                unimplemented!()
            }
        }

        let (trigger, cancel) = CancelToken::new();
        trigger.cancel();

        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(HangingProvider);
        let agent = Agent::new(params, run_params).with_cancel(cancel);

        let result = agent.run(default_input()).await;
        assert!(matches!(result, Err(AgentError::Cancelled)));
        assert_ends_with_cancel_marker(&history);
    }

    #[test_case(
        vec![tool_call_response("nonexistent_tool_xyz", "t1"), text_response(StopReason::EndTurn)],
        "t1"
        ; "parse_error"
    )]
    #[test_case(
        vec![tool_call_response("glob", "t1"), tool_call_response("glob", "t2"), tool_call_response("glob", "t3"), text_response(StopReason::EndTurn)],
        "t3"
        ; "doom_loop"
    )]
    #[tokio::test]
    async fn error_emits_tool_done_event(responses: Vec<StreamResponse>, expected_error_id: &str) {
        let mut history = History::new(Vec::new());
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(responses));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(default_input()).await;
        let events = drain_events(&event_rx);

        assert!(has_event(&events, |e| matches!(
            e,
            AgentEvent::ToolDone(done) if done.is_error && done.id == expected_error_id
        )));
    }

    struct PanickingProvider;
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

    fn vcc_overflow_history() -> Vec<Message> {
        let mut msgs = Vec::new();
        for i in 0..6 {
            msgs.push(Message::user(format!("do task {i}")));
            msgs.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("t{i}"),
                    name: "bash".into(),
                    input: serde_json::json!({"command": format!("echo step{i}")}),
                }],
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

    #[tokio::test]
    async fn do_compact_uses_vcc_and_skips_llm_when_under_limit() {
        let mut history = History::new(vcc_overflow_history());
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(PanickingProvider);
        let mut agent = Agent::new(params, run_params);
        agent.do_compact().await.unwrap();
        drop(event_rx);
        let msgs = agent.history.as_slice();
        assert!(matches!(msgs[0].role, Role::Assistant));
        assert!(msgs.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            ContentBlock::Text { text } if text.starts_with("This summary captures")
        ))));
        assert!(msgs.len() > 1, "tail must be preserved");
    }

    #[tokio::test]
    async fn do_compact_falls_back_to_llm_when_vcc_insufficient() {
        let mut history = History::new(vcc_overflow_history());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        params.model = {
            let mut m = default_model();
            m.context_window = 1;
            m
        };
        let mut agent = Agent::new(params, run_params);
        agent.do_compact().await.unwrap();
        let msgs = agent.history.as_slice();
        assert_eq!(
            msgs.len(),
            3,
            "expected [user, assistant, continue-synthetic]"
        );
        assert!(matches!(msgs[0].role, Role::User));
        assert!(matches!(msgs[1].role, Role::Assistant));
    }

    async fn run_nudge(responses: Vec<StreamResponse>) -> (Vec<Envelope>, Option<u32>) {
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
        (events, done)
    }

    #[tokio::test]
    async fn nudge_on_empty_after_tools() {
        let (events, done) = run_nudge(vec![
            tool_call_response("glob", "t1"),
            empty_response(),
            text_response(StopReason::EndTurn),
        ])
        .await;
        assert!(has_event(&events, |e| matches!(e, AgentEvent::Nudge)));
        assert_eq!(done.expect("expected Done event"), 3);
    }

    #[tokio::test]
    async fn no_nudge_when_text_after_tools() {
        let (events, done) = run_nudge(vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ])
        .await;
        assert!(!has_event(&events, |e| matches!(e, AgentEvent::Nudge)));
        assert_eq!(done.expect("expected Done event"), 2);
    }

    #[tokio::test]
    async fn no_nudge_without_recent_tools() {
        let (events, done) =
            run_nudge(vec![empty_response(), text_response(StopReason::EndTurn)]).await;
        assert!(!has_event(&events, |e| matches!(e, AgentEvent::Nudge)));
        assert_eq!(done.expect("expected Done event"), 1);
    }

    #[tokio::test]
    async fn try_auto_compact_calibrates_multiplier_on_overflow() {
        let mut history = History::new(vec![Message::user("go".into())]);
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        let mut agent = Agent::new(params, run_params);
        agent.model = Arc::new(small_context_model(200_000, 8_192));
        agent.context_size = 10_000;

        let usage = TokenUsage::default();
        let _ = agent.try_auto_compact(&usage, true).await.unwrap();

        assert_eq!(
            agent.token_estimation_multiplier, 5.0,
            "multiplier should be capped at MAX_TOKEN_ESTIMATION_MULTIPLIER"
        );
    }

    // ---- Flow mode shift tests (plan §13) ----

    fn shift_tool_call(tool_id: &str, target: &str, rationale: &str) -> StreamResponse {
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
                    },
                ],
                ..Default::default()
            },
            usage: TokenUsage::default(),
            stop_reason: Some(StopReason::ToolUse),
        }
    }

    fn tmp_flow_store() -> (tempfile::TempDir, Arc<craft_storage::flow::FlowStore>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = Arc::new(craft_storage::flow::FlowStore::from_root(
            tmp.path().to_path_buf(),
        ));
        (tmp, store)
    }

    fn flow_agent_params(
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
            )),
            session_id: None,
            timeouts: craft_providers::Timeouts::default(),
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
            subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
            registry: Arc::new(crate::tools::ToolRegistry::with_natives()),
            compression: craft_config::CompressionConfig::default(),
            findings_store: None,
            fs: Arc::new(crate::tools::LocalFs),
            doom: Arc::new(std::sync::Mutex::new(crate::agent::doom::DoomTracker::new())),
            flow_thread_history: Some(state.thread_history),
            flow_thread_manager: Some(state.thread_manager),
            flow_advisor: Some(state.advisor),
            flow_progress_tx: Some(progress_tx),
        }
    }

    fn flow_input() -> AgentInput {
        AgentInput {
            message: "please flow".into(),
            mode: AgentMode::Flow("test-workstream".into()),
            ..Default::default()
        }
    }

    /// A scripted Flow run that emits a `shift` to `Scout` shifts, emits
    /// `FlowProgress::TurnTypeEntered { turn_type: Scout }`, and continues.
    /// This is the test that proves the architecture works end-to-end
    /// without an orchestrator (plan §13, acceptance criterion 6).
    #[tokio::test]
    async fn flow_shift_to_scout_emits_turn_type_entered() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store, ptx);
        // Turn 1 (General): shift to scout. Turn 2 (Scout): bare EndTurn —
        // with ShiftOut this hands control back to general. Turn 3 (General):
        // EndTurn ends the run.
        params.provider = Arc::new(MockProvider::new(vec![
            shift_tool_call("t1", "scout", "need a codebase map"),
            text_response(StopReason::EndTurn),
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        let events = drain_events(&event_rx);
        let progress: Vec<_> = prx.try_iter().collect();
        // The shift to Scout was accepted and emitted TurnTypeEntered.
        assert!(
            progress.iter().any(|p| matches!(
                p,
                crate::agent::flow_loop::FlowProgress::TurnTypeEntered {
                    turn_type: crate::agent::turn_type::TurnType::Scout,
                    ..
                }
            )),
            "expected TurnTypeEntered(Scout) in progress: {progress:?}"
        );
        // The run terminated cleanly.
        assert!(
            events
                .iter()
                .any(|e| matches!(e.event, AgentEvent::Done { .. }))
        );
    }

    /// A scripted Flow run with no shift stays `General` (no TurnTypeEntered).
    #[tokio::test]
    async fn flow_no_shift_stays_general() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store, ptx);
        params.provider = Arc::new(MockProvider::new(vec![
            text_response(StopReason::EndTurn),
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        let progress: Vec<_> = prx.try_iter().collect();
        assert!(
            !progress.iter().any(|p| matches!(
                p,
                crate::agent::flow_loop::FlowProgress::TurnTypeEntered { .. }
            )),
            "no shift should produce no TurnTypeEntered: {progress:?}"
        );
    }

    /// A narrow turn that ends with EndTurn (no tool calls, no shift) emits
    /// `TurnOutcome::ShiftOut`: the loop commits the turn's write, shifts back
    /// to `general`, and resumes. The run does not end at the narrow EndTurn;
    /// it ends only when `general` subsequently ends. Here Scout ends with a
    /// bare EndTurn; the run must continue into a General turn.
    #[tokio::test]
    async fn flow_narrow_endturn_shifts_out_to_general() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store, ptx);
        params.provider = Arc::new(MockProvider::new(vec![
            // Turn 1 (General): shift to scout.
            shift_tool_call("t1", "scout", "map it"),
            // Turn 2 (Scout): bare EndTurn -> ShiftOut hands control to general.
            text_response(StopReason::EndTurn),
            // Turn 3 (General): EndTurn ends the run.
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        let progress: Vec<_> = prx.try_iter().collect();
        // Scout was entered, then general was re-entered via ShiftOut.
        let entered: Vec<_> = progress
            .iter()
            .filter_map(|p| match p {
                crate::agent::flow_loop::FlowProgress::TurnTypeEntered { turn_type, .. } => {
                    Some(*turn_type)
                }
                _ => None,
            })
            .collect();
        assert!(
            entered
                == vec![
                    crate::agent::turn_type::TurnType::Scout,
                    crate::agent::turn_type::TurnType::General,
                ],
            "ShiftOut should re-enter general after the Scout EndTurn: {entered:?}"
        );
    }

    /// A scripted Flow run where the model shifts to a type not in the
    /// current type's `transitions` gets an `Illegal` message and stays.
    /// Scout declares only `tpm` and `general`; shifting Scout to `execute`
    /// is illegal.
    #[tokio::test]
    async fn flow_illegal_shift_pushes_message_and_stays() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, _prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store, ptx);
        params.provider = Arc::new(MockProvider::new(vec![
            // Turn 1 (General): shift to scout (legal).
            shift_tool_call("t1", "scout", "map it"),
            // Turn 2 (Scout): try to skip straight to execute — illegal from
            // scout, so the shift is rejected and the scout turn continues.
            shift_tool_call("t2", "execute", "skip ahead"),
            // Turn 3 (Scout): bare EndTurn -> ShiftOut to general.
            text_response(StopReason::EndTurn),
            // Turn 4 (General): EndTurn ends the run.
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        // The illegal-shift message should be in the chat history.
        assert!(
            history.as_slice().iter().any(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text { text } if text.starts_with("Illegal shift")))
            }),
            "expected an Illegal shift message in history"
        );
    }

    /// The `Tpm -> Plan` goal-approval gate emits `FlowProgress::GoalReady`,
    /// ends the run with `StopReason::AwaitingGoalApproval`, and leaves the
    /// thread on `Tpm` (the shift to Plan must NOT advance). Plan §7.
    #[tokio::test]
    async fn flow_tpm_to_plan_emits_goal_ready_and_awaits_approval() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, event_rx) = make_run_params(&mut history);
        // Seed: the agent is already in Tpm (we shift Tpm in the first turn,
        // then the second turn is the Tpm goal text + shift to Plan).
        let mut params = flow_agent_params(store, ptx);
        params.provider = Arc::new(MockProvider::new(vec![
            shift_tool_call("t1", "tpm", "shape the goal"),
            // Tpm turn: emit the goal text, then shift to Plan.
            StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "# Goal\n\nShip login with SSO.".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "t2".into(),
                            name: "shift".into(),
                            input: serde_json::json!({
                                "target": "plan",
                                "rationale": "goal ready",
                            }),
                        },
                    ],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::ToolUse),
            },
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        let progress: Vec<_> = prx.try_iter().collect();
        assert!(
            progress
                .iter()
                .any(|p| matches!(p, crate::agent::flow_loop::FlowProgress::GoalReady { .. })),
            "expected GoalReady in progress: {progress:?}"
        );
        let events = drain_events(&event_rx);
        let awaiting = events.iter().any(|e| {
            matches!(
                e.event,
                AgentEvent::Done {
                    stop_reason: Some(StopReason::AwaitingGoalApproval),
                    ..
                }
            )
        });
        assert!(awaiting, "expected Done(AwaitingGoalApproval)");
        // The shift to Plan must NOT have advanced: no TurnTypeEntered(Plan)
        // should have been emitted (the gate ends the run before the shift).
        assert!(
            !progress.iter().any(|p| matches!(
                p,
                crate::agent::flow_loop::FlowProgress::TurnTypeEntered {
                    turn_type: crate::agent::turn_type::TurnType::Plan,
                    ..
                }
            )),
            "turn_type must stay Tpm at the gate, but Plan was entered"
        );
    }

    /// Resuming after the goal-approval gate via `with_flow_resume(_, Plan)`
    /// re-enters the agent as `plan`: it emits `TurnTypeEntered(Plan)` (so the
    /// host's stage display updates from `tpm` to `plan`) and leaves the agent
    /// in `plan` for the next turn. Without the resume stage the agent restarts
    /// in `general` and the model often skips writing a plan.
    #[tokio::test]
    async fn flow_resume_after_gate_enters_plan() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store.clone(), ptx);
        // First run: General shifts to Tpm; Tpm writes the goal and shifts to
        // Plan, tripping the gate (GoalReady + AwaitingGoalApproval).
        params.provider = Arc::new(MockProvider::new(vec![
            shift_tool_call("t1", "tpm", "shape the goal"),
            StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "# Goal\n\nShip login with SSO.".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "t2".into(),
                            name: "shift".into(),
                            input: serde_json::json!({
                                "target": "plan",
                                "rationale": "goal ready",
                            }),
                        },
                    ],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::ToolUse),
            },
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;
        // Drain the gate run's progress; Plan must NOT have been entered yet.
        let gate_progress: Vec<_> = prx.try_iter().collect();
        assert!(
            !gate_progress.iter().any(|p| matches!(
                p,
                crate::agent::flow_loop::FlowProgress::TurnTypeEntered {
                    turn_type: crate::agent::turn_type::TurnType::Plan,
                    ..
                }
            )),
            "gate must not enter Plan: {gate_progress:?}"
        );

        // Resume run: re-open the typed log and re-enter as Plan.
        let (ptx2, prx2) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let (run_params2, _event_rx2) = make_run_params(&mut history);
        let mut params2 = flow_agent_params(store, ptx2);
        params2.provider = Arc::new(MockProvider::new(vec![
            // Plan turn writes the plan doc and ends (no shift).
            StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "# Plan\n\n1. Add SSO flow.".into(),
                    }],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::EndTurn),
            },
            text_response(StopReason::EndTurn),
        ]));
        let resume_input = flow_input().with_flow_resume(
            crate::FLOW_APPROVE_ANSWER.into(),
            crate::agent::turn_type::TurnType::Plan,
        );
        let agent2 = Agent::new(params2, run_params2);
        let _ = agent2.run(resume_input).await;

        let resume_progress: Vec<_> = prx2.try_iter().collect();
        assert!(
            resume_progress.iter().any(|p| matches!(
                p,
                crate::agent::flow_loop::FlowProgress::TurnTypeEntered {
                    turn_type: crate::agent::turn_type::TurnType::Plan,
                    ..
                }
            )),
            "resume must emit TurnTypeEntered(Plan): {resume_progress:?}"
        );
    }

    /// A shift into a narrow type pushes a synthetic stage brief naming the
    /// type, its write commitment, and its legal next shifts. Verifies the
    /// brief is data-driven from the `TurnTypeSpec` and lands in chat history.
    #[tokio::test]
    async fn flow_shift_into_narrow_type_pushes_stage_brief() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, _prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store, ptx);
        // Turn 1 (General): shift to scout. Turn 2 (Scout): bare EndTurn —
        // with ShiftOut this hands control back to general. Turn 3 (General):
        // EndTurn ends the run.
        params.provider = Arc::new(MockProvider::new(vec![
            shift_tool_call("t1", "scout", "need a codebase map"),
            text_response(StopReason::EndTurn),
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        let brief = history.as_slice().iter().rev().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text }
                    if text.starts_with("You are now in the `scout` turn type") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
        });
        let brief = brief.expect("expected a scout stage brief in history");
        assert!(
            brief.contains("codebase_context"),
            "brief should name the write entry: {brief}"
        );
        assert!(
            brief.contains("`tpm`"),
            "brief should list the legal next shift to tpm: {brief}"
        );
    }

    /// The stage brief inlines persisted core-read entries so the new type
    /// starts with its obvious context rather than re-querying. TPM reads
    /// `codebase_context` at `Own`; after a Scout turn commits one, the TPM
    /// brief should surface that committed entry.
    #[tokio::test]
    async fn flow_stage_brief_inlines_core_reads() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, _prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = flow_agent_params(store, ptx);
        let scout_findings = "the codebase has 3 crates keyed off craft-agent";
        params.provider = Arc::new(MockProvider::new(vec![
            // Turn 1 (General): shift into Scout.
            shift_tool_call("t1", "scout", "map it"),
            // Turn 2 (Scout): emit findings, then shift to Tpm. The boundary
            // after this turn commits the scout write (CodebaseContext) and
            // advances to Tpm, pushing the Tpm brief.
            StreamResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: scout_findings.into(),
                        },
                        ContentBlock::ToolUse {
                            id: "t2".into(),
                            name: "shift".into(),
                            input: serde_json::json!({
                                "target": "tpm",
                                "rationale": "shape the goal",
                            }),
                        },
                    ],
                    ..Default::default()
                },
                usage: TokenUsage::default(),
                stop_reason: Some(StopReason::ToolUse),
            },
            // Turn 3 (Tpm): bare EndTurn — with ShiftOut this hands control
            // back to general. Turn 4 (General): EndTurn ends the run.
            text_response(StopReason::EndTurn),
            text_response(StopReason::EndTurn),
        ]));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(flow_input()).await;

        let tpm_brief = history.as_slice().iter().rev().find_map(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text }
                    if text.starts_with("You are now in the `tpm` turn type") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
        });
        let tpm_brief = tpm_brief.expect("expected a tpm stage brief");
        assert!(
            tpm_brief.contains(scout_findings),
            "tpm brief should inline the scout's committed codebase_context: {tpm_brief}"
        );
        assert!(
            tpm_brief.contains("Acceptance criteria"),
            "tpm brief should surface the goal-doc guidance: {tpm_brief}"
        );
        assert!(
            tpm_brief.contains("Context (from the typed log)"),
            "tpm brief should render a context section: {tpm_brief}"
        );
    }

    /// `parse_shift_output` round-trips the wire shape produced by
    /// `ToolOutput::ShiftTurnType::as_display_text`.
    #[test]
    fn parse_shift_output_round_trips() {
        let original = crate::types::ToolOutput::ShiftTurnType {
            target: crate::agent::turn_type::TurnType::Plan,
            rationale: "goal approved".into(),
        };
        let text = original.as_display_text();
        let parsed = parse_shift_output(&text).expect("parse");
        match parsed {
            crate::types::ToolOutput::ShiftTurnType { target, rationale } => {
                assert_eq!(target, crate::agent::turn_type::TurnType::Plan);
                assert_eq!(rationale, "goal approved");
            }
            other => panic!("expected ShiftTurnType, got {other:?}"),
        }
    }

    /// A Flow-mode Agent auto-wires a `flow_search` backend against its
    /// `ThreadHistory` so a resuming agent can read its own past entries
    /// (plan §7). Pre-populate the typed log with a Goal, build the agent,
    /// and confirm `tool_context().flow_search` returns the goal for a
    /// "goal" query. Outside Flow mode the handle is `None`.
    #[tokio::test]
    async fn flow_search_returns_persisted_entries_after_resume() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, _prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        // Seed the typed log with a Goal entry against the root thread before
        // building the agent, simulating a resume.
        {
            let hist = std::sync::Arc::new(std::sync::Mutex::new(
                crate::agent::typed_log::ThreadHistory::open(
                    Arc::clone(&store),
                    "test-project",
                    "test-workstream",
                ),
            ));
            hist.lock().unwrap().append(
                crate::agent::typed_log::ThreadId::new("test-workstream"),
                crate::agent::typed_log::EntryType::Goal,
                "ship the login flow with these acceptance criteria",
            );
        }
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let params = flow_agent_params(store, ptx);
        let agent = Agent::new(params, run_params);
        let ctx = agent.tool_context();
        let backend = ctx
            .flow_search
            .as_ref()
            .expect("flow_search auto-wired in Flow mode");
        let hits = backend
            .search("test-project", "test-workstream", "login goal", 5)
            .await
            .unwrap();
        assert!(
            hits.iter().any(|h| h.path.starts_with("goal:")),
            "expected a goal hit, got: {hits:?}"
        );
    }

    #[test]
    fn parse_shift_output_returns_none_on_garbage() {
        assert!(parse_shift_output("not json").is_none());
        assert!(parse_shift_output("{}").is_none());
        assert!(parse_shift_output(r#"{"shift":{"target":"unknown","rationale":""}}"#).is_none());
    }

    /// A Flow-mode Agent's `tool_context` carries the live thread manager,
    /// thread id, typed log, and progress channel so the `task` tool can
    /// register child threads (Item 2).
    #[tokio::test]
    async fn flow_tool_context_exposes_thread_handles() {
        let (_tmp, store) = tmp_flow_store();
        let (ptx, _prx) = flume::unbounded::<crate::agent::flow_loop::FlowProgress>();
        let mut history = History::new(Vec::new());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let params = flow_agent_params(store, ptx);
        let agent = Agent::new(params, run_params);
        let ctx = agent.tool_context();
        assert!(ctx.flow_thread_manager.is_some());
        assert!(ctx.flow_thread_id.is_some());
        assert!(ctx.flow_thread_history.is_some());
        assert!(ctx.flow_progress_tx.is_some());
    }

    fn advisor_note(
        severity: AdvisorSeverity,
        message: &str,
    ) -> super::super::advisor::AdvisorNote {
        super::super::advisor::AdvisorNote {
            severity,
            message: message.into(),
        }
    }

    fn advisor_cfg(
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

    #[test_case(None,        craft_config::AdvisorAutoAct::Concern, 2, 0, false, false ; "no_note_stops")]
    #[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Off, 2, 0, false, false ; "off_threshold_stops")]
    #[test_case(Some(AdvisorSeverity::Nit), craft_config::AdvisorAutoAct::Concern, 2, 0, false, false ; "below_threshold_stops")]
    #[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 0, false, true  ; "blocker_above_concern_continues")]
    #[test_case(Some(AdvisorSeverity::Concern), craft_config::AdvisorAutoAct::Concern, 2, 0, false, true  ; "concern_at_threshold_continues")]
    #[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 0, 0, false, false ; "zero_budget_stops")]
    #[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 2, false, false ; "exhausted_budget_stops")]
    #[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 1, false, true  ; "budget_remaining_continues")]
    #[test_case(Some(AdvisorSeverity::Blocker), craft_config::AdvisorAutoAct::Concern, 2, 0, true,  false ; "pending_approval_stops")]
    fn advisor_turn_action_decision(
        note: Option<AdvisorSeverity>,
        auto_act: craft_config::AdvisorAutoAct,
        max_act_turns: u32,
        continuations: u32,
        pending_approval: bool,
        expect_continue: bool,
    ) {
        let note = note.map(|s| advisor_note(s, "real bug"));
        let cfg = advisor_cfg(auto_act, max_act_turns);
        let action = advisor_turn_action(note, &cfg, pending_approval, continuations);
        assert_eq!(
            matches!(action, AdvisorTurnAction::Continue(_)),
            expect_continue,
            "continuation mismatch"
        );
    }

    #[test]
    fn advisor_turn_action_continues_with_note() {
        let note = advisor_note(AdvisorSeverity::Blocker, "leaks secret");
        let cfg = advisor_cfg(craft_config::AdvisorAutoAct::Concern, 2);
        let action = advisor_turn_action(Some(note), &cfg, false, 0);
        let AdvisorTurnAction::Continue(returned) = action else {
            panic!("expected Continue");
        };
        assert_eq!(returned.severity, AdvisorSeverity::Blocker);
        assert_eq!(returned.message, "leaks secret");
    }

    #[test]
    fn advisor_followup_message_carries_note_and_is_hidden() {
        let note = advisor_note(AdvisorSeverity::Concern, "missing error handling");
        let msg = advisor_followup_message(&note);
        assert!(matches!(msg.role, Role::User));
        let text = msg.first_text_content().unwrap();
        assert!(text.contains("<advisor-note>"));
        assert!(text.contains("concern"));
        assert!(text.contains("missing error handling"));
        // Empty display_text hides the synthetic injection from the chat view.
        assert_eq!(msg.display_text.as_deref(), Some(""));
    }
}
