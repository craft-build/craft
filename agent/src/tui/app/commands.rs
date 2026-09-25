//! The command table: slash completion, palette entries, and the one
//! dispatch arm every advertised command resolves to.

use tokio::sync::mpsc;

use super::{App, Message};
use crate::tui::modals::Modal;
use crate::tui::provider::{AgentEvent, Command, Tone};
use crate::tui::selection::copy_to_clipboard;

/// One command as shown in slash completion and the command palette. Both
/// surfaces derive from [`COMMANDS`] so a command can never be advertised
/// in one and absent (or a no-op) in the other.
pub struct CommandSpec {
    /// Dispatch id resolved by [`App::run_command`].
    pub id: &'static str,
    /// Slash form; `None` = palette-only.
    pub slash: Option<&'static str>,
    /// Alternate slash spelling for the same `id` (e.g. `/exit` for
    /// `/quit`); advertised next to `slash` in completion and help.
    pub alias: Option<&'static str>,
    /// Palette label.
    pub label: &'static str,
    /// Palette right-aligned hint.
    pub hint: &'static str,
    /// Slash-completion description.
    pub desc: &'static str,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        id: "new",
        slash: None,
        alias: None,
        label: "New session",
        hint: "",
        desc: "",
    },
    CommandSpec {
        id: "sessions",
        slash: Some("/sessions"),
        alias: None,
        label: "Switch session",
        hint: "/sessions",
        desc: "List sessions",
    },
    CommandSpec {
        id: "toggle-sidebar",
        slash: None,
        alias: None,
        label: "Toggle context panel",
        hint: "ctrl+b",
        desc: "",
    },
    CommandSpec {
        id: "model",
        slash: Some("/model"),
        alias: None,
        label: "Change model",
        hint: "ctrl+l",
        desc: "Switch model",
    },
    CommandSpec {
        id: "clear",
        slash: Some("/clear"),
        alias: None,
        label: "Clear context",
        hint: "/clear",
        desc: "Clear conversation context",
    },
    CommandSpec {
        id: "copy",
        slash: None,
        alias: None,
        label: "Copy last message",
        hint: "",
        desc: "",
    },
    CommandSpec {
        id: "undo",
        slash: Some("/undo"),
        alias: None,
        label: "Undo last edit",
        hint: "/undo",
        desc: "Revert the last edit",
    },
    CommandSpec {
        id: "compact",
        slash: Some("/compact"),
        alias: None,
        label: "Compact context",
        hint: "/compact",
        desc: "Compact context to save tokens",
    },
    CommandSpec {
        id: "usage",
        slash: Some("/usage"),
        alias: None,
        label: "Show usage",
        hint: "/usage",
        desc: "Show this session's tokens and cost",
    },
    CommandSpec {
        id: "stats",
        slash: Some("/stats"),
        alias: None,
        label: "Show stats",
        hint: "/stats",
        desc: "Show cost across all sessions",
    },
    CommandSpec {
        id: "auto-review",
        slash: Some("/auto-review"),
        alias: None,
        label: "Toggle auto-review",
        hint: "/auto-review",
        desc: "Toggle LLM auto-review of permissions",
    },
    CommandSpec {
        id: "help",
        slash: Some("/help"),
        alias: None,
        label: "Help",
        hint: "/help",
        desc: "Show keybindings",
    },
    CommandSpec {
        id: "quit",
        slash: Some("/quit"),
        alias: Some("/exit"),
        label: "Quit",
        hint: "/quit",
        desc: "Quit craft",
    },
];

/// Whether placing `text` in the composer would open the slash-command menu.
/// Shared by slash completion and history recall so a recalled entry can be
/// recognized as a command without rebuilding the match list.
pub(crate) fn opens_slash_menu(text: &str) -> bool {
    text.starts_with('/')
        && COMMANDS
            .iter()
            .flat_map(|spec| [spec.slash, spec.alias])
            .flatten()
            .any(|cmd| text == "/" || cmd.starts_with(text))
}

/// (binding, action) rows of the `/help` sheet: the real key chords plus
/// the same command table the completion popup and palette derive from.
pub fn help_rows() -> Vec<(String, String)> {
    let mut rows = vec![
        ("ctrl+p".into(), "command palette".into()),
        ("ctrl+l".into(), "model menu".into()),
        ("ctrl+b".into(), "toggle context panel".into()),
        ("ctrl+f".into(), "cycle effort".into()),
        (
            "tab / shift+tab".into(),
            "focus next/previous tool card".into(),
        ),
        ("ctrl+y".into(), "approve pending edit".into()),
        ("ctrl+shift+y".into(), "approve pending edit, always".into()),
        ("ctrl+n".into(), "reject pending edit".into()),
        ("up / down".into(), "recall input history".into()),
        ("esc".into(), "close menu / interrupt the turn".into()),
        ("ctrl+c / ctrl+q".into(), "quit".into()),
        ("pgup / pgdn / g / G".into(), "scroll the transcript".into()),
        (String::new(), String::new()),
    ];
    for (slash, desc) in COMMANDS.iter().flat_map(|spec| {
        [spec.slash, spec.alias]
            .into_iter()
            .flatten()
            .map(|slash| (slash, spec.desc))
    }) {
        rows.push((slash.to_string(), desc.to_string()));
    }
    rows
}

impl App {
    /// Open the model picker with the current selection highlighted,
    /// replacing any modal already open.
    pub(crate) fn open_model_menu(&mut self) {
        if !self.session.models.is_empty() {
            self.modal = Modal::ModelMenu(
                self.session
                    .model_idx
                    .min(self.session.models.len().saturating_sub(1)),
            );
        }
    }

    /// Open the persisted-session picker (empty when no sessions exist).
    fn open_sessions(&mut self) {
        self.modal = Modal::Sessions {
            entries: super::sidebar::load_session_entries(),
            selected: 0,
        };
    }

    /// Push a provider-style notice locally (used for confirmations of
    /// app-side actions like clipboard copies).
    pub(crate) fn push_notice(&mut self, tone: Tone, text: impl Into<String>) {
        self.conversation.apply(AgentEvent::Notice {
            tone,
            text: text.into(),
        });
    }

    fn copy_last_assistant(&mut self) {
        let text = self
            .conversation
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(text) if !text.is_empty() => Some(text.clone()),
                _ => None,
            });
        match text {
            Some(text) => {
                copy_to_clipboard(&text);
                self.push_notice(Tone::Success, "copied the last reply to the clipboard");
            }
            None => self.push_notice(Tone::Neutral, "nothing to copy yet"),
        }
    }

    /// The one dispatch arm every advertised command resolves to; slash
    /// names map here through [`COMMANDS`].
    pub(crate) fn run_command(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        match id {
            "new" => {
                self.reset_conversation();
                let _ = tx.send(Command::Reset);
            }
            "sessions" => self.open_sessions(),
            "toggle-sidebar" => self.session.sidebar_open = !self.session.sidebar_open,
            "model" => self.open_model_menu(),
            "clear" => {
                self.reset_conversation();
                let _ = tx.send(Command::Clear);
            }
            "copy" => self.copy_last_assistant(),
            "undo" => {
                let _ = tx.send(Command::Undo);
            }
            "compact" => {
                let _ = tx.send(Command::Compact);
            }
            "usage" => {
                self.modal = Modal::Usage(self.usage.clone());
                let _ = tx.send(Command::GetUsage);
            }
            "stats" => self.modal = Modal::Stats(super::sidebar::load_stats()),
            "auto-review" => {
                let _ = tx.send(Command::ToggleAutoReview);
            }
            "help" => self.modal = Modal::Help,
            "quit" => self.should_quit = true,
            _ => {}
        }
    }

    pub(crate) fn run_slash(&mut self, cmd: &str, tx: &mpsc::UnboundedSender<Command>) {
        if let Some(spec) = COMMANDS
            .iter()
            .find(|spec| spec.slash == Some(cmd) || spec.alias == Some(cmd))
        {
            self.run_command(spec.id, tx);
        }
    }

    pub(crate) fn run_palette(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        self.run_command(id, tx);
    }

    pub(crate) fn approve(
        &mut self,
        idx: usize,
        tx: &mpsc::UnboundedSender<Command>,
        always: bool,
    ) {
        if let Some(Message::Tool { id, diff, .. }) = self.conversation.messages.get_mut(idx) {
            *diff = Some(super::DiffState::Approved);
            let _ = tx.send(Command::Approve {
                id: id.clone(),
                always,
            });
        }
        self.conversation.focused = None;
    }

    pub(crate) fn reject_confirmed(&mut self, tx: &mpsc::UnboundedSender<Command>, always: bool) {
        if let Modal::ConfirmReject(id) = std::mem::replace(&mut self.modal, Modal::None) {
            if let Some(Message::Tool { diff, .. }) = self
                .conversation
                .messages
                .iter_mut()
                .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id))
            {
                *diff = Some(super::DiffState::Rejected);
            }
            let _ = tx.send(Command::Reject { id, always });
        }
        self.conversation.focused = None;
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::usage_rows;
    use super::*;
    use crate::tui::app::{App, Message, Modal};

    /// W8: every command advertised in the slash popup or the palette
    /// resolves to a real dispatch arm (command sent, modal opened, or an
    /// immediate visible effect).
    #[test]
    fn every_advertised_command_dispatches_to_a_real_arm() {
        for spec in COMMANDS {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut app = App::new();
            app.conversation
                .messages
                .push(Message::Assistant("hello".into()));
            let sidebar = app.session.sidebar_open;
            app.run_command(spec.id, &tx);
            let sent = rx.try_recv().is_ok();
            let modal = !matches!(app.modal, Modal::None);
            let flipped = app.session.sidebar_open != sidebar;
            let noticed = matches!(
                app.conversation.messages.last(),
                Some(Message::Notice { .. })
            );
            assert!(
                sent || modal || flipped || noticed || app.should_quit,
                "command {:?} resolves to a no-op",
                spec.id
            );
        }
    }

    /// W8 + slash completion: both surfaces read the same table, so the
    /// slash completion's entries all exist and carry a description.
    #[test]
    fn slash_entries_all_resolve_to_dispatch_ids() {
        let ids: std::collections::HashSet<&str> = COMMANDS.iter().map(|s| s.id).collect();
        for spec in COMMANDS {
            assert!(ids.contains(spec.id));
            if spec.slash.is_some() {
                assert!(!spec.desc.is_empty(), "{} lacks a description", spec.id);
            }
        }
    }

    /// W9: the help sheet lists every slash command the completion popup
    /// advertises (sourced from the same table, so it cannot lie).
    #[test]
    fn help_rows_cover_every_slash_command() {
        let rows = help_rows();
        for slash in COMMANDS.iter().flat_map(|s| [s.slash, s.alias]).flatten() {
            assert!(
                rows.iter().any(|(key, _)| key == slash),
                "{slash} missing from the help sheet"
            );
        }
    }

    /// `/quit` and its `/exit` alias both flag the app to leave.
    #[test]
    fn quit_and_exit_alias_leave() {
        for cmd in ["/quit", "/exit"] {
            let (tx, _rx) = mpsc::unbounded_channel();
            let mut app = App::new();
            assert!(!app.should_quit);
            app.run_slash(cmd, &tx);
            assert!(app.should_quit, "{cmd} did not quit");
        }
    }

    /// The alias is a first-class slash entry: it opens the menu and dispatches.
    #[test]
    fn exit_alias_completes_and_dispatches() {
        let mut app = App::new();
        app.composer.text = "/exit".into();
        assert!(app.slash_open(), "/exit should open the slash menu");
        assert!(
            app.slash_matches().iter().any(|(cmd, _)| *cmd == "/exit"),
            "/exit missing from completion"
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.submit(&tx);
        assert!(app.should_quit, "/exit did not quit");
        assert!(rx.try_recv().is_err(), "quit sends no provider command");
    }

    /// W8: palette `copy` copies the last assistant reply and confirms with
    /// a success notice; with no reply it explains instead.
    #[test]
    fn copy_reports_through_a_notice() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.run_command("copy", &tx);
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::Notice {
                tone: Tone::Neutral,
                ..
            })
        ));
        app.conversation
            .messages
            .push(Message::Assistant("the reply".into()));
        app.run_command("copy", &tx);
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::Notice {
                tone: Tone::Success,
                ..
            })
        ));
    }

    /// `/auto-review` routes a toggle command to the provider.
    #[test]
    fn auto_review_slash_sends_toggle_command() {
        let mut app = App::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.run_slash("/auto-review", &tx);
        assert!(matches!(rx.try_recv(), Ok(Command::ToggleAutoReview)));
        // The command stays reachable from the composer's slash popup.
        app.composer.text = "/auto".into();
        assert!(
            app.slash_matches()
                .iter()
                .any(|(cmd, _)| *cmd == "/auto-review")
        );
    }

    #[test]
    fn usage_slash_opens_the_session_overlay() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.handle_event(AgentEvent::UsageSnapshot(usage_rows()));
        app.run_slash("/usage", &tx);
        assert!(matches!(&app.modal, Modal::Usage(rows) if rows.len() == 2));
        assert!(
            matches!(rx.try_recv(), Ok(Command::GetUsage)),
            "/usage refreshes the snapshot from the provider"
        );
        // Any key dismisses the read-only overlay.
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &tx,
        );
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn stats_slash_opens_the_ledger_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.run_slash("/stats", &tx);
        assert!(matches!(app.modal, Modal::Stats(_)));
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &tx,
        );
        assert!(matches!(app.modal, Modal::None));
    }

    /// A fresh snapshot while `/usage` is open replaces the overlay's rows.
    #[test]
    fn usage_snapshot_refreshes_open_overlay() {
        let mut app = App::new();
        app.run_slash("/usage", &mpsc::unbounded_channel().0);
        app.handle_event(AgentEvent::UsageSnapshot(usage_rows()));
        assert!(matches!(&app.modal, Modal::Usage(rows) if rows.len() == 2));
    }
}
