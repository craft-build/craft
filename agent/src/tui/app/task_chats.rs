//! Subagent task chats (task 96): one transcript per `task` tool call,
//! selectable from the Ctrl-N modal picker, cancellable with Esc-Esc.
//!
//! The visible transcript always lives in `App::conversation` /
//! `App::view` — entering a task chat swaps its saved state into those
//! slots, leaving the main chat parked inside the `TaskChat` — so the
//! shared render / scroll / mouse code operates on whatever the user is
//! looking at without branching.

use super::App;
use super::models::{Conversation, ViewModel};

/// How a task chat ended, from the vaguest to the most specific. The
/// transcript close (`terminalize`) only ever records [`TaskOutcome::Unknown`];
/// the tool-call verdict (`is_error`) arrives later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The turn ended without a verdict for this task.
    Unknown,
    /// The task tool call completed successfully.
    Done,
    /// The task tool call failed (or was cancelled via Esc-Esc).
    Error,
}

impl TaskOutcome {
    /// Only the placeholder gives way, so a late verdict can correct an
    /// unknown ending but never walks back a decided one.
    pub fn refines(self, previous: Self) -> bool {
        previous == Self::Unknown && self != Self::Unknown
    }
}

/// Renderable status of a task chat, derived from the outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Working,
    Done,
    Error,
}

impl From<Option<TaskOutcome>> for TaskStatus {
    fn from(outcome: Option<TaskOutcome>) -> Self {
        match outcome {
            None | Some(TaskOutcome::Unknown) | Some(TaskOutcome::Done) => Self::Done,
            Some(TaskOutcome::Error) => Self::Error,
        }
    }
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Working => "Working",
            Self::Done => "Done",
            Self::Error => "Error",
        }
    }
}

/// One `task`-spawned subagent's chat: its own conversation transcript,
/// its own view state (scroll position survives leaving and returning),
/// and the outcome of the spawning tool call.
pub struct TaskChat {
    /// The spawning `task` tool call's id; also the cancellation key.
    pub tool_use_id: String,
    /// The description passed to the task call (the chat's name).
    pub name: String,
    /// The task's own transcript; subagent events apply here.
    pub conversation: Conversation,
    /// Scroll/viewport state, swapped into `App::view` while focused.
    pub view: ViewModel,
    /// `None` while the task is still working.
    pub outcome: Option<TaskOutcome>,
}

impl TaskChat {
    pub fn new(tool_use_id: impl Into<String>, name: impl Into<String>) -> Self {
        TaskChat {
            tool_use_id: tool_use_id.into(),
            name: name.into(),
            conversation: Conversation::new(),
            view: ViewModel::new(),
            outcome: None,
        }
    }

    /// Current renderable status.
    pub fn status(&self) -> TaskStatus {
        // `None` (no terminal event yet) is the only Working state; a
        // terminalized-but-unverdicted chat reads as Done.
        if self.outcome.is_none() {
            TaskStatus::Working
        } else {
            self.outcome.into()
        }
    }

    /// Record an outcome, honoring [`TaskOutcome::refines`]: a decided
    /// ending is never walked back. Returns whether the outcome changed.
    pub fn finish(&mut self, outcome: TaskOutcome) -> bool {
        match self.outcome {
            Some(previous) if !outcome.refines(previous) => false,
            _ => {
                self.outcome = Some(outcome);
                true
            }
        }
    }

    /// Close a still-working chat with the placeholder outcome; a late
    /// verdict can still correct it to Done or Error.
    pub fn terminalize(&mut self) {
        if self.outcome.is_none() {
            self.outcome = Some(TaskOutcome::Unknown);
        }
    }
}

impl App {
    /// Index of the task chat for `tool_use_id`, if any.
    pub(crate) fn task_chat_index(&self, tool_use_id: &str) -> Option<usize> {
        self.task_chats
            .iter()
            .position(|c| c.tool_use_id == tool_use_id)
    }

    /// The conversation that belongs to the main chat, wherever it is
    /// currently parked (inside the focused task chat's slot while a
    /// task chat is mounted).
    pub(crate) fn main_conversation_mut(&mut self) -> &mut Conversation {
        match self.active_task {
            // The focused task chat occupies the render slots, so the
            // main transcript sits in its `conversation` field.
            Some(i) => &mut self.task_chats[i].conversation,
            None => &mut self.conversation,
        }
    }

    /// Open the task-chat picker modal (task 96, reference list-picker
    /// style) with the cursor on the currently focused chat. A no-op
    /// without task chats.
    pub fn open_task_picker(&mut self) {
        if self.task_chats.is_empty() {
            return;
        }
        self.overlays.modal = super::Modal::TaskPicker {
            selected: self.active_task.map_or(0, |i| i + 1),
        };
    }

    /// Mount the chat at view position `pos` (0 = main chat).
    pub(crate) fn focus_chat_position(&mut self, pos: usize) {
        let target = if pos == 0 { None } else { Some(pos - 1) };
        if target == self.active_task {
            return;
        }
        // Swap the mountable state out/in so each chat keeps its own
        // transcript and scroll position across focus changes.
        match (self.active_task, target) {
            (Some(old), Some(new)) => {
                // old's state goes to its home, new's comes in; the main
                // chat's state moves from old's slot to new's.
                std::mem::swap(
                    &mut self.conversation,
                    &mut self.task_chats[old].conversation,
                );
                std::mem::swap(&mut self.view, &mut self.task_chats[old].view);
                std::mem::swap(
                    &mut self.conversation,
                    &mut self.task_chats[new].conversation,
                );
                std::mem::swap(&mut self.view, &mut self.task_chats[new].view);
            }
            (Some(old), None) => {
                std::mem::swap(
                    &mut self.conversation,
                    &mut self.task_chats[old].conversation,
                );
                std::mem::swap(&mut self.view, &mut self.task_chats[old].view);
            }
            (None, Some(new)) => {
                std::mem::swap(
                    &mut self.conversation,
                    &mut self.task_chats[new].conversation,
                );
                std::mem::swap(&mut self.view, &mut self.task_chats[new].view);
            }
            (None, None) => {}
        }
        self.active_task = target;
        self.esc_pending = None;
    }

    /// Close every still-working task chat with the placeholder outcome.
    /// Called when the provider returns to idle: the transcripts are
    /// closed, but a late tool verdict can still refine them.
    pub(crate) fn terminalize_task_chats(&mut self) {
        for chat in &mut self.task_chats {
            chat.terminalize();
        }
    }

    /// Drop all task-chat state (session reset / context clear).
    /// Unmounts the focused chat first so the main transcript is back in
    /// the render slots before the vec is emptied.
    pub(crate) fn clear_task_chats(&mut self) {
        self.focus_chat_position(0);
        self.task_chats.clear();
        self.esc_pending = None;
    }

    /// Route an inner subagent event into its task chat's transcript,
    /// creating the chat on first sight of the tool-use id.
    pub(crate) fn apply_subagent_event(
        &mut self,
        tool_use_id: &str,
        description: &str,
        event: crate::tui::provider::AgentEvent,
    ) {
        let idx = match self.task_chat_index(tool_use_id) {
            Some(idx) => idx,
            None => {
                self.task_chats
                    .push(TaskChat::new(tool_use_id, description));
                self.task_chats.len() - 1
            }
        };
        // The focused chat's transcript is mounted in the shared slot.
        if self.active_task == Some(idx) {
            self.conversation.apply(event);
        } else {
            self.task_chats[idx].conversation.apply(event);
        }
    }

    /// Record the tool-call verdict for one task chat. Returns false when
    /// no such chat exists or the verdict does not refine the outcome.
    pub(crate) fn finish_task_chat(&mut self, tool_use_id: &str, is_error: bool) -> bool {
        let outcome = if is_error {
            TaskOutcome::Error
        } else {
            TaskOutcome::Done
        };
        match self.task_chat_index(tool_use_id) {
            Some(idx) => {
                if self.active_task == Some(idx) {
                    self.conversation.flush_reveal();
                } else {
                    self.task_chats[idx].conversation.flush_reveal();
                }
                self.task_chats[idx].finish(outcome)
            }
            None => false,
        }
    }

    /// True while any task chat has not seen a terminal event — drives
    /// the main chat's "N tasks running" hint line.
    pub fn any_task_working(&self) -> bool {
        self.task_chats.iter().any(|c| c.outcome.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_completion_flushes_only_its_transcript_in_either_mount_position() {
        for mounted in [false, true] {
            let mut app = App::new();
            app.conversation
                .apply(crate::tui::provider::AgentEvent::AssistantDelta(
                    "main reply".into(),
                ));
            app.apply_subagent_event(
                "t1",
                "refactor",
                crate::tui::provider::AgentEvent::AssistantDelta("child reply".into()),
            );
            if mounted {
                app.focus_chat_position(1);
            }
            assert!(app.finish_task_chat("t1", false));
            let child = if mounted {
                &app.conversation
            } else {
                &app.task_chats[0].conversation
            };
            assert!(!child.reveal_pending(true));
            assert_eq!(child.visible_text(0), Some("child reply"));
            assert!(app.main_conversation_mut().reveal_pending(true));
        }
    }

    #[test]
    fn refines_only_upgrades_the_placeholder() {
        // A late verdict corrects an unknown close…
        assert!(TaskOutcome::Done.refines(TaskOutcome::Unknown));
        assert!(TaskOutcome::Error.refines(TaskOutcome::Unknown));
        // …but never walks back a decided ending, in either direction.
        assert!(!TaskOutcome::Unknown.refines(TaskOutcome::Unknown));
        assert!(!TaskOutcome::Done.refines(TaskOutcome::Done));
        assert!(!TaskOutcome::Error.refines(TaskOutcome::Done));
        assert!(!TaskOutcome::Done.refines(TaskOutcome::Error));
        assert!(!TaskOutcome::Error.refines(TaskOutcome::Error));
    }

    #[test]
    fn finish_honors_refines_semantics() {
        let mut chat = TaskChat::new("t1", "refactor");
        // Terminalize (turn end) → Unknown; a late error verdict lands.
        chat.terminalize();
        assert_eq!(chat.outcome, Some(TaskOutcome::Unknown));
        assert!(chat.finish(TaskOutcome::Error));
        assert_eq!(chat.outcome, Some(TaskOutcome::Error));
        // A done verdict arriving late must not flip the error.
        assert!(!chat.finish(TaskOutcome::Done));
        assert_eq!(chat.outcome, Some(TaskOutcome::Error));
        // Terminalizing an already-decided chat is a no-op.
        chat.terminalize();
        assert_eq!(chat.outcome, Some(TaskOutcome::Error));
    }

    #[test]
    fn status_maps_outcome_to_working_done_error() {
        let mut chat = TaskChat::new("t1", "x");
        assert_eq!(chat.status(), TaskStatus::Working);
        chat.finish(TaskOutcome::Done);
        assert_eq!(chat.status(), TaskStatus::Done);
        // Unknown (terminalized without a verdict) reads as Done.
        let mut chat2 = TaskChat::new("t2", "y");
        chat2.terminalize();
        assert_eq!(chat2.status(), TaskStatus::Done);
        chat2.finish(TaskOutcome::Error);
        assert_eq!(chat2.status(), TaskStatus::Error);
    }
}
