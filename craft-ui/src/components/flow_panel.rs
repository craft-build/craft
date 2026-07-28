use craft_agent::TurnType;

/// A snapshot the status-line hint renders. Owned so the hint is decoupled
/// from the `SessionState` borrow: the app hands the panel a fresh snapshot
/// whenever the workstream changes.
#[derive(Debug, Clone, Default)]
pub struct FlowSnapshot {
    pub workstream_id: String,
    pub stage: Option<TurnType>,
}

impl FlowSnapshot {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.workstream_id.is_empty()
    }
}

/// Keeps the live Flow snapshot (workstream id + current stage) plus running
/// and total counts of task-spawned child threads, so the input-box status
/// line can render a compact `flow · <stage> · <active>/<total>` hint each
/// frame. The counts are maintained from `FlowProgress::ThreadSpawn`/
/// `ThreadExit` events, which mirror the live `ThreadManager` tree.
pub struct FlowPanel {
    pub(crate) snapshot: FlowSnapshot,
    pub(crate) live_threads: usize,
    pub(crate) total_threads: usize,
}

impl FlowPanel {
    pub fn new() -> Self {
        Self {
            snapshot: FlowSnapshot::default(),
            live_threads: 0,
            total_threads: 0,
        }
    }

    pub fn set_snapshot(&mut self, snapshot: FlowSnapshot) {
        self.snapshot = snapshot;
    }

    pub fn reset(&mut self) {
        self.snapshot = FlowSnapshot::default();
        self.live_threads = 0;
        self.total_threads = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_clears_state() {
        let mut p = FlowPanel::new();
        p.snapshot = FlowSnapshot {
            workstream_id: "w".into(),
            stage: Some(TurnType::Plan),
        };
        p.live_threads = 3;
        p.total_threads = 5;
        p.reset();
        assert!(p.snapshot.is_empty());
        assert_eq!(p.live_threads, 0);
        assert_eq!(p.total_threads, 0);
    }
}
