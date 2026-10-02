//! View models: the message list, viewport state, and session chrome.

use ratatui::layout::Rect;

use crate::tui::provider::{AgentEvent, ModelChoice, Tone, ToolCallData, ToolKind, ToolLine};
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
    Tool {
        id: String,
        kind: ToolKind,
        lines: Vec<ToolLine>,
        diff: Option<DiffState>,
        /// Auto-review status for this call, rendered as a line under the
        /// card rather than inside it (updated in place by [`AgentEvent::AutoReview`]).
        review: Option<AutoReviewLine>,
        /// Base64 image rendered inline under the card body (`view_image`).
        image: Option<String>,
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
}

/// Message-list state: what the provider streams in, plus focus/collapse
/// chrome that rides on it.
pub struct Conversation {
    pub messages: Vec<Message>,
    pub collapsed: Vec<String>, // tool ids currently collapsed
    /// Tool ids whose bodies were expanded past the truncation cap.
    pub expanded_bodies: Vec<String>,
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

    /// Boundaries, completion and cancellation expose all received text.
    pub(crate) fn flush_reveal(&mut self) {
        self.reveal = None;
        self.assistant_open = false;
        self.thinking_open = false;
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
                kind,
                lines,
                awaiting_approval,
                image,
            }) => {
                // Tool boundaries close any open streamed paragraph.
                self.assistant_open = false;
                self.thinking_open = false;
                // Cards merge by id: a start event shows the running card, the
                // completion event fills in its body.
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
                        ..
                    }) => {
                        // Completion events carry the authoritative kind
                        // (summaries arrive with the result).
                        *existing_kind = kind;
                        *body = lines;
                        *slot = image;
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
                            kind,
                            lines,
                            diff,
                            review: None,
                            image,
                        });
                        if collapsible {
                            self.collapsed.push(id);
                        }
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

/// Viewport state: the scrollback document (segments + the viewport's
/// position in it) plus the renderer-written frame snapshot (message
/// doc-row anchors, per-row text, hit regions) and pointer state.
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
    /// Text of the last rendered frame, one entry per terminal row (filled by
    /// the renderer; used to extract selection text on copy).
    pub frame_text: Vec<String>,
    /// Selectable regions of the last frame: chat messages and composer input.
    pub msg_area: Rect,
    pub composer_area: Rect,
    pub selection: Option<Selection>,
    /// Screen rects of collapsible tool cards in the last frame: (message
    /// index, rect). Used for hover highlight and click-to-toggle.
    pub tool_regions: Vec<(usize, Rect)>,
    /// One-row screen rects of "click to expand" notice rows: (message
    /// index, rect). Checked before card regions on click.
    pub notice_regions: Vec<(usize, Rect)>,
    pub hover_tool: Option<usize>,
    /// Click target the current press started on; a press without drag
    /// activates it.
    pub pending_click: Option<PendingClick>,
    /// Segment index highlighted by the open search modal (F.3); rendered
    /// as reversed cells like the reference's `Cursor::render(highlight)`.
    pub highlight_segment: Option<usize>,
}

/// What a card press activates: toggling the whole card's collapse or just
/// its body's truncation.
#[derive(Clone, Copy, Debug)]
pub enum PendingClick {
    Card(usize),
    Notice(usize),
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
            frame_text: Vec::new(),
            msg_area: Rect::default(),
            composer_area: Rect::default(),
            selection: None,
            tool_regions: Vec::new(),
            notice_regions: Vec::new(),
            hover_tool: None,
            pending_click: None,
            highlight_segment: None,
        }
    }
}

/// Session chrome: model catalog + selection, cwd/branch, tokens, sidebar.
pub struct Session {
    pub models: Vec<ModelChoice>,
    pub model_idx: usize,
    pub thinking: crate::thinking::ThinkingConfig,
    pub cwd: String,
    pub branch: String,
    pub token_label: String,
    pub sidebar_open: bool,
}

impl Session {
    pub(crate) fn new() -> Self {
        Session {
            models: seed_models(),
            model_idx: 0,
            thinking: crate::thinking::ThinkingConfig::default(),
            cwd: String::new(),
            branch: String::new(),
            token_label: "…".into(),
            sidebar_open: true,
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
            id: "t1".into(),
            kind: ToolKind::Read {
                path: "src/a.rs".into(),
                summary: String::new(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
        }));
        conv.apply(AgentEvent::ToolCall(ToolCallData {
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
        }));
        assert_eq!(conv.messages.len(), 1);
        match &conv.messages[0] {
            Message::Tool { kind, lines, .. } => {
                assert!(matches!(kind, ToolKind::Read { summary, .. } if summary == "1 lines"));
                assert_eq!(lines.len(), 1);
            }
            _ => panic!("expected a tool card"),
        }
    }

    /// Auto-review attaches under the card by id, leaving the card's own kind
    /// and body intact, and survives the tool result merge.
    #[test]
    fn auto_review_rides_under_the_card() {
        let mut conv = Conversation::new();
        conv.apply(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: vec![],
            awaiting_approval: false,
            image: None,
        }));
        conv.apply(AgentEvent::AutoReview {
            id: "t1".into(),
            tone: Tone::Success,
            text: "auto-review allow: low — in-project".into(),
        });
        // The result merge updates the card body without dropping the review.
        conv.apply(AgentEvent::ToolCall(ToolCallData {
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
}
