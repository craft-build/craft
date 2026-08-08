use crate::agent::doom::SharedDoomTracker;
use crate::agent::escalation::EscalationTracker;

pub(super) struct AgentDoom {
    pub(super) doom: SharedDoomTracker,
    pub(super) escalation: EscalationTracker,
}
