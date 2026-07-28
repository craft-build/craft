use std::borrow::Cow;
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
use craft_agent::TurnType;
use craft_agent::{AgentInput, AgentMode};
use craft_storage::StateDir;
use craft_storage::plans;
use ratatui::style::{Color, Modifier, Style};

use super::App;

/// Flow mode's persisted workstream state carried in `SessionMeta`. The TUI
/// `Mode` holds the id separately so the `Mode` enum stays `Copy`. Only the
/// workstream id and the current stage survive the de-chunking rework: thread
/// counts shown in the status line are derived live from the `ThreadManager`
/// tree on each render, never persisted as chunk maps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FlowState {
    pub workstream_id: String,
    pub stage: Option<TurnType>,
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
        self.build_agent_input_with(msg, false)
    }

    /// Build an agent input, optionally marking it as a flow-mode resume of a
    /// previously-failed run. `flow_resume` is only meaningful in Flow mode.
    pub(crate) fn build_agent_input_with(
        &self,
        msg: &QueuedMessage,
        flow_resume: bool,
    ) -> AgentInput {
        AgentInput {
            message: msg.text.clone(),
            mode: self.agent_mode(),
            images: msg.images.clone(),
            thinking: self.state.thinking,
            fast: self.state.fast,
            goal: self.state.session.meta.goal.clone(),
            goal_criteria: self.state.session.meta.goal_criteria.clone(),
            flow_resume,
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
