use std::collections::BTreeMap;

use crate::components::form::render_form;
use crate::components::hint_line;
use crate::components::keybindings::{ActionId, KeybindingResolver, key};
use crate::theme;

use craft_agent::{ThreadStatus, TurnType};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

const PANEL_LABEL: &str = " Flow ";

const DISMISS_KEYS: &str = if cfg!(target_os = "macos") {
    "⌃T/Esc"
} else {
    "Ctrl+T/Esc"
};
const HINT_PAIRS: &[(&str, &str)] = &[(DISMISS_KEYS, "dismiss"), ("↑/↓", "select chunk")];

// 2 borders + 1 header (stage) + 1 spacer + 1 hint bar. Chunk lines add to this.
const CHROME_LINES: u16 = 5;

/// One chunk row in the panel snapshot: its title (when known) plus lifecycle
/// status. The id remains the map key in [`FlowSnapshot::chunks`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowSnapshotChunk {
    pub title: String,
    pub status: ThreadStatus,
    pub stage: Option<TurnType>,
    pub order: usize,
    pub depends_on: Vec<String>,
}

/// A snapshot the panel renders. Owned so the panel is decoupled from the
/// `SessionState` borrow: the app hands the panel a fresh snapshot whenever
/// the workstream changes, then `view` reads from it.
#[derive(Debug, Clone, Default)]
pub struct FlowSnapshot {
    pub workstream_id: String,
    pub stage: Option<TurnType>,
    pub chunks: BTreeMap<String, FlowSnapshotChunk>,
    /// Max concurrent chunks from config. `> 1` makes the graph render a
    /// fan-out/fan-in parallel section after Plan; `1` renders a linear chain.
    /// Max concurrent chunks from config. The orchestrator uses this for its
    /// concurrency limit; the graph renders dependency edges directly from
    /// `depends_on` so it no longer reads this field, but it's kept on the
    /// snapshot for future use (e.g. visualizing the concurrency budget).
    #[allow(dead_code)]
    pub parallel_chunks: u32,
}

impl FlowSnapshot {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.workstream_id.is_empty()
    }
}

#[derive(Debug, PartialEq)]
pub enum FlowPanelAction {
    Consumed,
    Passthrough,
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visibility {
    Shown,
    Hidden,
    UserDismissed,
}

pub struct FlowPanel {
    visibility: Visibility,
    pub(crate) snapshot: FlowSnapshot,
    keybindings: std::sync::Arc<KeybindingResolver>,
    /// Ordered chunk ids for up/down selection. Recomputed on each render so
    /// newly-added chunks appear once the next snapshot lands.
    ordered_chunk_ids: Vec<String>,
    /// Index into `ordered_chunk_ids`, or `None` when nothing is selected.
    selected: Option<usize>,
}

impl FlowPanel {
    pub fn new() -> Self {
        Self {
            visibility: Visibility::Hidden,
            snapshot: FlowSnapshot::default(),
            keybindings: std::sync::Arc::new(KeybindingResolver::new()),
            ordered_chunk_ids: Vec::new(),
            selected: None,
        }
    }

    pub fn set_keybindings(&mut self, resolver: std::sync::Arc<KeybindingResolver>) {
        self.keybindings = resolver;
    }

    pub fn set_snapshot(&mut self, snapshot: FlowSnapshot) {
        self.snapshot = snapshot;
    }

    pub fn is_visible(&self) -> bool {
        self.visibility == Visibility::Shown
    }

    pub fn toggle(&mut self) {
        self.visibility = if self.is_visible() {
            Visibility::UserDismissed
        } else {
            Visibility::Shown
        };
    }

    pub fn hide(&mut self) {
        if self.is_visible() {
            self.visibility = Visibility::UserDismissed;
        }
    }

    /// Reveal the panel unless the user has explicitly dismissed it. Called
    /// when a flow run kicks off so the user sees live progress without having
    /// to toggle it on; a manual `hide()`/`toggle()` overrides this.
    pub fn show_if_not_dismissed(&mut self) {
        if self.visibility == Visibility::Hidden {
            self.visibility = Visibility::Shown;
        }
    }

    pub fn reset(&mut self) {
        self.visibility = Visibility::Hidden;
        self.snapshot = FlowSnapshot::default();
        self.ordered_chunk_ids.clear();
        self.selected = None;
    }

    pub fn height(&self) -> u16 {
        if !self.is_visible() {
            return 0;
        }
        let chunks = self.snapshot.chunks.len() as u16;
        CHROME_LINES + chunks.max(1)
    }

    pub fn hint_line(&self) -> Option<Line<'static>> {
        if self.visibility != Visibility::UserDismissed {
            return None;
        }
        let t = theme::current();
        Some(Line::from(vec![
            Span::styled(" Flow ", Style::new().fg(t.foreground)),
            Span::styled(key::PLAN_TOGGLE.label, t.keybind_key),
            Span::raw(" "),
        ]))
    }

    pub fn selected_chunk(&self) -> Option<&str> {
        let idx = self.selected?;
        self.ordered_chunk_ids.get(idx).map(String::as_str)
    }

    pub fn handle_key(&mut self, key_event: KeyEvent) -> FlowPanelAction {
        if self.keybindings.matches(ActionId::Quit, key_event)
            || key_event.code == KeyCode::Esc
            || self.keybindings.matches(ActionId::PlanToggle, key_event)
        {
            return FlowPanelAction::Hide;
        }
        match key_event.code {
            KeyCode::Tab => FlowPanelAction::Passthrough,
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                FlowPanelAction::Consumed
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                FlowPanelAction::Consumed
            }
            _ => FlowPanelAction::Consumed,
        }
    }

    fn move_selection(&mut self, delta: i32) {
        let n = self.ordered_chunk_ids.len();
        if n == 0 {
            self.selected = None;
            return;
        }
        let cur = self.selected.unwrap_or(0) as i32;
        let next = ((cur + delta).rem_euclid(n as i32)) as usize;
        self.selected = Some(next);
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        if !self.is_visible() {
            return;
        }
        // Sort chunk ids by plan order (not BTreeMap's alphabetical) so the
        // panel reflects the plan agent's intended execution sequence.
        let mut sorted: Vec<(&String, &FlowSnapshotChunk)> = self.snapshot.chunks.iter().collect();
        sorted.sort_by_key(|(_, c)| c.order);
        self.ordered_chunk_ids = sorted.iter().map(|(id, _)| (*id).clone()).collect();
        if let Some(idx) = self.selected
            && idx >= self.ordered_chunk_ids.len()
        {
            self.selected = None;
        }

        let t = theme::current();
        let mut lines: Vec<Line<'static>> = Vec::new();

        let stage_label = self.snapshot.stage.map(|s| s.as_str()).unwrap_or("idle");
        lines.push(Line::from(vec![
            Span::styled("workstream ", t.tool_dim),
            Span::styled(self.snapshot.workstream_id.clone(), t.foreground),
        ]));
        lines.push(Line::from(vec![
            Span::styled("stage ", t.tool_dim),
            Span::styled(stage_label, t.active),
        ]));
        lines.push(Line::default());

        if self.snapshot.chunks.is_empty() {
            lines.push(Line::from(Span::styled("no chunks yet", t.tool_dim)));
        } else {
            for (i, (id, chunk)) in sorted.into_iter().enumerate() {
                let (glyph, style) = status_style(chunk.status, &t);
                let label = if chunk.title.is_empty() {
                    id.clone()
                } else {
                    chunk.title.clone()
                };
                let marker = if self.selected == Some(i) {
                    "▸ "
                } else {
                    "  "
                };
                let mut spans = vec![Span::raw(marker), Span::styled(format!("{glyph} "), style)];
                let label_style = if self.selected == Some(i) {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new().fg(t.foreground)
                };
                spans.push(Span::styled(label, label_style));
                if !chunk.title.is_empty() && chunk.title != *id {
                    spans.push(Span::styled(format!("  {id}"), t.tool_dim));
                }
                if chunk.status == ThreadStatus::Running
                    && let Some(stage) = chunk.stage
                {
                    spans.push(Span::styled(format!("  [{}]", stage.as_str()), t.tool_dim));
                }
                lines.push(Line::from(spans));
            }
        }

        lines.push(Line::default());
        lines.push(hint_line(HINT_PAIRS));

        render_form(&t, PANEL_LABEL, frame, area, lines, (0, 0));
    }
}

fn status_style(status: ThreadStatus, t: &theme::Theme) -> (&'static str, Style) {
    match status {
        ThreadStatus::Queued => ("·", t.tool_dim),
        ThreadStatus::Running => ("▸", t.active),
        ThreadStatus::NeedsReview => ("?", Style::new().fg(t.mode_plan)),
        ThreadStatus::Blocked => ("✗", Style::new().fg(t.mode_bash)),
        ThreadStatus::Done => ("✓", t.active),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use test_case::test_case;

    fn panel_with_snapshot() -> FlowPanel {
        let mut p = FlowPanel::new();
        p.visibility = Visibility::Shown;
        let mut chunks = BTreeMap::new();
        chunks.insert(
            "chunk-1".into(),
            FlowSnapshotChunk {
                title: "Add auth".into(),
                status: ThreadStatus::Running,
                stage: Some(TurnType::Execute),
                ..Default::default()
            },
        );
        chunks.insert(
            "chunk-2".into(),
            FlowSnapshotChunk {
                title: String::new(),
                status: ThreadStatus::Done,
                stage: None,
                ..Default::default()
            },
        );
        p.snapshot = FlowSnapshot {
            workstream_id: "abc123".into(),
            stage: Some(TurnType::Execute),
            chunks,
            ..Default::default()
        };
        p
    }

    #[test]
    fn toggle_cycles_visibility() {
        let mut p = FlowPanel::new();
        assert!(!p.is_visible());
        p.toggle();
        assert!(p.is_visible());
        p.toggle();
        assert!(!p.is_visible());
    }

    #[test]
    fn hide_marks_user_dismissed() {
        let mut p = panel_with_snapshot();
        assert!(p.is_visible());
        p.hide();
        assert!(!p.is_visible());
        assert!(p.hint_line().is_some());
    }

    #[test]
    fn reset_clears_state() {
        let mut p = panel_with_snapshot();
        p.reset();
        assert!(!p.is_visible());
        assert!(p.snapshot.is_empty());
    }

    #[test]
    fn height_grows_with_chunks() {
        let mut p = FlowPanel::new();
        assert_eq!(p.height(), 0);
        p.visibility = Visibility::Shown;
        let empty_height = p.height();
        p.snapshot = FlowSnapshot {
            workstream_id: "w".into(),
            stage: None,
            chunks: [(
                String::from("c"),
                FlowSnapshotChunk {
                    title: String::new(),
                    status: ThreadStatus::Queued,
                    stage: None,
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        assert_eq!(p.height(), empty_height);
        p.snapshot.chunks.insert(
            "c2".into(),
            FlowSnapshotChunk {
                title: String::new(),
                status: ThreadStatus::Queued,
                stage: None,
                ..Default::default()
            },
        );
        assert_eq!(p.height(), empty_height + 1);
    }

    #[test_case(key(KeyCode::Esc) ; "esc")]
    #[test_case(key::QUIT.to_key_event() ; "ctrl_c")]
    #[test_case(key::PLAN_TOGGLE.to_key_event() ; "ctrl_t")]
    fn dismiss_keys(k: KeyEvent) {
        let mut p = panel_with_snapshot();
        assert_eq!(p.handle_key(k), FlowPanelAction::Hide);
    }

    #[test]
    fn tab_passes_through() {
        let mut p = panel_with_snapshot();
        assert_eq!(
            p.handle_key(key(KeyCode::Tab)),
            FlowPanelAction::Passthrough
        );
    }

    #[test]
    fn unknown_key_consumed() {
        let mut p = panel_with_snapshot();
        assert_eq!(
            p.handle_key(key(KeyCode::Char('x'))),
            FlowPanelAction::Consumed
        );
    }

    #[test]
    fn status_glyph_round_trips() {
        for s in [
            ThreadStatus::Queued,
            ThreadStatus::Running,
            ThreadStatus::NeedsReview,
            ThreadStatus::Blocked,
            ThreadStatus::Done,
        ] {
            assert_eq!(ThreadStatus::parse(&status_name(s)), Some(s));
        }
    }

    fn status_name(s: ThreadStatus) -> String {
        match s {
            ThreadStatus::Queued => "queued",
            ThreadStatus::Running => "running",
            ThreadStatus::NeedsReview => "needs_review",
            ThreadStatus::Blocked => "blocked",
            ThreadStatus::Done => "done",
        }
        .into()
    }
}
