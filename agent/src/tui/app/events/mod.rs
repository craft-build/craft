//! Keyboard and mouse dispatch for the normal (modal-free) surface.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use super::{App, Message, Modal, PendingClick};
use crate::tui::app::EFFORTS;
use crate::tui::keybindings::ActionId;
use crate::tui::provider::{AgentEvent, Command};
use crate::tui::selection::{clamp_to, copy_to_clipboard, extract_selection_text, rect_contains};

// `super::X` paths in the moved impl files used to point at `app`; with the
// extra module level they now land here, so keep these two names in scope.
use super::{FLASH_TTL, TaskOutcome};

mod keys;
mod mouse;
mod paste;
#[cfg(test)]
mod tests;

/// Who owns the keyboard right now, in dispatch priority order.
/// `keyboard_owner` resolves the top of the stack; a prompt that
/// declines a chord lets the key keep walking down (`owner_below`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardOwner {
    Modal,
    PermissionPrompt,
    QuestionForm,
    Search,
    FilePicker,
    PlanForm,
    Base,
}

impl App {
    /// Resolve which surface owns the keyboard, mirroring the overlay
    /// stack priority: the modal first, then the answerable prompts
    /// (permission F.5 / question A.5, whose keys must stay answerable
    /// even if search/picker was already open), then the informational
    /// overlays.
    fn keyboard_owner(&self) -> KeyboardOwner {
        if !matches!(self.overlays.modal, Modal::None) {
            KeyboardOwner::Modal
        } else if self.overlays.permission_prompt.is_open() {
            KeyboardOwner::PermissionPrompt
        } else if self.overlays.question_form.is_open() {
            KeyboardOwner::QuestionForm
        } else {
            self.owner_below()
        }
    }

    /// The next owner under the prompts: search (F.3), then the file
    /// picker (F.3, Ctrl-S), then the plan form (F.3, Ctrl-T), then the
    /// base surface.
    fn owner_below(&self) -> KeyboardOwner {
        if self.overlays.search.is_open() {
            KeyboardOwner::Search
        } else if self.overlays.file_picker.is_open() {
            KeyboardOwner::FilePicker
        } else if self.plan_form_active() {
            KeyboardOwner::PlanForm
        } else {
            KeyboardOwner::Base
        }
    }
}
