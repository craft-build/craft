//! Modal overlays: the single exclusive modal state (command palette, model
//! menu, reject confirmation) and their key handling. A modal owns the
//! keyboard while open — keys never fall through to base chords, so e.g.
//! ctrl+q does not quit under an open palette.

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

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
    /// "Reject this diff?" confirmation: tool id awaiting the decision.
    ConfirmReject(String),
    /// `/usage`: this session's per-model tokens and cost.
    Usage(Vec<crate::tui::provider::UsageRow>),
    /// `/stats`: cross-session totals from the cost ledger.
    Stats(StatsView),
    /// `/help`: static keybinding/command sheet (throwaway; Phase 8 #80
    /// replaces it with data-driven keybindings).
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
    pub total_tokens: u64,
    pub sessions: usize,
    pub empty: bool,
}

impl App {
    pub(crate) fn handle_modal_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // 1. Confirm dialog swallows everything.
        if matches!(self.modal, Modal::ConfirmReject(_)) {
            match key.code {
                KeyCode::Char('Y') => self.reject_confirmed(tx, true),
                KeyCode::Char('y') | KeyCode::Enter => self.reject_confirmed(tx, false),
                _ => self.modal = Modal::None,
            }
            return;
        }

        // 2. Command palette.
        if matches!(self.modal, Modal::Palette { .. }) {
            self.handle_palette_key(key, tx);
            return;
        }

        // 3. Model menu.
        if matches!(self.modal, Modal::ModelMenu(_)) {
            self.handle_model_menu_key(key, tx);
            return;
        }

        // 4. Read-only usage/stats/help sheets.
        if matches!(self.modal, Modal::Usage(_)) {
            self.handle_usage_key(key, tx, true);
            return;
        }
        if matches!(self.modal, Modal::Stats(_)) {
            self.handle_usage_key(key, tx, false);
            return;
        }
        if matches!(self.modal, Modal::Help) {
            self.modal = Modal::None;
            return;
        }

        // 5. Sessions picker.
        if matches!(self.modal, Modal::Sessions { .. }) {
            self.handle_sessions_key(key, tx);
            return;
        }

        // 6. Theme picker.
        if matches!(self.modal, Modal::ThemePicker { .. }) {
            self.handle_theme_picker_key(key);
        }
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
        } = std::mem::replace(&mut self.modal, Modal::None)
        else {
            return;
        };
        let preview = |sel: usize, app: &mut Self, entries: &[String]| {
            if let Some(name) = entries.get(sel) {
                let _ = theme::set_named(name);
            }
            app.modal = Modal::ThemePicker {
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
        let Modal::Sessions { entries, selected } = std::mem::replace(&mut self.modal, Modal::None)
        else {
            return;
        };
        match key.code {
            KeyCode::Up => {
                self.modal = Modal::Sessions {
                    entries,
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                let max = entries.len().saturating_sub(1);
                self.modal = Modal::Sessions {
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
            KeyCode::Esc => self.modal = Modal::None,
            KeyCode::Char('c') | KeyCode::Char('C') if ctrl => self.modal = Modal::None,
            KeyCode::Char('r') | KeyCode::Char('R') if ctrl && allow_refresh => {
                let _ = tx.send(Command::FetchUsage);
            }
            KeyCode::Up => self.usage_scroll = self.usage_scroll.saturating_sub(1),
            KeyCode::Down => self.usage_scroll = self.usage_scroll.saturating_add(1),
            KeyCode::PageUp => self.usage_scroll = self.usage_scroll.saturating_sub(10),
            KeyCode::PageDown => self.usage_scroll = self.usage_scroll.saturating_add(10),
            _ => {}
        }
    }

    fn handle_palette_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // Snapshot the filtered items while the palette is still open.
        let items = self.palette_items();
        let Modal::Palette {
            mut query,
            selected,
        } = std::mem::replace(&mut self.modal, Modal::None)
        else {
            return;
        };
        match key.code {
            KeyCode::Esc => {} // dismiss (modal already replaced with None)
            KeyCode::Up => {
                self.modal = Modal::Palette {
                    query,
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Down => {
                let max = items.len().saturating_sub(1);
                self.modal = Modal::Palette {
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
                self.modal = Modal::Palette { query, selected: 0 }
            }
            KeyCode::Char(c) => {
                query.push(c);
                self.modal = Modal::Palette { query, selected: 0 }
            }
            _ => {
                // Unhandled keys leave the palette open.
                self.modal = Modal::Palette { query, selected };
            }
        }
    }

    fn handle_model_menu_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let Modal::ModelMenu(sel) = std::mem::replace(&mut self.modal, Modal::None) else {
            return;
        };
        // Tier assignment: a toggle on the highlighted model, persisted
        // globally. Never closes the picker, never selects a session model.
        if let Some(tier) = tier_for_key(&key) {
            if let Some(choice) = self.session.models.get(sel) {
                let spec = format!("{}/{}", choice.provider, choice.model);
                apply_tier_toggle(&spec, tier, tier_state_dir().as_ref());
            }
            self.modal = Modal::ModelMenu(sel);
            return;
        }
        match key.code {
            KeyCode::Up => self.modal = Modal::ModelMenu(sel.saturating_sub(1)),
            KeyCode::Down => {
                self.modal =
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
        app.modal = Modal::ModelMenu(1);
        app.handle_modal_key(key('!'), &tx);
        assert!(matches!(app.modal, Modal::ModelMenu(1)));
        assert_eq!(app.session.model_idx, 0, "tier keys never select a model");
        assert!(rx.try_recv().is_err(), "no SelectModel command is sent");
        // Non-tier keys (Esc) still close.
        app.handle_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn usage_overlay_ctrl_r_refetches_and_stays_open() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Usage(Vec::new());
        app.handle_modal_key(ctrl('r'), &tx);
        assert!(
            matches!(app.modal, Modal::Usage(_)),
            "ctrl+r keeps the overlay open"
        );
        assert!(matches!(rx.try_recv(), Ok(Command::FetchUsage)));
    }

    #[test]
    fn usage_overlay_esc_and_ctrl_c_close() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Usage(Vec::new());
        app.handle_modal_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
        app.modal = Modal::Usage(Vec::new());
        app.handle_modal_key(ctrl('c'), &tx);
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn usage_overlay_scrolls_and_consumes_other_keys() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Usage(Vec::new());
        for _ in 0..3 {
            app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        }
        assert_eq!(app.usage_scroll, 3);
        app.handle_modal_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &tx);
        assert_eq!(app.usage_scroll, 0);
        // Plain 'q' is consumed: modal stays open, no quit.
        app.handle_modal_key(key('q'), &tx);
        assert!(matches!(app.modal, Modal::Usage(_)));
        assert!(!app.should_quit);
    }

    #[test]
    fn stats_overlay_scrolls_instead_of_closing_and_has_no_reload() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Stats(StatsView::default());
        app.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.usage_scroll, 1);
        assert!(
            matches!(app.modal, Modal::Stats(_)),
            "arrows scroll, they do not close"
        );
        // Ctrl+R is usage-only: no quota fetch from the stats sheet.
        app.handle_modal_key(ctrl('r'), &tx);
        assert!(rx.try_recv().is_err());
        app.handle_modal_key(ctrl('c'), &tx);
        assert!(matches!(app.modal, Modal::None));
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
        } = &app.modal
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
            matches!(&app.modal, Modal::ThemePicker { selected, .. } if *selected == idx + 1),
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
        assert!(matches!(app.modal, Modal::None));
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
        assert!(matches!(app.modal, Modal::None));
        assert_eq!(theme::current_theme_name(), theme::DEFAULT_THEME);
    }
}
