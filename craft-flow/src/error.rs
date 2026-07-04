//! Typed errors for the Flow pipeline. Wraps [`FlowStore`]'s storage errors and
//! the embedder/runner failures that were previously flattened into `String`,
//! so callers can match on category (retryable I/O vs. bad input vs. agent
//! failure) instead of parsing opaque text.

use thiserror::Error;

use crate::Stage;
use craft_storage::flow::FlowError as StorageFlowError;

/// A typed error returned by the Flow pipeline's injectable seams
/// ([`crate::StageRunner`], [`crate::search::Embedder`], `reindex`, `search`).
///
/// The orchestrator's public [`crate::FlowOutcome::Failed`] boundary keeps a
/// `String` reason for display, but the internal plumbing uses this enum so
/// error categories are preserved end-to-end.
#[derive(Debug, Error)]
pub enum FlowRunError {
    /// A persisted-state read/write failed (permissions, corruption, traversal
    /// guard). Generally retryable.
    #[error("flow storage error: {0}")]
    Storage(#[from] StorageFlowError),

    /// The embedder (ONNX or injected) failed to produce vectors.
    #[error("flow embedder error: {0}")]
    Embedder(String),

    /// A stage subagent returned an unrecoverable failure. Carries the stage
    /// that detected the failure so the orchestrator can attribute it.
    #[error("flow stage failed: {reason}")]
    Stage {
        stage: Option<Stage>,
        reason: String,
    },

    /// Catch-all for malformed inputs that don't fit a more specific variant.
    #[error("flow error: {0}")]
    Other(String),
}

impl FlowRunError {
    /// Attach the orchestrator-stage that surfaced this error, so the
    /// `FlowOutcome::Failed` boundary can attribute it. Idempotent: a stage
    /// already set (e.g. by a structured `Stage` variant) is preserved.
    pub fn with_stage(self, stage: Stage) -> Self {
        match self {
            FlowRunError::Stage {
                stage: existing,
                reason,
            } => FlowRunError::Stage {
                stage: Some(existing.unwrap_or(stage)),
                reason,
            },
            other => FlowRunError::Stage {
                stage: Some(stage),
                reason: other.to_string(),
            },
        }
    }

    /// The stage attributed to this error, if any (used by the orchestrator to
    /// build `FlowOutcome::Failed`). Returns [`crate::Stage::Verifier`] as a
    /// neutral fallback when no stage was attached.
    pub fn stage(&self) -> Stage {
        match self {
            FlowRunError::Stage { stage, .. } => stage.unwrap_or(Stage::Verifier),
            _ => Stage::Verifier,
        }
    }

    /// Flatten to the display string used at the `FlowOutcome::Failed` boundary.
    pub fn into_reason(self) -> String {
        self.to_string()
    }
}

impl From<String> for FlowRunError {
    fn from(s: String) -> Self {
        FlowRunError::Other(s)
    }
}
