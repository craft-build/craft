//! Layout composition: chat column (messages / composer / status / footer)
//! plus the optional right sidebar, with overlays on top.

mod composer;
mod messages;
mod overlays;
mod sidebar;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};

use crate::tui::app::App;

const SIDEBAR_WIDTH: u16 = 34;

pub fn draw(f: &mut Frame, app: &mut App) {
    app.status_tick = app.status_tick.wrapping_add(1);
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(theme::BG_APP)),
        area,
    );

    let show_sidebar = app.session.sidebar_open && area.width >= 80;
    let (chat, side) = if show_sidebar {
        let cols = Layout::horizontal([Constraint::Min(60), Constraint::Length(SIDEBAR_WIDTH)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    // Composer height grows with wrapped content: box = text rows + 2 padding.
    let text_w = (chat.width as usize)
        .saturating_sub(2 + composer::TEXT_LEFT_PAD + composer::TEXT_RIGHT_PAD) // inset + inner pads
        .max(1);
    let row_count = messages::wrap_rows(&app.composer.text, text_w).len();
    let composer_h = row_count.min(composer::MAX_TEXT_ROWS) as u16 + 2;
    let bottom_h = composer_h + 3; // border(1) + box + status(1) + blank(1)

    let rows = Layout::vertical([Constraint::Min(3), Constraint::Length(bottom_h)]).split(chat);
    let msg_area = rows[0];
    let bottom = rows[1];

    // Shared top border across composer + status + footer.
    f.render_widget(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(theme::BORDER_SUBTLE)),
        bottom,
    );
    // Nested layout (instead of manual offsets) so tiny terminals that shrink
    // `bottom` below BOTTOM_HEIGHT can't produce out-of-bounds rows.
    let sub = Layout::vertical([
        Constraint::Length(1),          // border row
        Constraint::Length(composer_h), // composer
        Constraint::Length(1),          // status row
        Constraint::Length(1),          // blank row
    ])
    .split(bottom);
    let composer_area = sub[1];
    let status_area = sub[2];

    messages::render(f, app, msg_area);
    composer::render_input(f, app, composer_area);
    composer::render_status(f, app, status_area);

    if let Some(side) = side {
        sidebar::render(f, app, side);
    }

    // Overlays, back to front.
    overlays::render_slash(f, app, chat, bottom);
    overlays::render_model_menu(f, app, chat, bottom);
    overlays::render_palette(f, app, area);
    overlays::render_confirm(f, app, area);
    overlays::render_usage(f, app, area);
    overlays::render_stats(f, app, area);

    // Snapshot the frame as plain text (selection copy extracts from this),
    // then draw the current text selection as reversed cells.
    app.view.msg_area = msg_area;
    app.view.composer_area = composer_area;
    snapshot_frame(f, app);
    render_selection(f, app);
}

fn snapshot_frame(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let buf = f.buffer_mut();
    app.view.frame_text = (0..area.height)
        .map(|r| {
            (0..area.width)
                .map(|c| buf.cell((c, r)).map(|cell| cell.symbol()).unwrap_or(" "))
                .collect()
        })
        .collect();
}

fn render_selection(f: &mut Frame, app: &App) {
    let Some(sel) = &app.view.selection else {
        return;
    };
    if sel.is_empty() {
        return;
    }
    let ((r1, c1), (r2, c2)) = sel.normalized();
    let region = sel.region;
    let area = f.area();
    let buf = f.buffer_mut();
    // The highlight binds to real text: each row is clamped to its text
    // extent inside the selection's region, so padding, margins, the
    // sidebar, and decoration glyphs are never highlighted.
    for r in r1..=r2.min(area.height.saturating_sub(1)) {
        if r < region.y || r >= region.y + region.height {
            continue;
        }
        let Some(row) = app.view.frame_text.get(r as usize) else {
            continue;
        };
        let Some((first, last)) = crate::tui::selection::text_extent(row, region) else {
            continue;
        };
        let (first, last) = (first as u16, last as u16);
        let row_from = if r == r1 { c1 } else { region.x };
        let row_to = if r == r2 {
            c2
        } else {
            region.x + region.width - 1
        };
        let from = row_from.max(first);
        let to = row_to.min(last);
        for c in from..=to {
            if let Some(cell) = buf.cell_mut((c, r)) {
                cell.modifier.insert(ratatui::style::Modifier::REVERSED);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::App;
    use crate::tui::provider::{
        AgentEvent, LineKind, ModelChoice, PlanItem, Status, Tone, ToolCallData, ToolKind,
        ToolLine, TouchedFile,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn seeded_app() -> App {
        let mut app = App::new();
        app.handle_event(AgentEvent::PlanSet(vec![
            PlanItem {
                label: "Read the refresh token path".into(),
                done: true,
                active: false,
            },
            PlanItem {
                label: "Guard refreshToken() with a mutex".into(),
                done: false,
                active: true,
            },
        ]));
        app.handle_event(AgentEvent::FilesSet(vec![TouchedFile {
            path: "src/auth/refresh.ts".into(),
            status: "modified".into(),
            tone: Tone::Warning,
        }]));
        app.handle_event(AgentEvent::StatusChanged(Status::Done));
        app.handle_event(AgentEvent::TokenUsage("44.8K (4%)".into()));
        app.handle_event(AgentEvent::AssistantText(
            "Looking at the refresh path first.".into(),
        ));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t1".into(),
            awaiting_approval: false,
            kind: ToolKind::Read {
                path: "src/auth/refresh.ts".into(),
                summary: "38 lines".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Context,
                text: "export async function refreshToken() {".into(),
            }],
        }));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t2".into(),
            awaiting_approval: true,
            kind: ToolKind::Edit {
                path: "src/auth/refresh.ts".into(),
            },
            lines: vec![
                ToolLine {
                    kind: LineKind::Del,
                    text: "  session.token = res.token".into(),
                },
                ToolLine {
                    kind: LineKind::Add,
                    text: "  if (inflight) return inflight".into(),
                },
            ],
        }));
        app.handle_event(AgentEvent::ToolCall(ToolCallData {
            id: "t3".into(),
            awaiting_approval: false,
            kind: ToolKind::Bash {
                cmd: "pnpm test auth/refresh.spec.ts".into(),
            },
            lines: vec![ToolLine {
                kind: LineKind::Success,
                text: "✓ all 6 tests passed".into(),
            }],
        }));
        app.handle_event(AgentEvent::StatusChanged(Status::WaitingApproval));
        app
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn overlays_render_without_panic() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut app = seeded_app();

        app.modal = crate::tui::modals::Modal::Palette {
            query: "mo".into(),
            selected: 0,
        };
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("Change model"));

        app.modal = crate::tui::modals::Modal::ModelMenu(2);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Claude Opus 4.1"));

        app.modal = crate::tui::modals::Modal::None;
        app.composer.text = "/cl".into();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("/clear"));

        app.composer.clear();
        app.modal = crate::tui::modals::Modal::ConfirmReject("t2".into());
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Reject this diff?"));
        assert!(text.contains("src/auth/refresh.ts"));
    }

    #[test]
    fn usage_and_stats_overlays_render_rows_and_costs() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut app = seeded_app();
        app.handle_event(AgentEvent::UsageSnapshot(vec![
            crate::tui::provider::UsageRow {
                model: "anthropic/claude-sonnet-5".into(),
                tokens: 12_345,
                cost: Some(0.0123),
            },
            crate::tui::provider::UsageRow {
                model: "mock/free-model".into(),
                tokens: 500,
                cost: None,
            },
        ]));
        app.modal = crate::tui::modals::Modal::Usage(app.usage.clone());
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Session usage"));
        assert!(text.contains("claude-sonnet-5"));
        assert!(text.contains("$0.01"), "priced model shows its cost");
        assert!(text.contains("\u{2014}"), "unpriced model shows an em dash");

        app.modal = crate::tui::modals::Modal::Stats(crate::tui::modals::StatsView {
            rows: vec![crate::tui::provider::UsageRow {
                model: "claude-sonnet-5".into(),
                tokens: 100_000,
                cost: Some(1.5),
            }],
            total_cost: 1.5,
            total_tokens: 100_000,
            sessions: 3,
            empty: false,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Cost stats"));
        assert!(text.contains("$1.50"));
        assert!(text.contains("3 sessions"));

        app.modal = crate::tui::modals::Modal::Stats(crate::tui::modals::StatsView {
            empty: true,
            ..Default::default()
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("no runs recorded"));
    }

    /// A catalog larger than the space above the composer windows around the
    /// selection instead of painting over the chat and composer rows.
    #[test]
    fn model_menu_windows_around_selection_for_large_catalogs() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut app = seeded_app();
        let models: Vec<ModelChoice> = (0..30)
            .map(|i| ModelChoice {
                provider: "p".into(),
                model: format!("m{i:02}"),
                label: format!("m{i:02}"),
                provider_label: "p".into(),
            })
            .collect();
        app.handle_event(AgentEvent::CatalogSet {
            models,
            current: 25,
        });
        app.modal = crate::tui::modals::Modal::ModelMenu(25);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("m25"), "window keeps the selection visible");
        assert!(text.contains("m29"), "window extends to the catalog tail");
        assert!(!text.contains("m00"), "out-of-window rows are clipped");
        assert!(!text.contains("m01"), "out-of-window rows are clipped");
    }

    #[test]
    fn selection_highlights_cells() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut app = seeded_app();
        // Drag right-to-left across part of the first assistant line (row 1).
        let region = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 36,
        };
        app.view.selection = Some(crate::tui::selection::Selection {
            anchor: (1, 12),
            head: (1, 4),
            region,
        });
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        // Cells inside the range are reversed, outside are not.
        assert!(
            buf.cell((4, 1))
                .unwrap()
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(
            buf.cell((12, 1))
                .unwrap()
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(
            !buf.cell((13, 1))
                .unwrap()
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        // And the frame snapshot holds the selectable text.
        assert!(app.view.frame_text[1].contains("Looking at the refresh path"));
    }

    #[test]
    fn status_indicator_reflects_and_animates_with_agent_status() {
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
        let mut app = seeded_app();

        app.handle_event(AgentEvent::StatusChanged(Status::Running));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let first = buffer_text(&terminal);
        assert!(first.contains("running"));

        // The spinner frame advances with the tick counter.
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let second = buffer_text(&terminal);
        assert!(second.contains("running"));
        assert_ne!(first, second, "spinner frame changes between draws");

        app.handle_event(AgentEvent::StatusChanged(Status::WaitingApproval));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("awaiting approval"));

        app.handle_event(AgentEvent::StatusChanged(Status::Failed));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("failed"));
    }

    #[test]
    fn narrow_terminal_hides_sidebar() {
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        let mut app = seeded_app();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        // Should not panic and should not render the sidebar plan heading.
        assert!(!buffer_text(&terminal).contains("PLAN"));
    }
}
