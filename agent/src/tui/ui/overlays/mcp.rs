//! `/mcp`: the MCP server status sheet.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::theme;
use super::{boxed, centered, dim, render_rows};
use crate::mcp::config::{McpServerInfo, McpServerStatus};
use crate::tui::app::App;
use crate::tui::modals::Modal;

/// `/mcp`: MCP server status screen (B.11). One row per server from the
/// live snapshot — name, transport, tool/prompt counts on the left, status
/// (color-coded) on the right — plus a detail line for the auth URL of a
/// NeedsAuth server or a failure reason.
pub fn render_mcp(f: &mut Frame, app: &App, area: Rect) {
    let Modal::Mcp { selected } = &app.overlays.modal else {
        return;
    };
    let reader = app
        .mcp
        .as_ref()
        .map(|handle| handle.reader())
        .unwrap_or_else(crate::mcp::McpSnapshotReader::empty);
    render_mcp_sheet(f, &reader, *selected, app.overlays.mcp_expanded, area);
}

/// The `/mcp` sheet itself, drawn from any snapshot reader so tests can
/// feed it a hand-built snapshot. One row per server — name, transport,
/// tool/prompt/resource counts on the left, status (color-coded) on the
/// right — plus a detail line carrying the auth URL of a NeedsAuth server,
/// and the expanded server's resources (name + uri) under its row.
fn render_mcp_sheet(
    f: &mut Frame,
    reader: &crate::mcp::McpSnapshotReader,
    selected: usize,
    expanded: Option<usize>,
    area: Rect,
) {
    let t = theme::current();
    dim(f, area);
    let snapshot = reader.load();
    let infos = snapshot.infos.clone();
    let resources = snapshot.resources.clone();

    // Row plan: every server's header row (the one the selection marks),
    // plus a detail row for NeedsAuth / Failed servers.
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut header_rows: Vec<usize> = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        header_rows.push(rows.len());
        let (status_label, status_color) = mcp_status_style(info);
        let left = format!(
            " {} · {} · {} tools · {} prompts · {} resources",
            info.name, info.transport_kind, info.tool_count, info.prompt_count, info.resource_count
        );
        let status = status_label.to_string();
        let gap = 76usize.saturating_sub(left.chars().count() + status.chars().count());
        rows.push(vec![
            Span::styled(left, Style::default().fg(t.text_primary)),
            Span::raw(" ".repeat(gap)),
            Span::styled(status, Style::default().fg(status_color)),
        ]);
        if let McpServerStatus::NeedsAuth { url } = &info.status
            && let Some(url) = url
        {
            rows.push(vec![Span::styled(
                format!("   login required: {url} (l to log in)"),
                Style::default().fg(t.danger),
            )]);
        }
        if expanded == Some(i) {
            let server_resources: Vec<_> =
                resources.iter().filter(|r| r.server == info.name).collect();
            if server_resources.is_empty() {
                rows.push(vec![Span::styled(
                    "   (no resources)".to_string(),
                    Style::default().fg(t.text_tertiary),
                )]);
            } else {
                for resource in server_resources {
                    rows.push(vec![Span::styled(
                        format!("   {} — {}", resource.name, resource.uri),
                        Style::default().fg(t.text_secondary),
                    )]);
                }
            }
        }
    }

    let width = 78.min(area.width.saturating_sub(4));
    // 16 not 10: an expanded resource list adds rows under its server.
    let n = rows.len().clamp(1, 16) as u16;
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "MCP servers — t toggle, r reconnect, enter resources, esc close",
            Style::default()
                .fg(t.text_primary)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: 1,
        },
    );
    if rows.is_empty() {
        rows.push(vec![Span::styled(
            " no MCP servers configured".to_string(),
            Style::default().fg(t.text_tertiary),
        )]);
    }
    render_rows(
        f,
        &rows,
        header_rows.get(selected).copied().unwrap_or(usize::MAX),
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}

/// Status label and theme color for one server row: Running rides success,
/// Connecting warning, Failed and NeedsAuth danger, Disabled the dim text.
fn mcp_status_style(info: &McpServerInfo) -> (&'static str, ratatui::style::Color) {
    let t = theme::current();
    match &info.status {
        McpServerStatus::Running => ("running", t.success),
        McpServerStatus::Connecting => ("connecting", t.warning),
        McpServerStatus::Disabled => ("disabled", t.text_disabled),
        McpServerStatus::Failed(_) => ("failed", t.danger),
        McpServerStatus::NeedsAuth { .. } => ("needs auth", t.danger),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::{McpServerInfo, McpServerStatus};
    use crate::mcp::{McpSnapshot, McpSnapshotReader};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn server_info(name: &str, status: McpServerStatus) -> McpServerInfo {
        McpServerInfo {
            name: name.into(),
            transport_kind: "stdio",
            tool_count: 3,
            prompt_count: 1,
            resource_count: 0,
            status,
            config_path: PathBuf::new(),
            url: None,
            oauth: None,
        }
    }

    /// B.11: the `/mcp` sheet renders one row per server from a snapshot —
    /// name, transport, counts, and every status spelling — plus the auth
    /// URL detail line for a NeedsAuth server.
    #[test]
    fn mcp_sheet_renders_rows_from_a_snapshot() {
        let reader = McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![
                server_info("running-srv", McpServerStatus::Running),
                server_info("connecting-srv", McpServerStatus::Connecting),
                server_info("disabled-srv", McpServerStatus::Disabled),
                server_info("failed-srv", McpServerStatus::Failed("spawn failed".into())),
                server_info(
                    "auth-srv",
                    McpServerStatus::NeedsAuth {
                        url: Some("https://auth.example/login".into()),
                    },
                ),
            ],
            prompts: Vec::new(),
            resources: Vec::new(),
            generation: 1,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_mcp_sheet(f, &reader, 0, None, f.area()))
            .unwrap();
        let text = buffer_text(&terminal);
        for needle in [
            "MCP servers",
            "running-srv",
            "running",
            "connecting-srv",
            "connecting",
            "disabled-srv",
            "disabled",
            "failed-srv",
            "failed",
            "needs auth",
            "https://auth.example/login",
            "3 tools",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from the sheet");
        }
    }

    /// Phase 5: the sheet shows the resource count, and Enter-expanded
    /// state renders the server's resources (name + uri) under its row.
    #[test]
    fn mcp_sheet_renders_expanded_resources() {
        let mut info = server_info("res-srv", McpServerStatus::Running);
        info.resource_count = 2;
        let reader = McpSnapshotReader::from_snapshot(McpSnapshot {
            infos: vec![info],
            prompts: Vec::new(),
            resources: vec![
                crate::mcp::McpResourceInfo {
                    server: "res-srv".into(),
                    uri: "file:///notes.txt".into(),
                    name: "notes".into(),
                    description: String::new(),
                    mime: Some("text/plain".into()),
                    size: Some(12),
                },
                crate::mcp::McpResourceInfo {
                    server: "res-srv".into(),
                    uri: "db://users".into(),
                    name: "users".into(),
                    description: String::new(),
                    mime: None,
                    size: None,
                },
            ],
            generation: 1,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_mcp_sheet(f, &reader, 0, Some(0), f.area()))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("2 resources"), "{text}");
        assert!(text.contains("notes — file:///notes.txt"), "{text}");
        assert!(text.contains("users — db://users"), "{text}");

        // Collapsed: the resource rows are absent but the count remains.
        terminal
            .draw(|f| render_mcp_sheet(f, &reader, 0, None, f.area()))
            .unwrap();
        let collapsed = buffer_text(&terminal);
        assert!(collapsed.contains("2 resources"));
        assert!(!collapsed.contains("file:///notes.txt"));
    }

    /// An empty snapshot (no servers configured) still renders the sheet
    /// with its placeholder row instead of panicking.
    #[test]
    fn mcp_sheet_renders_an_empty_placeholder() {
        let reader = McpSnapshotReader::empty();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_mcp_sheet(f, &reader, 0, None, f.area()))
            .unwrap();
        assert!(buffer_text(&terminal).contains("no MCP servers configured"));
    }
}
