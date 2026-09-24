//! Doom-loop detection: a run-scoped score that rises on pathological agent
//! behavior and decays on healthy progress (reference
//! `craft-agent/src/agent/doom.rs`).
//!
//! Score reaches `GRACE_THRESHOLD` => the agent is asked to summarize and
//! stop (once per run). Score reaches `HARD_STOP_THRESHOLD` => the run ends
//! regardless. `reset_for_new_user_input` deliberately keeps `recent_calls`
//! so loops that survive a "continue" are still detected.
//!
//! A doom-loop batch (the model emitted the same tool call N times in a row)
//! is itself the definition of "stuck", so a single one contributes
//! `GRACE_THRESHOLD` and triggers the grace call immediately — the per-call
//! warning alone is too easy for the model to ignore when it repeats.
//!
//! Known divergences from the reference (both deliberate, revisit with the
//! Phase 5 session work): the tracker and the `RecentCalls` window are
//! run-scoped here, not session-scoped, so a doom loop that survives a
//! "continue" is re-detected from scratch on the next run; and a blocked
//! call's error result is committed as soon as it is seen, which can precede
//! still-running earlier calls' results in history (results stay paired by
//! id, so replay is unaffected).

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

pub(crate) const GRACE_THRESHOLD: u32 = 15;
pub(crate) const HARD_STOP_THRESHOLD: u32 = 25;

const SCORE_DOOM_LOOP: u32 = 15;
// Unwired until embeddings land (Phase 5+); kept so the API is complete.
#[allow(dead_code)]
const SCORE_STAGNATION: u32 = 3;
const SCORE_INEFFECTIVE_COMPACT: u32 = 2;
const SCORE_TOOL_ERROR: u32 = 1;
const DECAY_TOOL_SUCCESS: u32 = 1;
const DECAY_EFFECTIVE_COMPACT: u32 = 1;
#[allow(dead_code)]
const STAGNATION_MIN_UNPRODUCTIVE_TURNS: u32 = 3;

/// Identical repeated calls that constitute a doom loop.
const DOOM_LOOP_THRESHOLD: usize = 3;

/// What the model reads when its identical retry is blocked instead of run
/// (reference `tool_dispatch.rs` `DOOM_LOOP_MESSAGE`).
pub(crate) const DOOM_LOOP_MESSAGE: &str = "You have called this tool with identical input 3 times in a row. This call was NOT executed. You are stuck in a loop. Retrying the same input will be blocked again. Stop, summarize what you have tried, and take a different approach (different arguments, a different tool, or report the blocker to the user).";

/// The one-shot "summarize and stop" prompt fired at `GRACE_THRESHOLD`
/// (reference `run/mod.rs` `GRACE_CALL_PROMPT`).
pub(crate) const GRACE_CALL_PROMPT: &str = "Your recent actions look like a doom-loop (repeated calls, errors, or stagnation). Summarize your progress so far and tell the user what still needs to be done. Do NOT call any tools.";

/// Compaction that recovers less than this fraction is "ineffective" for
/// scoring purposes (reference `run/compaction.rs`).
pub(crate) const INEFFECTIVE_COMPACTION_THRESHOLD: f32 = 0.1;

/// Reference also scores validator rejections (+1); no validator exists in
/// this port yet, so the hook ships unused for API parity.
#[allow(dead_code)]
const SCORE_VALIDATOR_REJECT: u32 = 1;

/// Per-batch counts used by doom-loop scoring (reference
/// `tool_dispatch.rs` `ToolBatchOutcome`).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ToolBatchOutcome {
    pub(crate) errors: u32,
    pub(crate) successes: u32,
    pub(crate) doom_loops: u32,
}

#[derive(Default)]
pub(crate) struct DoomTracker {
    score: u32,
    grace_called: bool,
    turns_since_success: u32,
    // Session-scoped once Phase 5 lands; the run loop currently drives the
    // RecentCalls window directly.
    #[allow(dead_code)]
    pub(crate) recent_calls: RecentCalls,
}

impl DoomTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn score(&self) -> u32 {
        self.score
    }

    #[allow(dead_code)]
    pub(crate) fn grace_called(&self) -> bool {
        self.grace_called
    }

    pub(crate) fn mark_grace_called(&mut self) {
        self.grace_called = true;
    }

    pub(crate) fn should_grace(&self) -> bool {
        !self.grace_called && self.score >= GRACE_THRESHOLD
    }

    pub(crate) fn should_hard_stop(&self) -> bool {
        self.score >= HARD_STOP_THRESHOLD
    }

    pub(crate) fn note_doom_loop(&mut self) {
        self.add(SCORE_DOOM_LOOP);
    }

    #[allow(dead_code)]
    pub(crate) fn note_stagnation(&mut self) {
        self.turns_since_success = self.turns_since_success.saturating_add(1);
        if self.turns_since_success >= STAGNATION_MIN_UNPRODUCTIVE_TURNS {
            self.add(SCORE_STAGNATION);
        }
    }

    pub(crate) fn note_ineffective_compaction(&mut self) {
        self.add(SCORE_INEFFECTIVE_COMPACT);
    }

    pub(crate) fn note_tool_error(&mut self) {
        self.add(SCORE_TOOL_ERROR);
    }

    pub(crate) fn note_tool_success(&mut self) {
        self.turns_since_success = 0;
        self.sub(DECAY_TOOL_SUCCESS);
    }

    pub(crate) fn note_effective_compaction(&mut self) {
        self.sub(DECAY_EFFECTIVE_COMPACT);
    }

    /// Reset the volatile signals (score, grace flag) but keep loop-detection
    /// state so trans-prompt loops remain visible.
    #[allow(dead_code)]
    pub(crate) fn reset_for_new_user_input(&mut self) {
        self.score = 0;
        self.grace_called = false;
        self.turns_since_success = 0;
    }

    fn add(&mut self, n: u32) {
        self.score = self.score.saturating_add(n);
    }

    fn sub(&mut self, n: u32) {
        self.score = self.score.saturating_sub(n);
    }
}

/// Rolling window of recent (tool name, input hash) pairs used to detect a
/// doom loop: the last `DOOM_LOOP_THRESHOLD - 1` calls identical to the
/// incoming one.
#[derive(Default)]
pub(crate) struct RecentCalls(VecDeque<(String, u64)>);

impl RecentCalls {
    fn hash_input(input: &serde_json::Value) -> u64 {
        let mut h = DefaultHasher::new();
        input.to_string().hash(&mut h);
        h.finish()
    }

    pub(crate) fn is_doom_loop(&self, name: &str, input: &serde_json::Value) -> bool {
        let hash = Self::hash_input(input);
        self.0.len() >= DOOM_LOOP_THRESHOLD - 1
            && self
                .0
                .iter()
                .rev()
                .take(DOOM_LOOP_THRESHOLD - 1)
                .all(|(n, h)| n == name && *h == hash)
    }

    pub(crate) fn record(&mut self, name: String, input: &serde_json::Value) {
        self.0.push_back((name, Self::hash_input(input)));
        if self.0.len() > DOOM_LOOP_THRESHOLD {
            self.0.pop_front();
        }
    }

    /// Wipe the recent-call history. Called when a doom loop is detected so
    /// the model gets a clean window to try a different approach: without
    /// this, the saturated identical history would re-trigger the doom check
    /// on the very next identical retry, so the warning would repeat
    /// identically every turn.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_is_clean() {
        let t = DoomTracker::new();
        assert_eq!(t.score(), 0);
        assert!(!t.grace_called());
        assert!(!t.should_grace());
        assert!(!t.should_hard_stop());
    }

    #[test]
    fn doom_loops_accumulate() {
        let mut t = DoomTracker::new();
        t.note_doom_loop();
        assert_eq!(t.score(), 15);
        t.note_doom_loop();
        assert_eq!(t.score(), 30);
    }

    #[test]
    fn grace_fires_only_once() {
        let mut t = DoomTracker::new();
        t.note_doom_loop();
        assert!(t.should_grace());
        t.mark_grace_called();
        assert!(!t.should_grace());
        t.note_doom_loop();
        assert!(t.should_hard_stop());
    }

    #[test]
    fn good_behavior_decays_score() {
        let mut t = DoomTracker::new();
        t.note_doom_loop();
        assert_eq!(t.score(), 15);
        for _ in 0..10 {
            t.note_tool_success();
        }
        assert_eq!(t.score(), 5);
    }

    #[test]
    fn decay_saturates_at_zero() {
        let mut t = DoomTracker::new();
        for _ in 0..10 {
            t.note_tool_success();
        }
        assert_eq!(t.score(), 0);
    }

    #[test]
    fn reset_clears_score_and_grace_only() {
        let mut t = DoomTracker::new();
        t.note_doom_loop();
        t.mark_grace_called();
        t.recent_calls
            .record("read".into(), &serde_json::json!({"p":1}));
        t.recent_calls
            .record("read".into(), &serde_json::json!({"p":1}));
        t.reset_for_new_user_input();
        assert_eq!(t.score(), 0);
        assert!(!t.grace_called());
        assert!(
            t.recent_calls
                .is_doom_loop("read", &serde_json::json!({"p":1})),
            "recent calls preserved"
        );
    }

    #[test]
    fn stagnation_ignored_while_tools_succeed() {
        let mut t = DoomTracker::new();
        for _ in 0..10 {
            t.note_stagnation();
            t.note_tool_success();
        }
        assert_eq!(
            t.score(),
            0,
            "productive research must not accrue doom score"
        );
    }

    #[test]
    fn stagnation_scores_only_without_progress() {
        let mut t = DoomTracker::new();
        for _ in 0..2 {
            t.note_stagnation();
        }
        assert_eq!(t.score(), 0, "below min no score");
        t.note_stagnation();
        assert_eq!(t.score(), 3, "at min first score");
        for _ in 0..2 {
            t.note_stagnation();
        }
        assert_eq!(t.score(), 9, "sustained no progress");
    }

    #[test]
    fn stagnation_resumes_after_success_window_closes() {
        let mut t = DoomTracker::new();
        for _ in 0..3 {
            t.note_stagnation();
        }
        assert_eq!(t.score(), 3);
        t.note_tool_success();
        for _ in 0..2 {
            t.note_stagnation();
        }
        assert_eq!(
            t.score(),
            2,
            "two stagnant turns alone do not re-arm the score"
        );
        t.note_stagnation();
        assert_eq!(t.score(), 5, "third consecutive stagnant turn scores again");
    }

    fn recorded(entries: &[(&str, serde_json::Value)]) -> RecentCalls {
        let mut recent = RecentCalls::default();
        for (name, input) in entries {
            recent.record((*name).into(), input);
        }
        recent
    }

    #[test]
    fn recent_calls_flags_identical_repeat() {
        let input = serde_json::json!({"path":"a.txt"});
        let recent = recorded(&[("read", input.clone()), ("read", input.clone())]);
        assert!(recent.is_doom_loop("read", &input));
    }

    #[test]
    fn recent_calls_ignores_different_input_or_tool() {
        let recent = recorded(&[
            ("read", serde_json::json!({"path":"a.txt"})),
            ("read", serde_json::json!({"path":"a.txt"})),
        ]);
        assert!(!recent.is_doom_loop("read", &serde_json::json!({"path":"b.txt"})));
        assert!(!recent.is_doom_loop("glob", &serde_json::json!({"path":"a.txt"})));
    }

    #[test]
    fn clear_disarms_the_window() {
        let input = serde_json::json!({"path":"a.txt"});
        let mut recent = recorded(&[("read", input.clone()), ("read", input.clone())]);
        recent.clear();
        assert!(!recent.is_doom_loop("read", &input));
    }
}
