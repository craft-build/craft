//! Compaction trigger engine: runs configured stages when the estimated
//! context fill crosses each stage's threshold, with an effectiveness score
//! that disarms a stage whose last run barely shrank the context (avoiding
//! cyclical compaction when the context cannot shrink further).

use std::collections::HashSet;

use rig_core::completion::CompletionModel;

use crate::config::{CompactionConfig, CompactionKind};
use crate::history::Message;

use super::estimate::estimate_tokens;
use super::llm::llm_compact;
use super::vcc::vcc_compact;

/// A stage whose last run saved less than this fraction is disarmed until
/// another stage compacts effectively (ported from Craft's threshold).
pub const INEFFECTIVE_SAVINGS: f32 = 0.1;

/// Ordered stage list (ascending by context ratio).
#[derive(Debug, Clone)]
pub struct CompactionEngine {
    stages: Vec<CompactionConfig>,
}

/// Per-session effectiveness state, persisted across turns.
#[derive(Debug, Default, Clone)]
pub struct CompactionState {
    disarmed: HashSet<CompactionKind>,
}

impl CompactionEngine {
    pub fn new(mut stages: Vec<CompactionConfig>) -> Self {
        stages.sort_by(|a, b| {
            a.context
                .partial_cmp(&b.context)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { stages }
    }

    /// Run every armed stage whose threshold is crossed, lowest first.
    /// `history` is compacted in place. Returns whether any stage ran.
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
        let mut ran = false;
        for stage in &self.stages {
            let threshold = (context_length as f64 * stage.context) as u64;
            if threshold == 0 {
                continue;
            }
            let before = estimate_tokens(history);
            if before < threshold {
                continue;
            }
            if state.disarmed.contains(&stage.kind) {
                continue;
            }
            let before_len = history.len();
            let _under_limit = match stage.kind {
                CompactionKind::Vcc => vcc_compact(history, threshold, estimate_tokens),
                CompactionKind::Llm => llm_compact(model, history, threshold)
                    .await
                    .unwrap_or(false),
            };
            // A stage that declined to run (too-short history, empty head)
            // leaves the history untouched; that is not an ineffective run,
            // so the stage stays armed.
            if history.len() == before_len && estimate_tokens(history) == before {
                continue;
            }
            ran = true;
            let after = estimate_tokens(history);
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
}
