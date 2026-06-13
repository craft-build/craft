//! Doom-loop detection: a session-scoped score that rises on pathological
//! agent behavior and decays on healthy progress.
//!
//! Replaces the old per-run `num_turns >= max_turns` budget, which both
//! falsely fired on legitimate long sessions (because every "continue" reset
//! the counter) and missed real loops that span across user prompts.
//!
//! Score reaches `GRACE_THRESHOLD` => the agent is asked to summarize and
//! stop (once per session). Score reaches `HARD_STOP_THRESHOLD` => the run
//! ends regardless. A new user message resets the score and grace flag but
//! deliberately keeps `recent_calls` and `turn_embeddings` so loops that
//! survive a "continue" are still detected.

#[cfg(feature = "onnx")]
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::tool_dispatch::RecentCalls;

pub const GRACE_THRESHOLD: u32 = 15;
pub const HARD_STOP_THRESHOLD: u32 = 25;

const SCORE_DOOM_LOOP: u32 = 5;
const SCORE_STAGNATION: u32 = 3;
const SCORE_INEFFECTIVE_COMPACT: u32 = 2;
const SCORE_TOOL_ERROR: u32 = 1;
const SCORE_VALIDATOR_REJECT: u32 = 1;
const DECAY_TOOL_SUCCESS: u32 = 1;
const DECAY_EFFECTIVE_COMPACT: u32 = 1;

#[derive(Default)]
pub struct DoomTracker {
    score: u32,
    grace_called: bool,
    #[cfg(feature = "onnx")]
    pub(super) turn_embeddings: VecDeque<Vec<f32>>,
    pub(super) recent_calls: RecentCalls,
}

impl DoomTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn score(&self) -> u32 {
        self.score
    }

    pub fn grace_called(&self) -> bool {
        self.grace_called
    }

    pub fn mark_grace_called(&mut self) {
        self.grace_called = true;
    }

    pub fn should_grace(&self) -> bool {
        !self.grace_called && self.score >= GRACE_THRESHOLD
    }

    pub fn should_hard_stop(&self) -> bool {
        self.score >= HARD_STOP_THRESHOLD
    }

    pub fn note_doom_loop(&mut self) {
        self.add(SCORE_DOOM_LOOP);
    }

    pub fn note_stagnation(&mut self) {
        self.add(SCORE_STAGNATION);
    }

    pub fn note_ineffective_compaction(&mut self) {
        self.add(SCORE_INEFFECTIVE_COMPACT);
    }

    pub fn note_tool_error(&mut self) {
        self.add(SCORE_TOOL_ERROR);
    }

    pub fn note_validator_rejection(&mut self) {
        self.add(SCORE_VALIDATOR_REJECT);
    }

    pub fn note_tool_success(&mut self) {
        self.sub(DECAY_TOOL_SUCCESS);
    }

    pub fn note_effective_compaction(&mut self) {
        self.sub(DECAY_EFFECTIVE_COMPACT);
    }

    /// Called when the user submits a new message: reset the volatile signals
    /// (score, grace flag) but keep loop-detection state so trans-prompt loops
    /// remain visible.
    pub fn reset_for_new_user_input(&mut self) {
        self.score = 0;
        self.grace_called = false;
    }

    fn add(&mut self, n: u32) {
        self.score = self.score.saturating_add(n);
    }

    fn sub(&mut self, n: u32) {
        self.score = self.score.saturating_sub(n);
    }
}

pub type SharedDoomTracker = Arc<Mutex<DoomTracker>>;

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn fresh_tracker_is_clean() {
        let t = DoomTracker::new();
        assert_eq!(t.score(), 0);
        assert!(!t.grace_called());
        assert!(!t.should_grace());
        assert!(!t.should_hard_stop());
    }

    #[test_case(2, 10 ; "doom_loop_alone_below_grace")]
    #[test_case(3, 15 ; "doom_loops_reach_grace")]
    fn doom_loops_accumulate(loops: u32, expected: u32) {
        let mut t = DoomTracker::new();
        for _ in 0..loops {
            t.note_doom_loop();
        }
        assert_eq!(t.score(), expected);
    }

    #[test]
    fn grace_fires_only_once() {
        let mut t = DoomTracker::new();
        for _ in 0..5 {
            t.note_doom_loop();
        }
        assert!(t.should_grace());
        t.mark_grace_called();
        assert!(!t.should_grace());
        for _ in 0..5 {
            t.note_doom_loop();
        }
        assert!(t.should_hard_stop());
    }

    #[test]
    fn good_behavior_decays_score() {
        let mut t = DoomTracker::new();
        for _ in 0..3 {
            t.note_doom_loop();
        }
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
    fn add_saturates_at_max() {
        let mut t = DoomTracker::new();
        t.score = u32::MAX - 1;
        t.note_doom_loop();
        assert_eq!(t.score(), u32::MAX);
    }

    #[cfg(feature = "onnx")]
    #[test]
    fn reset_clears_score_and_grace_only() {
        let mut t = DoomTracker::new();
        for _ in 0..5 {
            t.note_doom_loop();
        }
        t.mark_grace_called();
        t.turn_embeddings.push_back(vec![1.0, 2.0]);
        t.reset_for_new_user_input();
        assert_eq!(t.score(), 0);
        assert!(!t.grace_called());
        assert_eq!(t.turn_embeddings.len(), 1, "embeddings preserved");
    }
}
