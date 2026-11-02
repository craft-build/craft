//! The persistence layer: checkpoints the history change feed at step and
//! turn boundaries. A persistence error is logged and surfaced as a
//! craft event; it never fails the run.

use std::sync::Arc;

use async_trait::async_trait;

use animist::agent::{Agent, LayerImpl, Result, RunContext, RunStatus, StepAction, StepOutcome};

use super::events::{CraftEvent, EventSink};
use super::persist::Persister;

pub(crate) struct PersistLayer {
    persister: Arc<tokio::sync::Mutex<Persister>>,
    sink: EventSink,
}

impl PersistLayer {
    pub(crate) fn new(persister: Arc<tokio::sync::Mutex<Persister>>, sink: EventSink) -> Self {
        Self { persister, sink }
    }

    async fn checkpoint(&self) {
        let mut persister = self.persister.lock().await;
        if let Err(error) = persister.checkpoint().await {
            tracing::warn!("session checkpoint failed: {error}");
            self.sink
                .craft(CraftEvent::PersistenceFailed(error.to_string()));
        }
    }
}

#[async_trait]
impl LayerImpl for PersistLayer {
    async fn step_end(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        _outcome: &StepOutcome,
    ) -> Result<StepAction> {
        // Runs after the step's results commit (and the agent's squash).
        self.checkpoint().await;
        Ok(StepAction::default())
    }

    async fn turn_end(&self, _agent: &Agent, _run: &RunContext, _status: RunStatus) -> Result<()> {
        self.checkpoint().await;
        Ok(())
    }
}
