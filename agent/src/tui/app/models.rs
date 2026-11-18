//! View models: the message list, viewport state, and session chrome.

use crate::tui::geom::Rect;

use crate::tui::provider::{
    AgentEvent, CallState, LineKind, ModelChoice, Tone, ToolCallData, ToolKind, ToolLine,
};
use crate::tui::selection::Selection;
use crate::tui::ui::scrollback::{ScrollPos, SegmentCache};

/// Models shown before the provider's catalog arrives (or under the test mock).
const SEED_MODELS: [(&str, &str); 4] = [
    ("GLM-5.3", "Zhipu AI Coding Plan"),
    ("Claude Sonnet 4.5", "Anthropic"),
    ("Claude Opus 4.1", "Anthropic"),
    ("DeepSeek V3.2", "DeepSeek"),
];

fn seed_models() -> Vec<ModelChoice> {
    SEED_MODELS
        .iter()
        .map(|(label, provider)| ModelChoice {
            provider: provider.to_string(),
            model: label.to_string(),
            label: label.to_string(),
            provider_label: provider.to_string(),
            thinking_choices: crate::thinking::ThinkingConfig::choices(),
            vision: true,
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffState {
    Pending,
}

pub enum Message {
    User(String),
    Assistant(String),
    /// Model reasoning (thinking) text, rendered dimmer than replies.
    Thinking(String),
    /// A tone-tagged system notice (retry / auth / compaction / doom /
    /// guardrail status), rendered as one muted line without card chrome.
    Notice {
        tone: Tone,
        text: String,
    },
    /// A compaction summary replayed from history (`craft.compaction`),
    /// rendered as a muted card: what replaced the compacted-away turns.
    Compaction {
        kind: String,
        text: String,
    },
    Tool {
        id: String,
        parent_call_id: Option<String>,
        kind: ToolKind,
        lines: Vec<ToolLine>,
        /// The verbatim original behind this card's "show full output"
        /// affordance, when an alteration revised the replayed body.
        /// `None` for every live card.
        full: Option<Vec<ToolLine>>,
        diff: Option<DiffState>,
        /// Auto-review status for this call, rendered as a line under the
        /// card rather than inside it (updated in place by [`AgentEvent::AutoReview`]).
        review: Option<AutoReviewLine>,
        /// Base64 image rendered inline under the card body (`view_image`).
        image: Option<String>,
        /// Call lifecycle; the finish event's merge is authoritative.
        state: CallState,
    },
}

/// One auto-review status line shown beneath a tool card: a tone and text,
/// e.g. "auto-review allow: low — in-project edit".
#[derive(Clone, Debug, PartialEq)]
pub struct AutoReviewLine {
    pub tone: Tone,
    pub text: String,
}

impl Message {
    pub(crate) fn is_collapsible_tool(&self) -> bool {
        matches!(self, Message::Tool { kind, .. } if kind.collapsible())
    }

    pub(crate) fn is_pending_diff(&self) -> bool {
        matches!(
            self,
            Message::Tool {
                diff: Some(DiffState::Pending),
                ..
            }
        )
    }

    /// A root tool call that never joins a compact group: a subagent
    /// `task` (a peer of the whole turn) or an `Edit` mutation (the
    /// user must always see file changes). Both always render their
    /// dedicated cards outside the folded group. Nested children of
    /// any kind stay with their code parent's run.
    pub(crate) fn is_always_card(&self) -> bool {
        matches!(
            self,
            Message::Tool {
                kind: ToolKind::Task { .. } | ToolKind::Edit { .. },
                parent_call_id: None,
                ..
            }
        )
    }
}

/// Whether `m` can belong to a compact root group (see
/// [`Message::is_always_card`]).
fn joins_compact_run(m: &Message) -> bool {
    match m {
        Message::Thinking(_) => true,
        Message::Tool { .. } => !m.is_always_card(),
        _ => false,
    }
}

/// Message-list state: what the provider streams in, plus focus/collapse
/// chrome that rides on it.
pub struct Conversation {
    pub messages: Vec<Message>,
    pub collapsed: Vec<String>, // tool ids currently collapsed
    /// Tool ids whose bodies were expanded past the truncation cap.
    pub expanded_bodies: Vec<String>,
    /// Message indices of [`Message::Thinking`] blocks the user expanded;
    /// thinking renders collapsed behind its header unless listed here.
    pub expanded_thinking: Vec<usize>,
    /// Pinned expansion of tool groups: group key → expanded. Unlisted
    /// groups follow the auto rule (expanded while any member runs,
    /// collapsed once every member settles).
    pub group_pins: Vec<(String, bool)>,
    pub focused: Option<usize>, // message index of focused tool block
    /// True while a streamed [`Message::Assistant`] is still being appended to.
    pub(crate) assistant_open: bool,
    /// True while a streamed [`Message::Thinking`] is still being appended to.
    thinking_open: bool,
    /// Only the active streamed paragraph needs a separate visible cursor.
    reveal: Option<(usize, crate::tui::animation::Typewriter)>,
}

impl Conversation {
    pub(crate) fn new() -> Self {
        Conversation {
            messages: Vec::new(),
            collapsed: Vec::new(),
            expanded_bodies: Vec::new(),
            expanded_thinking: Vec::new(),
            group_pins: Vec::new(),
            focused: None,
            assistant_open: false,
            thinking_open: false,
            reveal: None,
        }
    }

    /// Complete stored text for settled messages, visible prefix for the stream.
    pub(crate) fn visible_text(&self, index: usize) -> Option<&str> {
        let text = match self.messages.get(index)? {
            Message::Assistant(text) | Message::Thinking(text) => text.as_str(),
            _ => return None,
        };
        Some(match &self.reveal {
            Some((active, cursor)) if *active == index => cursor.visible(text),
            _ => text,
        })
    }

    /// Advance before drawing; true only when the visible prefix changed.
    pub(crate) fn tick_reveal(&mut self, now: std::time::Instant) -> bool {
        let Some((index, cursor)) = &mut self.reveal else {
            return false;
        };
        match self.messages.get(*index) {
            Some(Message::Assistant(text) | Message::Thinking(text)) => cursor.tick(text, now),
            _ => {
                self.reveal = None;
                false
            }
        }
    }

    pub(crate) fn reveal_pending(&self, show_thinking: bool) -> bool {
        self.reveal.as_ref().is_some_and(|(index, cursor)| {
            cursor.is_animating()
                && match self.messages.get(*index) {
                    Some(Message::Assistant(_)) => true,
                    Some(Message::Thinking(_)) => show_thinking,
                    _ => false,
                }
        })
    }

    /// True while `index` is the thinking block still being streamed into.
    pub(crate) fn thinking_active(&self, index: usize) -> bool {
        self.thinking_open
            && index + 1 == self.messages.len()
            && matches!(self.messages.last(), Some(Message::Thinking(_)))
    }

    /// Boundaries, completion and cancellation expose all received text.
    pub(crate) fn flush_reveal(&mut self) {
        self.reveal = None;
        self.assistant_open = false;
        self.thinking_open = false;
    }

    /// End (exclusive) of the maximal contiguous run of tool messages
    /// starting at `start`. Root groups and nested groups are both such
    /// runs (children insert directly after their code parent).
    pub(crate) fn tool_run_end(&self, start: usize) -> usize {
        let mut end = start;
        while matches!(self.messages.get(end), Some(Message::Tool { .. })) {
            end += 1;
        }
        end
    }

    /// End (exclusive) of the maximal contiguous run of tool and thinking
    /// messages starting at `start`: a compact root group's span, which
    /// absorbs the thinking blocks interleaved with its tools. Always-
    /// card calls (root `task`, root `Edit`) terminate the run — they
    /// never fold into a group, so the run splits around their
    /// dedicated cards.
    pub(crate) fn compact_run_end(&self, start: usize) -> usize {
        let mut end = start;
        while self.messages.get(end).is_some_and(joins_compact_run) {
            end += 1;
        }
        end
    }

    /// Whether the compact run starting at `idx` contains a tool call: a
    /// thinking block groups only when tools follow before the run breaks.
    pub(crate) fn compact_run_has_tool(&self, idx: usize) -> bool {
        (idx..self.compact_run_end(idx))
            .any(|i| matches!(self.messages.get(i), Some(Message::Tool { .. })))
    }

    /// End (exclusive) of the maximal contiguous run of children sharing
    /// `parent` starting at `start` (one nested group).
    pub(crate) fn child_run_end(&self, start: usize, parent: &str) -> usize {
        let mut end = start;
        while let Some(Message::Tool {
            parent_call_id: Some(p),
            ..
        }) = self.messages.get(end)
        {
            if p != parent {
                break;
            }
            end += 1;
        }
        end
    }

    /// Stable key of the group whose first member is message `idx`:
    /// the parent id for nested groups, the first root tool's id for
    /// root groups (a compact run starting at a thinking block keys on
    /// its first tool — thinking members carry no id). Indices shift
    /// when children insert mid-run; ids never do.
    pub(crate) fn group_key(&self, idx: usize) -> Option<String> {
        let mut idx = idx;
        while matches!(self.messages.get(idx), Some(Message::Thinking(_))) {
            idx += 1;
        }
        match self.messages.get(idx)? {
            Message::Tool {
                parent_call_id: Some(parent),
                ..
            } => Some(format!("child:{parent}")),
            Message::Tool { id, .. } => Some(format!("root:{id}")),
            _ => None,
        }
    }

    /// Whether any member the group's header counts is still live: the
    /// children of a nested group, the root-level members of a root
    /// group (nested headers track their own children). In compact
    /// display a root group also spans its thinking members, and a
    /// trailing thinking block still streaming keeps it live.
    pub(crate) fn group_running(&self, idx: usize, grouped_roots: bool) -> bool {
        match self.messages.get(idx) {
            Some(Message::Tool {
                parent_call_id: Some(parent),
                ..
            }) => {
                let end = self.child_run_end(idx, parent);
                self.messages[idx..end].iter().any(|m| {
                    matches!(
                        m,
                        Message::Tool {
                            state: CallState::Running,
                            ..
                        }
                    )
                })
            }
            Some(Message::Tool { .. }) | Some(Message::Thinking(_)) if grouped_roots => {
                self.root_run_live(idx, self.compact_run_end(idx))
            }
            Some(Message::Tool { .. }) => self.root_run_live(idx, self.tool_run_end(idx)),
            _ => false,
        }
    }

    /// Whether the root run `idx..end` has a running root member or ends
    /// in a thinking block still being streamed into.
    fn root_run_live(&self, idx: usize, end: usize) -> bool {
        self.messages[idx..end].iter().any(|m| {
            matches!(
                m,
                Message::Tool {
                    parent_call_id: None,
                    state: CallState::Running,
                    ..
                }
            )
        }) || self.trailing_thinking_active(end)
    }

    /// Whether the run ending at `end` (exclusive) ends in a thinking
    /// block still being streamed into.
    pub(crate) fn trailing_thinking_active(&self, end: usize) -> bool {
        matches!(self.messages.get(end - 1), Some(Message::Thinking(_)))
            && self.thinking_active(end - 1)
    }

    /// Whether the group keyed by `key` renders expanded. Nested
    /// (codemode-children) groups auto-expand while any member runs and
    /// auto-collapse when they settle; compact root groups never
    /// auto-expand — their header tracks the live count instead, and
    /// only a pin (a header click) opens them. `running` feeds only the
    /// nested auto policy; root keys ignore it.
    pub(crate) fn group_expanded(&self, key: &str, running: bool) -> bool {
        match self.group_pins.iter().find(|(k, _)| k == key) {
            Some(&(_, pinned)) => pinned,
            None => running && key.starts_with("child:"),
        }
    }

    /// Whether the tool message at `idx` renders: members of a collapsed
    /// group are hidden behind its header. Nested children always group
    /// by parent; root tools group by run only when `grouped_roots` is
    /// set (compact display), mirroring the render walk. Always-card
    /// kinds (`task`, root `Edit`) never group, so they always render.
    pub(crate) fn tool_group_expanded(&self, idx: usize, grouped_roots: bool) -> bool {
        match self.messages.get(idx) {
            Some(m) if m.is_always_card() => true,
            Some(Message::Tool {
                parent_call_id: Some(parent),
                ..
            }) => {
                let mut start = idx;
                while start > 0
                    && matches!(
                        &self.messages[start - 1],
                        Message::Tool {
                            parent_call_id: Some(p),
                            ..
                        }
                        if p == parent
                    )
                {
                    start -= 1;
                }
                let key = format!("child:{parent}");
                self.group_expanded(&key, self.group_running(start, grouped_roots))
            }
            Some(Message::Tool {
                parent_call_id: None,
                ..
            }) if grouped_roots => {
                // The root group spans from the run's first member — a
                // root tool or a leading thinking block — to the run's
                // end; nested child runs at its head form their own
                // groups.
                let mut start = idx;
                while start > 0 && self.messages.get(start - 1).is_some_and(joins_compact_run) {
                    start -= 1;
                }
                while let Some(Message::Tool {
                    parent_call_id: Some(p),
                    ..
                }) = self.messages.get(start)
                {
                    start = self.child_run_end(start, p);
                }
                match self.group_key(start) {
                    Some(key) => self.group_expanded(&key, false),
                    None => true,
                }
            }
            _ => true,
        }
    }

    /// Pin the group's opposite state, surviving streaming.
    pub(crate) fn toggle_group(&mut self, key: &str, running: bool) {
        let expanded = !self.group_expanded(key, running);
        match self.group_pins.iter_mut().find(|(k, _)| k == key) {
            Some(pin) => pin.1 = expanded,
            None => self.group_pins.push((key.to_owned(), expanded)),
        }
    }

    /// Key of the nested group a newly added running child restarts,
    /// when every prior counted child had settled (the settled →
    /// running transition that clears a nested pin; root pins are
    /// manual choices and never clear). `id` identifies the new child
    /// to exclude.
    fn restarted_child_group_key(&self, id: &str, parent: &str) -> Option<String> {
        let restarted = !self.messages.iter().any(|m| {
            matches!(m, Message::Tool { id: mid, parent_call_id: Some(p), state: CallState::Running, .. }
                if p == parent && mid != id)
        });
        restarted.then(|| format!("child:{parent}"))
    }

    /// Merge a provider event into the message list. Non-message events
    /// (status, plan, catalog, ...) are ignored; App routes those itself.
    pub(crate) fn apply(&mut self, ev: AgentEvent) {
        self.apply_at(ev, std::time::Instant::now());
    }

    fn apply_at(&mut self, ev: AgentEvent, now: std::time::Instant) {
        let continues = match &ev {
            AgentEvent::AssistantDelta(_) => {
                self.assistant_open && matches!(self.messages.last(), Some(Message::Assistant(_)))
            }
            AgentEvent::ReasoningDelta(_) => {
                self.thinking_open && matches!(self.messages.last(), Some(Message::Thinking(_)))
            }
            _ => false,
        };
        if matches!(
            &ev,
            AgentEvent::AssistantDelta(_) | AgentEvent::ReasoningDelta(_)
        ) {
            if !continues {
                self.flush_reveal();
                self.reveal = Some((
                    self.messages.len(),
                    crate::tui::animation::Typewriter::new(now),
                ));
            }
            let delta = match &ev {
                AgentEvent::AssistantDelta(text) | AgentEvent::ReasoningDelta(text) => text,
                _ => unreachable!(),
            };
            if let Some((index, cursor)) = &mut self.reveal {
                let existing = match self.messages.get(*index) {
                    Some(Message::Assistant(text) | Message::Thinking(text)) => text.as_str(),
                    _ => "",
                };
                // Tick against the old buffer before the append, preserving
                // progress even when several deltas arrive between frames.
                cursor.push(existing, delta, now);
            }
        } else if matches!(
            &ev,
            AgentEvent::AssistantText(_)
                | AgentEvent::AssistantEnd
                | AgentEvent::Notice { .. }
                | AgentEvent::ToolCall(_)
                | AgentEvent::StatusChanged(
                    crate::tui::provider::Status::Done | crate::tui::provider::Status::Failed
                )
                | AgentEvent::SessionLoaded { .. }
        ) {
            self.flush_reveal();
        }
        match ev {
            AgentEvent::AssistantText(text) => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.messages.push(Message::Assistant(text));
            }
            AgentEvent::AssistantDelta(text) => {
                self.thinking_open = false;
                match self.messages.last_mut() {
                    Some(Message::Assistant(buf)) if self.assistant_open => buf.push_str(&text),
                    _ => self.messages.push(Message::Assistant(text)),
                }
                self.assistant_open = true;
            }
            AgentEvent::ReasoningDelta(text) => {
                self.assistant_open = false;
                match self.messages.last_mut() {
                    Some(Message::Thinking(buf)) if self.thinking_open => buf.push_str(&text),
                    _ => self.messages.push(Message::Thinking(text)),
                }
                self.thinking_open = true;
            }
            AgentEvent::AssistantEnd => {
                self.assistant_open = false;
                self.thinking_open = false;
            }
            AgentEvent::Notice { tone, text } => {
                // Notices break paragraphs just like tool boundaries do.
                self.assistant_open = false;
                self.thinking_open = false;
                self.messages.push(Message::Notice { tone, text });
            }
            AgentEvent::AutoReview { id, tone, text } => {
                // Attach to the call's card so the status renders as a line
                // under it; the card body itself is left to the tool output.
                let card = self
                    .messages
                    .iter_mut()
                    .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id));
                if let Some(Message::Tool { review, .. }) = card {
                    *review = Some(AutoReviewLine { tone, text });
                }
            }
            AgentEvent::ToolCall(ToolCallData {
                id,
                parent_call_id,
                kind,
                lines,
                awaiting_approval,
                image,
                state,
            }) => {
                // Tool boundaries close any open streamed paragraph.
                self.assistant_open = false;
                self.thinking_open = false;
                // Cards merge by id: a start event shows the running card, the
                // completion event fills in its body (and state).
                let existing = self
                    .messages
                    .iter_mut()
                    .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id));
                match existing {
                    Some(Message::Tool {
                        kind: existing_kind,
                        lines: body,
                        diff,
                        image: slot,
                        state: card_state,
                        ..
                    }) => {
                        // Completion events carry the authoritative kind
                        // (summaries arrive with the result) and state.
                        *existing_kind = kind;
                        *body = lines;
                        *slot = image;
                        *card_state = state;
                        if awaiting_approval {
                            if matches!(diff, None | Some(DiffState::Pending)) {
                                *diff = Some(DiffState::Pending);
                            }
                        } else if *diff == Some(DiffState::Pending) {
                            // The permission modal recorded the decision;
                            // drop the stale "needs approval" badge.
                            *diff = None;
                        }
                    }
                    _ => {
                        let diff = if matches!(kind, ToolKind::Edit { .. }) && awaiting_approval {
                            Some(DiffState::Pending)
                        } else {
                            None
                        };
                        let collapsible = kind.collapsible();
                        self.messages.push(Message::Tool {
                            id: id.clone(),
                            parent_call_id: parent_call_id.clone(),
                            kind,
                            lines,
                            full: None,
                            diff,
                            review: None,
                            image,
                            state,
                        });
                        if parent_call_id.is_some() {
                            // Exclude the newly pushed child itself from the
                            // subtree scan.
                            let prior = self.messages.len() - 1;
                            let subtree_end = self.messages[..prior]
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, message)| {
                                    matches!(message, Message::Tool {
                                        id: owner,
                                        parent_call_id: owner_parent,
                                        ..
                                    } if parent_call_id.as_deref() == Some(owner.as_str())
                                        || owner_parent.as_deref()
                                            == parent_call_id.as_deref())
                                })
                                .map(|(index, _)| index + 1)
                                .unwrap_or(self.messages.len() - 1);
                            let card = self.messages.pop().expect("card was just pushed");
                            self.messages.insert(subtree_end, card);
                        }
                        // A nested group restarting after every counted
                        // child had settled drops its pin: the pin
                        // described the previous run's preferred state.
                        // Root groups never auto-expand, so their pins
                        // are purely manual and survive restarts.
                        if state == CallState::Running
                            && let Some(parent) = parent_call_id.as_deref()
                            && let Some(key) = self.restarted_child_group_key(&id, parent)
                        {
                            self.group_pins.retain(|(k, _)| k != &key);
                        }
                        if collapsible {
                            self.collapsed.push(id);
                        }
                    }
                }
            }
            AgentEvent::ToolProgress { id, content } => {
                let card = self
                    .messages
                    .iter_mut()
                    .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id));
                if let Some(Message::Tool { lines, .. }) = card {
                    lines.extend(
                        content
                            .lines()
                            .map(|line| ToolLine::new(LineKind::Muted, line)),
                    );
                    if lines.len() > 200 {
                        let tail = lines.split_off(lines.len() - 200);
                        *lines = tail;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod reveal_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn stored_text_is_complete_while_visible_prefix_advances() {
        let now = Instant::now();
        let mut chat = Conversation::new();
        chat.apply_at(AgentEvent::AssistantDelta("é中🦀a".into()), now);
        assert!(matches!(&chat.messages[0], Message::Assistant(text) if text == "é中🦀a"));
        assert_eq!(chat.visible_text(0), Some(""));
        assert!(chat.tick_reveal(now + Duration::from_millis(15)));
        assert_eq!(chat.visible_text(0), Some("é中"));
        chat.apply_at(
            AgentEvent::AssistantDelta("b".into()),
            now + Duration::from_millis(15),
        );
        assert_eq!(chat.visible_text(0), Some("é中"));
        assert!(chat.tick_reveal(now + Duration::from_millis(45)));
        assert_eq!(chat.visible_text(0), Some("é中🦀ab"));
        assert!(!chat.reveal_pending(true));
        assert!(!chat.tick_reveal(now + Duration::from_secs(1)));
    }

    #[test]
    fn superseded_attempt_text_is_not_duplicated_into_the_next_reply() {
        // The continuation layer's retry abandons an attempt mid-stream:
        // the model must close that buffer (the renderer maps
        // `AttemptInterrupted` to `AssistantEnd`) so the retried attempt's
        // deltas start a fresh message instead of concatenating.
        let now = Instant::now();
        let mut chat = Conversation::new();
        chat.apply_at(AgentEvent::AssistantDelta("abandoned ".into()), now);
        chat.apply_at(AgentEvent::AssistantEnd, now);
        chat.apply_at(AgentEvent::AssistantDelta("fresh".into()), now);
        assert_eq!(chat.messages.len(), 2, "two separate replies");
        assert!(
            matches!(&chat.messages[0], Message::Assistant(text) if text == "abandoned "),
            "the superseded text stays on screen, unmodified"
        );
        assert!(matches!(&chat.messages[1], Message::Assistant(text) if text == "fresh"));
    }

    #[test]
    fn reasoning_transition_and_end_flush_received_text() {
        let now = Instant::now();
        let mut chat = Conversation::new();
        chat.apply_at(AgentEvent::ReasoningDelta("thinking".into()), now);
        assert!(
            !chat.reveal_pending(false),
            "hidden reasoning owes no frames"
        );
        assert!(chat.reveal_pending(true));
        chat.apply_at(AgentEvent::AssistantDelta("answer".into()), now);
        assert_eq!(chat.visible_text(0), Some("thinking"));
        assert_eq!(chat.visible_text(1), Some(""));
        chat.apply_at(AgentEvent::AssistantEnd, now);
        assert_eq!(chat.visible_text(1), Some("answer"));
        assert!(!chat.reveal_pending(true));
        chat.apply_at(AgentEvent::AssistantDelta("next".into()), now);
        assert_eq!(chat.messages.len(), 3);
        chat.flush_reveal();
        assert_eq!(chat.visible_text(2), Some("next"));
    }

    #[test]
    fn completion_and_full_messages_are_immediately_visible() {
        let now = Instant::now();
        let mut chat = Conversation::new();
        chat.apply_at(AgentEvent::AssistantDelta("partial".into()), now);
        chat.apply_at(
            AgentEvent::StatusChanged(crate::tui::provider::Status::Done),
            now,
        );
        assert_eq!(chat.visible_text(0), Some("partial"));
        chat.apply_at(AgentEvent::AssistantText("complete".into()), now);
        assert_eq!(chat.visible_text(1), Some("complete"));
        assert!(!chat.reveal_pending(true));
        // Loaded messages are inserted directly; they never acquire a cursor.
        chat.messages.push(Message::Assistant("loaded".into()));
        assert_eq!(chat.visible_text(2), Some("loaded"));
    }

    #[test]
    fn failure_and_notice_boundaries_flush() {
        let now = Instant::now();
        let mut chat = Conversation::new();
        chat.apply_at(AgentEvent::ReasoningDelta("received reasoning".into()), now);
        chat.apply_at(
            AgentEvent::StatusChanged(crate::tui::provider::Status::Failed),
            now,
        );
        assert_eq!(chat.visible_text(0), Some("received reasoning"));
        chat.apply_at(AgentEvent::AssistantDelta("received reply".into()), now);
        chat.apply_at(
            AgentEvent::Notice {
                tone: Tone::Info,
                text: "boundary".into(),
            },
            now,
        );
        assert_eq!(chat.visible_text(1), Some("received reply"));
        assert!(!chat.reveal_pending(true));
    }
}

/// Painter-written frame handoff: the per-row text snapshot and hit regions
/// the last paint produced, plus the transcript area's width. Painters run
/// deferred (after the frame closure returns), so they hand data back
/// through this shared lock instead of borrowing `App`; event handlers read
/// it one frame later.
#[derive(Default)]
pub struct PaintData {
    /// Text of the last rendered frame, one entry per terminal row (used to
    /// extract selection text on copy).
    pub frame_text: Vec<String>,
    /// Selectable regions of the last frame: chat messages and composer input.
    pub msg_area: Rect,
    pub composer_area: Rect,
    /// Screen rects of collapsible tool cards in the last frame: (message
    /// index, rect). Used for hover highlight and click-to-toggle.
    pub tool_regions: Vec<(usize, Rect)>,
    /// One-row screen rects of "click to expand" notice rows: (message
    /// index, rect). Checked before card regions on click.
    pub notice_regions: Vec<(usize, Rect)>,
    /// One-row screen rects of thinking-block headers: (message index,
    /// rect). A click toggles the reasoning body's visibility.
    pub thinking_regions: Vec<(usize, Rect)>,
    /// One-row screen rects of tool-group headers: (message index of the
    /// group's first member, rect). A click toggles the group.
    pub group_regions: Vec<(usize, Rect)>,
    /// One-row screen rects of the todo widget's section headers; a click
    /// folds/unfolds that section.
    pub todo_header: Rect,
    pub files_header: Rect,
    /// Outer width of the transcript area in the last frame; the slt painter
    /// reads it to pre-wrap rows before its own layout resolves (one frame
    /// stale, 0 until the first paint).
    pub width: u16,
}

/// Viewport state: the scrollback document (segments + the viewport's
/// position in it) plus pointer state. The renderer-written frame snapshot
/// lives behind [`ViewModel::paint`] (see [`PaintData`]).
pub struct ViewModel {
    /// Top of the viewport as a place in the segment document. Width-
    /// independent, so a resize keeps the anchor segment.
    pub scroll: ScrollPos,
    pub follow: bool,
    /// The transcript as rendered segments; refilled by the renderer each
    /// frame in deterministic message order, so stored positions survive
    /// refills and appends.
    pub segments: SegmentCache,
    pub(crate) assistant_cache: crate::tui::ui::messages::AssistantCache,
    pub view_height: u16,
    pub view_width: u16,
    /// Doc row where each message starts (filled by the renderer).
    pub msg_starts: Vec<usize>,
    /// Painter-written frame handoff (per-row text, hit regions, width).
    pub paint: std::sync::Arc<std::sync::Mutex<PaintData>>,
    pub selection: Option<Selection>,
    pub hover_tool: Option<usize>,
    /// Click target the current press started on; a press without drag
    /// activates it.
    pub pending_click: Option<PendingClick>,
    /// Segment index highlighted by the open search modal; rendered
    /// as reversed cells.
    pub highlight_segment: Option<usize>,
}

/// What a card press activates: toggling the whole card's collapse or just
/// its body's truncation.
#[derive(Clone, Copy, Debug)]
pub enum PendingClick {
    Card(usize),
    Notice(usize),
    /// A thinking block's header row: toggles its collapsed body.
    Thinking(usize),
    /// A tool group's header row (the message index of its first
    /// member): toggles the group's pinned expansion.
    Group(usize),
    /// The todo widget's plan header.
    Todo,
    /// The todo widget's files-touched header.
    Files,
}

impl ViewModel {
    pub(crate) fn new() -> Self {
        ViewModel {
            scroll: ScrollPos::default(),
            follow: true,
            segments: SegmentCache::new(),
            assistant_cache: crate::tui::ui::messages::AssistantCache::default(),
            view_height: 0,
            view_width: 0,
            msg_starts: Vec::new(),
            paint: std::sync::Arc::new(std::sync::Mutex::new(PaintData::default())),
            selection: None,
            hover_tool: None,
            pending_click: None,
            highlight_segment: None,
        }
    }
}

/// Session chrome: model catalog + selection, cwd/branch, tokens, and the
/// collapse state of the todo widget above the composer.
pub struct Session {
    pub models: Vec<ModelChoice>,
    pub model_idx: usize,
    pub thinking: crate::thinking::ThinkingConfig,
    pub codemode: crate::config::CodemodeMode,
    pub cwd: String,
    pub branch: String,
    pub token_label: String,
    /// Todo widget: plan checklist expanded (Ctrl-T in Build mode) or
    /// folded to its one-line summary.
    pub todo_open: bool,
    /// Todo widget: files-touched list expanded (Ctrl-B) or folded to its
    /// count.
    pub files_open: bool,
}

impl Session {
    pub(crate) fn new() -> Self {
        Session {
            models: seed_models(),
            model_idx: 0,
            thinking: crate::thinking::ThinkingConfig::default(),
            codemode: crate::config::CodemodeMode::Off,
            cwd: String::new(),
            branch: String::new(),
            token_label: "…".into(),
            todo_open: true,
            files_open: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::LineKind;

    /// Streamed deltas append to one bubble until explicitly closed.
    #[test]
    fn assistant_deltas_append_then_close() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::AssistantDelta("Hello".into()));
        conv.apply(AgentEvent::AssistantDelta(", world".into()));
        conv.apply(AgentEvent::AssistantEnd);
        conv.apply(AgentEvent::AssistantDelta("Again".into()));
        let texts: Vec<&str> = conv
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hello, world", "Again"]);
    }

    /// Reasoning deltas stream into their own dimmed block, which closes when
    /// the reply text starts instead of merging into it.
    #[test]
    fn reasoning_deltas_form_their_own_block() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ReasoningDelta("considering ".into()));
        conv.apply(AgentEvent::ReasoningDelta("options".into()));
        conv.apply(AgentEvent::AssistantDelta("Answer".into()));
        let contents: Vec<(&str, &str)> = conv
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::Thinking(t) => Some(("thinking", t.as_str())),
                Message::Assistant(t) => Some(("assistant", t.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            contents,
            [("thinking", "considering options"), ("assistant", "Answer")]
        );
    }

    /// Start and completion events for the same call render one card.
    #[test]
    fn tool_call_events_merge_by_id() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Read {
                path: "src/a.rs".into(),
                summary: String::new(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        }));
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Read {
                path: "src/a.rs".into(),
                summary: "1 lines".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Context,
                text: "fn main() {}".into(),
                ..Default::default()
            }],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        assert_eq!(conv.messages.len(), 1);
        match &conv.messages[0] {
            Message::Tool {
                kind, lines, state, ..
            } => {
                assert!(matches!(kind, ToolKind::Read { summary, .. } if summary == "1 lines"));
                assert_eq!(lines.len(), 1);
                assert_eq!(
                    *state,
                    CallState::Complete,
                    "the finish merge is authoritative"
                );
            }
            _ => panic!("expected a tool card"),
        }
    }

    /// Two code runs can both emit raw child id `code-0`: parent-qualified
    /// ids keep the children separate and directly under their parents.
    #[test]
    fn nested_cards_stay_under_their_parent_and_progress_replaces() {
        let mut conv = Conversation::new();
        for parent in ["t1", "t2"] {
            conv.apply(AgentEvent::ToolCall(ToolCallData {
                parent_call_id: None,
                id: parent.into(),
                kind: ToolKind::Code {
                    script: "print(1)".into(),
                    summary: String::new(),
                    failure: None,
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state: CallState::Running,
            }));
            conv.apply(AgentEvent::ToolCall(ToolCallData {
                parent_call_id: Some(parent.into()),
                id: format!("{parent}:code-0"),
                kind: ToolKind::Read {
                    path: "a.rs".into(),
                    summary: String::new(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state: CallState::Running,
            }));
        }
        conv.apply(AgentEvent::ToolProgress {
            id: "t1".into(),
            content: "partial".into(),
        });
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Code {
                script: "print(1)".into(),
                summary: "0 calls · 0.0s · ok".into(),
                failure: None,
            },
            lines: vec![ToolLine::new(LineKind::Success, "final")],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        let ids: Vec<(&str, Option<&str>)> = conv
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool {
                    id, parent_call_id, ..
                } => Some((id.as_str(), parent_call_id.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            [
                ("t1", None),
                ("t1:code-0", Some("t1")),
                ("t2", None),
                ("t2:code-0", Some("t2"))
            ]
        );
        match &conv.messages[0] {
            Message::Tool {
                kind: ToolKind::Code { summary, .. },
                lines,
                ..
            } => {
                assert!(summary.contains("ok"));
                assert_eq!(lines.len(), 1);
                assert_eq!(lines[0].text, "final");
            }
            _ => panic!("expected a code card"),
        }
    }

    /// Auto-review attaches under the card by id, leaving the card's own kind
    /// and body intact, and survives the tool result merge.
    #[test]
    fn auto_review_rides_under_the_card() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        }));
        conv.apply(AgentEvent::AutoReview {
            id: "t1".into(),
            tone: Tone::Success,
            text: "auto-review allow: low — in-project".into(),
        });
        // The result merge updates the card body without dropping the review.
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Success,
                text: "test result: ok".into(),
                ..Default::default()
            }],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        match &conv.messages[0] {
            Message::Tool {
                kind,
                lines,
                review,
                ..
            } => {
                assert!(matches!(kind, ToolKind::Bash { cmd } if cmd == "cargo test"));
                assert_eq!(lines.len(), 1, "card shows the tool output");
                assert_eq!(
                    review.as_ref().map(|r| r.text.as_str()),
                    Some("auto-review allow: low — in-project"),
                    "review survives the result merge"
                );
            }
            _ => panic!("expected a tool card"),
        }
    }

    /// A failed result merge (or any ToolCall carrying `Failed`) settles
    /// the card as failed, feeding the group counters.
    #[test]
    fn failed_result_merges_mark_the_card_failed() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        }));
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: vec![ToolLine::new(LineKind::Del, "error: test failed")],
            awaiting_approval: false,
            image: None,
            state: CallState::Failed,
        }));
        match &conv.messages[0] {
            Message::Tool { state, .. } => assert_eq!(*state, CallState::Failed),
            _ => panic!("expected a tool card"),
        }
    }

    /// Group expansion: auto while running, auto-collapsed once settled,
    /// pinned by a click, and the pin clears when a settled group starts
    /// running again.
    #[test]
    fn group_pins_follow_auto_expansion_until_repinned() {
        let mut conv = Conversation::new();
        let parent_start = AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "t1".into(),
            kind: ToolKind::Code {
                script: "print(1)".into(),
                summary: String::new(),
                failure: None,
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Running,
        });
        conv.apply(parent_start);
        let child = |id: &str, state: CallState| {
            AgentEvent::ToolCall(ToolCallData {
                parent_call_id: Some("t1".into()),
                id: id.into(),
                kind: ToolKind::Read {
                    path: "a.rs".into(),
                    summary: String::new(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state,
            })
        };
        conv.apply(child("t1:c0", CallState::Running));
        // The finish merge settles the only child.
        conv.apply(child("t1:c0", CallState::Complete));
        let key = "child:t1";
        // Auto: expanded while any member runs, collapsed once settled.
        assert!(conv.group_expanded(key, true), "running groups expand");
        assert!(!conv.group_expanded(key, false), "settled groups collapse");
        // Pin collapsed while running: stays collapsed through the stream.
        conv.toggle_group(key, true);
        assert!(conv.group_pins.contains(&(key.to_owned(), false)));
        assert!(!conv.group_expanded(key, true));
        // Pin expanded while settled: stays expanded.
        conv.toggle_group(key, false);
        assert!(conv.group_expanded(key, false));
        // The settled → running transition clears the pin (auto again).
        conv.apply(child("t1:c2", CallState::Running));
        assert!(
            !conv.group_pins.iter().any(|(k, _)| k == key),
            "the restarted group drops its pin"
        );
        assert!(conv.group_expanded(key, true));
    }

    /// Root groups pin on the run's first root tool id; running never
    /// expands them (only a pin opens a root group), and a root pin is
    /// a manual choice that survives a restart of the run.
    #[test]
    fn root_group_pins_key_on_the_runs_first_root_tool() {
        let mut conv = Conversation::new();
        let bash = |id: &str, state: CallState| {
            AgentEvent::ToolCall(ToolCallData {
                parent_call_id: None,
                id: id.into(),
                kind: ToolKind::Bash {
                    cmd: "echo hi".into(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state,
            })
        };
        conv.apply(bash("r1", CallState::Complete));
        conv.apply(bash("r2", CallState::Running));
        let key = conv.group_key(0).expect("root group key");
        assert_eq!(key, "root:r1");
        assert!(conv.group_running(0, true), "r2 still runs");
        // Running never expands a root group: the header tracks the
        // live count instead.
        assert!(
            !conv.group_expanded(&key, true),
            "a running root group stays collapsed"
        );
        // A header click pins it open.
        conv.toggle_group(&key, true);
        assert_eq!(conv.group_pins, [(key.clone(), true)]);
        // Settle r2, then restart the run with r3: the manual pin
        // survives.
        conv.apply(bash("r2", CallState::Complete));
        conv.apply(bash("r3", CallState::Running));
        assert_eq!(
            conv.group_pins,
            [(key, true)],
            "a root pin survives the run restarting"
        );
    }

    /// Compact root groups span their thinking members: the run keys on
    /// its first root tool, goes live while a trailing thinking block
    /// streams, and cards-mode grouping ignores thinking entirely.
    #[test]
    fn compact_root_groups_span_thinking_members() {
        let bash = |id: &str, state: CallState| {
            AgentEvent::ToolCall(ToolCallData {
                parent_call_id: None,
                id: id.into(),
                kind: ToolKind::Bash {
                    cmd: "echo hi".into(),
                },
                lines: vec![],
                awaiting_approval: false,
                image: None,
                state,
            })
        };
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ReasoningDelta("first".into()));
        conv.apply(bash("r1", CallState::Complete));
        assert_eq!(
            conv.group_key(0).as_deref(),
            Some("root:r1"),
            "a thinking-led run keys on its first tool"
        );
        assert!(
            !conv.group_running(0, true),
            "a settled run with settled thinking is not live"
        );

        // A new thinking block joins the run and keeps it live.
        conv.apply(AgentEvent::ReasoningDelta("next".into()));
        assert!(
            conv.group_running(0, true),
            "a trailing thinking block still streaming keeps the run live"
        );
        assert!(
            !conv.group_running(0, false),
            "cards-mode grouping ignores thinking members"
        );
    }

    /// Opening a new thinking block after a settled root run keeps the
    /// run's pin: root pins are manual choices, and thinking joining
    /// the run only lights the header's live cue.
    #[test]
    fn new_thinking_block_keeps_a_settled_root_pin() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            parent_call_id: None,
            id: "r1".into(),
            kind: ToolKind::Bash {
                cmd: "echo hi".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
            state: CallState::Complete,
        }));
        conv.toggle_group("root:r1", false);
        assert_eq!(conv.group_pins, [("root:r1".to_owned(), true)]);
        conv.apply(AgentEvent::ReasoningDelta("more".into()));
        assert_eq!(
            conv.group_pins,
            [("root:r1".to_owned(), true)],
            "a thinking block joining the run leaves the pin alone"
        );
    }
}
