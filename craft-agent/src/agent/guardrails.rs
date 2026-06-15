use std::collections::HashMap;

use serde_json::Value;
use tracing::warn;

const EXACT_REPEAT_WARN: usize = 2;
const EXACT_REPEAT_BLOCK: usize = 4;
const SAME_TOOL_FAIL_WARN: usize = 3;
const SAME_TOOL_FAIL_BLOCK: usize = 6;
const NO_PROGRESS_WARN: usize = 2;
const NO_PROGRESS_BLOCK: usize = 4;

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

struct ToolTracker {
    exact_fail_count: usize,
    any_fail_count: usize,
    last_result_hash: Option<u64>,
    same_result_count: usize,
    last_input_hash: Option<u64>,
}

impl ToolTracker {
    fn new() -> Self {
        Self {
            exact_fail_count: 0,
            any_fail_count: 0,
            last_result_hash: None,
            same_result_count: 0,
            last_input_hash: None,
        }
    }
}

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
        &self,
        tool: &str,
        input: &Value,
        is_read_only: bool,
    ) -> GuardrailDecision {
        let tracker = match self.trackers.get(tool) {
            Some(t) => t,
            None => return GuardrailDecision::Allow,
        };

        let input_hash = Self::hash_value(input);
        if tracker.exact_fail_count >= EXACT_REPEAT_BLOCK
            && input_hash == tracker.last_input_hash.unwrap_or(0)
        {
            return GuardrailDecision::Block;
        }
        if tracker.exact_fail_count >= EXACT_REPEAT_WARN
            && input_hash == tracker.last_input_hash.unwrap_or(0)
        {
            return GuardrailDecision::Warn;
        }

        if tracker.any_fail_count >= SAME_TOOL_FAIL_BLOCK {
            return GuardrailDecision::Block;
        }
        if tracker.any_fail_count >= SAME_TOOL_FAIL_WARN {
            return GuardrailDecision::Warn;
        }

        if is_read_only && tracker.same_result_count >= NO_PROGRESS_BLOCK {
            return GuardrailDecision::Block;
        }
        if is_read_only && tracker.same_result_count >= NO_PROGRESS_WARN {
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
            tracker.exact_fail_count += 1;
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

        if let Some(w) = &warning {
            warn!(tool, "guardrail warning: {}", w.reason);
        }

        warning
    }

    #[cfg(test)]
    pub fn reset(&mut self) {
        self.trackers.clear();
    }
}

#[cfg(test)]
mod tests {

    use serde_json::json;

    use super::*;

    #[test]
    fn allow_when_no_history() {
        let g = ToolGuardrails::new();
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
}
