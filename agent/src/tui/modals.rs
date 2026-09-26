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

        // 4. Read-only usage/stats/help sheets: any key dismisses.
        if matches!(self.modal, Modal::Usage(_) | Modal::Stats(_) | Modal::Help) {
            self.modal = Modal::None;
            return;
        }

        // 5. Sessions picker.
        if matches!(self.modal, Modal::Sessions { .. }) {
            self.handle_sessions_key(key, tx);
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
}
