use std::collections::HashMap;

use tracing::warn;

use crate::AgentEvent;

const DEFAULT_FAILURE_THRESHOLD: f64 = 0.60;
const DEFAULT_MIN_CALLS: u32 = 5;

#[derive(Debug, Clone, Copy)]
pub struct EscalationConfig {
    pub failure_threshold: f64,
    pub min_calls: u32,
}

impl Default for EscalationConfig {
    fn default() -> Self {
        Self {
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
            min_calls: DEFAULT_MIN_CALLS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModelTier {
    Weak,
    Default,
    Strong,
}

impl ModelTier {
    pub fn from_model_id(model_id: &str) -> Self {
        let id = model_id.to_lowercase();
        if id.contains("haiku") || id.contains("flash") || id.contains("mini") {
            Self::Weak
        } else if id.contains("opus")
            || id.contains("o3")
            || id.contains("pro")
            || id.contains("ultra")
        {
            Self::Strong
        } else {
            Self::Default
        }
    }

    pub fn escalate(self) -> Option<Self> {
        match self {
            Self::Weak => Some(Self::Default),
            Self::Default => Some(Self::Strong),
            Self::Strong => None,
        }
    }

    pub fn suggested_model_spec(self) -> Option<&'static str> {
        match self.escalate()? {
            Self::Default => Some("anthropic/claude-sonnet-4-20250514"),
            Self::Strong => Some("anthropic/claude-opus-4-20250514"),
            Self::Weak => None,
        }
    }
}

#[derive(Debug)]
pub struct EscalationTracker {
    config: EscalationConfig,
    stats: HashMap<String, CallStats>,
}

#[derive(Debug, Default)]
struct CallStats {
    calls: u32,
    failures: u32,
}

impl EscalationTracker {
    pub fn new(config: EscalationConfig) -> Self {
        Self {
            config,
            stats: HashMap::new(),
        }
    }

    pub fn record(&mut self, model_id: &str, failed: bool) {
        let stats = self.stats.entry(model_id.to_owned()).or_default();
        stats.calls += 1;
        if failed {
            stats.failures += 1;
        }
    }

    /// Check if the model should be escalated. If so, emit a `ModelEscalation`
    /// event so the UI can switch models on the next turn.
    pub fn check_and_emit(
        &mut self,
        model_id: &str,
        current_tier: ModelTier,
        event_tx: &crate::EventSender,
    ) {
        if !self.should_escalate(model_id) {
            return;
        }
        let Some(suggested_spec) = current_tier.suggested_model_spec() else {
            warn!(
                model = model_id,
                "failure rate high but already at strongest tier"
            );
            return;
        };
        warn!(
            model = model_id,
            suggested = suggested_spec,
            "escalating model due to high failure rate"
        );
        let _ = event_tx.send(AgentEvent::ModelEscalation {
            from: model_id.to_owned(),
            to: suggested_spec.to_owned(),
        });
        self.clear();
    }

    pub(crate) fn should_escalate(&self, model_id: &str) -> bool {
        let Some(stats) = self.stats.get(model_id) else {
            return false;
        };
        if stats.calls < self.config.min_calls {
            return false;
        }
        let rate = stats.failures as f64 / stats.calls as f64;
        rate >= self.config.failure_threshold
    }

    fn clear(&mut self) {
        self.stats.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> EscalationConfig {
        EscalationConfig {
            failure_threshold: 0.60,
            min_calls: 5,
        }
    }

    #[test]
    fn no_escalation_below_min_calls() {
        let mut tracker = EscalationTracker::new(config());
        for _ in 0..4 {
            tracker.record("model-a", true);
        }
        assert!(!tracker.should_escalate("model-a"));
    }

    #[test]
    fn escalates_when_failure_rate_exceeds_threshold() {
        let mut tracker = EscalationTracker::new(config());
        for i in 0..5 {
            tracker.record("model-a", i < 3);
        }
        assert!(tracker.should_escalate("model-a"));
    }

    #[test]
    fn no_escalation_when_failure_rate_below_threshold() {
        let mut tracker = EscalationTracker::new(config());
        for i in 0..5 {
            tracker.record("model-a", i < 2);
        }
        assert!(!tracker.should_escalate("model-a"));
    }

    #[test]
    fn model_tier_from_id() {
        assert_eq!(
            ModelTier::from_model_id("anthropic/claude-haiku-4-20250514"),
            ModelTier::Weak
        );
        assert_eq!(
            ModelTier::from_model_id("google/gemini-2.0-flash"),
            ModelTier::Weak
        );
        assert_eq!(
            ModelTier::from_model_id("anthropic/claude-sonnet-4-20250514"),
            ModelTier::Default
        );
        assert_eq!(
            ModelTier::from_model_id("openai/gpt-4o"),
            ModelTier::Default
        );
        assert_eq!(
            ModelTier::from_model_id("anthropic/claude-opus-4-20250514"),
            ModelTier::Strong
        );
        assert_eq!(ModelTier::from_model_id("openai/o3"), ModelTier::Strong);
    }

    #[test]
    fn tier_escalation() {
        assert_eq!(ModelTier::Weak.escalate(), Some(ModelTier::Default));
        assert_eq!(ModelTier::Default.escalate(), Some(ModelTier::Strong));
        assert_eq!(ModelTier::Strong.escalate(), None);
    }

    #[test]
    fn unknown_model_no_escalation() {
        let tracker = EscalationTracker::new(config());
        assert!(!tracker.should_escalate("unknown"));
    }

    #[test]
    fn clear_resets() {
        let mut tracker = EscalationTracker::new(config());
        for _ in 0..5 {
            tracker.record("model-a", true);
        }
        tracker.clear();
        assert!(!tracker.should_escalate("model-a"));
    }
}
