use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Generate a fresh opaque workstream id (16 hex chars from 8 random bytes).
/// Matches the opaque-string convention of session ids without pulling uuid
/// into craft-ui.
pub(super) fn new_workstream_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

use crate::agent::QueuedMessage;
use crate::components::Status;
use crate::theme;
use craft_agent::{AgentInput, AgentMode};
use craft_flow::{ChunkStatus, Stage};
use craft_storage::StateDir;
use craft_storage::plans;
use ratatui::style::{Color, Modifier, Style};

use super::App;

/// A single chunk tracked by the Flow panel, mirroring the relevant slice of
/// `craft_flow::Chunk`. Kept minimal (id/title/status) since the panel only
/// renders these. Iteration counts live in craft-flow's persisted documents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FlowChunkState {
    pub title: String,
    pub status: ChunkStatus,
    pub stage: Option<Stage>,
    /// Chunk's position in the plan array. Preserves plan order so the panel
    /// renders chunks in the order the plan agent intended (not alphabetical).
    pub order: usize,
    /// Ids of chunks this chunk depends on (plan DAG edges). Drives the graph.
    pub depends_on: Vec<String>,
}

/// Flow mode needs a workstream id; `FlowState` is the persisted workstream
/// carried in `SessionMeta`. The TUI mode holds the id separately so the
/// `Mode` enum stays `Copy`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FlowState {
    pub workstream_id: String,
    pub stage: Option<Stage>,
    pub chunks: BTreeMap<String, FlowChunkState>,
}

impl FlowState {
    /// Replace chunk state from a craft-flow workstream snapshot. Called when
    /// the agent pushes a Flow stage/chunk update.
    #[allow(dead_code)]
    pub(crate) fn sync_from(
        &mut self,
        stage: Option<Stage>,
        chunks: impl IntoIterator<Item = (String, String, ChunkStatus)>,
    ) {
        self.stage = stage;
        self.chunks = chunks
            .into_iter()
            .map(|(id, title, status)| {
                (
                    id,
                    FlowChunkState {
                        title,
                        status,
                        stage: None,
                        order: 0,
                        depends_on: Vec::new(),
                    },
                )
            })
            .collect();
    }

    #[allow(dead_code)]
    pub(crate) fn set_chunk_status(&mut self, chunk_id: &str, status: ChunkStatus) {
        self.chunks
            .entry(chunk_id.to_string())
            .or_insert_with(|| FlowChunkState {
                title: String::new(),
                status,
                stage: None,
                order: 0,
                depends_on: Vec::new(),
            })
            .status = status;
    }

    /// Update a chunk's title, status, per-chunk pipeline stage, plan order,
    /// and dependencies from a `FlowProgress::Chunk` event. Title, stage, order,
    /// and depends_on are only overwritten when the event carries meaningful
    /// values (non-empty title, Some stage, non-zero order, non-empty deps), so
    /// later status-only updates (e.g. Done, Running transitions from
    /// `emit_chunk`) never clobber data learned at the Queued event.
    pub(crate) fn set_chunk(
        &mut self,
        chunk_id: &str,
        title: &str,
        status: ChunkStatus,
        stage: Option<Stage>,
        order: usize,
        depends_on: &[String],
    ) {
        let entry = self
            .chunks
            .entry(chunk_id.to_string())
            .or_insert_with(|| FlowChunkState {
                title: String::new(),
                status,
                stage: None,
                order,
                depends_on: depends_on.to_vec(),
            });
        entry.status = status;
        if !title.is_empty() {
            entry.title = title.to_string();
        }
        if stage.is_some() {
            entry.stage = stage;
        }
        // Order 0 / empty deps come from transitional events (Running/Done/
        // Blocked/emit_chunk) that don't carry DAG metadata — don't clobber
        // the values learned at the Queued event.
        if order != 0 {
            entry.order = order;
        }
        if !depends_on.is_empty() {
            entry.depends_on = depends_on.to_vec();
        }
    }

    #[allow(dead_code)]
    pub(crate) fn clear_chunks(&mut self) {
        self.stage = None;
        self.chunks.clear();
    }

    /// Mark every chunk not already in a terminal state (Done/Blocked) as
    /// Blocked. Called when the flow run reaches a terminal outcome (Done /
    /// Failed / Cancelled) so the panel never freezes mid-run showing a chunk
    /// stuck in Running/Queued. Idempotent: chunks already terminal keep their
    /// status and last pipeline stage.
    pub(crate) fn finalize_non_terminal(&mut self) {
        for c in self.chunks.values_mut() {
            if !matches!(c.status, ChunkStatus::Done | ChunkStatus::Blocked) {
                c.status = ChunkStatus::Blocked;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Build,
    Plan,
    Flow,
}

pub(crate) enum PlanTrigger {
    WriteDone,
    InteractivePrompt,
}

impl Mode {
    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Build => theme::current().mode_build,
            Self::Plan => theme::current().mode_plan,
            Self::Flow => theme::current().mode_build,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum PlanState {
    #[default]
    None,
    Drafting(PathBuf),
    Ready(PathBuf),
}

impl PlanState {
    pub(crate) fn path(&self) -> Option<&Path> {
        match self {
            Self::None => Option::None,
            Self::Drafting(p) | Self::Ready(p) => Some(p),
        }
    }

    pub(crate) fn mark_ready(&mut self) {
        if let Self::Drafting(p) = self {
            *self = Self::Ready(std::mem::take(p));
        }
    }

    pub(crate) fn mark_drafting(&mut self) {
        if let Self::Ready(p) = self {
            *self = Self::Drafting(std::mem::take(p));
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    pub(crate) fn allocate_path(&mut self, storage: &StateDir) {
        if matches!(self, Self::None) {
            *self = Self::Drafting(
                plans::new_plan_path(storage).unwrap_or_else(|_| PathBuf::from("plans/plan.md")),
            );
        }
    }
}

impl App {
    pub(crate) fn transition_plan(&mut self, trigger: PlanTrigger) {
        if self.state.mode != Mode::Plan {
            return;
        }
        match trigger {
            PlanTrigger::WriteDone => {
                if self.state.plan.is_ready() {
                    return;
                }
                self.state.plan.mark_ready();
                self.plan_form.on_plan_ready();
            }
            PlanTrigger::InteractivePrompt => {
                if self.state.plan.is_ready() {
                    self.state.plan.mark_drafting();
                    self.plan_form.on_plan_drafting();
                }
            }
        }
    }

    pub(super) fn enter_plan(&mut self) {
        self.state.plan.allocate_path(&self.storage);
        self.state.mode = Mode::Plan;
    }

    pub(super) fn toggle_mode(&mut self) -> Vec<super::Action> {
        match self.state.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.enter_flow(),
            Mode::Flow => self.state.mode = Mode::Build,
        };
        vec![]
    }

    pub(super) fn enter_flow(&mut self) {
        if self.state.flow.workstream_id.is_empty() {
            self.state.flow.workstream_id = new_workstream_id();
        }
        self.state.mode = Mode::Flow;
    }

    pub(super) fn agent_mode(&self) -> AgentMode {
        match self.state.mode {
            Mode::Plan => match self.state.plan.path() {
                Some(p) => AgentMode::Plan(p.to_path_buf()),
                None => {
                    debug_assert!(false, "Plan mode without path - invariant violated");
                    AgentMode::Build
                }
            },
            Mode::Build => AgentMode::Build,
            Mode::Flow => AgentMode::Flow(self.state.flow.workstream_id.clone()),
        }
    }

    pub(crate) fn build_agent_input(&self, msg: &QueuedMessage) -> AgentInput {
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            thinking: self.state.thinking,
            fast: self.state.fast,
            goal: self.state.session.meta.goal.clone(),
            ..Default::default()
        }
    }

    pub(super) fn mode_label(&self) -> (Cow<'static, str>, Style) {
        let label: Cow<'static, str> = if self.is_bash_input() {
            "[BASH]".into()
        } else {
            match self.state.mode {
                Mode::Build => "[BUILD]".into(),
                Mode::Plan => "[PLAN]".into(),
                Mode::Flow => "[FLOW]".into(),
            }
        };
        let style = Style::new()
            .fg(self.effective_mode_color())
            .add_modifier(Modifier::BOLD);
        (label, style)
    }

    pub(crate) fn is_bash_input(&self) -> bool {
        self.input_box
            .buffer
            .lines()
            .first()
            .is_some_and(|l| l.starts_with('!'))
    }

    pub(super) fn effective_mode_color(&self) -> Color {
        if self.is_bash_input() {
            theme::current().mode_bash
        } else {
            self.state.mode.color()
        }
    }

    pub(super) fn separator_style(&self) -> Style {
        if self.status == Status::Streaming {
            theme::current().input_border
        } else {
            Style::new().fg(self.effective_mode_color())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &str, status: ChunkStatus) -> (String, FlowChunkState) {
        (
            id.into(),
            FlowChunkState {
                title: id.into(),
                status,
                stage: Some(Stage::Execute),
                ..Default::default()
            },
        )
    }

    fn flow_with_chunks(chunks: Vec<(&str, ChunkStatus)>) -> FlowState {
        let mut flow = FlowState::default();
        for (id, status) in chunks {
            flow.chunks.insert(id.into(), chunk(id, status).1);
        }
        flow
    }

    #[test]
    fn finalize_non_terminal_marks_running_and_queued_blocked() {
        let mut flow = flow_with_chunks(vec![
            ("done", ChunkStatus::Done),
            ("running", ChunkStatus::Running),
            ("queued", ChunkStatus::Queued),
            ("blocked", ChunkStatus::Blocked),
        ]);
        flow.finalize_non_terminal();
        assert_eq!(flow.chunks.get("done").unwrap().status, ChunkStatus::Done);
        assert_eq!(
            flow.chunks.get("running").unwrap().status,
            ChunkStatus::Blocked
        );
        assert_eq!(
            flow.chunks.get("queued").unwrap().status,
            ChunkStatus::Blocked
        );
        assert_eq!(
            flow.chunks.get("blocked").unwrap().status,
            ChunkStatus::Blocked
        );
    }

    #[test]
    fn finalize_non_terminal_preserves_last_stage() {
        let mut flow = flow_with_chunks(vec![("running", ChunkStatus::Running)]);
        flow.finalize_non_terminal();
        let c = flow.chunks.get("running").unwrap();
        assert_eq!(c.status, ChunkStatus::Blocked);
        assert_eq!(c.stage, Some(Stage::Execute));
    }

    #[test]
    fn clear_chunks_resets_stage_and_map() {
        let mut flow = flow_with_chunks(vec![("a", ChunkStatus::Done)]);
        flow.stage = Some(Stage::Plan);
        flow.clear_chunks();
        assert!(flow.chunks.is_empty());
        assert_eq!(flow.stage, None);
    }
}
