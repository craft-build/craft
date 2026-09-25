//! Application state and input handling. The App renders whatever the
//! provider streams in and translates key presses into provider commands.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::tui::composer::Composer;
use crate::tui::modals::{Modal, SessionEntry, StatsView};
use crate::tui::provider::{
    AgentEvent, Command, LoadedMessage, ModelChoice, PlanItem, Status, Tone, ToolCallData,
    ToolKind, ToolLine, TouchedFile, UsageRow,
};
use crate::tui::repaint;
use crate::tui::selection::{
    Selection, clamp_to, copy_to_clipboard, extract_selection_text, rect_contains,
};
use crate::tui::ui::scrollback::{Layout, ScrollPos, SegmentCache};

/// How long a status-row flash toast stays up (reference default,
/// `DEFAULT_FLASH_DURATION_MS`).
const FLASH_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

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
        })
        .collect()
}

pub const EFFORTS: [&str; 3] = ["low", "medium", "high"];

/// One command as shown in slash completion and the command palette. Both
/// surfaces derive from [`COMMANDS`] so a command can never be advertised
/// in one and absent (or a no-op) in the other.
pub struct CommandSpec {
    /// Dispatch id resolved by [`App::run_command`].
    pub id: &'static str,
    /// Slash form; `None` = palette-only.
    pub slash: Option<&'static str>,
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
        label: "New session",
        hint: "",
        desc: "",
    },
    CommandSpec {
        id: "sessions",
        slash: Some("/sessions"),
        label: "Switch session",
        hint: "/sessions",
        desc: "List sessions",
    },
    CommandSpec {
        id: "toggle-sidebar",
        slash: None,
        label: "Toggle context panel",
        hint: "ctrl+b",
        desc: "",
    },
    CommandSpec {
        id: "model",
        slash: Some("/model"),
        label: "Change model",
        hint: "ctrl+l",
        desc: "Switch model",
    },
    CommandSpec {
        id: "clear",
        slash: Some("/clear"),
        label: "Clear context",
        hint: "/clear",
        desc: "Clear conversation context",
    },
    CommandSpec {
        id: "copy",
        slash: None,
        label: "Copy last message",
        hint: "",
        desc: "",
    },
    CommandSpec {
        id: "undo",
        slash: Some("/undo"),
        label: "Undo last edit",
        hint: "/undo",
        desc: "Revert the last edit",
    },
    CommandSpec {
        id: "compact",
        slash: Some("/compact"),
        label: "Compact context",
        hint: "/compact",
        desc: "Compact context to save tokens",
    },
    CommandSpec {
        id: "usage",
        slash: Some("/usage"),
        label: "Show usage",
        hint: "/usage",
        desc: "Show this session's tokens and cost",
    },
    CommandSpec {
        id: "stats",
        slash: Some("/stats"),
        label: "Show stats",
        hint: "/stats",
        desc: "Show cost across all sessions",
    },
    CommandSpec {
        id: "auto-review",
        slash: Some("/auto-review"),
        label: "Toggle auto-review",
        hint: "/auto-review",
        desc: "Toggle LLM auto-review of permissions",
    },
    CommandSpec {
        id: "help",
        slash: Some("/help"),
        label: "Help",
        hint: "/help",
        desc: "Show keybindings",
    },
];

/// Whether placing `text` in the composer would open the slash-command menu.
/// Shared by slash completion and history recall so a recalled entry can be
/// recognized as a command without rebuilding the match list.
fn opens_slash_menu(text: &str) -> bool {
    text.starts_with('/')
        && COMMANDS
            .iter()
            .filter_map(|spec| spec.slash)
            .any(|cmd| text == "/" || cmd.starts_with(text))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DiffState {
    Pending,
    Approved,
    Rejected,
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
    fn is_collapsible_tool(&self) -> bool {
        matches!(self, Message::Tool { kind, .. } if kind.collapsible())
    }

    fn is_pending_diff(&self) -> bool {
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
    assistant_open: bool,
    /// True while a streamed [`Message::Thinking`] is still being appended to.
    thinking_open: bool,
}

impl Conversation {
    fn new() -> Self {
        Conversation {
            messages: Vec::new(),
            collapsed: Vec::new(),
            expanded_bodies: Vec::new(),
            focused: None,
            assistant_open: false,
            thinking_open: false,
        }
    }

    /// Merge a provider event into the message list. Non-message events
    /// (status, plan, catalog, ...) are ignored; App routes those itself.
    pub(crate) fn apply(&mut self, ev: AgentEvent) {
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
                        ..
                    }) => {
                        // Completion events carry the authoritative kind
                        // (summaries arrive with the result).
                        *existing_kind = kind;
                        *body = lines;
                        if awaiting_approval && matches!(diff, None | Some(DiffState::Pending)) {
                            *diff = Some(DiffState::Pending);
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
}

/// What a card press activates: toggling the whole card's collapse or just
/// its body's truncation.
#[derive(Clone, Copy, Debug)]
pub enum PendingClick {
    Card(usize),
    Notice(usize),
}

impl ViewModel {
    fn new() -> Self {
        ViewModel {
            scroll: ScrollPos::default(),
            follow: true,
            segments: SegmentCache::new(),
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
        }
    }
}

/// Session chrome: model catalog + selection, cwd/branch, tokens, sidebar.
pub struct Session {
    pub models: Vec<ModelChoice>,
    pub model_idx: usize,
    pub effort_idx: usize,
    pub cwd: String,
    pub branch: String,
    pub token_label: String,
    pub sidebar_open: bool,
}

impl Session {
    fn new() -> Self {
        Session {
            models: seed_models(),
            model_idx: 0,
            effort_idx: 2, // "high", the prototype default
            cwd: "~/Projects/craft-web".into(),
            branch: "fix/session-refresh".into(),
            token_label: "…".into(),
            sidebar_open: true,
        }
    }
}

pub struct App {
    // --- provider-driven state ---
    pub plan: Vec<PlanItem>,
    pub files: Vec<TouchedFile>,
    pub status: Status,
    /// Set when the user interrupted a running turn; cleared by the next
    /// provider status change. Keeps `busy()` false so a repeated Ctrl-C
    /// quits without misreporting the turn as failed.
    pub interrupt_requested: bool,
    /// Animation frame counter for the status indicator (advanced per frame).
    pub status_tick: usize,
    /// When the current turn started; drives the elapsed-seconds counter
    /// next to the thinking/running spinner.
    pub turn_started: Option<std::time::Instant>,
    /// Timed flash toast for the status row: text plus its expiry.
    pub flash: Option<(String, std::time::Instant)>,

    // --- state groups (fields stay pub for the renderer for now;
    // accessor encapsulation is a follow-up) ---
    pub conversation: Conversation,
    pub view: ViewModel,
    pub session: Session,

    // --- composer ---
    pub composer: Composer,
    /// Rolling user-input history (↑/↓ recall; persisted per state dir).
    pub input_history: crate::storage::input_history::InputHistory,
    /// Position in `input_history` while recalling; `None` = live editing.
    pub history_index: Option<usize>,
    /// The in-progress text saved when history recall starts, restored on
    /// ↓ past the newest entry.
    pub history_draft: String,

    // --- overlays ---
    /// The one modal currently open (palette / model menu / confirm),
    /// exclusive by construction. The slash popup is not a modal: it rides
    /// on the composer (shown when the composer starts with '/').
    pub modal: Modal,
    pub slash_selected: usize, // row in the slash popup
    /// Latest per-model usage snapshot from the provider (the `/usage`
    /// overlay's data; refreshed after every completed run).
    pub usage: Vec<UsageRow>,

    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        App {
            plan: Vec::new(),
            files: Vec::new(),
            status: Status::Done,
            interrupt_requested: false,
            status_tick: 0,
            turn_started: None,
            flash: None,
            conversation: Conversation::new(),
            view: ViewModel::new(),
            session: Session::new(),
            composer: Composer::new(),
            input_history: crate::storage::input_history::InputHistory::default(),
            history_index: None,
            history_draft: String::new(),
            modal: Modal::None,
            slash_selected: 0,
            usage: Vec::new(),
            should_quit: false,
        }
    }

    pub fn model(&self) -> (&str, &str) {
        self.session
            .models
            .get(self.session.model_idx)
            .map(|m| (m.label.as_str(), m.provider_label.as_str()))
            .unwrap_or(("no model", "no provider"))
    }

    /// Open the model picker with the current selection highlighted,
    /// replacing any modal already open.
    fn open_model_menu(&mut self) {
        if !self.session.models.is_empty() {
            self.modal = Modal::ModelMenu(
                self.session
                    .model_idx
                    .min(self.session.models.len().saturating_sub(1)),
            );
        }
    }

    pub fn effort(&self) -> &'static str {
        EFFORTS[self.session.effort_idx]
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Thinking | Status::Running) && !self.interrupt_requested
    }

    /// Show `msg` in the status row until [`FLASH_SECS`] elapses or the
    /// next keypress (e.g. "Copied" after a selection copy).
    pub fn flash(&mut self, msg: impl Into<String>) {
        self.flash = Some((msg.into(), std::time::Instant::now()));
    }

    /// Whether a live flash still holds the status row's right side.
    pub fn flash_text(&self) -> Option<&str> {
        self.flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < FLASH_TTL)
            .map(|(s, _)| s.as_str())
    }

    /// Drop an expired flash; true when the caller owes a repaint.
    pub fn clear_expired_flash(&mut self) -> bool {
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH_TTL)
        {
            self.flash = None;
            true
        } else {
            false
        }
    }

    // ------------------------------------------------------------------
    // Provider events
    // ------------------------------------------------------------------

    /// How soon the loop must wake for this app's screen, and whether the
    /// clock alone owes a frame. Only the spinner statuses animate; every
    /// other status paints pixels that a timeout cannot change.
    pub fn cadence(&self) -> repaint::Cadence {
        repaint::Cadence::any([
            repaint::Cadence::when(
                matches!(
                    self.status,
                    Status::Thinking | Status::Running | Status::WaitingApproval
                ),
                repaint::Cadence::SPINNER,
            ),
            // A live flash expires on the clock: keep waking up so the
            // status row drops it without waiting for the next event.
            repaint::Cadence::when(self.flash.is_some(), repaint::Cadence::SPINNER),
        ])
    }

    pub fn handle_event(&mut self, ev: AgentEvent) {
        let was_following = self.view.follow;
        match ev {
            AgentEvent::StatusChanged(s) => {
                // A fresh busy status starts the elapsed-seconds clock; a
                // settled status stops it.
                if matches!(s, Status::Thinking | Status::Running)
                    && !matches!(self.status, Status::Thinking | Status::Running)
                {
                    self.turn_started = Some(std::time::Instant::now());
                } else if !matches!(s, Status::Thinking | Status::Running) {
                    self.turn_started = None;
                }
                self.status = s;
                self.interrupt_requested = false;
            }
            AgentEvent::PlanSet(plan) => self.plan = plan,
            AgentEvent::FilesSet(files) => self.files = files,
            AgentEvent::TokenUsage(label) => self.session.token_label = label,
            AgentEvent::CatalogSet { models, current } => {
                if !models.is_empty() {
                    self.session.models = models;
                    self.session.model_idx = current.min(self.session.models.len() - 1);
                }
            }
            AgentEvent::SessionInfo { cwd, branch } => {
                self.session.cwd = cwd;
                self.session.branch = branch;
            }
            AgentEvent::UsageSnapshot(rows) => {
                self.usage = rows;
                // Keep an open /usage overlay fresh when a run completes.
                if matches!(self.modal, Modal::Usage(_)) {
                    self.modal = Modal::Usage(self.usage.clone());
                }
            }
            AgentEvent::SessionLoaded { messages } => {
                self.reset_conversation();
                for msg in messages {
                    let msg = match msg {
                        LoadedMessage::User(text) => Message::User(text),
                        LoadedMessage::Assistant(text) => Message::Assistant(text),
                    };
                    self.conversation.messages.push(msg);
                }
            }
            // Message-bearing events merge into the conversation.
            ev => self.conversation.apply(ev),
        }
        if was_following {
            self.view.follow = true;
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Indices of tool blocks that can be focused: collapsible blocks and
    /// pending diffs, in display order.
    fn focus_targets(&self) -> Vec<usize> {
        self.conversation
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.is_collapsible_tool() || m.is_pending_diff())
            .map(|(i, _)| i)
            .collect()
    }

    fn focused_pending_diff(&self) -> Option<usize> {
        self.conversation.focused.filter(|&i| {
            self.conversation
                .messages
                .get(i)
                .map(|m| m.is_pending_diff())
                .unwrap_or(false)
        })
    }

    fn last_pending_diff(&self) -> Option<usize> {
        self.conversation
            .messages
            .iter()
            .rposition(|m| m.is_pending_diff())
    }

    pub fn slash_matches(&self) -> Vec<(&'static str, &'static str)> {
        let q = self.composer.text.as_str();
        if !opens_slash_menu(q) {
            return Vec::new();
        }
        COMMANDS
            .iter()
            .filter_map(|spec| spec.slash.map(|slash| (slash, spec.desc)))
            .filter(|(cmd, _)| q == "/" || cmd.starts_with(q))
            .collect()
    }

    pub fn slash_open(&self) -> bool {
        matches!(self.modal, Modal::None) && !self.slash_matches().is_empty()
    }

    pub fn palette_items(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let q = match &self.modal {
            Modal::Palette { query, .. } => query.to_lowercase(),
            _ => String::new(),
        };
        COMMANDS
            .iter()
            .map(|spec| (spec.id, spec.label, spec.hint))
            .filter(|(_, label, _)| label.to_lowercase().contains(&q))
            .collect()
    }

    // ------------------------------------------------------------------
    // Actions
    // ------------------------------------------------------------------

    fn submit(&mut self, tx: &mpsc::UnboundedSender<Command>) {
        let text = self.composer.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        // Enter on an open slash menu executes the highlighted command.
        let slash = self.slash_matches();
        if text.starts_with('/') && !slash.is_empty() {
            let (cmd, _) = slash[self.slash_selected.min(slash.len() - 1)];
            self.composer.clear();
            self.input_history.push(text);
            self.history_index = None;
            self.history_draft.clear();
            self.run_slash(cmd, tx);
            return;
        }
        self.conversation.assistant_open = false;
        self.conversation.messages.push(Message::User(text.clone()));
        let _ = tx.send(Command::SendMessage(text.clone()));
        self.input_history.push(text);
        self.history_index = None;
        self.history_draft.clear();
        self.composer.clear();
        self.view.follow = true;
    }

    /// Whether the history entry at `index` is normal text that ↑/↓ may
    /// recall. Slash commands are skipped so recalling one cannot reopen the
    /// slash menu and hijack the history keys. Ported from the reference
    /// `InputBox::history_up`.
    fn recallable(&self, index: usize) -> bool {
        self.input_history
            .get(index)
            .is_some_and(|entry| !opens_slash_menu(entry))
    }

    /// Recall the previous (older) normal-text history entry, saving the
    /// in-progress text as the draft restored by [`Self::history_down`].
    pub fn history_up(&mut self) {
        let from = match self.history_index {
            None => self.input_history.len(),
            Some(0) => return,
            Some(i) => i,
        };
        let Some(new_index) = (0..from).rev().find(|&i| self.recallable(i)) else {
            return; // no older normal-text entry to recall
        };
        if self.history_index.is_none() {
            self.history_draft = self.composer.text.clone();
        }
        self.history_index = Some(new_index);
        let entry = self
            .input_history
            .get(new_index)
            .expect("index found by the recallable scan")
            .to_string();
        self.composer.set_text(entry);
    }

    /// Recall the next (newer) normal-text history entry; ↓ past the newest
    /// restores the draft saved on entry.
    pub fn history_down(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        match (i + 1..self.input_history.len()).find(|&j| self.recallable(j)) {
            Some(new_index) => {
                self.history_index = Some(new_index);
                let entry = self
                    .input_history
                    .get(new_index)
                    .expect("index found by the recallable scan")
                    .to_string();
                self.composer.set_text(entry);
            }
            None => {
                self.history_index = None;
                let draft = std::mem::take(&mut self.history_draft);
                self.composer.set_text(draft);
            }
        }
    }

    fn reset_conversation(&mut self) {
        self.conversation.messages.clear();
        self.conversation.collapsed.clear();
        self.conversation.expanded_bodies.clear();
        self.conversation.focused = None;
        self.view.hover_tool = None;
        self.view.pending_click = None;
        self.view.tool_regions.clear();
        self.view.notice_regions.clear();
        self.modal = Modal::None;
        self.view.scroll = ScrollPos::default();
        self.view.segments.clear();
        self.view.follow = true;
    }
}

/// Aggregate `cost.jsonl` for the `/stats` overlay. A missing state dir or
/// ledger reads as an empty table ("no runs recorded").
fn load_stats() -> StatsView {
    let empty = StatsView {
        empty: true,
        ..StatsView::default()
    };
    let Ok(dir) = crate::storage::StateDir::resolve() else {
        return empty;
    };
    let Ok(ledger) = crate::storage::stats::CostLedger::from_state_dir(&dir) else {
        return empty;
    };
    match ledger.summary() {
        Ok(summary) if summary.records > 0 => {
            let sessions = summary.session_count();
            let total_cost = summary.total_cost;
            let total_tokens = summary.total_tokens;
            StatsView {
                rows: summary
                    .by_model
                    .into_iter()
                    .map(|(model, cost, tokens)| {
                        // A $0 total on a spec the pricing table does not know
                        // means unpriced, not free — show "—" like `/usage`.
                        let cost = if cost == 0.0 && crate::usage::resolve_spec(&model).is_none() {
                            None
                        } else {
                            Some(cost)
                        };
                        UsageRow {
                            model,
                            tokens,
                            cost,
                        }
                    })
                    .collect(),
                total_cost,
                total_tokens,
                sessions,
                empty: false,
            }
        }
        _ => empty,
    }
}

/// Entries for the `/sessions` picker: newest first, filtered to this cwd
/// like the headless session lookup.
fn load_session_entries() -> Vec<SessionEntry> {
    let Ok(dir) = crate::storage::StateDir::resolve() else {
        return Vec::new();
    };
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    crate::headless::StoredSession::list(cwd.as_deref(), &dir)
        .unwrap_or_default()
        .into_iter()
        .map(|summary| SessionEntry {
            id: summary.id.as_str().to_owned(),
            title: summary.title,
            updated: rel_age(summary.updated_at),
        })
        .collect()
}

/// Relative age ("2h ago") of an epoch timestamp.
fn rel_age(epoch: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(epoch);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// (binding, action) rows of the `/help` sheet: the real key chords plus
/// the same command table the completion popup and palette derive from.
pub fn help_rows() -> Vec<(String, String)> {
    let mut rows = vec![
        ("ctrl+p".into(), "command palette".into()),
        ("ctrl+l".into(), "model menu".into()),
        ("ctrl+b".into(), "toggle context panel".into()),
        ("ctrl+f".into(), "cycle effort".into()),
        (
            "tab / shift+tab".into(),
            "focus next/previous tool card".into(),
        ),
        ("ctrl+y".into(), "approve pending edit".into()),
        ("ctrl+shift+y".into(), "approve pending edit, always".into()),
        ("ctrl+n".into(), "reject pending edit".into()),
        ("up / down".into(), "recall input history".into()),
        ("esc".into(), "close menu / interrupt the turn".into()),
        ("ctrl+c / ctrl+q".into(), "quit".into()),
        ("pgup / pgdn / g / G".into(), "scroll the transcript".into()),
        (String::new(), String::new()),
    ];
    for (slash, desc) in COMMANDS
        .iter()
        .filter_map(|spec| spec.slash.map(|slash| (slash, spec.desc)))
    {
        rows.push((slash.to_string(), desc.to_string()));
    }
    rows
}

impl App {
    /// Open the persisted-session picker (empty when no sessions exist).
    fn open_sessions(&mut self) {
        self.modal = Modal::Sessions {
            entries: load_session_entries(),
            selected: 0,
        };
    }

    /// Push a provider-style notice locally (used for confirmations of
    /// app-side actions like clipboard copies).
    fn push_notice(&mut self, tone: Tone, text: impl Into<String>) {
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
    fn run_command(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        match id {
            "new" => {
                self.reset_conversation();
                let _ = tx.send(Command::Reset);
            }
            "sessions" => self.open_sessions(),
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
            "compact" => {
                let _ = tx.send(Command::Compact);
            }
            "usage" => {
                self.modal = Modal::Usage(self.usage.clone());
                let _ = tx.send(Command::GetUsage);
            }
            "stats" => self.modal = Modal::Stats(load_stats()),
            "auto-review" => {
                let _ = tx.send(Command::ToggleAutoReview);
            }
            "help" => self.modal = Modal::Help,
            _ => {}
        }
    }

    fn run_slash(&mut self, cmd: &str, tx: &mpsc::UnboundedSender<Command>) {
        if let Some(spec) = COMMANDS.iter().find(|spec| spec.slash == Some(cmd)) {
            self.run_command(spec.id, tx);
        }
    }

    pub(crate) fn run_palette(&mut self, id: &str, tx: &mpsc::UnboundedSender<Command>) {
        self.run_command(id, tx);
    }

    fn approve(&mut self, idx: usize, tx: &mpsc::UnboundedSender<Command>, always: bool) {
        if let Some(Message::Tool { id, diff, .. }) = self.conversation.messages.get_mut(idx) {
            *diff = Some(DiffState::Approved);
            let _ = tx.send(Command::Approve {
                id: id.clone(),
                always,
            });
        }
        self.conversation.focused = None;
    }

    pub(crate) fn reject_confirmed(&mut self, tx: &mpsc::UnboundedSender<Command>, always: bool) {
        if let Modal::ConfirmReject(id) = std::mem::replace(&mut self.modal, Modal::None) {
            if let Some(Message::Tool { diff, .. }) = self
                .conversation
                .messages
                .iter_mut()
                .find(|m| matches!(m, Message::Tool { id: mid, .. } if *mid == id))
            {
                *diff = Some(DiffState::Rejected);
            }
            let _ = tx.send(Command::Reject { id, always });
        }
        self.conversation.focused = None;
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        let pos = if delta >= 0 {
            layout.advance(self.view.scroll, delta as u32)
        } else {
            layout.retreat(self.view.scroll, delta.unsigned_abs())
        };
        self.set_scroll_pos(pos);
    }

    pub fn scroll_to_top(&mut self) {
        self.set_scroll_pos(ScrollPos::default());
    }

    pub fn scroll_to_bottom(&mut self) {
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        // `bottom` (not `end`) so the viewport is exactly filled; `end`
        // would address rows past the last one and paint a blank screen.
        let target = if self.view.view_height > 0 {
            layout.bottom(self.view.view_height)
        } else {
            layout.end()
        };
        self.set_scroll_pos(target);
    }

    /// Applies a scroll position: clamped into the document, and re-pins
    /// follow once the viewport sits at the document bottom.
    fn set_scroll_pos(&mut self, pos: ScrollPos) {
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        let mut clamped = layout.clamp(pos);
        // Before the first frame (no viewport height) keep the follow flag
        // as-is: there is no bottom to be at yet. Otherwise, like the
        // flat-offset model before it, the viewport never scrolls past the
        // point where the document stops filling it.
        if self.view.view_height > 0 {
            let bottom = layout.bottom(self.view.view_height);
            clamped = clamped.min(bottom);
            self.view.follow = clamped >= bottom;
        }
        self.view.scroll = clamped;
        // Card rects move with the scroll; stale hover/click state is dropped.
        self.view.hover_tool = None;
        self.view.pending_click = None;
    }

    /// Collapsible tool card (message index) at a screen position, if any.
    pub fn tool_at(&self, row: u16, col: u16) -> Option<usize> {
        self.view
            .tool_regions
            .iter()
            .find(|(_, r)| rect_contains(*r, row, col))
            .map(|(i, _)| *i)
    }

    /// "Click to expand" notice row (message index) at a screen position.
    fn notice_at(&self, row: u16, col: u16) -> Option<usize> {
        self.view
            .notice_regions
            .iter()
            .find(|(_, r)| rect_contains(*r, row, col))
            .map(|(i, _)| *i)
    }

    /// Toggle a card body between truncated and fully expanded.
    fn toggle_body(&mut self, idx: usize) {
        if let Some(Message::Tool { id, .. }) = self.conversation.messages.get(idx) {
            let id = id.clone();
            if let Some(pos) = self
                .conversation
                .expanded_bodies
                .iter()
                .position(|c| c == &id)
            {
                self.conversation.expanded_bodies.remove(pos);
            } else {
                self.conversation.expanded_bodies.push(id);
            }
        }
    }

    fn toggle_tool(&mut self, idx: usize) {
        if let Some(Message::Tool { id, kind, .. }) = self.conversation.messages.get(idx)
            && kind.collapsible()
        {
            if let Some(pos) = self.conversation.collapsed.iter().position(|c| c == id) {
                self.conversation.collapsed.remove(pos);
            } else {
                self.conversation.collapsed.push(id.clone());
            }
        }
    }

    // ------------------------------------------------------------------
    // Mouse: wheel scroll + app-side text selection
    // ------------------------------------------------------------------

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            MouseEventKind::Moved => {
                self.view.hover_tool = self.tool_at(mouse.row, mouse.column);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A selection starts only inside a selectable region (chat
                // messages or the composer input) and is confined to it; a
                // click elsewhere (sidebar, chrome) just clears the highlight.
                let pos = (mouse.row, mouse.column);
                self.view.selection = [self.view.msg_area, self.view.composer_area]
                    .iter()
                    .copied()
                    .find(|r| rect_contains(*r, mouse.row, mouse.column))
                    .map(|region| Selection {
                        anchor: pos,
                        head: pos,
                        region,
                    });
                self.view.pending_click = self
                    .notice_at(mouse.row, mouse.column)
                    .map(PendingClick::Notice)
                    .or_else(|| {
                        self.tool_at(mouse.row, mouse.column)
                            .map(PendingClick::Card)
                    });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // A drag is a text selection, not a card press.
                self.view.pending_click = None;
                if let Some(sel) = &mut self.view.selection {
                    sel.head = clamp_to(sel.region, mouse.row, mouse.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let no_drag = self.view.selection.map(|s| s.is_empty()).unwrap_or(true);
                match (self.view.pending_click.take(), no_drag) {
                    // Press without drag on a card: toggle it. A notice-row
                    // press toggles only the body's truncation.
                    (Some(PendingClick::Card(idx)), true) => {
                        self.view.selection = None;
                        self.toggle_tool(idx);
                    }
                    (Some(PendingClick::Notice(idx)), true) => {
                        self.view.selection = None;
                        self.toggle_body(idx);
                    }
                    _ => self.copy_selection(),
                }
            }
            _ => {}
        }
    }

    /// Extract the selected text from the last rendered frame and copy it to
    /// the system clipboard. Tiny (single-cell) "selections" are treated as
    /// plain clicks and just clear the highlight.
    fn copy_selection(&mut self) {
        let Some(sel) = self.view.selection else {
            return;
        };
        if sel.is_empty() {
            self.view.selection = None;
            return;
        }
        let text = extract_selection_text(&self.view.frame_text, sel);
        self.view.selection = None;
        if !text.is_empty() {
            copy_to_clipboard(&text);
            self.flash("Copied");
        }
    }

    /// After focus changes, make sure the focused block is in view.
    fn ensure_focus_visible(&mut self) {
        let Some(i) = self.conversation.focused else {
            return;
        };
        let Some(&doc) = self.view.msg_starts.get(i) else {
            return;
        };
        let doc = doc as u32;
        let layout = Layout::new(&self.view.segments, self.view.view_width);
        let top = layout.doc_row(self.view.scroll);
        if doc < top || doc >= top + u32::from(self.view.view_height) {
            self.set_scroll_pos(layout.at_row(doc.saturating_sub(2)));
            // Like the flat-offset code before it: an explicit jump away
            // from the bottom must not be re-pinned by the next frame.
            self.view.follow = false;
        }
    }

    // ------------------------------------------------------------------
    // Key handling
    // ------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        // Any keypress dismisses a live flash toast.
        self.flash = None;
        // A modal owns the keyboard while open; its keys never fall through
        // to base chords (so ctrl+q does not quit under an open palette).
        if !matches!(self.modal, Modal::None) {
            self.handle_modal_key(key, tx);
            return;
        }
        self.handle_base_key(key, tx);
    }

    /// Keys for the normal (modal-free) surface: global chords, then
    /// navigation and composer editing.
    fn handle_base_key(&mut self, key: KeyEvent, tx: &mpsc::UnboundedSender<Command>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl {
            match key.code {
                KeyCode::Char('c') | KeyCode::Char('q') => {
                    if !self.composer.text.is_empty() {
                        self.composer.clear();
                    } else if self.busy() {
                        let _ = tx.send(Command::Interrupt);
                        self.interrupt_requested = true;
                    } else {
                        self.should_quit = true;
                    }
                    return;
                }
                KeyCode::Char('p') => {
                    self.modal = Modal::Palette {
                        query: String::new(),
                        selected: 0,
                    };
                    return;
                }
                KeyCode::Char('b') => {
                    self.session.sidebar_open = !self.session.sidebar_open;
                    return;
                }
                KeyCode::Char('l') => {
                    self.open_model_menu();
                    return;
                }
                KeyCode::Char('f') => {
                    self.session.effort_idx = (self.session.effort_idx + 1) % EFFORTS.len();
                    return;
                }
                KeyCode::Char('u') => {
                    self.scroll_by(-(self.view.view_height as i32 / 2).max(1));
                    return;
                }
                KeyCode::Char('d') => {
                    self.scroll_by((self.view.view_height as i32 / 2).max(1));
                    return;
                }
                KeyCode::Char('o') => {
                    self.toggle_focused();
                    return;
                }
                KeyCode::Char('Y') => {
                    if let Some(i) = self
                        .focused_pending_diff()
                        .or_else(|| self.last_pending_diff())
                    {
                        self.approve(i, tx, true);
                    }
                    return;
                }
                KeyCode::Char('y') => {
                    if let Some(i) = self
                        .focused_pending_diff()
                        .or_else(|| self.last_pending_diff())
                    {
                        self.approve(i, tx, false);
                    }
                    return;
                }
                KeyCode::Char('n') => {
                    if let Some(i) = self
                        .focused_pending_diff()
                        .or_else(|| self.last_pending_diff())
                        && let Message::Tool { id, .. } = &self.conversation.messages[i]
                    {
                        self.modal = Modal::ConfirmReject(id.clone());
                    }
                    return;
                }
                // Composer editing chords (reference `TextBuffer::handle_key`).
                // Effort cycling moved to Ctrl-F so Ctrl-E can be line-end;
                // with an empty composer it jumps the scrollback to bottom.
                KeyCode::Char('a') => {
                    self.composer.move_home();
                    return;
                }
                KeyCode::Char('e') => {
                    if self.composer.text.is_empty() {
                        self.scroll_to_bottom();
                    } else {
                        self.composer.move_end();
                    }
                    return;
                }
                KeyCode::Char('w') | KeyCode::Backspace => {
                    self.composer.delete_word_back();
                    return;
                }
                KeyCode::Char('k') => {
                    self.composer.kill_to_end_of_line();
                    return;
                }
                KeyCode::Delete => {
                    self.composer.delete_word_forward();
                    return;
                }
                KeyCode::Left => {
                    self.composer.move_word_left();
                    return;
                }
                KeyCode::Right => {
                    self.composer.move_word_right();
                    return;
                }
                _ => {}
            }
        }

        // Alt chords: word motions (Alt-←/→, Alt-b/f). Alt-O (editor) is
        // intercepted one level up, in the event loop, where the terminal
        // is reachable for the suspend/resume dance.
        if key.modifiers.contains(KeyModifiers::ALT)
            && !key.modifiers.contains(KeyModifiers::CONTROL)
        {
            match key.code {
                KeyCode::Left | KeyCode::Char('b') => {
                    self.composer.move_word_left();
                    return;
                }
                KeyCode::Right | KeyCode::Char('f') => {
                    self.composer.move_word_right();
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Esc => {
                // Close slash menu → clear focus → interrupt a running turn.
                if self.composer.text.starts_with('/') {
                    self.composer.clear();
                } else if self.conversation.focused.is_some() {
                    self.conversation.focused = None;
                } else if self.busy() {
                    let _ = tx.send(Command::Interrupt);
                }
            }
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            // The arrows never scroll the transcript (PageUp/Down, Ctrl-U/D,
            // and the wheel do that): on the first/last composer line they
            // drive the input history, elsewhere they move the cursor.
            KeyCode::Up => {
                if self.slash_open() {
                    self.slash_selected = self.slash_selected.saturating_sub(1);
                } else if !self.composer.cursor_on_first_line() {
                    self.composer.move_up();
                } else {
                    self.history_up();
                }
            }
            KeyCode::Down => {
                if self.slash_open() {
                    let max = self.slash_matches().len().saturating_sub(1);
                    self.slash_selected = (self.slash_selected + 1).min(max);
                } else if !self.composer.cursor_on_last_line() {
                    self.composer.move_down();
                } else {
                    self.history_down();
                }
            }
            KeyCode::PageUp => self.scroll_by(-(self.view.view_height as i32).max(1)),
            KeyCode::PageDown => self.scroll_by(self.view.view_height as i32),
            KeyCode::Home => self.scroll_to_top(),
            KeyCode::End => self.scroll_to_bottom(),
            KeyCode::Enter => {
                // Enter on a focused collapsible block (empty composer) toggles it.
                if self.composer.text.is_empty()
                    && self
                        .conversation
                        .focused
                        .map(|i| {
                            self.conversation
                                .messages
                                .get(i)
                                .map(|m| m.is_collapsible_tool())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false)
                {
                    self.toggle_focused();
                } else {
                    self.submit(tx);
                }
            }
            KeyCode::Char(c) => {
                // Vim-style scroll keys, only while the composer is empty so
                // typing a message never eats a character.
                if self.composer.text.is_empty() {
                    match c {
                        'g' => {
                            self.scroll_to_top();
                            return;
                        }
                        'G' => {
                            self.scroll_to_bottom();
                            return;
                        }
                        _ => {}
                    }
                }
                self.composer.insert_char(c);
                self.slash_selected = 0;
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                self.slash_selected = 0;
            }
            KeyCode::Left => self.composer.move_left(),
            KeyCode::Right => self.composer.move_right(),
            _ => {}
        }
    }

    /// Move focus to the next (dir = 1) / previous (dir = -1) focusable tool
    /// block, wrapping around.
    fn cycle_focus(&mut self, dir: i32) {
        let targets = self.focus_targets();
        if targets.is_empty() {
            return;
        }
        let len = targets.len() as i32;
        let next = match self.conversation.focused {
            None => {
                if dir > 0 {
                    targets[0]
                } else {
                    targets[len as usize - 1]
                }
            }
            Some(cur) => {
                let pos = targets.iter().position(|&t| t == cur).unwrap_or(0) as i32;
                targets[(pos + dir).rem_euclid(len) as usize]
            }
        };
        self.conversation.focused = Some(next);
        self.ensure_focus_visible();
    }

    fn toggle_focused(&mut self) {
        if let Some(i) = self.conversation.focused {
            self.toggle_tool(i);
        }
    }

    /// Paste lands in the composer as one unit; modal inputs own the
    /// keyboard so pastes are dropped while one is open.
    pub fn insert_paste(&mut self, text: &str) {
        if !matches!(self.modal, Modal::None) {
            return;
        }
        self.composer.insert_paste(text);
        self.slash_selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::provider::LineKind;
    use crate::tui::repaint::Cadence;
    use crate::tui::ui;
    use crate::tui::ui::scrollback::Layout;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
            let modal = !matches!(app.modal, Modal::None);
            let flipped = app.session.sidebar_open != sidebar;
            let noticed = matches!(
                app.conversation.messages.last(),
                Some(Message::Notice { .. })
            );
            assert!(
                sent || modal || flipped || noticed,
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

    /// W9: the help sheet lists every slash command the completion popup
    /// advertises (sourced from the same table, so it cannot lie).
    #[test]
    fn help_rows_cover_every_slash_command() {
        let rows = help_rows();
        for slash in COMMANDS.iter().filter_map(|s| s.slash) {
            assert!(
                rows.iter().any(|(key, _)| key == slash),
                "{slash} missing from the help sheet"
            );
        }
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

    /// Paint the app once so the renderer builds the segment document and
    /// resolves the viewport, as the real loop does before key handling.
    fn draw_app(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
    }

    fn scrolled_app() -> App {
        let mut app = App::new();
        for i in 0..30 {
            app.handle_event(AgentEvent::AssistantText(format!(
                "message number {i} with some wrapping text to fill rows"
            )));
        }
        app
    }

    /// While following, the viewport sits at the document bottom every
    /// frame, and streaming more content keeps it pinned there.
    #[test]
    fn follow_pins_the_viewport_to_the_bottom() {
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        assert!(app.view.follow);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            layout
                .total_rows()
                .saturating_sub(u32::from(app.view.view_height)),
            "follow shows the last viewport-height rows"
        );
        app.handle_event(AgentEvent::AssistantText("one more".into()));
        draw_app(&mut app, 80, 24);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            layout
                .total_rows()
                .saturating_sub(u32::from(app.view.view_height)),
            "new content keeps the bottom pinned"
        );
    }

    /// Scrolling up breaks follow; appending messages afterwards leaves
    /// the viewport where it was (append-stable addressable rows).
    #[test]
    fn scrolling_up_breaks_follow_and_appends_do_not_jump() {
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        app.scroll_by(-3);
        assert!(!app.view.follow, "scrolling up breaks follow");
        let before = {
            let layout = Layout::new(&app.view.segments, app.view.view_width);
            (app.view.scroll, layout.doc_row(app.view.scroll))
        };
        app.handle_event(AgentEvent::AssistantText("tail message".into()));
        draw_app(&mut app, 80, 24);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(app.view.scroll, before.0, "anchor position survives append");
        assert_eq!(
            layout.doc_row(app.view.scroll),
            before.1,
            "appended segments do not shift the viewport"
        );
        assert!(!app.view.follow);
    }

    /// The scroll position is width-independent: a resize keeps the anchor
    /// segment instead of jumping by a row delta.
    #[test]
    fn resize_keeps_the_anchor_segment() {
        let mut app = scrolled_app();
        draw_app(&mut app, 100, 24);
        app.scroll_by(-5);
        draw_app(&mut app, 100, 24);
        let anchored = app.view.scroll.seg;
        assert!(anchored > 0, "scrolled off the top of the document");
        draw_app(&mut app, 50, 24);
        assert_eq!(
            app.view.scroll.seg, anchored,
            "a resize is not a scroll: the anchor segment survives re-wrapping"
        );
    }

    /// The full keyboard scroll set: line, half page, page, top, bottom.
    #[test]
    fn keyboard_scroll_set_walks_and_clamps() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);

        // Top: Home and g.
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &tx);
        assert_eq!(app.view.scroll, ScrollPos::default());
        app.scroll_by(2);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
        assert_eq!(app.view.scroll, ScrollPos::default());

        // Bottom: End and G re-pin follow.
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &tx);
        assert!(app.view.follow);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll) + u32::from(app.view.view_height),
            layout.total_rows()
        );
        app.scroll_by(-2);
        assert!(!app.view.follow);
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &tx);
        assert!(app.view.follow, "G lands at the bottom and re-pins");

        // Line, half page, page: positions move by the expected row counts.
        // The arrows are history keys now, so the one-row step goes through
        // the public API instead of a keypress.
        app.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &tx);
        app.scroll_by(1);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(layout.doc_row(app.view.scroll), 1, "one row down");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            &tx,
        );
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            1 + u32::from(app.view.view_height) / 2,
            "Ctrl-D scrolls half a page"
        );
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &tx);
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll),
            1 + u32::from(app.view.view_height) / 2 + u32::from(app.view.view_height),
            "PageDown scrolls a full page"
        );

        // Scrolling past the top clamps at the document start.
        app.scroll_by(-9999);
        assert_eq!(app.view.scroll, ScrollPos::default());
    }

    /// g and G still type into the composer when it holds text.
    #[test]
    fn vim_scroll_keys_type_when_composer_has_text() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.insert_char('a');
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "agG");
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &tx);
    }

    /// Only the statuses that render the spinner glyph owe cadence frames;
    /// settled statuses sleep until a real event wakes the loop.
    #[test]
    fn cadence_tracks_animating_statuses() {
        let mut app = App::new();
        for status in [Status::Done, Status::Failed] {
            app.handle_event(AgentEvent::StatusChanged(status));
            assert_eq!(app.cadence(), Cadence::IDLE, "{status:?} is static");
        }
        for status in [Status::Thinking, Status::Running, Status::WaitingApproval] {
            app.handle_event(AgentEvent::StatusChanged(status));
            assert_eq!(app.cadence(), Cadence::SPINNER, "{status:?} animates");
        }
    }

    /// `/auto-review` routes a toggle command to the provider.
    #[test]
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

    /// Streamed deltas append to one bubble until explicitly closed.
    #[test]
    fn assistant_deltas_append_then_close() {
        let mut app = App::new();
        app.handle_event(AgentEvent::AssistantDelta("Hello".into()));
        app.handle_event(AgentEvent::AssistantDelta(", world".into()));
        app.handle_event(AgentEvent::AssistantEnd);
        app.handle_event(AgentEvent::AssistantDelta("Again".into()));
        let texts: Vec<&str> = app
            .conversation
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
        let mut app = App::new();
        app.handle_event(AgentEvent::ReasoningDelta("considering ".into()));
        app.handle_event(AgentEvent::ReasoningDelta("options".into()));
        app.handle_event(AgentEvent::AssistantDelta("Answer".into()));
        let contents: Vec<(&str, &str)> = app
            .conversation
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
        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Read {
                path: "src/a.rs".into(),
                summary: String::new(),
            },
            lines: vec![],
            awaiting_approval: false,
        }));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
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
        }));
        assert_eq!(app.conversation.messages.len(), 1);
        match &app.conversation.messages[0] {
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
        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            kind: ToolKind::Bash {
                cmd: "cargo test".into(),
            },
            lines: vec![],
            awaiting_approval: false,
        }));
        app.handle_event(AgentEvent::AutoReview {
            id: "t1".into(),
            tone: Tone::Success,
            text: "auto-review allow: low — in-project".into(),
        });
        // The result merge updates the card body without dropping the review.
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
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
        }));
        match &app.conversation.messages[0] {
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

    #[test]
    fn paste_ignored_when_modal_open() {
        let mut app = App::new();
        app.modal = Modal::Palette {
            query: "x".into(),
            selected: 0,
        };
        app.insert_paste("nope");
        assert!(app.composer.text.is_empty());
    }

    /// Modals are exclusive by construction: running the palette's "model"
    /// item replaces the palette with the model menu instead of stacking.
    #[test]
    fn model_menu_replaces_open_palette() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Palette {
            query: "mo".into(),
            selected: 0,
        };
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::ModelMenu(_)));
    }

    /// Esc dismisses the command palette without running a command.
    #[test]
    fn esc_closes_the_command_palette() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.modal = Modal::Palette {
            query: "mo".into(),
            selected: 0,
        };
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
    }

    fn mouse(kind: MouseEventKind, row: u16, col: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn app_with_collapsible_tool() -> App {
        let mut app = App::new();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            awaiting_approval: false,
            id: "r1".into(),
            kind: ToolKind::Read {
                path: "src/x.ts".into(),
                summary: "10 lines".into(),
            },
            lines: vec![],
        }));
        // Card spans rows 2..6, columns 2..40 (as the renderer would report).
        app.view.tool_regions = vec![(
            0,
            Rect {
                x: 2,
                y: 2,
                width: 38,
                height: 4,
            },
        )];
        assert!(app.conversation.collapsed.contains(&"r1".to_string()));
        app
    }

    #[test]
    fn hover_tracks_tool_card() {
        let mut app = app_with_collapsible_tool();
        app.handle_mouse(mouse(MouseEventKind::Moved, 3, 10));
        assert_eq!(app.view.hover_tool, Some(0));
        app.handle_mouse(mouse(MouseEventKind::Moved, 3, 80));
        assert_eq!(app.view.hover_tool, None);
    }

    #[test]
    fn click_toggles_collapsible_card() {
        let mut app = app_with_collapsible_tool();
        let down = mouse(MouseEventKind::Down(MouseButton::Left), 3, 10);
        let up = mouse(MouseEventKind::Up(MouseButton::Left), 3, 10);
        app.handle_mouse(down);
        app.handle_mouse(up);
        assert!(
            !app.conversation.collapsed.contains(&"r1".to_string()),
            "press expands"
        );
        app.handle_mouse(down);
        app.handle_mouse(up);
        assert!(
            app.conversation.collapsed.contains(&"r1".to_string()),
            "press again collapses"
        );
    }

    #[test]
    fn drag_selects_instead_of_toggling() {
        let mut app = app_with_collapsible_tool();
        app.view.msg_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 3, 10));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 4, 20));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 20));
        // Still collapsed: the drag became a selection, not a card press.
        assert!(app.conversation.collapsed.contains(&"r1".to_string()));
    }

    /// A long bash body truncates with a notice row; clicking the notice
    /// expands the body in place, and clicking the fold-back notice that
    /// replaces it re-truncates. The card's own collapse is untouched.
    #[test]
    fn click_notice_row_expands_and_retruncates_body() {
        let mut app = App::new();
        let lines: Vec<ToolLine> = (0..60)
            .map(|i| ToolLine {
                kind: LineKind::Context,
                text: format!("out {i}"),
                ..Default::default()
            })
            .collect();
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "b1".into(),
            kind: ToolKind::Bash { cmd: "make".into() },
            lines,
            awaiting_approval: false,
        }));
        draw_app(&mut app, 80, 24);
        let press = |app: &mut App, row, col| {
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), row, col));
            app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), row, col));
        };
        let notice = |app: &App| app.view.notice_regions.first().copied();
        let (idx, rect) = notice(&app).expect("truncated body carries a notice");
        press(&mut app, rect.y, rect.x + 4);
        assert!(
            app.conversation.expanded_bodies.iter().any(|c| c == "b1"),
            "notice press expands the body"
        );
        assert!(
            app.conversation.collapsed.is_empty(),
            "card collapse untouched"
        );
        draw_app(&mut app, 80, 24);
        let (_, rect) = notice(&app).expect("expanded body carries a fold-back notice");
        press(&mut app, rect.y, rect.x + 4);
        assert!(
            app.conversation.expanded_bodies.is_empty(),
            "fold-back press re-truncates"
        );
        let _ = idx;
    }

    fn usage_rows() -> Vec<UsageRow> {
        vec![
            UsageRow {
                model: "anthropic/claude-sonnet-5".into(),
                tokens: 12_345,
                cost: Some(0.0123),
            },
            UsageRow {
                model: "mock/free-model".into(),
                tokens: 500,
                cost: None,
            },
        ]
    }

    #[test]
    fn usage_slash_opens_the_session_overlay() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.handle_event(AgentEvent::UsageSnapshot(usage_rows()));
        app.run_slash("/usage", &tx);
        assert!(matches!(&app.modal, Modal::Usage(rows) if rows.len() == 2));
        assert!(
            matches!(rx.try_recv(), Ok(Command::GetUsage)),
            "/usage refreshes the snapshot from the provider"
        );
        // Any key dismisses the read-only overlay.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
    }

    #[test]
    fn stats_slash_opens_the_ledger_overlay() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.run_slash("/stats", &tx);
        assert!(matches!(app.modal, Modal::Stats(_)));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.modal, Modal::None));
    }

    /// A fresh snapshot while `/usage` is open replaces the overlay's rows.
    #[test]
    fn usage_snapshot_refreshes_open_overlay() {
        let mut app = App::new();
        app.run_slash("/usage", &mpsc::unbounded_channel().0);
        app.handle_event(AgentEvent::UsageSnapshot(usage_rows()));
        assert!(matches!(&app.modal, Modal::Usage(rows) if rows.len() == 2));
    }

    /// The ported composer chords: Ctrl-W deletes a word, Ctrl-K kills to the
    /// end of the line, Alt-Left moves back a word.
    #[test]
    fn composer_chords_edit_words_and_lines() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("foo bar".into());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.text, "foo ");

        app.composer.set_text("keep\nkill this".into());
        app.composer.cursor = "keep\n".chars().count();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.text, "keep\n");

        app.composer.set_text("one two".into());
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT), &tx);
        assert_eq!(app.composer.cursor, 4);
        app.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT), &tx);
        assert_eq!(app.composer.cursor, 7);
    }

    /// ↑ recalls older entries, clamps at the oldest, ↓ walks back toward the
    /// newest and restores the in-progress draft past it.
    #[test]
    fn history_recall_navigates_and_restores_draft() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.input_history.push("first".into());
        app.input_history.push("second".into());
        app.composer.set_text("draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "second");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "first");
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "first", "clamped at the oldest entry");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "second");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "draft", "draft restored past newest");
        assert!(app.history_index.is_none());
    }

    /// Slash commands are recorded in history but ↑/↓ skip over them, so
    /// recalling one can never reopen the slash menu and trap the arrows.
    #[test]
    fn history_recall_skips_slash_commands() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.input_history.push("old text".into());
        app.input_history.push("/help".into());
        app.input_history.push("new text".into());
        app.composer.set_text("draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "new text");
        assert!(!app.slash_open(), "slash menu stays closed while recalling");

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "old text", "skipped over /help");
        assert!(!app.slash_open());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "old text", "clamped at the oldest entry");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "new text", "skipped over /help");
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "draft", "draft restored past newest");
        assert!(app.history_index.is_none());
    }

    /// A slash command as the newest entry is never recalled; ↑ stays put and
    /// the menu does not open.
    #[test]
    fn history_up_ignores_a_newest_slash_command() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.input_history.push("/stats".into());
        app.composer.set_text("draft".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "draft", "nothing but slash commands");
        assert!(app.history_index.is_none());
        assert!(!app.slash_open());
    }

    /// With an empty composer, ↑/↓ drive the input history straight away —
    /// the arrows are not scrollback keys.
    #[test]
    fn arrows_drive_history_from_an_empty_composer() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        app.scroll_by(-3);
        assert!(!app.view.follow);
        app.input_history.push("earlier".into());

        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "earlier");
        assert!(!app.view.follow, "Up recalled history instead of scrolling");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.composer.text, "", "draft (empty) restored past newest");
        assert!(app.history_index.is_none());
    }

    /// Ctrl-E: line-end while the composer holds text; with an empty
    /// composer it jumps the scrollback to the bottom and re-pins follow.
    #[test]
    fn ctrl_e_is_both_line_end_and_jump_to_bottom() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("hello".into());
        app.composer.cursor = 0;
        app.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.cursor, 5, "line-end with text");

        let mut app = scrolled_app();
        draw_app(&mut app, 80, 24);
        app.scroll_by(-5);
        assert!(!app.view.follow, "scrolled off the bottom");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.view.follow, "Ctrl-E on empty composer jumps to bottom");
        let layout = Layout::new(&app.view.segments, app.view.view_width);
        assert_eq!(
            layout.doc_row(app.view.scroll) + u32::from(app.view.view_height),
            layout.total_rows()
        );
    }

    /// Submitting a message records it in the rolling input history.
    #[test]
    fn submit_records_history() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("hello world".into());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(app.input_history.len(), 1);
        assert_eq!(app.input_history.get(0), Some("hello world"));
        assert!(app.composer.text.is_empty());
    }

    /// Ctrl-C tri-state (reference `handle_ctrl` Quit branch): text first,
    /// then the running turn, then the app.
    #[test]
    fn ctrl_c_tri_state_clears_then_cancels_then_quits() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("draft".into());
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.composer.text.is_empty(), "first press clears input");
        assert!(!app.should_quit);
        assert!(rx.try_recv().is_err(), "no command sent while text present");

        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(
            matches!(rx.try_recv(), Ok(Command::Interrupt)),
            "second press cancels the running turn"
        );
        assert!(!app.should_quit);

        app.handle_event(AgentEvent::StatusChanged(Status::Done));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.should_quit, "idle press quits");
    }

    /// Effort lives on Ctrl-F; Ctrl-E moves to the end of the line.
    #[test]
    fn ctrl_f_cycles_effort_and_ctrl_e_moves_to_line_end() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut app = App::new();
        app.composer.set_text("hello".into());
        app.composer.cursor = 0;
        let before = app.session.effort_idx;
        app.handle_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.session.effort_idx, (before + 1) % EFFORTS.len());
        assert_eq!(app.composer.text, "hello", "ctrl-f does not type");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.composer.cursor, 5);
    }

    fn screen_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|r| {
                (0..buf.area.width)
                    .map(|c| buf[(c, r)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The status row gains the cwd (abbreviated to its last component) and
    /// the turn's elapsed-seconds prompt progress.
    #[test]
    fn status_row_shows_cwd_and_elapsed_seconds() {
        let mut app = App::new();
        app.session.sidebar_open = false;
        app.handle_event(AgentEvent::SessionInfo {
            cwd: "/Users/x/Projects/craft-code".into(),
            branch: "main".into(),
        });
        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        let text = screen_text(&mut app, 120, 24);
        assert!(
            text.contains("…/craft-code"),
            "cwd segment missing:\n{text}"
        );
        assert!(text.contains("running 0s"), "elapsed missing:\n{text}");
    }

    /// A flash toast takes over the status row's right side until it
    /// expires; a keypress clears it early.
    #[test]
    fn flash_toast_shows_then_expires() {
        let mut app = App::new();
        app.flash("Copied");
        let text = screen_text(&mut app, 120, 24);
        assert!(text.contains("Copied"), "flash missing:\n{text}");

        // Deadline already past: the draw path drops it.
        app.flash = Some((
            "Copied".into(),
            std::time::Instant::now() - FLASH_TTL - std::time::Duration::from_secs(1),
        ));
        let text = screen_text(&mut app, 120, 24);
        assert!(!text.contains("Copied"), "expired flash lingers:\n{text}");

        // A live flash is also dismissed by any keypress.
        app.flash("Copied");
        let (tx, _rx) = mpsc::unbounded_channel();
        app.handle_key(
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Char('a')),
            &tx,
        );
        let text = screen_text(&mut app, 120, 24);
        assert!(
            !text.contains("Copied"),
            "keypress must clear the flash:\n{text}"
        );
    }
}
