//! Snapshot layer: pre-captures `/undo` originals for writing calls, at the
//! tail of the chain so only calls that will really run are captured.

use async_trait::async_trait;

use animist::agent::tool::ToolContext;
use animist::agent::{Agent, LayerImpl, Result, RunContext, RunStatus, ToolDecision};

use crate::harness::dedup;
use crate::snapshot::SnapshotManager;

pub(crate) struct SnapshotLayer {
    snapshots: SnapshotManager,
}

impl SnapshotLayer {
    pub(crate) fn new(snapshots: SnapshotManager) -> Self {
        Self { snapshots }
    }
}

#[async_trait]
impl LayerImpl for SnapshotLayer {
    async fn pre_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
    ) -> Result<ToolDecision> {
        // Capture before execution, first-wins: the tools also note their
        // own paths while running, but the pre-execution content is the
        // original `/undo` must restore.
        if dedup::ToolDedupCache::is_write(&context.call.name) {
            for path in dedup::extract_write_paths(&context.call.name, &context.call.input) {
                self.snapshots.note(std::path::Path::new(&path));
            }
        }
        Ok(ToolDecision::Run)
    }

    async fn turn_end(&self, _agent: &Agent, _run: &RunContext, status: RunStatus) -> Result<()> {
        // Clean (and cancelled) runs close the capture session onto the
        // `/undo` stack; a failed run leaves it for the next attempt to
        // merge into.
        if status != RunStatus::Failed {
            self.snapshots.commit();
        }
        Ok(())
    }
}
