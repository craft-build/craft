//! `/mcp`: the MCP server status sheet.

use super::super::theme;
use crate::mcp::config::{McpServerInfo, McpServerStatus};
use crate::tui::app::App;
use crate::tui::modals::Modal;
use crate::tui::style::{Color, Style};
use crate::tui::text::Span;

use super::{SheetRow, centered_slt, row_spans, sheet_slt};

/// `/mcp` on the slt frame.
pub(super) fn render_mcp_slt(ui: &mut slt::Context, app: &App) {
    let Modal::Mcp { selected } = &app.overlays.modal else {
        return;
    };
    let reader = app
        .mcp
        .as_ref()
        .map(|handle| handle.reader())
        .unwrap_or_else(crate::mcp::McpSnapshotReader::empty);
    render_mcp_sheet_slt(ui, &reader, *selected, app.overlays.mcp_expanded);
}

/// The `/mcp` sheet on the slt frame, drawn from any snapshot reader so
/// tests can feed it a hand-built snapshot.
fn render_mcp_sheet_slt(
    ui: &mut slt::Context,
    reader: &crate::mcp::McpSnapshotReader,
    selected: usize,
    expanded: Option<usize>,
) {
    let t = theme::current();
    let (sw, sh) = (ui.width(), ui.height());
    let snapshot = reader.load();
    let infos = snapshot.infos.clone();
    let resources = snapshot.resources.clone();

    // Row plan: every server's header row (the one the selection marks),
    // plus a detail row for NeedsAuth / Failed servers.
    let mut all_rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut header_rows: Vec<usize> = Vec::new();
    for (i, info) in infos.iter().enumerate() {
        header_rows.push(all_rows.len());
        let (status_label, status_color) = mcp_status_style(info);
        let left = format!(
            " {} · {} · {} tools · {} prompts · {} resources",
            info.name, info.transport_kind, info.tool_count, info.prompt_count, info.resource_count
        );
        let status = status_label.to_string();
        let gap = 76usize.saturating_sub(left.chars().count() + status.chars().count());
        all_rows.push(vec![
            Span::styled(left, Style::default().fg(t.text_primary)),
            Span::raw(" ".repeat(gap)),
            Span::styled(status, Style::default().fg(status_color)),
        ]);
        if let McpServerStatus::NeedsAuth { url } = &info.status
            && let Some(url) = url
        {
            all_rows.push(vec![Span::styled(
                format!("   login required: {url} (l to log in)"),
                Style::default().fg(t.danger),
            )]);
        }
        if expanded == Some(i) {
            let server_resources: Vec<_> =
                resources.iter().filter(|r| r.server == info.name).collect();
            if server_resources.is_empty() {
                all_rows.push(vec![Span::styled(
                    "   (no resources)".to_string(),
                    Style::default().fg(t.text_tertiary),
                )]);
            } else {
                for resource in server_resources {
                    all_rows.push(vec![Span::styled(
                        format!("   {} — {}", resource.name, resource.uri),
                        Style::default().fg(t.text_secondary),
                    )]);
                }
            }
        }
    }

    let width = 78.min(sw.saturating_sub(4));
    // 16 not 10: an expanded resource list adds rows under its server.
    let n = all_rows.len().clamp(1, 16) as u32;
    let (x, y, w, h) = centered_slt(width, n + 4, sw, sh);
    if all_rows.is_empty() {
        all_rows.push(vec![Span::styled(
            " no MCP servers configured".to_string(),
            Style::default().fg(t.text_tertiary),
        )]);
    }
    let content_w = w.saturating_sub(2) as usize;
    let sel_row = header_rows.get(selected).copied().unwrap_or(usize::MAX);
    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![super::pickers::title_span(
            "MCP servers — t toggle, r reconnect, enter resources, esc close",
        )],
    }];
    rows.extend(all_rows.iter().enumerate().map(|(i, spans)| SheetRow {
        dx: 1,
        dy: 2 + i as u32,
        spans: row_spans(spans.clone(), i == sel_row, content_w),
    }));
    sheet_slt(ui, x, y, w, h, rows);
}
/// Status label and theme color for one server row: Running rides success,
/// Connecting warning, Failed and NeedsAuth danger, Disabled the dim text.
fn mcp_status_style(info: &McpServerInfo) -> (&'static str, Color) {
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
    use std::path::PathBuf;

    /// Render the sheet on an slt TestBackend and return the joined rows.
    fn sheet_text(reader: &McpSnapshotReader, selected: usize, expanded: Option<usize>) -> String {
        let mut tb = slt::TestBackend::new(100, 30);
        tb.render(|ui| render_mcp_sheet_slt(ui, reader, selected, expanded));
        (0..tb.height()).map(|y| tb.line(y)).collect::<Vec<_>>().join("\n")
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
        let text = sheet_text(&reader, 0, None);
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
        let text = sheet_text(&reader, 0, Some(0));
        assert!(text.contains("2 resources"), "{text}");
        assert!(text.contains("notes — file:///notes.txt"), "{text}");
        assert!(text.contains("users — db://users"), "{text}");

        // Collapsed: the resource rows are absent but the count remains.
        let collapsed = sheet_text(&reader, 0, None);
        assert!(collapsed.contains("2 resources"));
        assert!(!collapsed.contains("file:///notes.txt"));
    }

    /// An empty snapshot (no servers configured) still renders the sheet
    /// with its placeholder row instead of panicking.
    #[test]
    fn mcp_sheet_renders_an_empty_placeholder() {
        let reader = McpSnapshotReader::empty();
        assert!(sheet_text(&reader, 0, None).contains("no MCP servers configured"));
    }
}
