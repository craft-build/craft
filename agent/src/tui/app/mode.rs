//! Mode cycling (F.2): Build / Plan, toggled with Tab. Plan mode restricts
//! the agent to writing its allocated plan file (`run::AgentMode::Plan`).
//! Flow mode is ported last (task 99) and deliberately absent.

use std::path::PathBuf;

use ratatui::style::Color;

use crate::run::AgentMode;
use crate::tui::ui::theme;

use super::App;
use super::Status;
use crate::tui::provider::Command;
use tokio::sync::mpsc;

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
            Self::Build => theme::current().mode_build,
            Self::Plan => theme::current().mode_plan,
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

    // ------------------------------------------------------------------
    // Todo/plan panel + plan editor handoff (F.3, task 79)
    // ------------------------------------------------------------------

    /// The plan form owns plain keys while shown over the composer
    /// (reference `plan_form_active`).
    pub(crate) fn plan_form_active(&self) -> bool {
        self.mode == Mode::Plan && self.plan_form.is_visible()
    }

    /// Plan form lifecycle on status transitions: a settling Plan-mode
    /// turn shows the form only when the plan file was actually written
    /// during that turn (reference fires on the plan write itself, not on
    /// any turn); a fresh turn hides it again (reference
    /// `transition_plan`).
    pub(crate) fn update_plan_lifecycle(&mut self, was_busy: bool) {
        if self.mode != Mode::Plan {
            self.plan_turn_snapshot = None;
            return;
        }
        let busy = matches!(self.status, Status::Thinking | Status::Running);
        if !was_busy && busy {
            self.plan_turn_snapshot = Some(self.plan_content());
            if self.plan_ready {
                self.plan_ready = false;
                self.plan_form.on_plan_drafting();
            }
        } else if was_busy
            && !busy
            && !self.plan_ready
            && self.plan_turn_snapshot.take().is_some_and(|before| {
                let after = self.plan_content();
                !after.trim().is_empty() && after != before
            })
        {
            self.plan_ready = true;
            self.plan_form.on_plan_ready();
        }
    }

    fn plan_content(&self) -> String {
        self.plan_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    }

    /// Ctrl-T: toggle the plan form in Plan mode; in Build mode the panel
    /// is the sidebar's plan checklist, so the chord toggles the sidebar
    /// (the reference toggles its float panel, which is not ported).
    pub(crate) fn toggle_plan_panel(&mut self) {
        if self.mode == Mode::Plan {
            self.plan_form.toggle();
        } else {
            self.session.sidebar_open = !self.session.sidebar_open;
        }
    }

    /// Ctrl-O: hand the plan file to $VISUAL/$EDITOR via the run loop.
    pub(crate) fn open_plan_editor(&mut self) {
        match self.plan_path.clone() {
            Some(path) => self.editor_request = Some(path),
            None => self.flash("No plan file"),
        }
    }

    /// Take (consume) the pending plan-editor request.
    pub fn take_editor_request(&mut self) -> Option<std::path::PathBuf> {
        self.editor_request.take()
    }

    pub(crate) fn handle_plan_form_action(
        &mut self,
        action: crate::tui::plan_form::PlanFormAction,
        tx: &mpsc::UnboundedSender<Command>,
    ) {
        use crate::tui::plan_form::PlanFormAction;
        match action {
            PlanFormAction::Consumed | PlanFormAction::Passthrough => {}
            PlanFormAction::Hide => self.plan_form.hide(),
            PlanFormAction::OpenEditor => self.open_plan_editor(),
            PlanFormAction::Implement => self.implement_plan(false, tx),
            PlanFormAction::ClearAndImplement => self.implement_plan(true, tx),
        }
    }

    /// Reference `implement_plan`: switch to Build, (optionally) reset the
    /// session, then submit the implement-the-plan prompt.
    fn implement_plan(&mut self, clear_context: bool, tx: &mpsc::UnboundedSender<Command>) {
        let parallel = self.plan_form.parallel();
        self.plan_form.reset();
        self.plan_ready = false;
        self.mode = Mode::Build;

        if clear_context {
            let _ = tx.send(Command::Reset);
            self.reset_conversation();
        }

        let text = match &self.plan_path {
            Some(path) => {
                let path = path.display().to_string();
                if parallel {
                    format!(
                        "Implement the plan at `{path}`. Use parallel tool calls for steps that touch independent modules."
                    )
                } else {
                    format!("Implement the plan at `{path}`.")
                }
            }
            None => "Implement the plan.".to_string(),
        };
        self.composer.set_text(text);
        self.submit(tx);
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

    fn plan_mode_app_with_written_plan() -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        std::fs::write(&plan, "# Plan\n1. do the thing\n").unwrap();
        let mut app = App::new();
        app.plan_path = Some(plan);
        app.mode = Mode::Plan;
        (app, dir)
    }

    /// A Plan-mode turn that rewrites the plan file shows the form on
    /// settle; the next turn hides it again (reference `transition_plan`).
    #[test]
    fn plan_form_lifecycle_tracks_turns() {
        let (mut app, dir) = plan_mode_app_with_written_plan();
        let plan = dir.path().join("plan.md");
        app.handle_event(crate::tui::provider::AgentEvent::StatusChanged(
            Status::Running,
        ));
        // The agent rewrites the plan mid-turn.
        std::fs::write(&plan, "# Plan\n1. do the thing\n2. do it well\n").unwrap();
        app.handle_event(crate::tui::provider::AgentEvent::StatusChanged(
            Status::Done,
        ));
        assert!(app.plan_ready);
        assert!(
            app.plan_form.is_visible(),
            "plan-writing turn shows the form"
        );
        app.handle_event(crate::tui::provider::AgentEvent::StatusChanged(
            Status::Running,
        ));
        assert!(!app.plan_ready);
        assert!(!app.plan_form.is_visible(), "new turn drafts again");
    }

    /// An unrelated Plan-mode turn that leaves the plan untouched (or
    /// empty) never surfaces the form.
    #[test]
    fn plan_form_stays_hidden_without_a_plan_write() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("plan.md");
        let mut cases = 0;
        // Missing, then empty, then unchanged-non-empty plan file.
        for write_before in [None, Some(""), Some("# Plan\n1. step\n")] {
            if let Some(content) = write_before {
                std::fs::write(&plan, content).unwrap();
            }
            let mut app = App::new();
            app.plan_path = Some(plan.clone());
            app.mode = Mode::Plan;
            app.handle_event(crate::tui::provider::AgentEvent::StatusChanged(
                Status::Running,
            ));
            app.handle_event(crate::tui::provider::AgentEvent::StatusChanged(
                Status::Done,
            ));
            assert!(!app.plan_form.is_visible(), "case {cases}");
            cases += 1;
        }
    }

    /// Implementing from the form lands in Build mode with the
    /// implement-the-plan prompt submitted (reference `implement_plan`).
    #[test]
    fn implement_plan_switches_to_build_and_submits() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (mut app, _dir) = plan_mode_app_with_written_plan();
        app.plan_form.on_plan_ready();
        app.handle_plan_form_action(crate::tui::plan_form::PlanFormAction::Implement, &tx);
        assert_eq!(app.mode, Mode::Build);
        assert!(!app.plan_form.is_visible());
        assert!(app.composer.text.is_empty());
        let sent = match rx.try_recv() {
            Ok(Command::SendMessage(text, AgentMode::Build)) => text,
            other => panic!("expected SendMessage(Build), got {other:?}"),
        };
        assert!(sent.starts_with("Implement the plan at `"), "{sent}");
    }

    /// "Clear context and implement" resets the conversation first.
    #[test]
    fn clear_and_implement_resets_context() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (mut app, _dir) = plan_mode_app_with_written_plan();
        app.plan_form.on_plan_ready();
        app.conversation
            .messages
            .push(crate::tui::app::Message::User("old".into()));
        app.handle_plan_form_action(
            crate::tui::plan_form::PlanFormAction::ClearAndImplement,
            &tx,
        );
        assert_eq!(
            app.conversation.messages.len(),
            1,
            "old context dropped, only the implement prompt remains"
        );
        assert!(
            app.conversation
                .messages
                .iter()
                .all(|m| !matches!(m, crate::tui::app::Message::User(t) if t == "old"))
        );
    }

    /// Ctrl-T toggles the form in Plan mode; Ctrl-O requests the editor.
    #[test]
    fn ctrl_t_and_ctrl_o_wiring() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (mut app, _dir) = plan_mode_app_with_written_plan();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        app.handle_key(ctrl('t'), &tx);
        assert!(app.plan_form.is_visible(), "Ctrl-T shows the form");
        app.handle_key(ctrl('t'), &tx);
        assert!(!app.plan_form.is_visible(), "Ctrl-T dismisses it");
        // Ctrl-O hands the plan path to the run loop.
        app.handle_key(ctrl('o'), &tx);
        assert_eq!(
            app.take_editor_request(),
            app.plan_path,
            "Ctrl-O requests the plan file"
        );
        assert!(app.take_editor_request().is_none(), "request consumed");
    }

    /// Ctrl-T in Build mode toggles the sidebar (the todo/plan panel);
    /// Ctrl-O without a plan flashes instead of requesting an editor.
    #[test]
    fn build_mode_panel_and_missing_plan() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let sidebar = app.session.sidebar_open;
        app.handle_key(ctrl('t'), &tx);
        assert_eq!(app.session.sidebar_open, !sidebar);
        app.handle_key(ctrl('o'), &tx);
        assert!(app.take_editor_request().is_none());
        assert_eq!(app.flash_text(), Some("No plan file"));
    }
}
