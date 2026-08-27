use crate::components::ModalScroll;
use crate::components::Overlay;
use crate::components::keybindings::{ActionId, KeybindingResolver};
use crate::components::modal::Modal;
use crate::components::scrollbar::render_vertical_scrollbar;
use crate::theme;

use craft_storage::stats::{CostLedger, CostSummary, format_tokens, format_usd};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = " Cost & Usage ";
const MAX_MODEL_ROWS: usize = 12;
const MAX_SESSION_ROWS: usize = 8;

pub struct StatsModal {
    open: bool,
    summary: Option<CostSummary>,
    scroll: ModalScroll,
}

impl StatsModal {
    pub fn new() -> Self {
        Self {
            open: false,
            summary: None,
            scroll: ModalScroll::new_top(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn toggle(&mut self, ledger: &CostLedger) {
        self.open = !self.open;
        self.scroll.reset();
        if self.open {
            self.summary = match ledger.summary() {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to read cost ledger");
                    None
                }
            };
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.scroll.reset();
    }

    pub fn scroll(&mut self, delta: i32) {
        self.scroll.scroll(delta);
    }

    pub fn handle_key(&mut self, key_event: KeyEvent, resolver: &KeybindingResolver) -> bool {
        let close = key_event.code == KeyCode::Esc
            || resolver.matches(ActionId::Quit, key_event)
            || resolver.matches(ActionId::Help, key_event);
        if close {
            self.close();
            return true;
        }
        self.scroll.handle_key(key_event);
        true
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }

        let theme = theme::current();
        let mut lines: Vec<Line> = Vec::new();

        match &self.summary {
            None => {
                lines.push(Line::from(Span::styled(
                    "No cost data available.",
                    theme.keybind_desc,
                )));
            }
            Some(s) if s.records == 0 => {
                lines.push(Line::from(Span::styled(
                    "No usage recorded yet. Run a turn to accumulate cost.",
                    theme.keybind_desc,
                )));
            }
            Some(s) => render_summary(s, &theme, &mut lines),
        }

        let total = lines.len() as u16;
        let modal = Modal {
            title: TITLE,
            width_percent: 60,
            max_height_percent: 70,
        };
        let (popup, inner) = modal.render(frame, area, total);
        let viewport_h = inner.height;
        self.scroll.update_dimensions(total, viewport_h);
        let scroll = self.scroll.offset();

        let paragraph = Paragraph::new(lines).scroll((scroll, 0));
        frame.render_widget(paragraph, inner);

        if total > viewport_h {
            render_vertical_scrollbar(frame, inner, u32::from(total), u32::from(scroll));
        }

        popup
    }
}

fn render_summary(s: &CostSummary, theme: &crate::theme::Theme, lines: &mut Vec<Line>) {
    lines.push(Line::from(vec![
        Span::styled("Total cost: ", theme.keybind_desc),
        Span::styled(format_usd(s.total_cost), theme.keybind_key),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Total tokens: ", theme.keybind_desc),
        Span::styled(format_tokens(s.total_tokens), theme.keybind_key),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Records: ", theme.keybind_desc),
        Span::styled(
            format!("{} across {} sessions", s.records, s.session_count()),
            theme.keybind_key,
        ),
    ]));

    if !s.by_model.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  By model",
            theme.keybind_section,
        )));
        let label_w = s
            .by_model
            .iter()
            .take(MAX_MODEL_ROWS)
            .map(|(m, _, _)| UnicodeWidthStr::width(m.as_str()))
            .max()
            .unwrap_or(0)
            .max("model".len());
        lines.push(header_row(label_w));
        for (model, cost, tokens) in s.by_model.iter().take(MAX_MODEL_ROWS) {
            lines.push(model_row(model, *cost, *tokens, label_w, theme));
        }
        if s.by_model.len() > MAX_MODEL_ROWS {
            lines.push(Line::from(Span::styled(
                format!("  ...and {} more", s.by_model.len() - MAX_MODEL_ROWS),
                theme.keybind_desc,
            )));
        }
    }

    if !s.by_session.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Top sessions",
            theme.keybind_section,
        )));
        let label_w = "session".len();
        for (id, cost, tokens) in s.by_session.iter().take(MAX_SESSION_ROWS) {
            lines.push(session_row(id, *cost, *tokens, label_w, theme));
        }
    }
}

fn header_row(label_w: usize) -> Line<'static> {
    Line::from(vec![Span::raw(format!(
        "  {:width$}  cost       tokens",
        "model",
        width = label_w
    ))])
}

fn model_row(
    model: &str,
    cost: f64,
    tokens: u64,
    label_w: usize,
    theme: &crate::theme::Theme,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {:width$}  ", model, width = label_w),
            theme.keybind_key,
        ),
        Span::styled(format!("{:>9}  ", format_usd(cost)), theme.keybind_desc),
        Span::styled(format_tokens(tokens), theme.keybind_desc),
    ])
}

fn session_row(
    id: &str,
    cost: f64,
    tokens: u64,
    label_w: usize,
    theme: &crate::theme::Theme,
) -> Line<'static> {
    let short = short_id(id);
    Line::from(vec![
        Span::styled(
            format!("  {:width$}  ", short, width = label_w),
            theme.keybind_key,
        ),
        Span::styled(format!("{:>9}  ", format_usd(cost)), theme.keybind_desc),
        Span::styled(format_tokens(tokens), theme.keybind_desc),
    ])
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

impl Overlay for StatsModal {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key as key_ev;
    use crate::components::keybindings::key;
    use craft_storage::stats::{CostLedger, CostUsage, make_record};
    use crossterm::event::KeyCode;
    use test_case::test_case;

    fn default_resolver() -> KeybindingResolver {
        KeybindingResolver::new()
    }

    fn populated_ledger() -> (tempfile::TempDir, CostLedger) {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger = CostLedger::new(dir.path());
        ledger
            .append(&make_record(
                "s1",
                "anthropic/claude-sonnet-4",
                "anthropic",
                CostUsage {
                    input: 1000,
                    output: 500,
                    ..Default::default()
                },
                0.0123,
                false,
            ))
            .unwrap();
        (dir, ledger)
    }

    #[test]
    fn toggle_loads_summary() {
        let (_tmp, ledger) = populated_ledger();
        let mut modal = StatsModal::new();
        assert!(!modal.is_open());
        modal.toggle(&ledger);
        assert!(modal.is_open());
        let s = modal.summary.as_ref().expect("summary loaded");
        assert_eq!(s.records, 1);
    }

    #[test]
    fn close_resets_open_state() {
        let (_tmp, ledger) = populated_ledger();
        let mut modal = StatsModal::new();
        modal.toggle(&ledger);
        modal.close();
        assert!(!modal.is_open());
    }

    #[test_case(key_ev(KeyCode::Esc) ; "esc_closes")]
    #[test_case(key::QUIT.to_key_event() ; "ctrl_c_closes")]
    fn handle_key_closes(k: KeyEvent) {
        let (_tmp, ledger) = populated_ledger();
        let mut modal = StatsModal::new();
        modal.toggle(&ledger);
        assert!(modal.handle_key(k, &default_resolver()));
        assert!(!modal.is_open());
    }

    #[test]
    fn handle_key_consumes_all() {
        let (_tmp, ledger) = populated_ledger();
        let mut modal = StatsModal::new();
        modal.toggle(&ledger);
        assert!(modal.handle_key(key_ev(KeyCode::Char('a')), &default_resolver()));
        assert!(modal.is_open());
    }
}
