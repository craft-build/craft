//! Recovery classification taxonomy for tool/subagent/deterministic failures.
//!
//! Provides a single [`classify`] seam that maps an error to a typed
//! [`RecoveryFailureKind`] and a deterministic [`RecoveryAction`] so call
//! sites log structured recovery metadata (kind + action) instead of
//! opaque error strings. The action is not yet wired to drive retry /
//! escalate / stop behavior at the call sites — it is an observability-only
//! seam until the turn loop and subagent launcher branch on it.
//!
//! Provider streaming retries are already handled by `stream_with_retry`;
//! this module covers the layer above it (tool dispatch, subagent launch,
//! flow gate and reconciliation failures).

use std::time::Duration;

use craft_providers::AgentError;

/// Categories of failure the agent and flow pipelines can surface. Derived
/// from gsd's `recovery-classification.ts`, trimmed to what craft needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecoveryFailureKind {
    /// A structured-output schema mismatch (invalid JSON, missing required
    /// field). Deterministic: retrying with the same prompt will not help.
    ToolSchema,
    /// A required tool is temporarily unavailable (MCP server down, plugin
    /// loading). Transient: a retry may succeed.
    ToolUnavailable,
    /// A deterministic policy violation (unknown subagent type, unknown
    /// isolation mode, blocked permission). Deterministic: stop.
    DeterministicPolicy,
    /// The agent lifecycle cannot progress (max turns reached, stuck in a
    /// loop). Needs escalation, not a blind retry.
    LifecycleProgression,
    /// Worktree isolation setup failed (git unavailable, path invalid).
    /// Deterministic without environmental change: stop.
    WorktreeInvalid,
    /// A worker/subagent is stale (crashed, cancelled, or produced no usable
    /// output). Escalate to decide rework vs abort.
    StaleWorker,
    /// Drift between persisted state and on-disk artifacts detected by
    /// reconciliation. Escalate so the caller can decide repair vs abort.
    ReconciliationDrift,
    /// An illegal state-machine transition was attempted. Escalate.
    IllegalTransition,
    /// A provider API error (429, 5xx, transport). Transient when the
    /// underlying `AgentError::is_retryable()` says so.
    Provider,
    /// An unclassified runtime error. Escalate rather than guessing.
    RuntimeUnknown,
}

/// The action a caller should take for a given failure kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Retry up to `max` times, waiting `delay` between attempts.
    Retry { max: u32, delay: Duration },
    /// Escalate to a higher-level decision (rework, human, or abort).
    Escalate,
    /// Stop the run immediately; the failure is deterministic.
    Stop,
}

const RETRY_TOOL_UNAVAILABLE: RecoveryAction = RecoveryAction::Retry {
    max: 3,
    delay: Duration::from_secs(1),
};
const RETRY_PROVIDER: RecoveryAction = RecoveryAction::Retry {
    max: 5,
    delay: Duration::from_secs(2),
};

/// Deterministic mapping from failure kind to action.
pub fn action_for(kind: RecoveryFailureKind) -> RecoveryAction {
    match kind {
        RecoveryFailureKind::ToolSchema
        | RecoveryFailureKind::DeterministicPolicy
        | RecoveryFailureKind::WorktreeInvalid => RecoveryAction::Stop,
        RecoveryFailureKind::ToolUnavailable => RETRY_TOOL_UNAVAILABLE,
        RecoveryFailureKind::Provider => RETRY_PROVIDER,
        RecoveryFailureKind::LifecycleProgression
        | RecoveryFailureKind::StaleWorker
        | RecoveryFailureKind::ReconciliationDrift
        | RecoveryFailureKind::IllegalTransition
        | RecoveryFailureKind::RuntimeUnknown => RecoveryAction::Escalate,
    }
}

/// Classify a typed error by downcasting to known error types (`AgentError`)
/// and falling back to string-pattern matching on the display string for
/// errors from crates this module cannot import (e.g. `craft_flow`'s
/// `ReconciliationError` and `FlowRunError`).
pub fn classify(err: &(dyn std::error::Error + 'static)) -> (RecoveryFailureKind, RecoveryAction) {
    if let Some(api_err) = err.downcast_ref::<AgentError>() {
        let kind = if api_err.is_retryable() {
            RecoveryFailureKind::Provider
        } else {
            RecoveryFailureKind::RuntimeUnknown
        };
        return (kind, action_for(kind));
    }
    let kind = classify_string(&err.to_string());
    (kind, action_for(kind))
}

/// Classify a bare subagent error string (the `Err(String)` from
/// `run_subagent`) into a failure kind and action. Used by flow's dispatch
/// sites that only have the opaque string.
pub fn classify_subagent_error(msg: &str) -> (RecoveryFailureKind, RecoveryAction) {
    let kind = classify_string(msg);
    (kind, action_for(kind))
}

fn classify_string(msg: &str) -> RecoveryFailureKind {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("schema")
        || lower.contains("did not produce schema-valid json")
        || lower.contains("output schema")
    {
        return RecoveryFailureKind::ToolSchema;
    }
    if lower.contains("reconciliation") || lower.contains("drift") {
        return RecoveryFailureKind::ReconciliationDrift;
    }
    if lower.contains("illegal transition") || lower.contains("illegal state") {
        return RecoveryFailureKind::IllegalTransition;
    }
    if lower.contains("worktree") {
        return RecoveryFailureKind::WorktreeInvalid;
    }
    if lower.contains("unknown subagent type") || lower.contains("unknown isolation") {
        return RecoveryFailureKind::DeterministicPolicy;
    }
    if lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("503")
        || lower.contains("overloaded")
    {
        return RecoveryFailureKind::Provider;
    }
    if lower.contains("tool unavailable") || lower.contains("tool not found") {
        return RecoveryFailureKind::ToolUnavailable;
    }
    if lower.contains("stale") || lower.contains("cancelled") || lower.contains("no usable output")
    {
        return RecoveryFailureKind::StaleWorker;
    }
    RecoveryFailureKind::RuntimeUnknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(RecoveryFailureKind::ToolSchema, RecoveryAction::Stop ; "tool_schema_stops")]
    #[test_case(RecoveryFailureKind::DeterministicPolicy, RecoveryAction::Stop ; "deterministic_policy_stops")]
    #[test_case(RecoveryFailureKind::WorktreeInvalid, RecoveryAction::Stop ; "worktree_invalid_stops")]
    #[test_case(RecoveryFailureKind::LifecycleProgression, RecoveryAction::Escalate ; "lifecycle_escalates")]
    #[test_case(RecoveryFailureKind::StaleWorker, RecoveryAction::Escalate ; "stale_worker_escalates")]
    #[test_case(RecoveryFailureKind::ReconciliationDrift, RecoveryAction::Escalate ; "reconciliation_drift_escalates")]
    #[test_case(RecoveryFailureKind::IllegalTransition, RecoveryAction::Escalate ; "illegal_transition_escalates")]
    #[test_case(RecoveryFailureKind::RuntimeUnknown, RecoveryAction::Escalate ; "runtime_unknown_escalates")]
    fn action_for_matches_taxonomy(kind: RecoveryFailureKind, expected: RecoveryAction) {
        assert_eq!(action_for(kind), expected);
    }

    #[test_case(RecoveryFailureKind::ToolUnavailable ; "tool_unavailable_is_retry")]
    #[test_case(RecoveryFailureKind::Provider ; "provider_is_retry")]
    fn action_for_retries(kind: RecoveryFailureKind) {
        assert!(matches!(action_for(kind), RecoveryAction::Retry { .. }));
    }

    #[test]
    fn classify_typed_agent_error_retryable_is_provider() {
        let err = AgentError::Api {
            status: 429,
            message: "rate limited".into(),
        };
        let (kind, action) = classify(&err);
        assert_eq!(kind, RecoveryFailureKind::Provider);
        assert!(matches!(action, RecoveryAction::Retry { .. }));
    }

    #[test]
    fn classify_typed_agent_error_non_retryable_is_runtime_unknown() {
        let err = AgentError::Api {
            status: 400,
            message: "bad request".into(),
        };
        let (kind, action) = classify(&err);
        assert_eq!(kind, RecoveryFailureKind::RuntimeUnknown);
        assert_eq!(action, RecoveryAction::Escalate);
    }

    #[derive(Debug, thiserror::Error)]
    #[error("reconciliation drift: {0}")]
    struct TestReconciliationError(String);

    #[test]
    fn classify_reconciliation_error_string_is_drift_escalate() {
        let err = TestReconciliationError("chunk c1 execute doc missing".into());
        let (kind, action) = classify(&err);
        assert_eq!(kind, RecoveryFailureKind::ReconciliationDrift);
        assert_eq!(action, RecoveryAction::Escalate);
    }

    #[test]
    fn classify_subagent_error_rate_limited_is_provider_retry() {
        let (kind, action) = classify_subagent_error("rate limited by provider");
        assert_eq!(kind, RecoveryFailureKind::Provider);
        assert!(matches!(action, RecoveryAction::Retry { .. }));
    }

    #[test]
    fn classify_subagent_error_schema_mismatch_is_tool_schema_stop() {
        let (kind, action) = classify_subagent_error("subagent did not produce schema-valid JSON");
        assert_eq!(kind, RecoveryFailureKind::ToolSchema);
        assert_eq!(action, RecoveryAction::Stop);
    }

    #[test]
    fn classify_subagent_error_unknown_type_is_deterministic_stop() {
        let (kind, action) = classify_subagent_error("unknown subagent type: bogus");
        assert_eq!(kind, RecoveryFailureKind::DeterministicPolicy);
        assert_eq!(action, RecoveryAction::Stop);
    }

    #[test]
    fn classify_subagent_error_worktree_is_worktree_invalid_stop() {
        let (kind, action) = classify_subagent_error("worktree creation failed: not a git repo");
        assert_eq!(kind, RecoveryFailureKind::WorktreeInvalid);
        assert_eq!(action, RecoveryAction::Stop);
    }

    #[test]
    fn classify_subagent_error_opaque_is_runtime_unknown_escalate() {
        let (kind, action) = classify_subagent_error("something unexpected happened");
        assert_eq!(kind, RecoveryFailureKind::RuntimeUnknown);
        assert_eq!(action, RecoveryAction::Escalate);
    }
}
