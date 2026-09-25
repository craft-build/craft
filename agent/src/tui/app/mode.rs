//! Mode cycling (F.2): Build / Plan, toggled with Tab. Plan mode restricts
//! the agent to writing its allocated plan file (`run::AgentMode::Plan`).
//! Flow mode is ported last (task 99) and deliberately absent.

use std::path::PathBuf;

use ratatui::style::Color;

use crate::run::AgentMode;
use crate::tui::ui::theme;

use super::App;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    Build,
    Plan,
}

impl Mode {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Build => "[BUILD]",
            Self::Plan => "[PLAN]",
        }
    }

    pub(crate) fn color(&self) -> Color {
        match self {
            Self::Build => theme::MODE_BUILD,
            Self::Plan => theme::MODE_PLAN,
        }
    }
}

impl App {
    /// Tab: cycle Build -> Plan -> Build. The plan path is allocated on the
    /// first entry into Plan mode and reused for the rest of the session.
    pub(crate) fn toggle_mode(&mut self) {
        match self.mode {
            Mode::Build => self.enter_plan(),
            Mode::Plan => self.mode = Mode::Build,
        }
    }

    fn enter_plan(&mut self) {
        if self.plan_path.is_none() {
            self.plan_path = Some(Self::allocate_plan_path(
                crate::storage::StateDir::resolve().ok().as_ref(),
            ));
        }
        self.mode = Mode::Plan;
    }

    /// Fresh `<state>/plans/<slug>.md`, falling back to a relative path when
    /// the state dir (or slug allocation) fails — the reference's fallback.
    fn allocate_plan_path(dir: Option<&crate::storage::StateDir>) -> PathBuf {
        dir.and_then(|dir| crate::storage::plans::new_plan_path(dir).ok())
            .unwrap_or_else(|| PathBuf::from("plans/plan.md"))
    }

    pub(crate) fn agent_mode(&self) -> AgentMode {
        match (&self.mode, &self.plan_path) {
            (Mode::Plan, Some(path)) => AgentMode::Plan(path.clone()),
            _ => AgentMode::Build,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_build_and_plan() {
        let mut app = App::new();
        assert_eq!(app.mode, Mode::Build);
        assert_eq!(app.mode.label(), "[BUILD]");
        app.toggle_mode();
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(app.mode.label(), "[PLAN]");
        app.toggle_mode();
        assert_eq!(app.mode, Mode::Build);
    }

    #[test]
    fn plan_path_is_allocated_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let state = crate::storage::StateDir::from_path(dir.path().to_path_buf());
        let mut app = App::new();
        app.plan_path = Some(App::allocate_plan_path(Some(&state)));
        let first = app.plan_path.clone().expect("plan path allocated");
        assert_eq!(
            first.extension().and_then(|e| e.to_str()),
            Some("md"),
            "expected a markdown plan file, got {first:?}"
        );
        assert!(first.starts_with(dir.path().join("plans")));
        app.toggle_mode();
        app.toggle_mode();
        assert_eq!(app.plan_path.as_deref(), Some(first.as_path()));
    }

    #[test]
    fn agent_mode_maps_plan_path() {
        let mut app = App::new();
        assert_eq!(app.agent_mode(), AgentMode::Build);
        app.plan_path = Some(PathBuf::from("/tmp/plans/x.md"));
        app.mode = Mode::Plan;
        assert_eq!(
            app.agent_mode(),
            AgentMode::Plan(PathBuf::from("/tmp/plans/x.md"))
        );
    }
}
