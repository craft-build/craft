//! Per-tool guardrails against unproductive loops.
//!
//! Ported from the reference `craft-agent/src/agent/guardrails.rs`: the
//! dispatcher consults [`ToolGuardrails::check_before_call`] before executing
//! a call and feeds results back through [`ToolGuardrails::record_result`].
//! State resets when a compaction run rewrites history (the compacted
//! conversation no longer describes the calls that tripped the counters).
//!
//! Deliberate divergence from the reference: its failure counters never
//! decayed, so a tool that failed a few non-consecutive times (routine for a
//! general-purpose tool like `bash`, where a non-zero exit is reported as an
//! error) warned on *every* later call and, past the block threshold, was
//! locked out for the rest of the session. Here a successful call clears the
//! failure streak, and a block trips as a one-shot circuit breaker that
//! clears the triggering counters, so no tool can be permanently banned.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

const EXACT_REPEAT_WARN: usize = 2;
const EXACT_REPEAT_BLOCK: usize = 4;
const SAME_TOOL_FAIL_WARN: usize = 3;
const SAME_TOOL_FAIL_BLOCK: usize = 6;
const NO_PROGRESS_WARN: usize = 2;
const NO_PROGRESS_BLOCK: usize = 4;

/// Session-shared handle; the dispatcher and the compaction engine both
/// hold one.
pub type SharedGuardrails = Arc<Mutex<ToolGuardrails>>;

pub fn shared_guardrails() -> SharedGuardrails {
    Arc::new(Mutex::new(ToolGuardrails::new()))
}

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
        // failure warn applies. Under the old unconditional counter the
        // streak would have reached EXACT_REPEAT_BLOCK and blocked.
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
        // Non-consecutive failures must not accumulate into a block: a single
        // success resets the streak. This is the `bash` regression — routine
        // non-zero exits used to warn on every later call, then lock it out.
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
