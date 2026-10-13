//! Overlays: slash menu, model menu, command palette, confirm dialog.

use super::theme;
use crate::tui::style::Style;
use crate::tui::text::Span;

mod help;
mod mcp;
mod menus;
mod pickers;
mod search_file;
mod usage;

// ─────────────────────────────────────────────────────────────────────────
// The slt overlay stack
// ─────────────────────────────────────────────────────────────────────────

/// Render every open overlay, back to front, into the slt frame — the
/// same sheets and the same order as the overlay stack.
pub(crate) fn render_slt(ui: &mut slt::Context, app: &mut crate::tui::app::App) {
    search_file::render_search_slt(ui, app);
    search_file::render_file_picker_slt(ui, app);
    menus::render_slash_slt(ui, app);
    menus::render_model_menu_slt(ui, app);
    pickers::render_thinking_picker_slt(ui, app);
    menus::render_palette_slt(ui, app);
    usage::render_usage_slt(ui, app);
    usage::render_stats_slt(ui, app);
    help::render_help_slt(ui, app);
    pickers::render_sessions_slt(ui, app);
    pickers::render_theme_picker_slt(ui, app);
    mcp::render_mcp_slt(ui, app);
    pickers::render_task_picker_slt(ui, app);
    pickers::render_recipes_slt(ui, app);
}

/// Legacy `centered` math over the terminal rect: clamp, then halve the
/// slack. Returns `(x, y, width, height)`.
pub(super) fn centered_slt(width: u32, height: u32, sw: u32, sh: u32) -> (u32, u32, u32, u32) {
    let width = width.min(sw);
    let height = height.min(sh);
    ((sw - width) / 2, (sh - height) / 2, width, height)
}

/// One prebuilt sheet row: styled spans at `(dx, dy)` inside the sheet
/// box. Sheets position rows absolutely, exactly as the legacy
/// `Rect`-per-row renders did.
pub(super) struct SheetRow {
    pub dx: u32,
    pub dy: u32,
    pub spans: Vec<Span<'static>>,
}

/// Legacy `dim`: a full-screen `bg_sunken` fill that erases the UI
/// beneath. Painted as a trailing draw region — the transcript's own
/// deferred painters run after every container bg, so container-based
/// fills cannot cover them.
pub(super) fn dim_slt(ui: &mut slt::Context) {
    let sunken = theme::current().bg_sunken;
    let _ = ui.overlay(|ui| {
        ui.container().grow(1).draw(move |buf, _rect| {
            fill_slt(
                buf,
                buf.area.x,
                buf.area.y,
                buf.area.width,
                buf.area.height,
                sunken,
            );
        });
    });
}

/// Legacy `dim` + `boxed`: the dimmed modal with a raised-surface box
/// and its absolutely-positioned content rows.
pub(super) fn sheet_slt(
    ui: &mut slt::Context,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rows: Vec<SheetRow>,
) {
    let t = theme::current();
    let sunken = t.bg_sunken;
    let raised = t.bg_raised;
    let _ = ui.overlay(move |ui| {
        ui.container().grow(1).draw(move |buf, _rect| {
            fill_slt(
                buf,
                buf.area.x,
                buf.area.y,
                buf.area.width,
                buf.area.height,
                sunken,
            );
            paint_sheet_slt(buf, x, y, w, h, &rows, raised);
        });
    });
}

/// A raised-surface box at `(x, y)` with no dim — the legacy `Clear` +
/// `boxed()` combo of the composer-adjacent menus.
pub(super) fn boxed_slt(
    ui: &mut slt::Context,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rows: Vec<SheetRow>,
) {
    let raised = theme::current().bg_raised;
    let _ = ui.overlay(move |ui| {
        ui.container().grow(1).draw(move |buf, _rect| {
            paint_sheet_slt(buf, x, y, w, h, &rows, raised);
        });
    });
}

/// The box fill + content rows (shared by the dimmed and bare sheets).
fn paint_sheet_slt(
    buf: &mut slt::buffer::Buffer,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    rows: &[SheetRow],
    raised: crate::tui::style::Color,
) {
    fill_slt(buf, x, y, w, h, raised);
    for row in rows {
        paint_spans_slt(
            buf,
            x + row.dx,
            y + row.dy,
            w.saturating_sub(row.dx),
            &row.spans,
            raised,
        );
    }
}

/// Fill a rect with blank cells carrying `bg` (legacy `Clear` + bg Block).
fn fill_slt(
    buf: &mut slt::buffer::Buffer,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    bg: crate::tui::style::Color,
) {
    let style = Style::default().bg(bg);
    for row in y..y.saturating_add(h) {
        for col in x..x.saturating_add(w) {
            buf.set_string(col, row, " ", style);
        }
    }
}

/// Write one row of spans starting at `(x, y)`, clipped to `w` display
/// cells (ratatui Paragraph truncation: the row stops at the first char
/// that no longer fits). Spans without a bg carry the box surface, like
/// the parent-bg inheritance of container text.
fn paint_spans_slt(
    buf: &mut slt::buffer::Buffer,
    x: u32,
    y: u32,
    w: u32,
    spans: &[Span<'static>],
    parent_bg: crate::tui::style::Color,
) {
    let limit = x.saturating_add(w);
    let mut cx = x;
    'outer: for span in spans {
        let mut style = span.style;
        if style.bg.is_none() {
            style.bg = Some(parent_bg);
        }
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u32;
            if cx.saturating_add(cw) > limit || cx >= buf.area.width {
                break 'outer;
            }
            buf.set_string(cx, y, &ch.to_string(), style);
            cx += cw;
        }
    }
}

/// Legacy `render_rows` row build: pad the span list to `w` cells and
/// bake the selected/unselected bg into every span so the selected row
/// reads as one bar.
pub(super) fn row_spans(
    mut spans: Vec<Span<'static>>,
    selected: bool,
    w: usize,
) -> Vec<Span<'static>> {
    let t = theme::current();
    let bg = if selected { t.bg_overlay } else { t.bg_raised };
    let used: usize = spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if used < w {
        spans.push(Span::styled(" ".repeat(w - used), Style::default().bg(bg)));
    }
    spans
        .into_iter()
        .map(|s| {
            let style = if s.style.bg.is_none() {
                s.style.bg(bg)
            } else {
                s.style
            };
            Span::styled(s.content, style)
        })
        .collect()
}

/// Hard-clip a span list to `w` display cells — ratatui's Paragraph
/// truncation (the row simply stops at the first char that no longer
/// fits; no ellipsis). slt text nodes only clip at the buffer edge, so
/// sheet body rows clip explicitly.
pub(super) fn clip_spans<'a>(spans: Vec<Span<'a>>, w: usize) -> Vec<Span<'a>> {
    let mut out = Vec::with_capacity(spans.len());
    let mut used = 0usize;
    for span in spans {
        let mut run = String::new();
        for ch in span.content.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > w {
                if !run.is_empty() {
                    out.push(Span::styled(run, span.style));
                }
                return out;
            }
            used += cw;
            run.push(ch);
        }
        if !run.is_empty() {
            out.push(Span::styled(run, span.style));
        }
    }
    out
}
