//! Compaction trigger engine: runs configured stages when the estimated
//! context fill crosses each stage's threshold, with an effectiveness score
//! that disarms a stage whose last run barely shrank the context (avoiding
//! cyclical compaction when the context cannot shrink further).

use std::collections::HashSet;

use rig_core::completion::CompletionModel;

use crate::config::{
    CompactionBuffer, CompactionConfig, CompactionKind,
    DEFAULT_COMPACTION_BUFFER as DEFAULT_ENGINE_BUFFER,
};
use crate::history::Message;
use crate::run::SharedDedupCache;

use super::estimate::{TokenEstimator, estimate_tokens};
use super::llm::llm_compact;
use super::vcc::vcc_compact;

/// A stage whose last run saved less than this fraction is disarmed until
/// another stage compacts effectively (ported from Craft's threshold).
pub const INEFFECTIVE_SAVINGS: f32 = 0.1;

/// Ordered stage list (ascending by context ratio).
#[derive(Debug, Clone)]
pub struct CompactionEngine {
    stages: Vec<CompactionConfig>,
    buffer: CompactionBuffer,
}

/// Per-session effectiveness state, persisted across turns.
#[derive(Debug, Default, Clone)]
pub struct CompactionState {
    disarmed: HashSet<CompactionKind>,
    /// Calibrated token estimator: thresholds are checked against scaled
    /// estimates once the provider proves the raw ones too low.
    pub estimator: TokenEstimator,
    /// Cleared before every compaction run: compacted history may describe a
    /// different world than the one the dedup cache sampled.
    dedup: Option<SharedDedupCache>,
    /// Cleared with the dedup cache: the compacted conversation no longer
    /// describes the calls that tripped the guardrail counters.
    guardrails: Option<crate::run::SharedGuardrails>,
    /// Where the unanswered input starts, as a history index: everything from
    /// here on is held out of the summary and re-appended verbatim, so input
    /// no turn has answered yet is never summarized away (Craft's
    /// `carry_from`). `None` (the default) protects nothing, which is the
    /// correct state between turns.
    carry_from: Option<usize>,
}

impl CompactionState {
    /// Share the session's tool dedup cache so compaction can clear it.
    pub fn with_dedup(mut self, cache: SharedDedupCache) -> Self {
        self.dedup = Some(cache);
        self
    }

    /// Share the session's tool guardrails so compaction can reset them.
    pub fn with_guardrails(mut self, guardrails: crate::run::SharedGuardrails) -> Self {
        self.guardrails = Some(guardrails);
        self
    }

    /// Mark `index` as where the unanswered input starts; compaction holds
    /// everything from there on out of the summary.
    pub fn protect_from(&mut self, index: usize) {
        self.carry_from = Some(index);
    }

    /// All input has been answered: nothing to protect (call when a turn
    /// completes).
    pub fn mark_answered(&mut self) {
        self.carry_from = None;
    }

    /// Where the protected (unanswered) input currently starts, if any.
    pub fn carry_from(&self) -> Option<usize> {
        self.carry_from
    }

    /// Recalibrate the estimator after an overflow: `actual` is the
    /// provider-reported prompt size for a request estimated at
    /// `estimated` tokens.
    pub fn recalibrate(&mut self, actual: u64, estimated: u64) -> bool {
        self.estimator.recalibrate(actual, estimated)
    }
}

impl CompactionEngine {
    pub fn new(mut stages: Vec<CompactionConfig>) -> Self {
        stages.sort_by(|a, b| {
            a.context
                .partial_cmp(&b.context)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            stages,
            buffer: DEFAULT_ENGINE_BUFFER,
        }
    }

    /// Set the compaction buffer reserved below the context window
    /// (defaults to 20% of the window, matching Craft).
    pub fn with_buffer(mut self, buffer: CompactionBuffer) -> Self {
        self.buffer = buffer;
        self
    }

    /// Run every armed stage whose threshold is crossed, lowest first.
    /// `history` is compacted in place. Returns whether any stage ran.
    ///
    /// A stage runs when either its proactive fill threshold is crossed or
    /// the estimated context has already overflowed the buffer-subtracted
    /// window — overflow forces every armed stage in order (VCC first, then
    /// LLM), regardless of its ratio threshold.
    pub async fn maybe_compact<M: CompletionModel + Clone>(
        &self,
        state: &mut CompactionState,
        model: &M,
        history: &mut Vec<Message>,
        context_length: Option<u32>,
    ) -> bool {
        let Some(context_length) = context_length.filter(|length| *length > 0) else {
            return false;
        };
        // The buffer is headroom the model needs for its reply, so the fill
        // checks run against the window minus the buffer (Craft's
        // `is_overflow`: `usage >= window - buffer`).
        let usable = u64::from(context_length.saturating_sub(self.buffer.resolve(context_length)));
        let mut ran = false;
        for stage in &self.stages {
            let threshold = (context_length as f64 * stage.context) as u64;
            if threshold == 0 {
                continue;
            }
            let before = state.estimator.scale(estimate_tokens(history));
            let overflow = before >= usable;
            if !overflow && before < threshold {
                continue;
            }
            if state.disarmed.contains(&stage.kind) {
                continue;
            }
            if let Some(cache) = &state.dedup
                && let Ok(mut guard) = cache.lock()
            {
                guard.clear();
            }
            if let Some(guardrails) = &state.guardrails
                && let Ok(mut guard) = guardrails.lock()
            {
                guard.reset();
            }
            // Unanswered input is held out of the summary and re-appended
            // verbatim; `carry_len == 0` between turns.
            let carry_len = history
                .len()
                .saturating_sub(state.carry_from.unwrap_or(history.len()));
            let before_len = history.len();
            let _under_limit = match stage.kind {
                CompactionKind::Vcc => vcc_compact(history, threshold, estimate_tokens),
                CompactionKind::Llm => llm_compact(model, history, threshold, carry_len, None)
                    .await
                    .unwrap_or(false),
            };
            // A stage that declined to run (too-short history, empty head)
            // leaves the history untouched; that is not an ineffective run,
            // so the stage stays armed.
            if history.len() == before_len
                && state.estimator.scale(estimate_tokens(history)) == before
            {
                continue;
            }
            ran = true;
            // The carried input survived verbatim at the tail; re-anchor the
            // protection to where it now starts.
            if carry_len > 0 {
                state.carry_from = Some(history.len().saturating_sub(carry_len));
            }
            let after = state.estimator.scale(estimate_tokens(history));
            let savings = if before > 0 {
                1.0 - (after as f32 / before as f32)
            } else {
                0.0
            };
            if savings >= INEFFECTIVE_SAVINGS {
                // An effective run re-arms every stage.
                state.disarmed.clear();
            } else {
                state.disarmed.insert(stage.kind);
            }
        }
        ran
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::llm::LLM_SUMMARY_PREFIX;
    use crate::compaction::test_support::{assistant_text, tool_result_of, user};
    use crate::history::AssistantContent;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    fn stage(kind: CompactionKind, context: f64) -> CompactionConfig {
        CompactionConfig { kind, context }
    }

    fn tool_round(i: usize, output: &str) -> Vec<Message> {
        let id = format!("t{i}");
        vec![
            crate::compaction::test_support::assistant_tool_args(
                &id,
                "bash",
                serde_json::json!({"command": format!("echo {i}")}),
            ),
            tool_result_of(&id, output),
        ]
    }

    /// Long history: alternating prompts and large tool rounds.
    fn long_history() -> Vec<Message> {
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(user(&format!("do task {i} with a fairly long instruction")));
            messages.extend(tool_round(i, &"x".repeat(200)));
        }
        messages.push(user("second task begins here"));
        for i in 8..14 {
            messages.push(user(&format!(
                "do subtask {i} with a fairly long instruction"
            )));
            messages.extend(tool_round(i, &"y".repeat(200)));
        }
        messages
    }

    #[tokio::test]
    async fn runs_vcc_stage_when_threshold_crossed() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut state = CompactionState::default();
        let mut history = long_history();
        let tokens = estimate_tokens(&history);
        // Window sized so 60% of it is below the current estimate.
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let ran = engine
            .maybe_compact(
                &mut state,
                &MockCompletionModel::text("x"),
                &mut history,
                Some(context_length),
            )
            .await;
        assert!(ran);
        assert!(!history.is_empty());
        // VCC summary marker present (deterministic, no LLM call).
        assert!(matches!(&history[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains("This summary captures"))));
    }

    #[tokio::test]
    async fn runs_llm_stage_at_higher_threshold() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.8)]);
        let mut state = CompactionState::default();
        let mut history = long_history();
        let tokens = estimate_tokens(&history);
        // Window sized so 80% of it is below the current estimate.
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let model = MockCompletionModel::new([MockTurn::text("llm summary text")]);
        let ran = engine
            .maybe_compact(&mut state, &model, &mut history, Some(context_length))
            .await;
        assert!(ran);
        assert!(matches!(&history[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains(LLM_SUMMARY_PREFIX))));
    }

    #[tokio::test]
    async fn disarmed_stage_is_skipped_until_effective_run() {
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.6),
            stage(CompactionKind::Llm, 0.8),
        ]);
        let mut state = CompactionState::default();
        let mut history = long_history();
        // Window sized so vcc cannot get under its 0.6 threshold; the LLM
        // stage then runs and compacts effectively, re-arming everything.
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.99) as u32).max(1);
        let model = MockCompletionModel::new([MockTurn::text("summarized")]);
        assert!(
            engine
                .maybe_compact(&mut state, &model, &mut history, Some(context_length))
                .await
        );
        // LLM stage ran effectively (its static-ish summary is much smaller),
        // which re-arms everything.
        assert!(state.disarmed.is_empty());
    }

    #[tokio::test]
    async fn ineffective_vcc_is_not_run_twice() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut state = CompactionState::default();
        // Window so tight vcc cannot shrink history below the threshold and
        // savings are below 10%: craft a history where vcc's summary barely
        // shrinks (short messages). Use short messages.
        let mut history = vec![
            user(&"a".repeat(100)),
            assistant_text(&"b".repeat(100)),
            user(&"c".repeat(100)),
            assistant_text(&"d".repeat(100)),
            user(&"e".repeat(100)),
            assistant_text(&"f".repeat(100)),
            user(&"g".repeat(100)),
            assistant_text(&"h".repeat(100)),
        ];
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.99) as u32).max(1);
        let model = MockCompletionModel::text("x");
        let ran = engine
            .maybe_compact(&mut state, &model, &mut history, Some(context_length))
            .await;
        assert!(ran);
        assert!(state.disarmed.contains(&CompactionKind::Vcc));
        // Second call: nothing runs (stage disarmed) even though crossed.
        let again = engine
            .maybe_compact(&mut state, &model, &mut history, Some(context_length))
            .await;
        assert!(!again, "disarmed stage must not run again");
    }

    #[tokio::test]
    async fn declined_stage_stays_armed() {
        // A two-message history crosses the threshold but both compactors
        // decline; neither stage may be disarmed by the no-op.
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.6),
            stage(CompactionKind::Llm, 0.8),
        ]);
        let mut state = CompactionState::default();
        let mut history = vec![user(&"x".repeat(4000))];
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let model = MockCompletionModel::text("x");
        let ran = engine
            .maybe_compact(&mut state, &model, &mut history, Some(context_length))
            .await;
        assert!(!ran);
        assert!(state.disarmed.is_empty(), "declined runs must not disarm");
    }

    #[tokio::test]
    async fn calibrated_multiplier_tightens_thresholds() {
        // The raw estimate sits under the threshold, but a multiplier
        // recalibrated after an overflow pushes it over: the stage runs.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut state = CompactionState::default();
        let mut history = long_history();
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.5) as u32).max(1);
        assert!(
            !engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(context_length)
                )
                .await,
            "raw estimate sits under the threshold"
        );
        state.recalibrate(context_length as u64 * 2, tokens);
        let mut history = long_history();
        assert!(
            engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(context_length)
                )
                .await
        );
    }

    #[tokio::test]
    async fn compaction_clears_dedup_cache() {
        use crate::run::{ToolDedupCache, shared_cache};

        let cache = shared_cache();
        let input = serde_json::json!({"path": "/x.rs"});
        let key = ToolDedupCache::key("read", &input);
        {
            let mut guard = cache.lock().unwrap();
            guard.insert(
                key,
                &crate::history::ToolResult::text("c1", "read", "x"),
                None,
                "read",
                &input,
            );
        }
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut state = CompactionState::default().with_dedup(cache.clone());
        let mut history = long_history();
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        assert!(
            engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(context_length)
                )
                .await
        );
        assert!(
            cache.lock().unwrap().get(key, "read", &input).is_none(),
            "a compaction run must clear the dedup cache"
        );
    }

    #[tokio::test]
    async fn no_compaction_without_context_length() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut state = CompactionState::default();
        let mut history = long_history();
        let ran = engine
            .maybe_compact(
                &mut state,
                &MockCompletionModel::text("x"),
                &mut history,
                None,
            )
            .await;
        assert!(!ran);
        assert_eq!(history.len(), long_history().len());
    }

    #[tokio::test]
    async fn carry_protection_keeps_unanswered_input_verbatim() {
        // The last two messages are unanswered input; the LLM stage may
        // summarize everything before them, but must re-append them verbatim.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let mut state = CompactionState::default();
        let mut history = long_history();
        let (prompt, followup) = (user("unanswered prompt"), user("queued followup"));
        history.push(prompt.clone());
        history.push(followup.clone());
        state.protect_from(history.len() - 2);
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let model = MockCompletionModel::new([MockTurn::text("summary")]);
        assert!(
            engine
                .maybe_compact(&mut state, &model, &mut history, Some(context_length))
                .await
        );
        // Summary replaced the head, protected tail is intact and last.
        assert!(matches!(&history[0], Message::Assistant { content, .. }
            if matches!(&content[0], AssistantContent::Text(t)
                if t.text.contains(LLM_SUMMARY_PREFIX))));
        assert_eq!(history[history.len() - 2], prompt);
        assert_eq!(*history.last().unwrap(), followup);
        // The protection re-anchored to where the carried input now starts.
        assert_eq!(state.carry_from(), Some(history.len() - 2));
        // The summarizer never saw the carried input.
        let request = &model.requests()[0];
        let carried = request
            .chat_history
            .iter()
            .filter_map(|m| match m {
                rig_core::completion::message::Message::User { content } => content.first(),
                _ => None,
            })
            .filter_map(|b| match b {
                rig_core::completion::message::UserContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .any(|t| t.contains("unanswered prompt") || t.contains("queued followup"));
        assert!(!carried, "carried input must not be sent to the summarizer");
    }

    #[tokio::test]
    async fn mark_answered_clears_protection() {
        let mut state = CompactionState::default();
        state.protect_from(3);
        assert_eq!(state.carry_from(), Some(3));
        state.mark_answered();
        assert_eq!(state.carry_from(), None);
        assert_eq!(CompactionState::default().carry_from(), None);
    }

    #[tokio::test]
    async fn fully_protected_history_declines_instead_of_summarizing_input() {
        // Everything unanswered: the LLM stage must decline rather than
        // summarize the only input there is.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.1)]);
        let mut state = CompactionState::default();
        let mut history = long_history();
        state.protect_from(0);
        let tokens = estimate_tokens(&history);
        let context_length = ((tokens as f64 / 0.5) as u32).max(1);
        let model = MockCompletionModel::new([MockTurn::text("summary")]);
        assert!(
            !engine
                .maybe_compact(&mut state, &model, &mut history, Some(context_length))
                .await
        );
        assert_eq!(history.len(), long_history().len());
        assert!(model.requests().is_empty());
    }

    #[tokio::test]
    async fn overflow_of_buffer_subtracted_window_forces_stage_below_threshold() {
        // Fill is under the 0.6 proactive threshold but over the window minus
        // the buffer: the stage must still run (Craft's is_overflow forcing).
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Percent(50));
        let mut state = CompactionState::default();
        let mut history = long_history();
        let tokens = estimate_tokens(&history);
        // Window: fill ratio 0.5 (< 0.6 threshold), usable = 0.5 * window
        // (= tokens) so estimate >= usable -> overflow.
        let context_length = ((tokens as f64 / 0.5) as u32).max(1);
        assert!(
            engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(context_length)
                )
                .await,
            "overflow must force the stage despite the proactive threshold"
        );
    }

    #[tokio::test]
    async fn buffer_larger_than_window_saturates_and_forces() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.99)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(u32::MAX));
        let mut state = CompactionState::default();
        let mut history = long_history();
        let tokens = estimate_tokens(&history);
        // Fill ratio far below the 0.99 threshold, but the buffer eats the
        // whole window: the usable limit saturates at 0 and the stage runs.
        let context_length = ((tokens as f64 / 0.1) as u32).max(1);
        assert!(
            engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(context_length)
                )
                .await,
            "a buffer covering the whole window must force compaction, not underflow"
        );
    }

    #[tokio::test]
    async fn overflow_forcing_respects_disarmed_stages() {
        // Craft skips auto-compaction after ineffective runs even on overflow.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Percent(50));
        let mut state = CompactionState::default();
        let mut history = vec![
            user(&"a".repeat(100)),
            assistant_text(&"b".repeat(100)),
            user(&"c".repeat(100)),
            assistant_text(&"d".repeat(100)),
            user(&"e".repeat(100)),
            assistant_text(&"f".repeat(100)),
            user(&"g".repeat(100)),
            assistant_text(&"h".repeat(100)),
        ];
        let tokens = estimate_tokens(&history);
        // Window so tight vcc's savings fall below 10%: the stage disarms.
        let tight = ((tokens as f64 / 0.99) as u32).max(1);
        assert!(
            engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(tight)
                )
                .await
        );
        // A second run with a wide window but a huge buffer: fill is over the
        // buffer-subtracted usable window (overflow), under the threshold.
        let overflowing = ((tokens as f64 / 0.9) as u32).max(1);
        assert!(
            !engine
                .maybe_compact(
                    &mut state,
                    &MockCompletionModel::text("x"),
                    &mut history,
                    Some(overflowing)
                )
                .await,
            "a disarmed stage must not run even on overflow"
        );
    }
}
