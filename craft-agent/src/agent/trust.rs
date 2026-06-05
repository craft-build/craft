use std::collections::HashMap;

use craft_config::TrustDecayConfig;
use tracing::warn;

#[derive(Debug)]
pub(super) struct TrustTracker {
    failures: HashMap<String, u32>,
    config: TrustDecayConfig,
}

impl TrustTracker {
    pub(super) fn new(config: TrustDecayConfig) -> Self {
        Self {
            failures: HashMap::new(),
            config,
        }
    }

    pub(super) fn record_success(&mut self, tool: &str) {
        if self.config.reset_on_success {
            self.failures.remove(tool);
        }
    }

    pub(super) fn record_failure(&mut self, tool: &str) {
        let count = self.failures.entry(tool.to_owned()).or_insert(0);
        *count += 1;

        if *count == self.config.warn_after {
            warn!(tool, consecutive_failures = *count, "tool approaching drop threshold");
        }
    }

    #[allow(dead_code)]
    pub(super) fn is_dropped(&self, tool: &str) -> bool {
        let count = self.failures.get(tool).copied().unwrap_or(0);
        count >= self.config.drop_after
    }

    #[allow(dead_code)]
    pub(super) fn filter_tools(&self, tools: &[String]) -> Vec<String> {
        let remaining: Vec<_> = tools
            .iter()
            .filter(|t| !self.is_dropped(t))
            .cloned()
            .collect();

        if remaining.len() < self.config.min_tools {
            return tools.to_vec();
        }

        remaining
    }

    #[allow(dead_code)]
    pub(super) fn clear(&mut self) {
        self.failures.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> TrustDecayConfig {
        TrustDecayConfig {
            warn_after: 2,
            drop_after: 3,
            min_tools: 2,
            reset_on_success: true,
        }
    }

    fn tracker() -> TrustTracker {
        TrustTracker::new(config())
    }

    #[test]
    fn success_resets_counter() {
        let mut t = tracker();
        t.record_failure("mytool");
        t.record_failure("mytool");
        t.record_success("mytool");
        assert!(!t.is_dropped("mytool"));
    }

    #[test]
    fn dropped_after_threshold() {
        let mut t = tracker();
        t.record_failure("mytool");
        t.record_failure("mytool");
        assert!(!t.is_dropped("mytool"));
        t.record_failure("mytool");
        assert!(t.is_dropped("mytool"));
    }

    #[test]
    fn filter_removes_dropped_tools() {
        let mut t = tracker();
        for _ in 0..3 {
            t.record_failure("bad_tool");
        }
        let tools: Vec<String> = vec!["read".into(), "bad_tool".into(), "grep".into()];
        let filtered = t.filter_tools(&tools);
        assert_eq!(filtered, vec!["read", "grep"]);
    }

    #[test]
    fn min_tools_safeguard() {
        let mut t = TrustTracker::new(TrustDecayConfig {
            warn_after: 1,
            drop_after: 1,
            min_tools: 3,
            reset_on_success: true,
        });
        for tool in &["a", "b"] {
            t.record_failure(tool);
        }
        let tools: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let filtered = t.filter_tools(&tools);
        assert_eq!(filtered.len(), 3, "should not drop below min_tools");
    }

    #[test]
    fn clear_resets_all() {
        let mut t = tracker();
        for _ in 0..3 {
            t.record_failure("mytool");
        }
        t.clear();
        assert!(!t.is_dropped("mytool"));
    }
}
