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
}

impl App {
    pub(crate) fn handle_modal_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // 1. Confirm dialog swallows everything.
        if matches!(self.modal, Modal::ConfirmReject(_)) {
            match key.code {
                KeyCode::Char('y') | KeyCode::Enter => self.reject_confirmed(tx),
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
                self.modal = Modal::ModelMenu((sel + 1).min(self.models.len().saturating_sub(1)))
            }
            KeyCode::Enter => {
                self.model_idx = sel;
                if let Some(choice) = self.models.get(sel) {
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
