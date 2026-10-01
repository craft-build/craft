//! Modal overlays: the single exclusive modal state (command palette, model
//! menu, reject confirmation) and their key handling. A modal owns the
//! keyboard while open — keys never fall through to base chords, so e.g.
//! ctrl+q does not quit under an open palette.

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

use crate::mcp::McpCommand;
use crate::mcp::config::{McpServerInfo, McpServerStatus};
use crate::model_registry::{self, ModelTier};
use crate::storage::StateDir;
use crate::tui::app::App;
use crate::tui::provider::Command;
use crate::tui::ui::theme;

/// The one modal that may be open at a time. Variants are mutually exclusive
/// by construction: opening one replaces whatever was open.
pub enum Modal {
    None,
    /// Command palette: (query, selected row).
    Palette {
        query: String,
        selected: usize,
    },
    /// Model picker: selected row.
    ModelMenu(usize),
    /// `/usage`: this session's per-model tokens and cost.
    Usage(Vec<crate::tui::provider::UsageRow>),
    /// `/stats`: cross-session totals from the cost ledger.
    Stats(StatsView),
    /// `/help` or Ctrl+H: data-driven keybinding sheet (F.1); rows come
    /// from the `KEYBINDS` table via the user's overlay resolver.
    Help,
    /// `/sessions`: persisted-session picker; Enter loads the selection.
    Sessions {
        entries: Vec<SessionEntry>,
        selected: usize,
    },
    /// `/theme`: bundled-theme picker with live preview. `original` is the
    /// theme active on open; cancel restores it (F.5, reference theme_picker).
    ThemePicker {
        entries: Vec<String>,
        selected: usize,
        original: String,
    },
    /// `/mcp`: server-status screen (B.11). Rows come from the live MCP
    /// snapshot at render time; only the selection lives here.
    Mcp {
        selected: usize,
    },
    /// Ctrl-N task-chat picker (task 96, reference list-picker style):
    /// row 0 is the main chat, rows 1.. the task chats; rows read the
    /// live `task_chats` at render/handle time, only the selection lives
    /// here. Enter mounts the selected chat.
    TaskPicker {
        selected: usize,
    },
    /// `/recipe`: recipe picker (J.5). Enter runs the selection; recipes
    /// with parameters prefill the composer with `/recipe <name> key=`.
    Recipes {
        entries: Vec<RecipeEntry>,
        selected: usize,
    },
}

/// One row of the `/recipe` picker: a discovered, parseable recipe.
#[derive(Clone, Debug)]
pub struct RecipeEntry {
    /// Recipe `name` field or the file stem.
    pub name: String,
    pub description: String,
    /// Parameters that need a `key=value` argument (no default).
    pub params: Vec<String>,
}

/// One row of the `/sessions` picker, pre-rendered for display.
#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub id: String,
    pub title: String,
    /// Relative update time ("2h ago").
    pub updated: String,
}

/// Aggregate cost-ledger state rendered by the `/stats` overlay.
#[derive(Clone, Debug, Default)]
pub struct StatsView {
    pub rows: Vec<crate::tui::provider::UsageRow>,
    /// Cost per session id, spend desc (capped at build time).
    pub by_session: Vec<(String, f64, u64)>,
    /// Models beyond the rendered cap.
    pub models_overflow: usize,
    pub total_cost: f64,
    /// Ledger records with no price data, excluded from `total_cost`.
    pub unpriced_records: usize,
    pub total_tokens: u64,
    pub sessions: usize,
    pub empty: bool,
}

impl App {
    pub(crate) fn handle_modal_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // 2. Command palette.
        if matches!(self.overlays.modal, Modal::Palette { .. }) {
            self.handle_palette_key(key, tx);
            return;
        }

        // 3. Model menu.
        if matches!(self.overlays.modal, Modal::ModelMenu(_)) {
            self.handle_model_menu_key(key, tx);
            return;
        }

        // 4. Read-only usage/stats/help sheets.
        if matches!(self.overlays.modal, Modal::Usage(_)) {
            self.handle_usage_key(key, tx, true);
            return;
        }
        if matches!(self.overlays.modal, Modal::Stats(_)) {
            self.handle_usage_key(key, tx, false);
            return;
        }
        if matches!(self.overlays.modal, Modal::Help) {
            self.handle_help_key(key);
            return;
        }

        // 5. Sessions picker.
        if matches!(self.overlays.modal, Modal::Sessions { .. }) {
            self.handle_sessions_key(key, tx);
            return;
        }

        // 6. Theme picker.
        if matches!(self.overlays.modal, Modal::ThemePicker { .. }) {
            self.handle_theme_picker_key(key);
            return;
        }

        // 7. MCP server screen.
        if matches!(self.overlays.modal, Modal::Mcp { .. }) {
            self.handle_mcp_key(key);
            return;
        }

        // 8. Task-chat picker.
        if matches!(self.overlays.modal, Modal::TaskPicker { .. }) {
            self.handle_task_picker_key(key);
            return;
        }

        // 9. Recipe picker.
        if matches!(self.overlays.modal, Modal::Recipes { .. }) {
            self.handle_recipes_key(key, tx);
        }
    }

    /// Keybindings help modal (F.1): Esc / the help and quit chords close,
    /// arrows and page keys scroll; other keys close (kept from the old
    /// "any key closes" sheet so muscle memory still works).
    fn handle_help_key(&mut self, key: KeyEvent) {
        use crate::tui::keybindings::ActionId;
        if key.code == KeyCode::Esc
            || self.overlays.keybinds.matches(ActionId::Help, key)
            || self.overlays.keybinds.matches(ActionId::Quit, key)
        {
            self.overlays.modal = Modal::None;
            return;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.overlays.help_scroll = self.overlays.help_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.overlays.help_scroll = self.overlays.help_scroll.saturating_add(1)
            }
            KeyCode::PageUp => {
                self.overlays.help_scroll = self.overlays.help_scroll.saturating_sub(10)
            }
            KeyCode::PageDown => {
                self.overlays.help_scroll = self.overlays.help_scroll.saturating_add(10)
            }
            KeyCode::Home => self.overlays.help_scroll = 0,
            KeyCode::End => self.overlays.help_scroll = self.overlays.help_scroll_max,
            _ => self.overlays.modal = Modal::None,
        }
        // Clamp against the last-rendered bound so a saturated value can
        // never pin the sheet at the bottom.
        self.overlays.help_scroll = self.overlays.help_scroll.min(self.overlays.help_scroll_max);
    }

    /// Theme picker keys: arrows preview live, Enter applies + persists,
    /// Esc/Ctrl-C restores the theme active on open. All keys are consumed.
    fn handle_theme_picker_key(&mut self, key: KeyEvent) {
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        let Modal::ThemePicker {
            entries,
            selected,
            original,
        } = std::mem::replace(&mut self.overlays.modal, Modal::None)
        else {
            return;
        };
        let preview = |sel: usize, app: &mut Self, entries: &[String]| {
            if let Some(name) = entries.get(sel) {
                let _ = theme::set_named(name);
            }
            app.overlays.modal = Modal::ThemePicker {
                entries: entries.to_vec(),
                selected: sel,
                original: original.clone(),
            };
        };
        let max = entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => preview(selected.saturating_sub(1), self, &entries),
            KeyCode::Down | KeyCode::Char('j') => preview((selected + 1).min(max), self, &entries),
            KeyCode::Enter => {
                if let Some(name) = entries.get(selected) {
                    let _ = theme::set_named(name);
                    apply_theme_choice(name, theme_state_dir().as_ref());
                }
            }
            // Cancel: restore the theme active on open.
            KeyCode::Esc => {
                let _ = theme::set_named(&original);
            }
            KeyCode::Char('c') | KeyCode::Char('C') if ctrl => {
                let _ = theme::set_named(&original);
            }
            _ => preview(selected, self, &entries),
        }
    }

    fn handle_sessions_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let Modal::Sessions { entries, selected } =
            std::mem::replace(&mut self.overlays.modal, Modal::None)
        else {
            return;
        };
        match key.code {
            KeyCode::Up => {
                self.overlays.modal = Modal::Sessions {
                    entries,
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                let max = entries.len().saturating_sub(1);
                self.overlays.modal = Modal::Sessions {
                    entries,
                    selected: (selected + 1).min(max),
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = entries.get(selected) {
                    let _ = tx.send(Command::LoadSession {
                        id: entry.id.clone(),
                    });
                }
            }
            // Esc or any other key closes the picker (already None).
            _ => {}
        }
    }

    /// Task-chat picker keys (task 96): arrows / j-k move, Enter mounts
    /// the selected chat, Esc or any other key closes. All keys are
    /// consumed. Selection is clamped to main + task chats.
    fn handle_task_picker_key(&mut self, key: KeyEvent) {
        let Modal::TaskPicker { selected } =
            std::mem::replace(&mut self.overlays.modal, Modal::None)
        else {
            return;
        };
        let max = self.task_chats.len(); // row 0 is the main chat
        let moved = |app: &mut Self, sel: usize| {
            app.overlays.modal = Modal::TaskPicker {
                selected: sel.min(max),
            };
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => moved(self, selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => moved(self, selected + 1),
            KeyCode::Enter => self.focus_chat_position(selected.min(max)),
            // Esc or any other key closes the picker (already None).
            _ => {}
        }
    }

    /// `/recipe` picker keys: arrows move, Enter runs (or prefills
    /// parameters for), Esc closes. All keys are consumed.
    fn handle_recipes_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let Modal::Recipes { entries, selected } =
            std::mem::replace(&mut self.overlays.modal, Modal::None)
        else {
            return;
        };
        match key.code {
            KeyCode::Up => {
                self.overlays.modal = Modal::Recipes {
                    entries,
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                let max = entries.len().saturating_sub(1);
                self.overlays.modal = Modal::Recipes {
                    entries,
                    selected: (selected + 1).min(max),
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = entries.get(selected).cloned() {
                    if entry.params.is_empty() {
                        self.submit_recipe(&entry.name, "", tx);
                    } else {
                        // Prefill `key=` stubs for parameters without a
                        // default so Enter submits once they are filled.
                        // `set_text` leaves the cursor at the end so the
                        // user keeps typing after the stubs.
                        self.composer.set_text(format!(
                            "/recipe {} {}",
                            entry.name,
                            entry
                                .params
                                .iter()
                                .map(|p| format!("{p}="))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                    }
                }
            }
            // Esc or any other key closes the picker (already None).
            _ => {}
        }
    }

    /// `/usage` and `/stats` sheet keys: close on Esc/Ctrl-C, scroll the
    /// line list, and reload the provider quota on Ctrl+R when the sheet
    /// supports it (usage only). All keys are consumed (F.5, reference
    /// usage/stats modals).
    fn handle_usage_key(
        &mut self,
        key: KeyEvent,
        tx: &mpsc::UnboundedSender<Command>,
        allow_refresh: bool,
    ) {
        let ctrl = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.overlays.modal = Modal::None,
            KeyCode::Char('c') | KeyCode::Char('C') if ctrl => self.overlays.modal = Modal::None,
            KeyCode::Char('r') | KeyCode::Char('R') if ctrl && allow_refresh => {
                let _ = tx.send(Command::FetchUsage);
            }
            KeyCode::Up => {
                self.overlays.usage_scroll = self.overlays.usage_scroll.saturating_sub(1)
            }
            KeyCode::Down => {
                self.overlays.usage_scroll = self.overlays.usage_scroll.saturating_add(1)
            }
            KeyCode::PageUp => {
                self.overlays.usage_scroll = self.overlays.usage_scroll.saturating_sub(10)
            }
            KeyCode::PageDown => {
                self.overlays.usage_scroll = self.overlays.usage_scroll.saturating_add(10)
            }
            _ => {}
        }
        self.overlays.usage_scroll = self
            .overlays
            .usage_scroll
            .min(self.overlays.usage_scroll_max);
    }

    fn handle_palette_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // Snapshot the filtered items while the palette is still open.
        let items = self.palette_items();
        let Modal::Palette {
            mut query,
            selected,
        } = std::mem::replace(&mut self.overlays.modal, Modal::None)
        else {
            return;
        };
        match key.code {
            KeyCode::Esc => {} // dismiss (modal already replaced with None)
            KeyCode::Up => {
                self.overlays.modal = Modal::Palette {
                    query,
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                let max = items.len().saturating_sub(1);
                self.overlays.modal = Modal::Palette {
                    query,
                    selected: (selected + 1).min(max),
                }
            }
            KeyCode::Enter => {
                if let Some((id, ..)) = items.get(selected) {
                    let id = *id;
                    self.run_palette(id, tx);
                }
            }
            KeyCode::Backspace => {
                query.pop();
                self.overlays.modal = Modal::Palette { query, selected: 0 }
            }
            KeyCode::Char(c) => {
                query.push(c);
                self.overlays.modal = Modal::Palette { query, selected: 0 }
            }
            _ => {
                // Unhandled keys leave the palette open.
                self.overlays.modal = Modal::Palette { query, selected };
            }
        }
    }

    fn handle_model_menu_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let Modal::ModelMenu(sel) = std::mem::replace(&mut self.overlays.modal, Modal::None) else {
            return;
        };
        // Tier assignment: a toggle on the highlighted model, persisted
        // globally. Never closes the picker, never selects a session model.
        if let Some(tier) = tier_for_key(&key) {
            if let Some(choice) = self.session.models.get(sel) {
                let spec = format!("{}/{}", choice.provider, choice.model);
                apply_tier_toggle(&spec, tier, tier_state_dir().as_ref());
            }
            self.overlays.modal = Modal::ModelMenu(sel);
            return;
        }
        match key.code {
            KeyCode::Up => self.overlays.modal = Modal::ModelMenu(sel.saturating_sub(1)),
            KeyCode::Down => {
                self.overlays.modal =
                    Modal::ModelMenu((sel + 1).min(self.session.models.len().saturating_sub(1)))
            }
            KeyCode::Enter => {
                self.session.model_idx = sel;
                if let Some(choice) = self.session.models.get(sel) {
                    let _ = tx.send(Command::SelectModel {
                        provider: choice.provider.clone(),
                        model: choice.model.clone(),
                    });
                }
            }
            _ => {} // Esc or any other key closes the menu (already None)
        }
    }

    /// `/mcp` screen keys (B.11): arrows move the selection, `t` toggles the
    /// highlighted server, `r` reconnects it, Enter (or `e`) expands the
    /// row's resource list, Esc or `q` closes. Rows read the live snapshot,
    /// so a command's effect shows up on the next paint.
    fn handle_mcp_key(&mut self, key: KeyEvent) {
        let Modal::Mcp { selected } = std::mem::replace(&mut self.overlays.modal, Modal::None)
        else {
            return;
        };
        let infos: Vec<McpServerInfo> = match &self.mcp {
            Some(handle) => handle.reader().load().infos.clone(),
            None => Vec::new(),
        };
        if matches!(
            key.code,
            KeyCode::Enter | KeyCode::Char('e') | KeyCode::Char('E')
        ) {
            // Phase 5: expand/collapse the selected server's resources.
            self.overlays.mcp_expanded = if self.overlays.mcp_expanded == Some(selected) {
                None
            } else {
                (selected < infos.len()).then_some(selected)
            };
            self.overlays.modal = Modal::Mcp { selected };
            return;
        }
        match key.code {
            KeyCode::Up => {
                self.overlays.modal = Modal::Mcp {
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                self.overlays.modal = Modal::Mcp {
                    selected: (selected + 1).min(infos.len().saturating_sub(1)),
                }
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {}
            KeyCode::Esc => {}
            _ => {
                if let Some(handle) = &self.mcp
                    && matches!(
                        infos.get(selected).map(|i| &i.status),
                        Some(McpServerStatus::NeedsAuth { .. })
                    )
                    && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L'))
                    && let Some(info) = infos.get(selected)
                {
                    // OAuth login (B.11): run the browser flow off the UI
                    // thread, then ask the manager to reconnect.
                    let handle = handle.clone();
                    let reader = handle.reader();
                    let server = info.name.clone();
                    tokio::spawn(async move {
                        match crate::mcp::oauth::login_and_reconnect(&handle, &reader, &server)
                            .await
                        {
                            Ok(info) => {
                                tracing::info!(server, url = %info.auth_url, "MCP login complete")
                            }
                            Err(e) => tracing::warn!(server, error = %e, "MCP login failed"),
                        }
                    });
                } else if let Some(handle) = &self.mcp
                    && let Some(cmd) = mcp_command_for(&infos, selected, key.code)
                {
                    handle.send(cmd);
                }
                self.overlays.modal = Modal::Mcp { selected };
            }
        }
    }
}

/// The command (if any) the `/mcp` screen's `t`/`r` keys map to for the
/// highlighted row (B.11). Pure over the snapshot rows so the mapping is
/// testable without a live manager: `t` enables a disabled server or
/// disables anything else; `r` reconnects everything but a disabled one.
/// `l` is handled separately (OAuth login, not an `McpCommand`).
fn mcp_command_for(infos: &[McpServerInfo], selected: usize, code: KeyCode) -> Option<McpCommand> {
    let info = infos.get(selected)?;
    let name = info.name.clone();
    match (code, &info.status) {
        (KeyCode::Char('t') | KeyCode::Char('T'), McpServerStatus::Disabled) => {
            Some(McpCommand::Toggle {
                server: name,
                enabled: true,
            })
        }
        (KeyCode::Char('t') | KeyCode::Char('T'), _) => Some(McpCommand::Toggle {
            server: name,
            enabled: false,
        }),
        (KeyCode::Char('r') | KeyCode::Char('R'), status)
            if *status != McpServerStatus::Disabled =>
        {
            Some(McpCommand::Reconnect { server: name })
        }
        _ => None,
    }
}

/// Tier-assignment keys, tolerant of keyboard layout: Shift+1-4 may arrive as
/// the shifted symbol (`!@#$`), the bare digit with SHIFT held (kitty
/// protocol), or a layout-specific variant. Identical to the reference's
/// `tier_for_shortcut`.
fn tier_for_key(key: &KeyEvent) -> Option<ModelTier> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    match c {
        '1' | '!' | '¡' => Some(ModelTier::Strong),
        '2' | '@' | '"' | '™' => Some(ModelTier::Medium),
        '3' | '#' | '§' | '£' => Some(ModelTier::Weak),
        '4' | '$' | '€' | '¤' => Some(ModelTier::Compaction),
        _ => None,
    }
}

/// Toggle `spec`'s hold on `tier`: unassign if it already holds it, assign
/// otherwise (evicting any previous holder — overrides are tier-keyed).
/// `dir=None` updates nothing: the registry has no unpersisted write path,
/// and tests must not touch the user's real state dir.
pub(crate) fn apply_tier_toggle(spec: &str, tier: ModelTier, dir: Option<&StateDir>) {
    let Some(dir) = dir else {
        return;
    };
    if model_registry::override_tiers(spec).contains(&tier) {
        model_registry::unset_and_persist(spec, tier, dir);
    } else {
        model_registry::set_and_persist(spec.to_string(), tier, dir);
    }
}

#[cfg(not(test))]
fn tier_state_dir() -> Option<StateDir> {
    StateDir::resolve().ok()
}

#[cfg(test)]
fn tier_state_dir() -> Option<StateDir> {
    // Persistence is exercised through `apply_tier_toggle` with an injected
    // tempdir; key-handling tests must never write the real state dir.
    None
}

/// Persist the chosen theme name. `dir=None` skips persistence: tests must
/// not touch the user's real state dir (same pattern as `apply_tier_toggle`).
pub(crate) fn apply_theme_choice(name: &str, dir: Option<&StateDir>) {
    if let Some(dir) = dir {
        let _ = crate::storage::theme::persist_theme_name(dir, name);
    }
}

#[cfg(not(test))]
fn theme_state_dir() -> Option<StateDir> {
    StateDir::resolve().ok()
}

#[cfg(test)]
fn theme_state_dir() -> Option<StateDir> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn temp_state_dir() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::from_path(dir.path().to_path_buf());
        (dir, state)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    /// `/recipe` with parameters prefills the `key=` stubs and leaves the
    /// composer cursor at the end of the prefilled line (not wherever it
    /// was before), so typing continues after the stubs.
    #[test]
    fn recipe_prefill_places_cursor_at_end() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("/rec".into());
        app.composer.cursor = 2; // simulate a mid-text leftover position
        app.overlays.modal = Modal::Recipes {
            entries: vec![RecipeEntry {
                name: "deploy".into(),
                description: "ship it".into(),
                params: vec!["env".into(), "tag".into()],
            }],
            selected: 0,
        };
        app.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "/recipe deploy env= tag=");
        assert_eq!(
            app.composer.cursor,
            app.composer.text.chars().count(),
            "cursor == len of the prefilled line"
        );
    }

    // --- /mcp screen (B.11) ---

    fn server_info(name: &str, status: McpServerStatus) -> McpServerInfo {
        McpServerInfo {
            name: name.into(),
            transport_kind: "stdio",
            tool_count: 2,
            prompt_count: 1,
            resource_count: 0,
            status,
            config_path: std::path::PathBuf::from("/test/mcp.toml"),
            url: None,
            oauth: None,
        }
    }

    /// `t` enables a disabled server and disables anything else; `r`
    /// reconnects everything but a disabled one; `l` is the OAuth login path
    /// (handled outside `mcp_command_for`), and every other key sends nothing.
    #[test]
    fn mcp_keys_map_to_the_right_commands() {
        let infos = vec![
            server_info("running-srv", McpServerStatus::Running),
            server_info("connecting-srv", McpServerStatus::Connecting),
            server_info("disabled-srv", McpServerStatus::Disabled),
            server_info("failed-srv", McpServerStatus::Failed("boom".into())),
            server_info(
                "auth-srv",
                McpServerStatus::NeedsAuth {
                    url: Some("https://auth.example".into()),
                },
            ),
        ];
        let toggle = |i: usize, enabled: bool| {
            matches!(
                mcp_command_for(&infos, i, KeyCode::Char('t')),
                Some(McpCommand::Toggle { ref server, enabled: e }) if server == &infos[i].name && e == enabled
            )
        };
        assert!(toggle(0, false), "running disables");
        assert!(toggle(1, false), "connecting disables");
        assert!(toggle(2, true), "disabled enables");
        assert!(toggle(3, false), "failed disables");
        assert!(toggle(4, false), "needs-auth disables");
        for i in [0, 1, 3, 4] {
            assert!(
                matches!(
                    mcp_command_for(&infos, i, KeyCode::Char('r')),
                    Some(McpCommand::Reconnect { ref server }) if server == &infos[i].name
                ),
                "row {i} reconnects"
            );
        }
        assert!(
            mcp_command_for(&infos, 2, KeyCode::Char('r')).is_none(),
            "reconnect skips a disabled server"
        );
        assert!(
            mcp_command_for(&infos, 4, KeyCode::Char('l')).is_none(),
            "login is not an McpCommand; it runs the OAuth flow directly"
        );
        assert!(
            mcp_command_for(&infos, 9, KeyCode::Char('t')).is_none(),
            "an out-of-range selection sends nothing"
        );
    }

    /// Arrows move the selection and clamp at the edges; `t` keeps the
    /// screen open; `q` and Esc close it.
    #[test]
    fn mcp_screen_selection_moves_and_close_keys() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::Mcp { selected: 0 };
        // No handle: the row list is empty, so Down clamps to 0.
        app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 0 }));
        app.handle_modal_key(key('t'), &tx);
        assert!(
            matches!(app.overlays.modal, Modal::Mcp { selected: 0 }),
            "action keys keep the screen open"
        );
        app.handle_modal_key(key('q'), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
        app.overlays.modal = Modal::Mcp { selected: 3 };
        app.handle_modal_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 2 }));
        app.handle_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
    }
    /// Phase 5: Enter expands the selected server's resource list, a
    /// second Enter collapses it, and other keys leave the state alone.
    #[test]
    fn mcp_screen_enter_toggles_resource_expansion() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::Mcp { selected: 0 };
        assert!(app.overlays.mcp_expanded.is_none());

        app.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        // No handle: infos is empty, so the out-of-range selection does not
        // expand.
        assert!(app.overlays.mcp_expanded.is_none());
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 0 }));

        app.mcp = Some(crate::mcp::test_support::stub_handle_with_resources(vec![
            crate::mcp::McpResourceInfo {
                server: "srv".into(),
                uri: "file:///notes.txt".into(),
                name: "notes".into(),
                description: String::new(),
                mime: None,
                size: None,
            },
        ]));
        app.handle_modal_key(key('e'), &tx);
        assert_eq!(app.overlays.mcp_expanded, Some(0));
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 0 }));

        app.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(app.overlays.mcp_expanded, None);
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 0 }));
    }

    /// The send path against a real manager: a Failed server flips to
    /// Disabled through the command loop, proving the key round-trips to the
    /// manager and the snapshot republishes.
    #[tokio::test]
    async fn mcp_toggle_key_reaches_the_manager() {
        use crate::mcp::config::{McpConfig, RawServerConfig, RawStdioFields, RawTransport};
        use std::collections::HashMap;

        let raw = RawServerConfig {
            enabled: true,
            timeout: 1_000,
            transport: RawTransport::Stdio(RawStdioFields {
                command: vec!["/nonexistent/definitely-not-here".into()],
                environment: HashMap::new(),
            }),
        };
        let mut mcp = HashMap::new();
        mcp.insert("ghost".to_string(), raw);
        let handle = crate::mcp::start_with_config(McpConfig {
            mcp,
            origins: HashMap::new(),
        })
        .unwrap();
        handle.ready().await;

        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.mcp = Some(handle.clone());
        app.overlays.modal = Modal::Mcp { selected: 0 };

        wait_for_status(&handle, "ghost", |s| {
            matches!(s, McpServerStatus::Failed(_))
        })
        .await;

        app.handle_modal_key(key('t'), &tx);
        assert!(matches!(app.overlays.modal, Modal::Mcp { selected: 0 }));
        wait_for_status(&handle, "ghost", |s| *s == McpServerStatus::Disabled).await;
        handle.shutdown().await;
    }

    /// Poll the published snapshot (25ms steps, 5s cap) until `server`'s
    /// status satisfies `pred`.
    async fn wait_for_status(
        handle: &crate::mcp::McpHandle,
        server: &str,
        pred: impl Fn(&McpServerStatus) -> bool,
    ) {
        for _ in 0..200 {
            let matched = handle
                .reader()
                .load()
                .infos
                .iter()
                .find(|info| info.name == server)
                .is_some_and(|info| pred(&info.status));
            if matched {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("{server}'s status never matched");
    }

    #[test]
    fn tier_for_key_maps_shifted_digits_and_layout_variants() {
        assert_eq!(tier_for_key(&key('!')), Some(ModelTier::Strong));
        assert_eq!(tier_for_key(&key('1')), Some(ModelTier::Strong));
        assert_eq!(tier_for_key(&key('@')), Some(ModelTier::Medium));
        assert_eq!(tier_for_key(&key('"')), Some(ModelTier::Medium));
        assert_eq!(tier_for_key(&key('#')), Some(ModelTier::Weak));
        assert_eq!(tier_for_key(&key('£')), Some(ModelTier::Weak));
        assert_eq!(tier_for_key(&key('$')), Some(ModelTier::Compaction));
        assert_eq!(tier_for_key(&key('¤')), Some(ModelTier::Compaction));
        // kitty reports Shift+2 as digit+SHIFT; the digit arm catches it.
        assert_eq!(
            tier_for_key(&KeyEvent::new(KeyCode::Char('2'), KeyModifiers::SHIFT)),
            Some(ModelTier::Medium)
        );
        assert_eq!(tier_for_key(&key('x')), None);
        assert_eq!(
            tier_for_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn toggle_assigns_persists_and_toggles_off() {
        let (_dir, state) = temp_state_dir();
        let spec = "toggle-test/model-a";
        apply_tier_toggle(spec, ModelTier::Strong, Some(&state));
        assert_eq!(
            model_registry::override_tiers(spec),
            vec![ModelTier::Strong]
        );
        assert!(state.path().join("model-tiers").exists());
        // A second tier stacks on the same model.
        apply_tier_toggle(spec, ModelTier::Weak, Some(&state));
        assert_eq!(
            model_registry::override_tiers(spec),
            vec![ModelTier::Strong, ModelTier::Weak]
        );
        // Toggling a held tier removes only that tier.
        apply_tier_toggle(spec, ModelTier::Strong, Some(&state));
        assert_eq!(model_registry::override_tiers(spec), vec![ModelTier::Weak]);
    }

    #[test]
    fn assigning_a_held_tier_evicts_the_previous_holder() {
        let (_dir, state) = temp_state_dir();
        let a = "evict-test/model-a";
        let b = "evict-test/model-b";
        apply_tier_toggle(a, ModelTier::Medium, Some(&state));
        apply_tier_toggle(b, ModelTier::Medium, Some(&state));
        assert!(model_registry::override_tiers(a).is_empty());
        assert_eq!(model_registry::override_tiers(b), vec![ModelTier::Medium]);
    }

    #[test]
    fn tier_keys_keep_the_picker_open_and_leave_the_session_model_alone() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::ModelMenu(1);
        app.handle_modal_key(key('!'), &tx);
        assert!(matches!(app.overlays.modal, Modal::ModelMenu(1)));
        assert_eq!(app.session.model_idx, 0, "tier keys never select a model");
        assert!(rx.try_recv().is_err(), "no SelectModel command is sent");
        // Non-tier keys (Esc) still close.
        app.handle_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn usage_overlay_ctrl_r_refetches_and_stays_open() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::Usage(Vec::new());
        app.handle_modal_key(ctrl('r'), &tx);
        assert!(
            matches!(app.overlays.modal, Modal::Usage(_)),
            "ctrl+r keeps the overlay open"
        );
        assert!(matches!(rx.try_recv(), Ok(Command::FetchUsage)));
    }

    #[test]
    fn usage_overlay_esc_and_ctrl_c_close() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::Usage(Vec::new());
        app.handle_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
        app.overlays.modal = Modal::Usage(Vec::new());
        app.handle_modal_key(ctrl('c'), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
    }

    #[test]
    fn usage_overlay_scrolls_and_consumes_other_keys() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::Usage(Vec::new());
        for _ in 0..3 {
            app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        }
        assert_eq!(app.overlays.usage_scroll, 3);
        app.handle_modal_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &tx);
        assert_eq!(app.overlays.usage_scroll, 0);
        // Plain 'q' is consumed: modal stays open, no quit.
        app.handle_modal_key(key('q'), &tx);
        assert!(matches!(app.overlays.modal, Modal::Usage(_)));
        assert!(!app.should_quit);
    }

    #[test]
    fn stats_overlay_scrolls_instead_of_closing_and_has_no_reload() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.overlays.modal = Modal::Stats(StatsView::default());
        app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.overlays.usage_scroll, 1);
        assert!(
            matches!(app.overlays.modal, Modal::Stats(_)),
            "arrows scroll, they do not close"
        );
        // Ctrl+R is usage-only: no quota fetch from the stats sheet.
        app.handle_modal_key(ctrl('r'), &tx);
        assert!(rx.try_recv().is_err());
        app.handle_modal_key(ctrl('c'), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
    }

    #[test]
    fn theme_picker_opens_on_current_theme() {
        let _guard = theme::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, _rx) = mpsc::unbounded_channel();
        theme::set_named("nord").unwrap();
        let mut app = App::new();
        app.run_command("theme", &tx);
        let Modal::ThemePicker {
            entries,
            selected,
            original,
        } = &app.overlays.modal
        else {
            panic!("theme command did not open the picker");
        };
        assert_eq!(entries, &theme::all_theme_names());
        assert_eq!(original, "nord");
        assert_eq!(entries[*selected], "nord");
        theme::set_named(theme::DEFAULT_THEME).unwrap();
    }

    #[test]
    fn theme_picker_arrows_preview_live() {
        let _guard = theme::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        theme::set_named(theme::DEFAULT_THEME).unwrap();
        let names = theme::all_theme_names();
        let idx = names
            .iter()
            .position(|n| n == theme::DEFAULT_THEME)
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.run_command("theme", &tx);
        app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert!(
            matches!(&app.overlays.modal, Modal::ThemePicker { selected, .. } if *selected == idx + 1),
            "Down moved the cursor one row"
        );
        // Moving the cursor swaps the global theme for the highlighted entry.
        assert_eq!(theme::current_theme_name(), names[idx + 1]);
    }

    #[test]
    fn theme_picker_enter_persists_and_esc_restores() {
        let _guard = theme::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_dir, state) = temp_state_dir();

        // Enter: persist the selection (persistence via the injected dir).
        let name = theme::all_theme_names()[2].clone();
        apply_theme_choice(&name, Some(&state));
        assert_eq!(
            crate::storage::theme::read_theme_name(&state).as_deref(),
            Some(name.as_str())
        );

        // Esc: restore the theme active on open and close.
        let (tx, _rx) = mpsc::unbounded_channel();
        theme::set_named(theme::DEFAULT_THEME).unwrap();
        let mut app = App::new();
        app.run_command("theme", &tx);
        app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_ne!(theme::current_theme_name(), theme::DEFAULT_THEME);
        app.handle_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
        assert_eq!(theme::current_theme_name(), theme::DEFAULT_THEME);
    }

    #[test]
    fn theme_picker_ctrl_c_restores() {
        let _guard = theme::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, _rx) = mpsc::unbounded_channel();
        theme::set_named(theme::DEFAULT_THEME).unwrap();
        let mut app = App::new();
        app.run_command("theme", &tx);
        app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        app.handle_modal_key(ctrl('c'), &tx);
        assert!(matches!(app.overlays.modal, Modal::None));
        assert_eq!(theme::current_theme_name(), theme::DEFAULT_THEME);
    }
}
