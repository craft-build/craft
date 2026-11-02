//! Per-tool guardrails against unproductive loops.
//!
//! The guardrail layer consults [`ToolGuardrails::check_before_call`] before
//! executing a call and feeds results back through
//! [`ToolGuardrails::record_result`]. State resets when a compaction run
//! rewrites history (the compacted conversation no longer describes the
//! calls that tripped the counters).
//!
//! A successful call clears the failure streak (non-consecutive failures
//! are routine for a general-purpose tool like `bash`, where a non-zero
//! exit is reported as an error), and a block trips as a one-shot circuit
//! breaker that clears the triggering counters, so no tool can be
//! permanently banned.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use serde_json::Value;

use super::dedup::ToolDedupCache;
use super::doom::DoomTracker;

const EXACT_REPEAT_WARN: usize = 2;
const EXACT_REPEAT_BLOCK: usize = 4;
const SAME_TOOL_FAIL_WARN: usize = 3;
const SAME_TOOL_FAIL_BLOCK: usize = 6;
const NO_PROGRESS_WARN: usize = 2;
const NO_PROGRESS_BLOCK: usize = 4;

/// Model-visible text for a guardrail-blocked call (reference
/// `tool_dispatch.rs` `GUARDRAIL_BLOCK_MESSAGE`); the user-facing
/// `CraftEvent::Info` text keys off it.
pub(crate) const GUARDRAIL_BLOCK_MESSAGE: &str = "blocked by guardrails: this tool call keeps \
     repeating without progress; change your approach or use a different tool";

/// The prefix the guardrail layer puts on warnings carried by a ran result;
/// the `CraftEvent::Info` text strips it.
pub(crate) const GUARDRAIL_WARN_PREFIX: &str = "[guardrail] ";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuardrailDecision {
    Allow,
    Warn,
    Block,
}

#[derive(Debug)]
pub struct GuardrailWarning {
    pub reason: String,
}

#[derive(Debug)]
struct ToolTracker {
    exact_fail_count: usize,
    any_fail_count: usize,
    last_result_hash: Option<u64>,
    same_result_count: usize,
    last_input_hash: Option<u64>,
    /// Input hash of the most recent *errored* call: the exact-failure
    /// streak only grows while consecutive failures repeat this input.
    last_error_hash: Option<u64>,
}

impl ToolTracker {
    fn new() -> Self {
        Self {
            exact_fail_count: 0,
            any_fail_count: 0,
            last_result_hash: None,
            same_result_count: 0,
            last_input_hash: None,
            last_error_hash: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ToolGuardrails {
    trackers: HashMap<String, ToolTracker>,
}

impl ToolGuardrails {
    pub fn new() -> Self {
        Self {
            trackers: HashMap::new(),
        }
    }

    fn hash_value(v: &Value) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.to_string().hash(&mut h);
        h.finish()
    }

    fn hash_result(result: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        result.hash(&mut h);
        h.finish()
    }

    pub fn check_before_call(
        &mut self,
        tool: &str,
        input: &Value,
        is_read_only: bool,
    ) -> GuardrailDecision {
        let Some(tracker) = self.trackers.get_mut(tool) else {
            return GuardrailDecision::Allow;
        };

        let input_hash = Self::hash_value(input);
        let exact_repeat = input_hash == tracker.last_input_hash.unwrap_or(0);

        let blocked = (tracker.exact_fail_count >= EXACT_REPEAT_BLOCK && exact_repeat)
            || tracker.any_fail_count >= SAME_TOOL_FAIL_BLOCK
            || (is_read_only && tracker.same_result_count >= NO_PROGRESS_BLOCK);
        if blocked {
            // A block is a circuit breaker, not a ban: clearing the streak
            // lets the next attempt run (and, if it succeeds, reset cleanly)
            // rather than leaving the tool locked out forever, since a
            // blocked call never reaches `record_result` to clear the count.
            tracker.exact_fail_count = 0;
            tracker.any_fail_count = 0;
            tracker.same_result_count = 0;
            return GuardrailDecision::Block;
        }

        let warned = (tracker.exact_fail_count >= EXACT_REPEAT_WARN && exact_repeat)
            || tracker.any_fail_count >= SAME_TOOL_FAIL_WARN
            || (is_read_only && tracker.same_result_count >= NO_PROGRESS_WARN);
        if warned {
            return GuardrailDecision::Warn;
        }

        GuardrailDecision::Allow
    }

    pub fn record_result(
        &mut self,
        tool: &str,
        input: &Value,
        result: &str,
        is_error: bool,
        is_read_only: bool,
    ) -> Option<GuardrailWarning> {
        let tracker = self
            .trackers
            .entry(tool.to_string())
            .or_insert_with(ToolTracker::new);
        let input_hash = Self::hash_value(input);
        tracker.last_input_hash = Some(input_hash);

        let mut warning = None;

        if is_error {
            // "Exact" means the same call failing again: only an input
            // identical to the last errored call continues the streak, so
            // alternating between genuinely different failures cannot
            // trip the exact-loop thresholds.
            if tracker.last_error_hash == Some(input_hash) {
                tracker.exact_fail_count += 1;
            } else {
                tracker.exact_fail_count = 1;
            }
            tracker.last_error_hash = Some(input_hash);
            tracker.any_fail_count += 1;

            if tracker.exact_fail_count == EXACT_REPEAT_WARN {
                warning = Some(GuardrailWarning {
                    reason: format!(
                        "same tool+input failed {EXACT_REPEAT_WARN} times, consider a different approach"
                    ),
                });
            } else if tracker.any_fail_count == SAME_TOOL_FAIL_WARN {
                warning = Some(GuardrailWarning {
                    reason: format!(
                        "{tool} has failed {SAME_TOOL_FAIL_WARN} times total, consider using a different tool"
                    ),
                });
            }
        } else {
            tracker.exact_fail_count = 0;
            tracker.last_error_hash = None;
            // A success means the tool is working again: clear the streak so
            // non-consecutive failures cannot accumulate into a block.
            tracker.any_fail_count = 0;

            if is_read_only {
                let result_hash = Self::hash_result(result);
                if tracker.last_result_hash == Some(result_hash) {
                    tracker.same_result_count += 1;
                    if tracker.same_result_count == NO_PROGRESS_WARN {
                        warning = Some(GuardrailWarning {
                            reason: format!(
                                "{tool} returned identical results {NO_PROGRESS_WARN} times, you may be stuck"
                            ),
                        });
                    }
                } else {
                    tracker.same_result_count = 0;
                }
                tracker.last_result_hash = Some(result_hash);
            }
        }

        warning
    }

    /// Reset all trackers: a compaction run rewrote the history the counters
    /// were tripped against.
    pub fn reset(&mut self) {
        self.trackers.clear();
    }
}

/// The conversation's guardrail state: the doom tracker, the per-tool
/// guardrail counters and the dedup cache behind one lock, held as an `Arc`
/// by the guardrail layer, the policy layer (its "answered without
/// execution" marks) and the conversation's typed handle — which
/// compaction calls `reset` on.
///
/// `with` runs its closure synchronously, so the lock is never held across
/// an `.await` even though the layer hooks around it are async. Taking the
/// whole state at once also keeps one call's pre-exec decision atomic
/// against the concurrent pre-hooks of its sibling calls.
pub struct GuardrailState {
    inner: Mutex<GuardrailInner>,
}

/// The state behind [`GuardrailState`]'s lock. The guardrail layer's hooks
/// are the only readers of these fields; they are `pub(crate)` so the layer
/// can run its multi-step decisions in one `with` pass.
pub(crate) struct GuardrailInner {
    pub(crate) doom: DoomTracker,
    pub(crate) guardrails: ToolGuardrails,
    pub(crate) dedup: ToolDedupCache,
    /// Call ids answered without execution this run; `post_tool_exec` skips
    /// counter and cache bookkeeping for them. Cleared at `turn_start`.
    pub(crate) answered: HashSet<String>,
    /// Set by the driver before a follow-up run (advisor and overflow
    /// re-runs continue the same turn): `turn_start` skips the volatile
    /// doom reset and consumes the mark.
    follow_up: bool,
}

impl std::fmt::Debug for GuardrailState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardrailState").finish_non_exhaustive()
    }
}

impl GuardrailState {
    /// State for a conversation anchored at `write_root` (the workspace
    /// root, when there is one), so relative and absolute spellings of one
    /// file dedup and invalidate identically.
    pub fn new(write_root: Option<&str>) -> Self {
        Self {
            inner: Mutex::new(GuardrailInner {
                doom: DoomTracker::new(),
                guardrails: ToolGuardrails::new(),
                dedup: ToolDedupCache::with_root(write_root),
                answered: HashSet::new(),
                follow_up: false,
            }),
        }
    }

    /// Run `f` over the whole state and return its result. Poison-tolerant:
    /// the state is counters and caches, never an invariant a poisoned
    /// thread could corrupt further.
    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut GuardrailInner) -> T) -> T {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        f(&mut guard)
    }

    /// A new user turn arrived: reset the volatile doom signals (score,
    /// grace) while keeping the recent-call window, so loops that survive a
    /// "continue" are still detected. The guardrail counters and the dedup
    /// cache are conversation state and stay.
    pub fn reset_for_turn(&self) {
        self.with(|inner| {
            inner.doom.reset_for_new_user_input();
            inner.answered.clear();
        });
    }

    /// A compaction rewrote history: the counters were tripped against a
    /// conversation that no longer exists, the cache sampled one that is
    /// gone, and the doom score described that world. Reset everything.
    pub fn reset(&self) {
        self.with(|inner| {
            inner.doom = DoomTracker::new();
            inner.guardrails.reset();
            inner.dedup.clear();
            inner.answered.clear();
        });
    }

    /// Mark the next run as a driver follow-up (advisor and overflow
    /// re-runs continue the same turn): the layer's `turn_start` skips the
    /// volatile doom reset and consumes the mark.
    pub fn mark_follow_up(&self) {
        self.with(|inner| inner.follow_up = true);
    }

    /// Take the follow-up mark, if set.
    pub(crate) fn take_follow_up(&self) -> bool {
        self.with(|inner| std::mem::take(&mut inner.follow_up))
    }

    /// Record that `id` was answered without executing (a policy or
    /// guardrail denial), so the guardrail layer's post-hook skips its
    /// bookkeeping for it.
    pub(crate) fn mark_answered(&self, id: &str) {
        self.with(|inner| {
            inner.answered.insert(id.to_owned());
        });
    }

    /// Take the answered-without-execution mark for `id`, if set.
    pub(crate) fn take_answered(&self, id: &str) -> bool {
        self.with(|inner| inner.answered.remove(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn alternating_failures_never_trip_exact_loop() {
        // Two different failing inputs alternating must leave the exact-failure
        // streak at 1, so the exact-repeat thresholds never fire; only the
        // same-tool streak (its own guardrail) may warn.
        let mut g = ToolGuardrails::new();
        for _ in 0..EXACT_REPEAT_BLOCK * 2 {
            g.record_result("bash", &json!("cmd_a"), "err", true, false);
            g.record_result("bash", &json!("cmd_b"), "err", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &json!("cmd_a"), false),
            GuardrailDecision::Block,
            "only the any-failure circuit breaker may trip"
        );
        let mut g = ToolGuardrails::new();
        g.record_result("bash", &json!("cmd_a"), "err", true, false);
        g.record_result("bash", &json!("cmd_b"), "err", true, false);
        // The last errored call was cmd_b: the differing failures left the
        // exact streak at 1 (the unchecked counter would already have hit
        // EXACT_REPEAT_WARN) and any_fail below the same-tool warn, so
        // neither guardrail fires here.
        assert_eq!(
            g.check_before_call("bash", &json!("cmd_b"), false),
            GuardrailDecision::Allow
        );
    }

    #[test]
    fn interleaved_failure_prevents_exact_loop_block() {
        // A, A, B, A: the differing failure in the middle resets the exact
        // streak, so the last call leaves it at 1 — only the same-tool
        // failure warn applies.
        let mut g = ToolGuardrails::new();
        for input in ["cmd_a", "cmd_a", "cmd_b", "cmd_a"] {
            g.record_result("bash", &json!(input), "err", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &json!("cmd_a"), false),
            GuardrailDecision::Warn
        );
    }

    #[test]
    fn allow_when_no_history() {
        let mut g = ToolGuardrails::new();
        assert_eq!(
            g.check_before_call("bash", &json!("ls"), false),
            GuardrailDecision::Allow
        );
    }

    #[test]
    fn warn_after_exact_repeats() {
        let mut g = ToolGuardrails::new();
        let input = json!("ls");
        for _ in 0..EXACT_REPEAT_WARN {
            g.record_result("bash", &input, "error", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &input, false),
            GuardrailDecision::Warn
        );
    }

    #[test]
    fn block_after_many_exact_repeats() {
        let mut g = ToolGuardrails::new();
        let input = json!("ls");
        for _ in 0..EXACT_REPEAT_BLOCK {
            g.record_result("bash", &input, "error", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &input, false),
            GuardrailDecision::Block
        );
    }

    #[test]
    fn warn_after_same_tool_failures() {
        let mut g = ToolGuardrails::new();
        for i in 0..SAME_TOOL_FAIL_WARN {
            g.record_result("bash", &json!(format!("cmd{i}")), "error", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &json!("new_cmd"), false),
            GuardrailDecision::Warn
        );
    }

    #[test]
    fn no_progress_warning_for_read_only() {
        let mut g = ToolGuardrails::new();
        let result = "same output";
        for _ in 0..NO_PROGRESS_WARN + 1 {
            g.record_result("grep", &json!("pattern"), result, false, true);
        }
        assert_eq!(
            g.check_before_call("grep", &json!("pattern"), true),
            GuardrailDecision::Warn
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut g = ToolGuardrails::new();
        for _ in 0..SAME_TOOL_FAIL_BLOCK {
            g.record_result("bash", &json!("cmd"), "err", true, false);
        }
        g.reset();
        assert_eq!(
            g.check_before_call("bash", &json!("cmd"), false),
            GuardrailDecision::Allow
        );
    }

    #[test]
    fn success_clears_failure_streak() {
        // Non-consecutive failures must not accumulate into a block: a
        // single success resets the streak.
        let mut g = ToolGuardrails::new();
        for i in 0..SAME_TOOL_FAIL_WARN {
            g.record_result("bash", &json!(format!("cmd{i}")), "err", true, false);
        }
        g.record_result("bash", &json!("ok"), "done", false, false);
        assert_eq!(
            g.check_before_call("bash", &json!("next"), false),
            GuardrailDecision::Allow
        );
        for i in 0..SAME_TOOL_FAIL_WARN - 1 {
            g.record_result("bash", &json!(format!("again{i}")), "err", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &json!("again"), false),
            GuardrailDecision::Allow,
            "failures never reached the warn threshold after the reset"
        );
    }

    #[test]
    fn block_is_a_circuit_breaker_not_a_ban() {
        let mut g = ToolGuardrails::new();
        for i in 0..SAME_TOOL_FAIL_BLOCK {
            g.record_result("bash", &json!(format!("cmd{i}")), "err", true, false);
        }
        assert_eq!(
            g.check_before_call("bash", &json!("other"), false),
            GuardrailDecision::Block
        );
        // The block cleared the streak, so the tool is usable again rather
        // than locked out for the rest of the session.
        assert_eq!(
            g.check_before_call("bash", &json!("other"), false),
            GuardrailDecision::Allow
        );
    }

    // --- GuardrailState ---

    #[test]
    fn a_new_turn_resets_the_doom_score_but_keeps_the_window() {
        let state = GuardrailState::new(None);
        state.with(|inner| {
            inner.doom.note_doom_loop();
            inner.doom.mark_grace_called();
            inner
                .doom
                .recent_calls
                .record("read".into(), &json!({"p":1}));
            inner
                .doom
                .recent_calls
                .record("read".into(), &json!({"p":1}));
            inner.answered.insert("t1".into());
        });
        state.reset_for_turn();
        state.with(|inner| {
            assert_eq!(inner.doom.score(), 0);
            assert!(!inner.doom.should_grace());
            assert!(
                inner
                    .doom
                    .recent_calls
                    .is_doom_loop("read", &json!({"p":1})),
                "the recent-call window survives a new prompt"
            );
            assert!(inner.answered.is_empty());
        });
    }

    #[test]
    fn a_compaction_reset_clears_everything() {
        let state = GuardrailState::new(None);
        state.with(|inner| {
            inner.doom.note_doom_loop();
            inner
                .guardrails
                .record_result("bash", &json!("cmd"), "err", true, false);
        });
        state.reset();
        state.with(|inner| {
            assert_eq!(inner.doom.score(), 0);
            assert_eq!(
                inner
                    .guardrails
                    .check_before_call("bash", &json!("cmd"), false),
                GuardrailDecision::Allow
            );
        });
    }

    #[test]
    fn answered_marks_are_taken_once() {
        let state = GuardrailState::new(None);
        state.mark_answered("t1");
        assert!(state.take_answered("t1"));
        assert!(!state.take_answered("t1"), "a mark is consumed once");
    }

    #[test]
    fn no_progress_block_does_not_lock_reads() {
        let mut g = ToolGuardrails::new();
        let result = "same output";
        // The first identical result only seeds `last_result_hash`, so the
        // counter reaches NO_PROGRESS_BLOCK one record later.
        for _ in 0..NO_PROGRESS_BLOCK + 1 {
            g.record_result("read", &json!("path"), result, false, true);
        }
        assert_eq!(
            g.check_before_call("read", &json!("path"), true),
            GuardrailDecision::Block
        );
        assert_eq!(
            g.check_before_call("read", &json!("path"), true),
            GuardrailDecision::Allow
        );
    }
}
