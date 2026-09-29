//! The command table: slash completion, palette entries, and the one
//! dispatch arm every advertised command resolves to.

use tokio::sync::mpsc;

use super::{App, Message};
use crate::tui::modals::Modal;
use crate::tui::provider::{AgentEvent, Command, Tone};
use crate::tui::selection::copy_to_clipboard;
use crate::tui::ui::theme;

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
        id: "resume",
        slash: Some("/resume"),
        alias: Some("/continue"),
        label: "Resume latest session",
        hint: "/resume",
        desc: "Resume this directory's latest session",
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
        id: "mcp",
        slash: Some("/mcp"),
        alias: None,
        label: "MCP servers",
        hint: "/mcp",
        desc: "Show MCP servers",
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
        id: "theme",
        slash: Some("/theme"),
        alias: None,
        label: "Switch color theme",
        hint: "/theme",
        desc: "Switch color theme",
    },
    CommandSpec {
        id: "recipe",
        slash: Some("/recipe"),
        alias: None,
        label: "Run recipe",
        hint: "/recipe",
        desc: "Browse and run recipes",
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

impl App {
    /// Open the model picker with the current selection highlighted,
    /// replacing any modal already open.
    pub(crate) fn open_model_menu(&mut self) {
        if !self.session.models.is_empty() {
            self.overlays.modal = Modal::ModelMenu(
                self.session
                    .model_idx
                    .min(self.session.models.len().saturating_sub(1)),
            );
        }
    }

    /// Open the persisted-session picker (empty when no sessions exist).
    fn open_sessions(&mut self) {
        self.overlays.modal = Modal::Sessions {
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
            "resume" => {
                let _ = tx.send(Command::ResumeLatest);
            }
            "theme" => self.open_theme_picker(),
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
            "mcp" => self.open_mcp(),
            "recipe" => self.open_recipes(),
            "compact" => {
                let _ = tx.send(Command::Compact);
            }
            "usage" => {
                self.overlays.usage_scroll = 0;
                self.overlays.modal = Modal::Usage(self.overlays.usage.clone());
                // Rows come from the session ledger; quota is refetched on
                // every open, like the reference.
                let _ = tx.send(Command::GetUsage);
                let _ = tx.send(Command::FetchUsage);
            }
            "stats" => {
                self.overlays.usage_scroll = 0;
                self.overlays.modal = Modal::Stats(super::sidebar::load_stats());
            }
            "auto-review" => {
                let _ = tx.send(Command::ToggleAutoReview);
            }
            "help" => self.overlays.modal = Modal::Help,
            "quit" => self.should_quit = true,
            _ => {}
        }
    }

    /// `/theme`: open the picker with the cursor on the active theme.
    pub(crate) fn open_theme_picker(&mut self) {
        let entries = theme::all_theme_names();
        let original = theme::current_theme_name();
        let selected = entries
            .iter()
            .position(|name| *name == original)
            .unwrap_or(0);
        self.overlays.modal = Modal::ThemePicker {
            entries,
            selected,
            original,
        };
    }

    /// `/mcp`: open the server-status screen with the cursor on the first
    /// row. Rows read the live snapshot at render time, so the screen needs
    /// no state beyond the selection.
    pub(crate) fn open_mcp(&mut self) {
        self.overlays.modal = Modal::Mcp { selected: 0 };
    }

    /// Discover recipes for the `/recipe` surfaces (J.5): the picker and
    /// `/recipe <name> key=value ...`. Unreadable files are skipped.
    pub(crate) fn discover_recipes(&self) -> Vec<crate::tui::modals::RecipeEntry> {
        let files = crate::skills::Discovery::from_env()
            .discover_files("recipes", &["yaml", "yml", "json"]);
        let mut entries = Vec::new();
        for f in files {
            let Ok(r) = crate::recipe::load(&f.path) else {
                continue;
            };
            let name = r.name.clone().unwrap_or_else(|| f.name.clone());
            // Same resolution order as `craft recipe run`: the file stem
            // wins, so a later recipe named only via its `name` field
            // cannot shadow it.
            if entries
                .iter()
                .any(|e: &crate::tui::modals::RecipeEntry| e.name == name)
            {
                continue;
            }
            entries.push(crate::tui::modals::RecipeEntry {
                name,
                description: r.description.clone().unwrap_or_default(),
                params: r
                    .parameters
                    .iter()
                    .filter(|p| p.default.is_none())
                    .map(|p| p.name.clone())
                    .collect(),
            });
        }
        entries
    }

    /// `/recipe`: open the picker over discovered recipes.
    pub(crate) fn open_recipes(&mut self) {
        let entries = self.discover_recipes();
        self.overlays.modal = Modal::Recipes {
            entries,
            selected: 0,
        };
    }

    /// Run a recipe (J.5): parse `key=value` args, resolve parameters,
    /// render the minijinja template, echo the invocation, and send the
    /// prompt as a normal user message. Recipes are matched by file stem
    /// or `name` field, like `craft recipe run`.
    pub(crate) fn submit_recipe(
        &mut self,
        name: &str,
        args: &str,
        tx: &mpsc::UnboundedSender<Command>,
    ) {
        let files = crate::skills::Discovery::from_env()
            .discover_files("recipes", &["yaml", "yml", "json"]);
        let path = files
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.path.clone())
            .or_else(|| {
                files
                    .iter()
                    .filter(|f| {
                        crate::recipe::load(&f.path)
                            .ok()
                            .and_then(|r| r.name)
                            .as_deref()
                            == Some(name)
                    })
                    .map(|f| f.path.clone())
                    .next()
            });
        let Some(path) = path else {
            self.push_notice(
                Tone::Warning,
                format!("recipe '{name}' not found (try /recipe to browse)"),
            );
            return;
        };
        let recipe = match crate::recipe::load(&path) {
            Ok(r) => r,
            Err(e) => {
                self.push_notice(Tone::Danger, format!("load recipe: {e}"));
                return;
            }
        };

        let mut overrides = std::collections::HashMap::new();
        for raw in args.split_whitespace() {
            match raw.split_once('=') {
                Some((k, v)) => {
                    overrides.insert(k.trim().to_string(), v.trim().to_string());
                }
                None => {
                    self.push_notice(
                        Tone::Warning,
                        format!("skipped argument {raw:?}, expected key=value"),
                    );
                }
            }
        }

        let missing: Vec<String> = recipe
            .missing_required(&overrides)
            .iter()
            .map(|p| p.name.clone())
            .collect();
        if !missing.is_empty() {
            self.push_notice(
                Tone::Warning,
                format!(
                    "recipe '{name}' needs: {} (e.g. /recipe {name} {})",
                    missing.join(", "),
                    missing
                        .iter()
                        .map(|p| format!("{p}="))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            );
            return;
        }

        let prompt = match recipe
            .resolve_parameters(&overrides)
            .and_then(|params| recipe.render(&params, &path))
        {
            Ok(p) => crate::template::env_vars().apply(&p).into_owned(),
            Err(e) => {
                self.push_notice(Tone::Danger, format!("recipe '{name}': {e}"));
                return;
            }
        };

        let echo = if args.is_empty() {
            format!("/recipe {name}")
        } else {
            format!("/recipe {name} {args}")
        };
        self.conversation.assistant_open = false;
        self.conversation.messages.push(Message::User(echo));
        let _ = tx.send(Command::SendMessage(
            prompt,
            self.agent_mode(),
            std::mem::take(&mut self.images.attached),
        ));
        self.view.follow = true;
    }

    pub(crate) fn run_slash(&mut self, cmd: &str, tx: &mpsc::UnboundedSender<Command>) {
        if let Some(spec) = COMMANDS
            .iter()
            .find(|spec| spec.slash == Some(cmd) || spec.alias == Some(cmd))
        {
            self.run_command(spec.id, tx);
        }
    }

    /// Resolve a `/name` slash string to a custom command (J.5). Returns
    /// `None` when the name is claimed by a builtin command or unknown.
    pub(crate) fn custom_command(&self, slash: &str) -> Option<crate::command::CustomCommand> {
        let name = slash.strip_prefix('/')?;
        if COMMANDS
            .iter()
            .flat_map(|spec| [spec.slash, spec.alias])
            .flatten()
            .any(|builtin| builtin == slash)
        {
            return None;
        }
        self.custom_commands
            .iter()
            .find(|custom| custom.name == name)
            .cloned()
    }

    /// Run a custom command: substitute `$ARGUMENTS`, apply `{cwd}` /
    /// `{platform}` / `{date}` variables, echo the invocation, and send
    /// the rendered prompt as a normal user message.
    pub(crate) fn submit_custom_command(
        &mut self,
        custom: &crate::command::CustomCommand,
        args: &str,
        tx: &mpsc::UnboundedSender<Command>,
    ) {
        let rendered = crate::template::env_vars()
            .apply(&custom.render(args))
            .into_owned();
        let echo = if args.is_empty() {
            format!("/{}", custom.name)
        } else {
            format!("/{} {}", custom.name, args)
        };
        self.conversation.assistant_open = false;
        self.conversation.messages.push(Message::User(echo));
        let _ = tx.send(Command::SendMessage(
            rendered,
            self.agent_mode(),
            std::mem::take(&mut self.images.attached),
        ));
        self.view.follow = true;
    }

    pub(crate) fn run_palette(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        self.run_command(id, tx);
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::usage_rows;
    use super::*;
    use crate::tui::app::{App, Message, Modal};
    use crate::tui::provider::UsageFetchState;

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
            let modal = !matches!(app.overlays.modal, Modal::None);
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
    /// J.5: `/recipe` opens the picker; running an unknown recipe warns
    /// through a notice instead of sending anything.
    #[test]
    fn recipe_slash_opens_picker_and_unknown_warns() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.run_command("recipe", &tx);
        assert!(matches!(app.overlays.modal, Modal::Recipes { .. }));
        app.submit_recipe("definitely-missing", "", &tx);
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::Notice {
                tone: Tone::Warning,
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
    }

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
        assert!(matches!(&app.overlays.modal, Modal::Usage(rows) if rows.len() == 2));
        assert!(
            matches!(rx.try_recv(), Ok(Command::GetUsage)),
            "/usage refreshes the snapshot from the provider"
        );
        assert!(
            matches!(rx.try_recv(), Ok(Command::FetchUsage)),
            "/usage refetches the provider quota on every open"
        );
        // Any key dismisses the read-only overlay.
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &tx,
        );
        assert!(matches!(app.overlays.modal, Modal::None));
    }

    #[test]
    fn stats_slash_opens_the_ledger_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.run_slash("/stats", &tx);
        assert!(matches!(app.overlays.modal, Modal::Stats(_)));
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &tx,
        );
        assert!(matches!(app.overlays.modal, Modal::None));
    }

    /// A fresh snapshot while `/usage` is open replaces the overlay's rows.
    #[test]
    fn usage_snapshot_refreshes_open_overlay() {
        let mut app = App::new();
        app.run_slash("/usage", &mpsc::unbounded_channel().0);
        app.handle_event(AgentEvent::UsageSnapshot(usage_rows()));
        assert!(matches!(&app.overlays.modal, Modal::Usage(rows) if rows.len() == 2));
    }

    /// The provider quota answer survives close/reopen (F.5: the last
    /// successful fetch is kept, like the reference's usage_slot).
    #[test]
    fn quota_state_persists_across_close_and_reopen() {
        let mut app = App::new();
        app.run_slash("/usage", &mpsc::unbounded_channel().0);
        app.handle_event(AgentEvent::UsageQuota(UsageFetchState::Ready(
            crate::providers::ProviderUsage {
                plan: Some("lite".into()),
                limits: Vec::new(),
                by_model_today: Vec::new(),
            },
        )));
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &mpsc::unbounded_channel().0,
        );
        assert!(matches!(app.overlays.modal, Modal::None));
        assert!(
            matches!(&app.overlays.usage_quota, UsageFetchState::Ready(u) if u.plan.as_deref() == Some("lite"))
        );
        // Reopening resets the scroll but keeps the quota answer.
        app.overlays.usage_scroll = 5;
        app.run_slash("/usage", &mpsc::unbounded_channel().0);
        assert_eq!(app.overlays.usage_scroll, 0);
        assert!(matches!(
            &app.overlays.usage_quota,
            UsageFetchState::Ready(_)
        ));
    }

    /// J.5: a discovered custom command appears in slash completion next
    /// to the builtins.
    #[test]
    fn custom_command_appears_in_slash_completion() {
        let mut app = App::new();
        app.custom_commands = vec![crate::command::CustomCommand {
            name: "review".into(),
            description: "Code review".into(),
            content: "Review $ARGUMENTS".into(),
            scope: crate::command::CommandScope::Project,
            accepts_args: true,
        }];
        app.composer.text = "/rev".into();
        assert!(app.slash_open());
        assert!(
            app.slash_matches()
                .iter()
                .any(|(cmd, desc)| cmd == "/review" && desc == "Code review"),
            "custom command missing from completion"
        );
    }

    /// J.5: submitting `/name args` sends the rendered prompt with
    /// `$ARGUMENTS` substituted and echoes the invocation.
    #[test]
    fn custom_command_submit_substitutes_arguments() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.custom_commands = vec![crate::command::CustomCommand {
            name: "review".into(),
            description: String::new(),
            content: "Review the code in $ARGUMENTS".into(),
            scope: crate::command::CommandScope::Project,
            accepts_args: true,
        }];
        app.composer.set_text("/review src/lib.rs".into());
        app.submit(&tx);
        assert!(
            matches!(rx.try_recv(), Ok(Command::SendMessage(text, _, _)) if text.contains("src/lib.rs")),
            "rendered prompt not sent"
        );
        assert!(app.composer.text.is_empty(), "composer not cleared");
        assert_eq!(app.input_history.get(0), Some("/review src/lib.rs"));
        assert!(matches!(
            app.conversation.messages.last(),
            Some(Message::User(echo)) if echo.contains("/review src/lib.rs")
        ));
    }

    /// B.11: `/mcp` appears in slash completion and opens the server
    /// screen; any close key dismisses it.
    #[test]
    fn mcp_slash_opens_the_server_screen() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.text = "/mc".into();
        assert!(
            app.slash_matches().iter().any(|(cmd, _)| *cmd == "/mcp"),
            "/mcp missing from completion"
        );
        app.run_slash("/mcp", &tx);
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 0 }));
        app.handle_key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            ),
            &tx,
        );
        assert!(matches!(app.overlays.modal, Modal::None));
    }

    /// J.5: a custom command whose name collides with a builtin slash
    /// command never shadows it.
    #[test]
    fn builtin_slash_wins_name_collision() {
        let mut app = App::new();
        app.custom_commands = vec![crate::command::CustomCommand {
            name: "clear".into(),
            description: "imposter".into(),
            content: "should not run".into(),
            scope: crate::command::CommandScope::Project,
            accepts_args: false,
        }];
        app.composer.text = "/cle".into();
        let matches = app.slash_matches();
        assert!(matches.iter().any(|(cmd, _)| cmd == "/clear"));
        assert!(!matches.iter().any(|(_, desc)| *desc == "imposter"));
        assert!(app.custom_command("/clear").is_none());
    }
}
