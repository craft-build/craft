use std::sync::Arc;

use craft_providers::{Message, Role, StopReason, StreamResponse, TokenUsage};
use tracing::{error, info, warn};

use crate::agent::streaming::stream_with_retry;
use crate::agent::tool_dispatch::{self, ToolBatchOutcome};
use crate::tools::Deadline;
use crate::tools::ToolContext;
use crate::{AgentError, AgentEvent, AgentMode, ExtractedCommand};

use super::{Agent, NUDGE_PROMPT, TurnOutcome};

const STAGNATION_WINDOW_SIZE: usize = 5;
const STAGNATION_SIMILARITY_THRESHOLD: f32 = 0.85;

/// A model that stalls once often stalls again on the retry, so it gets a
/// second chance before the turn ends empty handed.
const MAX_NUDGES: u32 = 2;

impl<'h> Agent<'h> {
    pub(super) async fn turn(&mut self) -> Result<TurnOutcome, AgentError> {
        if self.io.cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        if let Some(ttsr) = self.flow.ttsr.as_ref() {
            ttsr.reset_turn();
        }

        if let Some(build) = &self.tool_state.tool_build {
            self.tools = crate::tools::build_active_tools(
                build,
                &self.io.model,
                &self.config,
                &self.tool_state.dynamic,
                &self.tool_state.promoted,
            );
            if !matches!(self.flow.mode, AgentMode::Flow(_)) {
                self.tools = crate::tools::strip_flow_only_tools(&self.tools);
            }
        }

        let intent = self.build_intent().await;

        if let Some(intent_vec) = &intent
            && let Some(scorer) = &self.recency.scorer
        {
            let restored = crate::agent::semantic::auto_retrieve(
                scorer,
                &self.compaction.compression_store,
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

        let repo_map_msg = if let Some(rm) = &self.recency.repo_map {
            let context_files: Vec<String> = self
                .tool_state
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

        let recency_messages = self.attach_recency_tail(messages);
        let messages = recency_messages.as_deref().unwrap_or(messages);

        let response = match stream_with_retry(
            &*self.io.provider,
            &self.io.model,
            messages,
            &self.system,
            &self.tools,
            &self.io.event_tx,
            &self.io.cancel,
            self.io.opts,
            self.io.session_id.as_ref(),
            &self.io.fallback_chain,
            self.flow.ttsr.clone(),
            self.num_turns,
        )
        .await
        {
            Ok((r, injection)) => {
                self.io.reauth_attempts = 0;
                if let Some(reminder) = injection {
                    self.history.push(Message::synthetic(reminder));
                }
                r
            }
            Err(e) if e.is_auth_error() => {
                return self.io.wait_for_reauth(e, self.num_turns).await;
            }
            Err(e) if e.is_overflow() => {
                info!("context overflow detected, will attempt auto-compact");
                return Ok(TurnOutcome::Overflow);
            }
            Err(e) => {
                let (kind, action) = crate::agent::recovery::classify(&e);
                error!(
                    error = %e,
                    model = %self.io.model.id,
                    self.num_turns,
                    recovery_kind = ?kind,
                    recovery_action = ?action,
                    "stream_message failed",
                );
                if matches!(action, crate::agent::recovery::RecoveryAction::Escalate) {
                    let _ = self.io.event_tx.send(AgentEvent::Info {
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
            model = %self.io.model.id,
            stop_reason = stop_reason.map_or("none", Into::into),
            "API response received"
        );

        self.emit_turn_complete(&response)?;
        let usage = response.usage;
        self.total_usage += usage;
        self.context_size = usage.total_input();
        self.compaction
            .cache_tracker
            .update(&usage, self.history.len());

        if let Some(scorer) = &self.recency.scorer {
            let turn_summary = crate::agent::semantic::intent_summary(self.history.as_slice());
            if !turn_summary.is_empty()
                && let Ok(emb) = scorer.embed_text(&turn_summary).await
            {
                let mut doom = self.doom.doom.lock().unwrap_or_else(|e| e.into_inner());
                doom.turn_embeddings.push_back(emb);
                if doom.turn_embeddings.len() > STAGNATION_WINDOW_SIZE {
                    doom.turn_embeddings.pop_front();
                }
                let embeddings = doom.turn_embeddings.make_contiguous();
                if crate::agent::semantic::detect_stagnation(
                    embeddings,
                    STAGNATION_SIMILARITY_THRESHOLD,
                ) {
                    let n = embeddings.len();
                    let sim = crate::agent::semantic::RelevanceScorer::similarity(
                        &embeddings[n - 2],
                        &embeddings[n - 1],
                    );
                    info!(sim, "stagnation detected");
                    doom.note_stagnation();
                    let _ = self
                        .io
                        .event_tx
                        .send(AgentEvent::StagnationDetected { similarity: sim });
                }
            }
        }

        if has_tools {
            let history_len_before = self.history.len();
            let batch = self.process_tool_calls(response).await?;
            self.context_size += super::compaction::estimate_message_tokens(
                &self.history.as_slice()[history_len_before..],
            );
            {
                let mut doom = self.doom.doom.lock().unwrap_or_else(|e| e.into_inner());
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
            self.doom
                .escalation
                .record(&self.io.model.id, batch.had_errors());
            self.doom.escalation.check_and_emit(
                &self.io.model.id,
                crate::agent::escalation::ModelTier::from_model_id(&self.io.model.id),
                &self.io.event_tx,
            );
        } else {
            if response.message.first_text_content().is_some() {
                self.history.push(response.message);
            } else if self.recover_stalled_turn()? {
                return Ok(TurnOutcome::Continue);
            }

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
        } else if let Some(ref goal) = self.flow.goal.clone() {
            let criteria = self.flow.goal_criteria.clone();
            self.run_goal_judge(goal, &criteria, stop_reason).await
        } else if matches!(self.flow.mode, AgentMode::Flow(_))
            && self.flow.turn_type != crate::agent::turn_type::TurnType::General
        {
            // A Flow narrow turn ended (EndTurn, no tool calls). Hand control
            // back to `general` so the root owner re-derives the next step.
            Ok(TurnOutcome::ShiftOut)
        } else {
            Ok(TurnOutcome::Done(stop_reason))
        }
    }

    /// The turn came back without text, so [`Message::empty_marker`] takes its
    /// place in history. Returns true when the model was nudged to try again.
    fn recover_stalled_turn(&mut self) -> Result<bool, AgentError> {
        // Asked before the marker lands, since it shifts the recent window.
        let nudge = self.tool_state.nudges < MAX_NUDGES && self.history.has_recent_tool_results(5);
        self.history.push(Message::empty_marker());
        if !nudge {
            return Ok(false);
        }

        self.tool_state.nudges += 1;
        warn!(
            nudges = self.tool_state.nudges,
            "empty response after tool calls, nudging model to continue"
        );
        self.io.event_tx.send(AgentEvent::Nudge)?;
        self.history.push(Message::synthetic(NUDGE_PROMPT.into()));
        Ok(true)
    }

    pub(super) async fn process_tool_calls(
        &mut self,
        response: StreamResponse,
    ) -> Result<ToolBatchOutcome, AgentError> {
        self.tool_state.nudges = 0;
        let ctx = self.tool_context();
        let mut recent = {
            let mut d = self.doom.doom.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut d.recent_calls)
        };
        let result = tool_dispatch::process_tool_calls(
            response,
            &mut recent,
            &mut self.tool_state.guardrails,
            self.tool_state.mcp.as_ref(),
            self.history,
            &self.io.event_tx,
            &ctx,
            &mut self.tool_state.dedup_cache,
            &mut self.tool_state.trust_tracker,
            &self.tool_state.snapshot,
            &self.tool_state.validator,
            &self.tool_state.formatter,
        )
        .await;
        {
            let mut d = self.doom.doom.lock().unwrap_or_else(|e| e.into_inner());
            d.recent_calls = recent;
        }
        result
    }

    pub(super) fn tool_context(&self) -> ToolContext {
        let flow_search = if let Some(handle) = self.flow.flow_search.clone() {
            Some(handle)
        } else if let Some(hist) = self.flow.thread_history.clone() {
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
            provider: Arc::clone(&self.io.provider),
            model: Arc::clone(&self.io.model),
            event_tx: self.io.event_tx.clone(),
            mode: self.flow.mode.clone(),
            tool_use_id: None,
            user_response_rx: self.io.user_response_rx.clone(),
            loaded_instructions: self.loaded_instructions.clone(),
            cancel: self.io.cancel.clone(),
            mcp: self.tool_state.mcp.clone(),
            deadline: Deadline::None,
            config: self.config.clone(),
            tool_output_lines: self.tool_state.tool_output_lines,
            permissions: Arc::clone(&self.tool_state.permissions),
            model_policy: Arc::clone(&self.model_policy),
            timeouts: self.io.timeouts,
            file_tracker: Arc::clone(&self.tool_state.file_tracker),
            prompt_slots: Arc::clone(&self.prompt_slots),
            subagent_cancels: Arc::clone(&self.tool_state.subagent_cancels),
            opts: self.io.opts,
            compression: self.compaction.compression.clone(),
            registry: Arc::clone(&self.tool_state.registry),
            compression_store: Arc::clone(&self.compaction.compression_store),
            findings_store: self.findings_store.clone(),
            fs: Arc::clone(&self.tool_state.fs),
            parent_messages: Arc::from(self.history.as_slice()),
            promoted: self.tool_state.promoted.clone(),
            dynamic: self.tool_state.dynamic.clone(),
            hooks: self.tool_state.hooks.clone(),
            snapshot_store: Arc::clone(&self.tool_state.snapshot_store),
            pending_edits: Arc::clone(&self.tool_state.pending_edits),
            session_id: self.io.session_id.as_ref().map(|s| s.as_str().to_string()),
            flow_search,
            host_question_routing: self.tool_state.host_question_routing,
            flow_thread_manager: self.flow.thread_manager.clone(),
            flow_thread_id: Some(self.flow.thread_id.clone()),
            flow_thread_history: self.flow.thread_history.clone(),
            flow_progress_tx: self.flow.flow_progress_tx.clone(),
        }
    }

    pub(super) async fn handle_queued_command(&mut self) -> Result<bool, AgentError> {
        let Some(ref source) = self.io.interrupt_source else {
            return Ok(false);
        };
        let Some(cmd) = source.poll() else {
            return Ok(false);
        };
        match cmd {
            ExtractedCommand::Interrupt(mut input, _) => {
                self.io.event_tx.send(AgentEvent::QueueItemConsumed {
                    text: input.message.clone(),
                    image_count: input.images.len(),
                })?;
                self.push_input_context(std::mem::take(&mut input.preamble));
                self.flow.mode = input.mode.clone();
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
                if let Some(msg) = self.tool_state.snapshot.rollback().await {
                    self.io.event_tx.send(AgentEvent::Info { message: msg })?;
                }
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::DoneReason;
    use crate::ExtractedCommand;
    use crate::agent::history::History;
    use crate::cancel::CancelToken;
    use async_trait::async_trait;
    use craft_providers::provider::Provider;
    use craft_providers::{
        Message, Model, ProviderEvent, RequestOptions, StopReason, StreamResponse,
    };
    use test_case::test_case;

    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            empty_response(),
            text_response(StopReason::EndTurn),
        ],
        3, 1
        ; "nudge_on_empty_after_tools"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            thinking_response(),
            empty_response(),
            empty_response(),
        ],
        4, 2
        ; "nudges_twice_then_gives_up"
    )]
    #[test_case(
        vec![
            tool_call_response("glob", "t1"),
            text_response(StopReason::EndTurn),
        ],
        2, 0
        ; "no_nudge_when_text_after_tools"
    )]
    #[test_case(
        vec![empty_response(), text_response(StopReason::EndTurn)],
        1, 0
        ; "no_nudge_without_recent_tools"
    )]
    #[tokio::test]
    async fn nudge_behavior(
        responses: Vec<StreamResponse>,
        expected_turns: u32,
        expected_nudges: usize,
    ) {
        let (events, done, history) = run_nudge(responses).await;

        let nudges = events
            .iter()
            .filter(|e| matches!(e.event, AgentEvent::Nudge))
            .count();
        assert_eq!(nudges, expected_nudges);
        assert_eq!(done.expect("expected Done event"), expected_turns);

        assert!(
            history
                .as_slice()
                .iter()
                .all(|m| m.content.iter().any(|b| !b.is_thinking())),
            "history holds a message no provider will accept: {:?}",
            history.as_slice()
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
                _: &serde_json::Value,
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
        params.provider = std::sync::Arc::new(HangingProvider);
        let agent = Agent::new(params, run_params).with_cancel(cancel);

        let result = agent.run(default_input()).await;
        assert_eq!(result.unwrap(), DoneReason::Cancelled);
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
        params.provider = std::sync::Arc::new(MockProvider::new(responses));
        let agent = Agent::new(params, run_params);
        let _ = agent.run(default_input()).await;
        let events = drain_events(&event_rx);

        assert!(has_event(&events, |e| matches!(
            e,
            AgentEvent::ToolDone(done) if done.is_error && done.id == expected_error_id
        )));
    }

    /// A truncated answer buys another turn, but only until one of the two
    /// budgets runs out: the continuation limit or the caller's `max_turns`.
    #[test_case(&[StopReason::EndTurn], None, 1, DoneReason::EndTurn ; "end_turn_completes")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn], None, 2, DoneReason::EndTurn ; "max_tokens_continues")]
    #[test_case(&[StopReason::MaxTokens; 4], None, 4, DoneReason::MaxTokens ; "max_tokens_gives_up_after_limit")]
    #[test_case(&[StopReason::MaxTokens, StopReason::EndTurn], Some(1), 1, DoneReason::MaxTurns ; "turn_budget_exhausted")]
    #[tokio::test]
    async fn turn_counting(
        stops: &[StopReason],
        max_turns: Option<u32>,
        expected_turns: u32,
        expected_reason: DoneReason,
    ) {
        let responses: Vec<_> = stops.iter().map(|s| text_response(*s)).collect();
        let provider = MockProvider::new(responses);
        let (turns, reason) = run_agent(provider, max_turns).await;
        assert_eq!(turns, expected_turns);
        assert_eq!(reason, expected_reason);
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
        params.provider = std::sync::Arc::new(MockProvider::new(responses));
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
        params.provider = std::sync::Arc::new(MockProvider::new(responses));
        let agent = Agent::new(params, run_params).with_interrupt_source(source);
        let result = agent.run(default_input()).await;

        assert!(result.is_ok());
    }
}
