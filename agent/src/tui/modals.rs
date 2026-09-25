//! Modal overlays: the single exclusive modal state (command palette, model
//! menu, reject confirmation) and their key handling. A modal owns the
//! keyboard while open — keys never fall through to base chords, so e.g.
//! ctrl+q does not quit under an open palette.

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::mpsc;

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
