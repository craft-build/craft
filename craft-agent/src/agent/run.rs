use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{error, info, warn};

use craft_providers::provider::Provider;
use craft_providers::{Message, Model, RequestOptions, StopReason, StreamResponse, TokenUsage};

use super::compaction::{self, CONTINUE_AFTER_COMPACT};
use super::dedup::ToolDedupCache;
use super::doom::SharedDoomTracker;
use super::escalation::EscalationTracker;
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

const MAX_REAUTH_ATTEMPTS: u32 = 2;
const HOOK_BEST_EFFORT_TIMEOUT: Duration = Duration::from_secs(5);
const GRACE_CALL_PROMPT: &str = "Your recent actions look like a doom-loop (repeated calls, errors, or stagnation). Summarize your progress so far and tell the user what still needs to be done. Do NOT call any tools.";
const DEFAULT_SMALL_MODEL_RATIO: f64 = 0.60;
const INEFFECTIVE_COMPACTION_THRESHOLD: f32 = 0.1;
#[cfg(feature = "onnx")]
const MANDATORY_RECENT_MESSAGES: usize = 6;
#[cfg(feature = "onnx")]
const STAGNATION_WINDOW_SIZE: usize = 5;
#[cfg(feature = "onnx")]
const STAGNATION_SIMILARITY_THRESHOLD: f32 = 0.85;

enum TurnOutcome {
    Continue,
    Done(Option<StopReason>),
    Overflow,
}

pub struct AgentParams {
    pub provider: Arc<dyn Provider>,
    pub model: Model,
    pub config: AgentConfig,
    pub tool_output_lines: ToolOutputLines,
    pub permissions: Arc<PermissionManager>,
    pub session_id: Option<String>,
    pub timeouts: craft_providers::Timeouts,
    pub file_tracker: Arc<FileReadTracker>,
    pub prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    pub subagent_cancels: Arc<CancelMap<String>>,
    pub compression: craft_config::CompressionConfig,
    pub findings_store: Option<super::findings_store::SharedFindingsStore>,
    pub fs: Arc<dyn crate::tools::FsBackend>,
    pub doom: SharedDoomTracker,
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
    user_response_rx: Option<Arc<tokio::sync::Mutex<flume::Receiver<String>>>>,
    interrupt_source: Option<Arc<dyn InterruptSource>>,
    cancel: CancelToken,
    total_usage: TokenUsage,
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
    permissions: Arc<PermissionManager>,
    opts: RequestOptions,
    session_id: Option<String>,
    timeouts: craft_providers::Timeouts,
    file_tracker: Arc<FileReadTracker>,
    prompt_slots: Arc<crate::prompt::ResolvedSlots>,
    subagent_cancels: Arc<CancelMap<String>>,
    compression: craft_config::CompressionConfig,
    findings_store: Option<super::findings_store::SharedFindingsStore>,
    cache_tracker: super::cache::PrefixCacheTracker,
    compression_store: super::compression_store::SharedCompressionStore,
    dedup_cache: ToolDedupCache,
    trust_tracker: TrustTracker,
    snapshot: SnapshotManager,
    validator: Validator,
    escalation: EscalationTracker,
    promoted: crate::tools::PromotedTools,
    dynamic: crate::tools::DynamicContext,
    tool_build: Option<crate::tools::ToolBuild>,
    hooks: Option<Arc<dyn crate::Hooks>>,
    #[cfg(feature = "onnx")]
    scorer: Option<super::semantic::RelevanceScorer>,
    #[cfg(feature = "onnx")]
    last_relevance_scores: Option<Vec<(usize, f32)>>,
    fs: Arc<dyn crate::tools::FsBackend>,
    goal: Option<String>,
    judge_continuations: u8,
    snapshot_store: Arc<crate::tools::safety::SnapshotStore>,
}

const MAX_JUDGE_CONTINUATIONS: u8 = 5;

impl<'h> Agent<'h> {
    pub fn new(params: AgentParams, run: AgentRunParams<'h>) -> Self {
        let dynamic = crate::tools::DynamicContext::from_config(&params.config);
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
            user_response_rx: None,
            interrupt_source: None,
            cancel: CancelToken::none(),
            total_usage: TokenUsage::default(),
            num_turns: 0,
            doom: params.doom,
            guardrails: ToolGuardrails::new(),
            ineffective_compaction_count: 0,
            auto_compact: compaction::auto_compact_enabled(),
            loaded_instructions: LoadedInstructions::new(),
            rollback_len: 0,
            mcp: None,
            reauth_attempts: 0,
            opts: RequestOptions::default(),
            session_id: params.session_id,
            file_tracker: params.file_tracker,
            prompt_slots: params.prompt_slots,
            subagent_cancels: params.subagent_cancels,
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
            escalation: EscalationTracker::new(Default::default()),
            promoted: run.promoted,
            dynamic,
            tool_build: run.tool_build,
            hooks: run.hooks,
            #[cfg(feature = "onnx")]
            scorer: if params.compression.semantic_enabled {
                Some(super::semantic::RelevanceScorer::new())
            } else {
                None
            },
            #[cfg(feature = "onnx")]
            last_relevance_scores: None,
            fs: params.fs,
            goal: None,
            judge_continuations: 0,
            snapshot_store: crate::tools::safety::SnapshotStore::fresh(),
        }
    }

    pub fn with_mcp(mut self, mcp: Option<McpHandle>) -> Self {
        self.trust_tracker = TrustTracker::new(self.config.trust_decay);
        self.validator = Validator::new(
            std::env::current_dir().unwrap_or_default(),
            self.config.validation.clone(),
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

    pub fn with_interrupt_source(mut self, source: Arc<dyn InterruptSource>) -> Self {
        self.interrupt_source = Some(source);
        self
    }

    pub fn with_cancel(mut self, cancel: CancelToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn with_loaded_instructions(mut self, loaded: LoadedInstructions) -> Self {
        self.loaded_instructions = loaded;
        self
    }

    #[cfg(feature = "onnx")]
    async fn build_intent(&self) -> Option<Vec<f32>> {
        let scorer = self.scorer.as_ref()?;
        scorer.build_intent(self.history.as_slice()).await.ok()
    }

    #[cfg(feature = "onnx")]
    async fn build_semantic_view(&self, intent: &[f32]) -> Option<Vec<Message>> {
        let scorer = self.scorer.as_ref()?;
        let scores = scorer
            .score_messages(self.history.as_slice(), intent)
            .await
            .ok()?;
        let token_budget = self
            .model
            .context_window
            .saturating_sub(self.config.compaction_buffer);
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
        self.goal = input.goal;
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
                TurnOutcome::Continue => {}
                TurnOutcome::Done(stop_reason) => {
                    self.snapshot.commit();
                    self.emit_done(stop_reason)?;
                    return Ok(());
                }
                TurnOutcome::Overflow => {
                    info!("context overflow detected, attempting auto-compact and retry");
                    let usage = self.total_usage;
                    self.try_auto_compact(&usage, true).await?;
                }
            }
        }
    }

    async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        if let Some(build) = &self.tool_build {
            self.tools = crate::tools::build_active_tools(
                build,
                &self.model,
                &self.config,
                &self.dynamic,
                &self.promoted,
            );
        }

        #[cfg(feature = "onnx")]
        let intent = self.build_intent().await;

        #[cfg(feature = "onnx")]
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

        #[cfg(feature = "onnx")]
        let semantic_view: Option<Vec<Message>> = match &intent {
            Some(intent_vec) => self.build_semantic_view(intent_vec).await,
            None => None,
        };

        #[cfg(feature = "onnx")]
        let messages: &[Message] = semantic_view
            .as_deref()
            .unwrap_or_else(|| self.history.as_slice());
        #[cfg(not(feature = "onnx"))]
        let messages: &[Message] = self.history.as_slice();

        let response = match stream_with_retry(
            &*self.provider,
            &self.model,
            messages,
            &self.system,
            &self.tools,
            &self.event_tx,
            &self.cancel,
            self.opts,
            self.session_id.as_deref(),
        )
        .await
        {
            Ok(r) => {
                self.reauth_attempts = 0;
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
                error!(error = %e, model = %self.model.id, self.num_turns, "stream_message failed");
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
        self.cache_tracker.update(&usage, self.history.len());

        #[cfg(feature = "onnx")]
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
            let batch = self.process_tool_calls(response).await?;
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

        if self.try_auto_compact(&usage, false).await? || self.handle_queued_command().await? {
            return Ok(TurnOutcome::Continue);
        }

        if has_tools {
            Ok(TurnOutcome::Continue)
        } else if let Some(ref goal) = self.goal.clone() {
            self.run_goal_judge(goal, stop_reason).await
        } else {
            Ok(TurnOutcome::Done(stop_reason))
        }
    }

    async fn run_goal_judge(
        &mut self,
        goal: &str,
        stop_reason: Option<StopReason>,
    ) -> Result<TurnOutcome, AgentError> {
        if self.judge_continuations >= MAX_JUDGE_CONTINUATIONS {
            warn!(
                continuations = self.judge_continuations,
                "judge continuation cap reached, allowing stop"
            );
            return Ok(TurnOutcome::Done(stop_reason));
        }
        let outcome = super::judge::evaluate(
            goal,
            self.history.as_slice(),
            &self.provider,
            &self.model,
            self.config.judge_model.as_deref(),
            self.timeouts,
            self.session_id.as_deref(),
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
            Err(e) => {
                warn!(error = %e, "judge evaluation failed, allowing stop (fail-open)");
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
                context_size: Some(response.usage.context_tokens()),
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

    fn tool_context(&self) -> ToolContext {
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
            compression_store: Arc::clone(&self.compression_store),
            findings_store: self.findings_store.clone(),
            fs: Arc::clone(&self.fs),
            parent_messages: Arc::from(self.history.as_slice()),
            promoted: self.promoted.clone(),
            dynamic: self.dynamic.clone(),
            hooks: self.hooks.clone(),
            snapshot_store: Arc::clone(&self.snapshot_store),
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
            || compaction::is_overflow(usage, &self.model, self.config.compaction_buffer);
        let proactive = !overflow
            && compaction::is_proactive_threshold(
                self.history,
                &self.model,
                self.small_model_ratio(),
            );

        if !overflow && !proactive {
            return Ok(false);
        }

        self.dedup_cache.clear();

        #[cfg(feature = "onnx")]
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
            compaction_buffer: self.config.compaction_buffer,
            cache_tracker: Some(&self.cache_tracker),
            compression_store: Some(&self.compression_store),
            #[cfg(feature = "onnx")]
            relevance_scores: self
                .scorer
                .as_ref()
                .and(self.last_relevance_scores.as_deref()),
            #[cfg(not(feature = "onnx"))]
            relevance_scores: None,
            #[cfg(feature = "onnx")]
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
            && !compaction::is_overflow(usage, &self.model, self.config.compaction_buffer)
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
        self.total_usage += compaction::compact_history(
            &*self.provider,
            &self.model,
            self.history,
            &self.event_tx,
            &self.cancel,
            #[cfg(feature = "onnx")]
            self.last_relevance_scores.as_deref(),
            #[cfg(not(feature = "onnx"))]
            None,
        )
        .await?;
        self.rollback_len = self.history.len();
        self.history
            .push(Message::synthetic(CONTINUE_AFTER_COMPACT.into()));
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use craft_providers::provider::{BoxFuture, Provider};
    use craft_providers::{
        ContentBlock, Message, Model, ProviderEvent, RequestOptions, Role, StopReason,
        StreamResponse, TokenUsage,
    };
    use serde_json::Value;
    use test_case::test_case;

    use super::*;
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

    impl Provider for MockProvider {
        fn stream_message<'a>(
            &'a self,
            _: &'a Model,
            _: &'a [Message],
            _: &'a str,
            _: &'a Value,
            _: &'a flume::Sender<ProviderEvent>,
            _: RequestOptions,
            _: Option<&str>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                let mut responses = self.responses.lock().unwrap();
                assert!(!responses.is_empty(), "MockProvider: no more responses");
                Ok(responses.remove(0))
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
            Box::pin(async { unimplemented!() })
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

    fn make_agent_params() -> AgentParams {
        AgentParams {
            provider: Arc::new(MockProvider::new(vec![])),
            model: default_model(),
            config: AgentConfig::default(),
            tool_output_lines: ToolOutputLines::default(),
            permissions: Arc::new(PermissionManager::new(
                craft_config::PermissionsConfig {
                    allow_all: true,
                    rules: vec![],
                },
                std::path::PathBuf::from("/tmp"),
            )),
            session_id: None,
            timeouts: craft_providers::Timeouts::default(),
            file_tracker: FileReadTracker::fresh(),
            prompt_slots: Arc::new(crate::prompt::ResolvedSlots::default()),
            subagent_cancels: Arc::new(crate::cancel::CancelMap::new()),
            compression: craft_config::CompressionConfig::default(),
            findings_store: None,
            fs: Arc::new(crate::tools::LocalFs),
            doom: Arc::new(std::sync::Mutex::new(crate::agent::doom::DoomTracker::new())),
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
        model.max_output_tokens = max_output_tokens;
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

    #[test_case(true,  900, true  ; "enabled_and_over_threshold")]
    #[test_case(true,  100, false ; "enabled_but_below_threshold")]
    #[test_case(false, 900, false ; "disabled_even_over_threshold")]
    #[tokio::test]
    async fn try_auto_compact_behavior(enabled: bool, total_input: u32, expected: bool) {
        let responses = if expected {
            vec![text_response(StopReason::EndTurn)]
        } else {
            vec![]
        };
        let mut history = History::new(vec![Message::user("go".into())]);
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = Arc::new(MockProvider::new(responses));
        params.model = small_context_model(1000, 200);
        let mut agent = Agent::new(params, run_params);
        agent.model = Arc::new(small_context_model(1000, 200));
        agent.auto_compact = enabled;

        let usage = TokenUsage {
            input: total_input,
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
        impl Provider for HangingProvider {
            fn stream_message<'a>(
                &'a self,
                _: &'a Model,
                _: &'a [Message],
                _: &'a str,
                _: &'a Value,
                _: &'a flume::Sender<ProviderEvent>,
                _: RequestOptions,
                _: Option<&'a str>,
            ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
                Box::pin(async {
                    std::future::pending::<()>().await;
                    unreachable!()
                })
            }
            fn list_models(&self) -> BoxFuture<'_, Result<Vec<String>, AgentError>> {
                Box::pin(async { unimplemented!() })
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
}
