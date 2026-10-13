//! Scrollable-sheet overlays: the shared sheet renderer plus the `/usage`
//! and `/stats` cost overlays.

use super::super::theme;
use crate::tui::app::App;
use crate::tui::modals::Modal;
use crate::tui::style::Style;
use crate::tui::text::{Line, Span};

/// One `model  tokens  cost` body line (styled like the old table rows).
fn usage_row_line(row: &crate::tui::provider::UsageRow, w: usize) -> Line<'static> {
    use crate::storage::stats::format_usd;
    use crate::usage::format_tokens;
    let label = format!(" {}", row.model);
    let tokens = format_tokens(row.tokens);
    let cost = match row.cost {
        Some(cost) => format_usd(cost),
        None => "—".to_string(),
    };
    let gap = w.saturating_sub(
        unicode_width::UnicodeWidthStr::width(label.as_str())
            + tokens
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum::<usize>()
            + cost
                .chars()
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum::<usize>()
            + 4,
    );
    Line::from(vec![
        Span::styled(label, Style::default().fg(theme::current().text_primary)),
        Span::raw(" ".repeat(gap)),
        Span::styled(
            format!("{tokens}  {cost}"),
            Style::default().fg(theme::current().text_secondary),
        ),
    ])
}

fn section_label(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {text}"),
        Style::default().fg(theme::current().text_tertiary),
    ))
}

fn format_reset(reset_at: u64) -> String {
    match jiff::Timestamp::from_millisecond(reset_at as i64) {
        // Local wall-clock time, matching the reference's local-time fallback.
        Ok(ts) => ts
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%m-%d %H:%M")
            .to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// The provider-quota section of `/usage`, one line per limit plus the
/// per-model today rows (F.5). Pure so layout can be unit-tested.
pub(crate) fn quota_lines(state: &crate::tui::provider::UsageFetchState) -> Vec<Line<'static>> {
    let t = theme::current();

    use crate::providers::{ModelUsageRow, ProviderUsage, UsageLimit};
    use crate::tui::provider::UsageFetchState;

    fn limit_line(limit: &UsageLimit) -> Line<'static> {
        let pct = match limit.percentage {
            Some(p) => format!(" {p}%"),
            None => String::new(),
        };
        let reset = limit
            .reset_at
            .map(|at| format!("  resets {}", format_reset(at)))
            .unwrap_or_default();
        let detail = limit
            .detail
            .as_deref()
            .map(|d| format!("  {d}"))
            .unwrap_or_default();
        let t = theme::current();
        Line::from(vec![
            Span::styled(
                format!("   {}", limit.label),
                Style::default().fg(t.text_primary),
            ),
            Span::styled(
                format!("{pct}{detail}{reset}"),
                Style::default().fg(t.text_secondary),
            ),
        ])
    }

    fn model_line(row: &ModelUsageRow) -> Line<'static> {
        let t = theme::current();
        Line::from(vec![
            Span::styled(
                format!("   {}", row.model),
                Style::default().fg(t.text_primary),
            ),
            Span::styled(
                format!(
                    "  {} today",
                    crate::storage::stats::format_usd(row.spend_microdollars as f64 / 1_000_000.0)
                ),
                Style::default().fg(t.text_secondary),
            ),
        ])
    }

    fn ready_lines(usage: &ProviderUsage) -> Vec<Line<'static>> {
        let plan = usage
            .plan
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        let mut lines = vec![section_label(&format!("Provider quota{plan}:"))];
        lines.extend(usage.limits.iter().map(limit_line));
        if !usage.by_model_today.is_empty() {
            lines.push(section_label(" By model (provider, today):"));
            lines.extend(usage.by_model_today.iter().map(model_line));
        }
        lines
    }

    let _ = ();
    match state {
        UsageFetchState::Idle => Vec::new(),
        UsageFetchState::Loading => vec![section_label("Provider quota: fetching…")],
        UsageFetchState::Unsupported => vec![section_label(
            "Provider quota: not available for this provider",
        )],
        UsageFetchState::Error(error) => vec![Line::from(Span::styled(
            format!(" Provider quota: {error}"),
            Style::default().fg(t.text_secondary),
        ))],
        UsageFetchState::Ready(usage) => ready_lines(usage),
    }
}

/// One `session  cost  tokens` row of the `/stats` top-sessions section.
fn session_line(entry: &(String, f64, u64)) -> Line<'static> {
    let t = theme::current();

    use crate::storage::stats::format_usd;
    use crate::usage::format_tokens;
    Line::from(vec![
        Span::styled(
            format!("   {}", entry.0.chars().take(8).collect::<String>()),
            Style::default().fg(t.text_primary),
        ),
        Span::styled(
            format!("  {}", format_usd(entry.1)),
            Style::default().fg(t.text_secondary),
        ),
        Span::styled(
            format!("  {}", format_tokens(entry.2)),
            Style::default().fg(t.text_secondary),
        ),
    ])
}

// ─────────────────────────────────────────────────────────────────────────
// The slt cost sheets
// ─────────────────────────────────────────────────────────────────────────

use super::{SheetRow, centered_slt, clip_spans, sheet_slt};

/// The shared scrollable sheet on the slt frame: dim, centered box, bold
/// title, a clipped body window, and the footer pinned to the last row
/// (legacy `render_usage_sheet` geometry). Returns `max_scroll`.
pub(super) fn render_usage_sheet_slt(
    ui: &mut slt::Context,
    title: &str,
    lines: &[Line<'static>],
    footer: Vec<Span<'static>>,
    scroll: usize,
) -> usize {
    let (sw, sh) = (ui.width(), ui.height());
    let height = (lines.len() as u32 + 4).min(sh);
    let width = 64.min(sw.saturating_sub(4));
    let (x, y, w, h) = centered_slt(width, height, sw, sh);

    let w_in = w.saturating_sub(2) as usize;
    // Title and footer claim two lines of the box; the body must fit in
    // the rest or it would spill over the dimmed chat on short terminals.
    let visible = h.saturating_sub(3) as usize;
    let scroll = scroll.min(lines.len().saturating_sub(visible.min(lines.len())));
    let max_scroll = lines.len().saturating_sub(visible.min(lines.len()));

    let mut rows = vec![SheetRow {
        dx: 1,
        dy: 1,
        spans: vec![super::pickers::title_span(title)],
    }];
    rows.extend(
        lines
            .iter()
            .skip(scroll)
            .take(visible)
            .enumerate()
            .map(|(i, line)| {
                // An empty Line still occupies its row in the legacy
                // sheet; a fully clipped row keeps one space so the
                // layout below is unaffected either way.
                let mut spans = clip_spans(line.spans.clone(), w_in);
                if spans.is_empty() {
                    spans.push(Span::raw(" "));
                }
                SheetRow {
                    dx: 1,
                    dy: 2 + i as u32,
                    spans,
                }
            }),
    );
    rows.push(SheetRow {
        dx: 1,
        dy: h.saturating_sub(1),
        spans: clip_spans(footer, w_in),
    });
    sheet_slt(ui, x, y, w, h, rows);
    max_scroll
}

/// `/usage` on the slt frame.
pub(super) fn render_usage_slt(ui: &mut slt::Context, app: &mut App) {
    let t = theme::current();

    let Modal::Usage(rows) = &app.overlays.modal else {
        return;
    };
    let w = 56usize; // body column width, as the old table used
    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            " no usage recorded yet",
            Style::default().fg(t.text_tertiary),
        )));
    } else {
        lines.push(section_label("Per model:"));
        lines.extend(rows.iter().map(|row| usage_row_line(row, w)));
    }
    let quota = quota_lines(&app.overlays.usage_quota);
    if !quota.is_empty() {
        lines.push(Line::from(""));
        lines.extend(quota);
    }
    let total_tokens: u64 = rows.iter().map(|r| r.tokens).sum();
    // Costs sum like the session ledger: `None` until a priced model shows up.
    let total_cost = rows.iter().filter_map(|r| r.cost).reduce(|a, b| a + b);
    let footer_cost = match total_cost {
        Some(cost) => crate::storage::stats::format_usd(cost),
        None => "—".to_string(),
    };
    let scroll = app.overlays.usage_scroll;
    app.overlays.usage_scroll_max = render_usage_sheet_slt(
        ui,
        "Session usage",
        &lines,
        vec![
            Span::styled(
                format!(" total {}", crate::usage::format_tokens(total_tokens)),
                Style::default().fg(t.text_secondary),
            ),
            Span::styled(
                format!("  {}", footer_cost),
                Style::default().fg(t.text_secondary),
            ),
            Span::styled(
                "  ctrl+r reload · esc to close",
                Style::default().fg(t.text_tertiary),
            ),
        ],
        scroll,
    );
}

/// `/stats` on the slt frame.
pub(super) fn render_stats_slt(ui: &mut slt::Context, app: &mut App) {
    let t = theme::current();

    let Modal::Stats(view) = &app.overlays.modal else {
        return;
    };
    if view.empty {
        app.overlays.usage_scroll_max = render_usage_sheet_slt(
            ui,
            "Cost stats",
            &[Line::from(Span::styled(
                " no runs recorded",
                Style::default().fg(t.text_tertiary),
            ))],
            vec![Span::styled(
                " esc to close",
                Style::default().fg(t.text_tertiary),
            )],
            0,
        );
        return;
    }
    let w = 56usize;
    let mut lines = vec![section_label("By model:")];
    lines.extend(view.rows.iter().map(|row| usage_row_line(row, w)));
    if view.models_overflow > 0 {
        lines.push(Line::from(Span::styled(
            format!(" +{} more models", view.models_overflow),
            Style::default().fg(t.text_tertiary),
        )));
    }
    if !view.by_session.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_label("Top sessions:"));
        lines.extend(view.by_session.iter().map(session_line));
    }
    let scroll = app.overlays.usage_scroll;
    app.overlays.usage_scroll_max = render_usage_sheet_slt(
        ui,
        "Cost stats",
        &lines,
        vec![
            Span::styled(
                format!(" total {}", crate::usage::format_tokens(view.total_tokens)),
                Style::default().fg(t.text_secondary),
            ),
            Span::styled(
                format!(
                    "  {}",
                    crate::storage::stats::format_total_cost(
                        view.total_cost,
                        view.unpriced_records
                    )
                ),
                Style::default().fg(t.text_secondary),
            ),
            Span::styled(
                format!("  {} sessions  esc to close", view.sessions),
                Style::default().fg(t.text_tertiary),
            ),
        ],
        scroll,
    );
}
