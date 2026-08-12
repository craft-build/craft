use std::sync::Arc;

use craft_config::ModelPolicy;
use craft_providers::{Message, Model, ModelTier, Role, TokenUsage};
use tracing::info;

use crate::agent::compaction::{self, continue_message};
use crate::agent::history::History;
use crate::{AgentError, AgentEvent};

use super::Agent;

const DEFAULT_SMALL_MODEL_RATIO: f64 = 0.60;
const INEFFECTIVE_COMPACTION_THRESHOLD: f32 = 0.1;
const CHARS_PER_TOKEN: usize = 4;

pub(super) struct AgentCompaction {
    pub(super) auto_compact: bool,
    pub(super) compression: craft_config::CompressionConfig,
    pub(super) cache_tracker: crate::agent::cache::PrefixCacheTracker,
    pub(super) compression_store: crate::agent::compression_store::SharedCompressionStore,
    pub(super) token_estimation_multiplier: f64,
    pub(super) last_relevance_scores: Option<Vec<(usize, f32)>>,
    pub(super) ineffective_compaction_count: u8,
    pub(super) rollback_len: usize,
}

pub async fn resolve_compaction_model(
    provider: &Arc<dyn craft_providers::provider::Provider>,
    model: &Model,
    timeouts: craft_providers::Timeouts,
    model_policy: &ModelPolicy,
) -> (Arc<dyn craft_providers::provider::Provider>, Model) {
    let compact_spec = craft_providers::model_registry::model_registry()
        .read()
        .unwrap()
        .spec_for_tier_any(ModelTier::Compaction);
    if let Some(spec) = compact_spec
        && model_policy.allows(&spec)
        && let Ok(mut m) = Model::from_spec(&spec)
        && let Ok(p) = craft_providers::provider::from_model(&mut m, timeouts).await
    {
        return (Arc::from(p), m);
    }
    (Arc::clone(provider), model.clone())
}

pub fn estimate_message_tokens(messages: &[Message]) -> u32 {
    if messages.is_empty() {
        return 0;
    }
    let total_bytes: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            craft_providers::ContentBlock::Text { text } => Some(text.len()),
            craft_providers::ContentBlock::ToolResult { content, .. } => Some(content.len()),
            craft_providers::ContentBlock::ToolUse { input, .. } => Some(input.to_string().len()),
            _ => None,
        })
        .sum();
    (total_bytes.max(CHARS_PER_TOKEN) / CHARS_PER_TOKEN) as u32
}

pub(super) fn strip_trailing_grace_prompt(history: &mut History, grace_prompt: &str) {
    loop {
        let msgs = history.as_slice();
        let n = msgs.len();
        if n == 0 {
            break;
        }
        let last = &msgs[n - 1];
        let last_is_grace = matches!(last.role, Role::User)
            && last
                .content
                .iter()
                .any(|b| matches!(b, craft_providers::ContentBlock::Text { text } if text == grace_prompt));
        if last_is_grace {
            history.truncate(n - 1);
            continue;
        }
        if matches!(last.role, Role::Assistant) && n >= 2 {
            let prev = &msgs[n - 2];
            let prev_is_grace = matches!(prev.role, Role::User)
                && prev.content.iter().any(|b| {
                    matches!(b, craft_providers::ContentBlock::Text { text } if text == grace_prompt)
                });
            if prev_is_grace {
                history.truncate(n - 2);
                continue;
            }
        }
        break;
    }
}

impl<'h> Agent<'h> {
    pub(super) async fn try_auto_compact(
        &mut self,
        usage: &TokenUsage,
        force_full: bool,
    ) -> Result<bool, AgentError> {
        if !self.compaction.auto_compact {
            return Ok(false);
        }

        if self.compaction.ineffective_compaction_count >= 2 {
            info!("skipping auto-compaction: last 2 attempts were ineffective");
            return Ok(false);
        }

        let overflow = force_full
            || compaction::is_overflow(usage, &self.io.model, self.effective_compaction_buffer());

        if overflow {
            let estimated = self.history.estimate_tokens(&self.io.model) as f64;
            if estimated > 0.0 && self.context_size > 0 {
                let ratio = self.context_size as f64 / estimated;
                if ratio > self.compaction.token_estimation_multiplier {
                    self.compaction.token_estimation_multiplier =
                        (ratio * 1.1).min(compaction::MAX_TOKEN_ESTIMATION_MULTIPLIER);
                    info!(
                        ratio,
                        new_multiplier = self.compaction.token_estimation_multiplier,
                        "calibrated token estimation multiplier after overflow"
                    );
                }
            }
        }

        let proactive = !overflow
            && compaction::is_proactive_threshold(
                self.history,
                &self.io.model,
                self.proactive_ratio(),
                self.compaction.token_estimation_multiplier,
            );

        if !overflow && !proactive {
            return Ok(false);
        }

        self.tool_state.dedup_cache.clear();

        if let Some(scorer) = &self.recency.scorer
            && let Ok(intent) = scorer.build_intent(self.history.as_slice()).await
            && let Ok(scores) = scorer
                .score_messages(self.history.as_slice(), &intent)
                .await
        {
            self.compaction.last_relevance_scores = Some(scores);
        }

        let ctx = compaction::CompactContext {
            usage,
            model: &self.io.model,
            compaction_buffer: self
                .config
                .resolve_compaction_buffer(self.io.model.context_window),
            cache_tracker: Some(&self.compaction.cache_tracker),
            compression_store: Some(&self.compaction.compression_store),
            relevance_scores: self
                .recency
                .scorer
                .as_ref()
                .and(self.compaction.last_relevance_scores.as_deref()),
            scorer: self.recency.scorer.as_ref(),
        };
        let removed = compaction::progressive_compact(
            self.history,
            self.compaction.compression.protect_recent_tool_outputs,
            &ctx,
        )
        .await;

        if overflow
            && removed > 0
            && !compaction::is_overflow(usage, &self.io.model, self.effective_compaction_buffer())
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
        self.io.event_tx.send(AgentEvent::AutoCompacting)?;
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
            self.compaction.ineffective_compaction_count += 1;
            info!(
                savings_pct = format!("{:.0}%", savings * 100.0),
                "compaction was ineffective"
            );
            self.doom
                .doom
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .note_ineffective_compaction();
        } else {
            self.compaction.ineffective_compaction_count = 0;
            self.doom
                .doom
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .note_effective_compaction();
        }
        Ok(true)
    }

    pub(super) async fn do_compact(&mut self) -> Result<(), AgentError> {
        let vcc_ok = compaction::vcc_compact(
            self.history,
            &self.io.model,
            self.config
                .resolve_compaction_buffer(self.io.model.context_window),
            self.compaction.token_estimation_multiplier,
        )?;
        if !vcc_ok {
            let (compact_provider, compact_model) = resolve_compaction_model(
                &self.io.provider,
                &self.io.model,
                self.io.timeouts,
                &self.model_policy,
            )
            .await;
            self.total_usage += compaction::compact_history(
                &*compact_provider,
                &compact_model,
                self.history,
                &self.io.event_tx,
                &self.io.cancel,
                self.compaction.last_relevance_scores.as_deref(),
                &self.config,
            )
            .await?;
        }
        self.compaction.rollback_len = self.history.len();
        self.io.event_tx.send(AgentEvent::CompactionDone)?;
        self.history
            .push(Message::synthetic(continue_message(&self.config)));
        if let Some(state) = self.flow.advisor_state.as_mut() {
            state.reset(&self.config.advisor);
        }
        if let Some(ttsr) = self.flow.ttsr.as_ref() {
            ttsr.reset();
        }
        self.tool_state.pending_edits.clear();
        Ok(())
    }

    pub(super) fn effective_compaction_buffer(&self) -> u32 {
        let provider_model_id = self.provider_model_id();
        if let Some(t) = craft_config::resolve_threshold(
            &self.config.compaction,
            provider_model_id.as_deref(),
            &self.io.model.id,
        ) && let Some(reserve) =
            craft_config::resolve_reserve_tokens(t, self.io.model.context_window)
        {
            return reserve;
        }
        self.config
            .resolve_compaction_buffer(self.io.model.context_window)
    }

    pub(super) fn small_model_ratio(&self) -> f64 {
        if self
            .config
            .small_model
            .should_activate(self.io.model.context_window)
            && self.config.small_model.aggressive_truncation
        {
            self.config.small_model.compaction_threshold
        } else {
            DEFAULT_SMALL_MODEL_RATIO
        }
    }

    pub(super) fn proactive_ratio(&self) -> f64 {
        let provider_model_id = self.provider_model_id();
        if let Some(t) = craft_config::resolve_threshold(
            &self.config.compaction,
            provider_model_id.as_deref(),
            &self.io.model.id,
        ) {
            if let Some(pct) = t.compact_percent {
                return (pct as f64).max(0.01) / 100.0;
            }
            if let Some(reserve) = t.reserve_tokens
                && self.io.model.context_window > 0
            {
                return 1.0 - (reserve as f64 / self.io.model.context_window as f64);
            }
        }
        self.small_model_ratio()
    }

    pub(super) fn provider_model_id(&self) -> Option<String> {
        Some(format!("{}/{}", self.io.model.provider, self.io.model.id))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::agent::history::History;
    use craft_providers::{Message, Role, StopReason, TokenUsage};
    use test_case::test_case;

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
        params.provider = std::sync::Arc::new(MockProvider::new(responses));
        let mut agent = Agent::new(params, run_params);
        agent.io.model = std::sync::Arc::new(small_context_model(200_000, 8_192));
        agent.compaction.auto_compact = enabled;
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
    async fn try_auto_compact_calibrates_multiplier_on_overflow() {
        let mut history = History::new(vec![Message::user("go".into())]);
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider =
            std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        let mut agent = Agent::new(params, run_params);
        agent.io.model = std::sync::Arc::new(small_context_model(200_000, 8_192));
        agent.context_size = 10_000;

        let usage = TokenUsage::default();
        let _ = agent.try_auto_compact(&usage, true).await.unwrap();

        assert_eq!(
            agent.compaction.token_estimation_multiplier, 5.0,
            "multiplier should be capped at MAX_TOKEN_ESTIMATION_MULTIPLIER"
        );
    }

    #[tokio::test]
    async fn do_compact_uses_vcc_and_skips_llm_when_under_limit() {
        let mut history = History::new(vcc_overflow_history());
        let (run_params, event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider = std::sync::Arc::new(PanickingProvider);
        let mut agent = Agent::new(params, run_params);
        agent.do_compact().await.unwrap();
        drop(event_rx);
        let msgs = agent.history.as_slice();
        assert!(matches!(msgs[0].role, Role::Assistant));
        assert!(msgs.iter().any(|m| m.content.iter().any(|b| matches!(
            b,
            craft_providers::ContentBlock::Text { text } if text.starts_with("This summary captures")
        ))));
        assert!(msgs.len() > 1, "tail must be preserved");
    }

    #[tokio::test]
    async fn do_compact_falls_back_to_llm_when_vcc_insufficient() {
        let mut history = History::new(vcc_overflow_history());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider =
            std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
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

    #[tokio::test]
    async fn do_compact_appends_post_instructions_to_continue_message() {
        const POST: &str = "Re-read plan.md";
        let mut history = History::new(vcc_overflow_history());
        let (run_params, _event_rx) = make_run_params(&mut history);
        let mut params = make_agent_params();
        params.provider =
            std::sync::Arc::new(MockProvider::new(vec![text_response(StopReason::EndTurn)]));
        params.model = {
            let mut m = default_model();
            m.context_window = 1;
            m
        };
        let mut agent = Agent::new(params, run_params);
        agent.config.post_compaction_instructions = Some(POST.into());
        agent.do_compact().await.unwrap();
        drop(agent);

        let last = history.as_slice().last().unwrap();
        assert!(matches!(&last.content[0],
            craft_providers::ContentBlock::Text { text } if text.ends_with(POST) && text != POST));
    }
}
