//! Composer (input box), status bar and footer hint row.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme;
use crate::markdown::highlight::{Highlighter, SegmentColor};
use crate::tui::app::App;
use crate::tui::provider::Status;
use crate::tui::shell::parse_shell_prefix;
use crate::tui::style::{Modifier, Style, StyleExt};
use crate::tui::text::Span;

/// Max text rows the composer grows to before it scrolls instead of expanding.
pub const MAX_TEXT_ROWS: usize = 12;
/// Inner left/right padding of the text area (left includes the accent bar).
pub const TEXT_LEFT_PAD: usize = 3; // 1 pad + ▎ bar + 1 space
pub const TEXT_RIGHT_PAD: usize = 2;

/// Bang-prefix highlighting (task 96, ported from the reference's
/// `shell_highlight_spans`): a first line starting `!` / `!!` renders its
/// sigil in the theme's warning color and the command syntax-highlighted
/// as bash. `None` keeps plain rendering — no bang prefix, or the syntect
/// engine not warmed up yet.
fn shell_spans(line: &str, base: Style) -> Option<Vec<Span<'static>>> {
    if !crate::markdown::highlight::is_ready() {
        return None;
    }
    let parsed = parse_shell_prefix(line)?;
    let t = theme::current();
    let prefix_style = Style::default().fg(t.warning).bg(t.bg_surface);
    let mut spans = vec![Span::styled(
        line[..parsed.prefix_len].to_owned(),
        prefix_style,
    )];
    let mut hl = Highlighter::for_token("bash");
    for seg in hl.highlight_line(&line[parsed.prefix_len..]) {
        let mut style = base;
        if let Some(color) = seg_color(seg.fg) {
            style = style.fg(color);
        }
        if seg.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if seg.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        spans.push(Span::styled(seg.text, style));
    }
    Some(spans)
}

fn seg_color(c: SegmentColor) -> Option<crate::tui::style::Color> {
    match c {
        SegmentColor::Rgb(r, g, b) => Some(crate::tui::style::Color::Rgb(r, g, b)),
        SegmentColor::Ansi(i) => Some(crate::tui::style::Color::Indexed(i)),
        SegmentColor::Default => None,
    }
}

/// Spinner / status word with the running timer and interrupt hint.
fn status_indicator(app: &App) -> Vec<Span<'static>> {
    let t = theme::current();

    let tertiary = Style::default().fg(t.text_tertiary);
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    // Prompt-progress: how long the current turn has been running, so a
    // silent model reads as slow rather than stuck.
    let elapsed = app
        .turn_started
        .map(|at| at.elapsed().as_secs())
        .unwrap_or(0);
    match app.status {
        Status::Thinking | Status::Running | Status::WaitingApproval => {
            let frame = SPINNER[app.status_tick % SPINNER.len()];
            let word = match app.status {
                Status::Thinking => "thinking",
                Status::Running => "running",
                _ => "awaiting approval",
            };
            vec![
                Span::styled(format!("{frame} "), Style::default().fg(t.accent)),
                Span::styled(format!("{word} {elapsed}s  esc interrupt"), tertiary),
            ]
        }
        Status::Failed => vec![
            Span::styled("✖ ", Style::default().fg(t.danger)),
            Span::styled("failed  esc interrupt", tertiary),
        ],
        Status::Done => vec![Span::styled("········  esc interrupt", tertiary)],
    }
}

/// Abbreviate a cwd for the status row: keep the last two components, `…`
/// prefix when something was cut (reference `cwd_branch_label` shape).
fn abbreviated_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return "—".into();
    }
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    match parts.len() {
        0 => "/".into(),
        1 => parts[0].into(),
        _ => format!("…/{}", parts[parts.len() - 1]),
    }
}

/// Clip a span list to `max` display cells, ending with an ellipsis if cut.
fn truncate_spans<'a>(spans: Vec<Span<'a>>, max: usize) -> (Vec<Span<'a>>, usize) {
    let mut out: Vec<Span<'a>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let len = span.content.chars().count();
        if used + len <= max {
            used += len;
            out.push(span);
        } else {
            let remaining = max.saturating_sub(used + 1);
            let text: String = span.content.chars().take(remaining).collect();
            out.push(Span::styled(format!("{text}…"), span.style));
            used += remaining + 1;
            break;
        }
    }
    (out, used)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Style {
        let t = theme::current();
        Style::default().fg(t.text_primary).bg(t.bg_surface)
    }

    /// The prefix span carries the sigil in the warning color; the command
    /// follows as syntect-highlighted spans covering the rest of the line.
    #[test]
    fn bang_prefix_gets_sigil_style_and_bash_highlighting() {
        crate::markdown::highlight::syntax_set(); // arm is_ready()
        let spans = shell_spans("! cargo test", base()).expect("styled");
        let t = theme::current();
        assert_eq!(spans[0].content, "! ");
        assert_eq!(spans[0].style.fg, Some(t.warning));
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "! cargo test");
        assert!(spans.len() > 1, "the command is highlighted, not plain");
    }

    #[test]
    fn double_bang_prefix_is_styled_too() {
        crate::markdown::highlight::syntax_set();
        let spans = shell_spans("!! ls -la", base()).expect("styled");
        assert_eq!(spans[0].content, "!! ");
    }

    /// No sigil (or a bang mid-line) keeps the plain single-span path.
    #[test]
    fn plain_lines_and_mid_string_bangs_stay_unstyled() {
        crate::markdown::highlight::syntax_set();
        assert!(shell_spans("hello ! world", base()).is_none());
        assert!(shell_spans("just a message", base()).is_none());
        assert!(shell_spans("! ", base()).is_none(), "empty command");
    }

    /// The mirror keeps the legacy wrapped-row layout: a wide-glyph line
    /// wraps at the same cells `wrap_rows` picks, and the cursor maps to
    /// (row, char offset within that row).
    #[test]
    fn mirror_maps_text_and_cursor_onto_wrapped_rows() {
        let text = "好好好cd second";
        let rows = crate::tui::ui::messages::wrap_rows(text, 5);
        assert!(rows.len() > 1, "input wraps at width 5: {rows:?}");
        let mut state = slt::TextareaState::new();
        sync_mirror(&mut state, text, &rows, 3);
        assert_eq!(
            state.lines,
            rows.iter()
                .map(|&(s, e)| text.chars().skip(s).take(e - s).collect::<String>())
                .collect::<Vec<_>>(),
            "widget rows are exactly the legacy wrapped rows"
        );
        assert_eq!((state.cursor_row, state.cursor_col), (1, 1));
        // Edge cursor at the very end lands on the last row's end.
        sync_mirror(&mut state, text, &rows, text.chars().count());
        let last = rows.len() - 1;
        assert_eq!(state.cursor_row, last);
        assert_eq!(state.cursor_col, rows[last].1 - rows[last].0);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Wave 2a: the slt composer panel
// ─────────────────────────────────────────────────────────────────────────

/// Render the composer box + status + footer hint rows into the slt
/// frame's bottom block (the shared top border is drawn by `frame.rs`).
///
/// `avail` is frame.rs's `width - sidebar_width`, but the legacy column
/// layout (`[Min(60), Length(34)]`) keeps the chat at least 60 wide and
/// squeezes the sidebar instead — the composer recomputes that width so
/// narrow terminals (the 80-col fixture terminal → chat 60) match.
///
/// Wave 2b: the text rows render through `slt::TextareaState` (real caret
/// — `▎` glyph plus the hardware cursor position). The state is a render
/// *mirror*: `App`'s composer owns text + cursor (history gating, word
/// ops, submit), and the closure consumes every key it dispatches, so the
/// widget never edits. Its `lines` are the legacy `wrap_rows` output (word
/// preferring) rather than the widget's own hard wrap, keeping row breaks
/// byte-identical; `wrap_width` stays `None` so each line renders as-is.
pub(crate) fn render_slt(
    ui: &mut slt::Context,
    app: &App,
    state: &mut slt::TextareaState,
    _avail: u32,
) {
    let t = theme::current();
    const SIDEBAR_MIN_WIDTH: u32 = 80; // legacy gate (frame.rs)
    const SIDEBAR_WIDTH: u32 = 34; // legacy `ui/mod.rs` SIDEBAR_WIDTH
    const CHAT_MIN_WIDTH: u32 = 60; // legacy `Constraint::Min(60)`
    let term_w = ui.width();
    let avail = if app.session.sidebar_open && term_w >= SIDEBAR_MIN_WIDTH {
        (term_w - SIDEBAR_WIDTH).max(CHAT_MIN_WIDTH)
    } else {
        term_w
    }
    .max(1);

    // Height math mirrors `draw()`: wrap width → row count → capped rows.
    let text_w = (avail as usize)
        .saturating_sub(2 + TEXT_LEFT_PAD + TEXT_RIGHT_PAD)
        .max(1);
    let rows = super::messages::wrap_rows(&app.composer.text, text_w);
    let view_rows = rows.len().min(MAX_TEXT_ROWS);
    let box_h = (view_rows + 3) as u32; // pad + rows + pad + info line

    // Sync the mirror before the widget renders: pre-wrapped rows plus the
    // cursor mapped onto them (the app cursor is a char index into the
    // whole text; the widget wants (row, col-in-row)).
    sync_mirror(state, &app.composer.text, &rows, app.composer.cursor);

    // Bang-prefix spans for the first wrapped row (painted by the overlay
    // pass below — the widget renders one style per row).
    let bang_spans = if avail >= 11 && !app.composer.text.is_empty() {
        let (s, e) = rows[0];
        let line: String = app.composer.text.chars().skip(s).take(e - s).collect();
        shell_spans(&line, Style::default().fg(t.text_primary).bg(t.bg_surface))
    } else {
        None
    };
    let empty = app.composer.text.is_empty();

    // ── Composer box: 1 cell of app-bg margin each side of the surface ──
    let mut text_rect: Option<slt::Rect> = None;
    let box_resp = ui
        .container()
        .bg(t.bg_surface)
        .ml(1)
        .w(avail.saturating_sub(2))
        .h(box_h)
        .col(|ui| {
            box_row(ui, &[]); // top padding row

            if avail < 11 {
                return; // legacy guard: inset too narrow for bar/text/info
            }

            // Text rows: 1 pad + bar cell + 1 pad, then the textarea at a
            // fixed width so its rect (used by the overlays one frame
            // later) is stable across frames. The accent bar itself is
            // painted by the box-wide overlay pass — the widget owns its
            // own rows, so the bar cannot interleave in flow.
            ui.line(|ui| {
                let surf = Style::default().bg(t.bg_surface);
                ui.styled(" ", surf);
                ui.styled(" ", surf);
                ui.styled(" ", surf);
                let resp = ui.container().w(text_w as u32).col(|ui| {
                    ui.textarea(state, view_rows as u32);
                });
                text_rect = Some(resp.rect);
            });

            box_row(ui, &[]); // padding row between text and the info line

            // Info line: `model · provider · thinking` left, context usage
            // right-aligned (same math as `render_input`).
            let tertiary = Style::default().fg(t.text_tertiary).bg(t.bg_surface);
            let sep = || Span::styled(" · ", tertiary);
            let (model, provider) = app.model();
            let mut left = vec![
                Span::styled(
                    app.mode.label(),
                    Style::default()
                        .fg(app.mode.color())
                        .bg(t.bg_surface)
                        .add_modifier(Modifier::BOLD),
                ),
                sep(),
                Span::styled(
                    model.to_string(),
                    Style::default().fg(t.blue_400).bg(t.bg_surface),
                ),
                sep(),
                Span::styled(provider.to_string(), tertiary),
                sep(),
            ];
            // Attached images (F.6): a count chip, so staged attachments
            // are visible before the message is sent.
            if !app.images.attached.is_empty() {
                left.push(Span::styled(
                    format!(
                        "[{} image{}] ",
                        app.images.attached.len(),
                        if app.images.attached.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    Style::default().fg(t.accent).bg(t.bg_surface),
                ));
                left.push(sep());
            }
            left.push(Span::styled(
                app.thinking_label(),
                Style::default().fg(t.warning).bg(t.bg_surface),
            ));
            let surf = Style::default().bg(t.bg_surface);
            let right = format!("{}  ", app.session.token_label);
            let info_w = (avail as usize).saturating_sub(6); // inset.width - pads
            let right_w = right.chars().count();
            let (left, left_w) = truncate_spans(left, info_w.saturating_sub(right_w + 1));
            let gap = info_w.saturating_sub(left_w + right_w);
            let mut info = left;
            info.push(Span::styled(" ".repeat(gap), surf));
            info.push(Span::styled(right, tertiary));
            box_row(ui, &info);
        });

    // ── Overlay pass over the finished box (wave 2b) ──
    //
    // The textarea renders with slt's own row styles, so the craft theme
    // is re-applied on top: the accent bar down column 1 (the widget owns
    // its rows, so the bar cannot flow inline through them), the text
    // cells restyled to the craft palette, bang-prefix syntax colors on
    // the first wrapped row, and the placeholder on an empty composer.
    // Rects are the previous frame's — the same one-frame handoff cadence
    // the transcript regions use (`Response::rect` reports post-layout
    // geometry, so a first-of-kind or resized frame simply skips). The
    // bar is derived from the text rect (its wrapper predicts reliably;
    // the outer box container's response rect does not).
    let bar_fg = t.accent;
    let bar_bg = t.bg_surface;
    if let Some(rect) = text_rect.filter(|r| r.width > 0 && r.height > 0) {
        let base = Style::default().fg(t.text_primary).bg(t.bg_surface);
        let placeholder_style = Style::default().fg(t.text_tertiary).bg(t.bg_surface);
        let bang = bang_spans.clone();
        // Box rows: top pad + text rows + gap pad + info line.
        let bar_x = rect.x.saturating_sub(2);
        let bar_rows = rect.y.saturating_sub(1)..rect.y + rect.height + 2;
        let _ = ui.overlay(|ui| {
            let _ = ui.container().grow(1).draw(move |buf, _area| {
                for y in bar_rows.clone() {
                    buf.set_string(bar_x, y, "▎", Style::default().fg(bar_fg).bg(bar_bg));
                }
                restyle_text_area(buf, rect, base, bang.as_deref());
                if empty {
                    // The placeholder replaces the widget's empty-row
                    // glyph (its `▎` included) — the hardware cursor
                    // still lands on the first cell from `set_cursor_pos`.
                    buf.set_string(rect.x, rect.y, "Message Craft…", placeholder_style);
                }
            });
        });
    }

    // ── Status row (single bottom row, full width) ──
    let tertiary = Style::default().fg(t.text_tertiary);
    let sep = || Span::styled(" · ", tertiary);
    // Two leading spaces line the indicator up under the composer's
    // accent bar, which sits two cells in from the row's left edge.
    let mut left = vec![Span::raw("  ")];
    left.extend(status_indicator(app));
    left.extend([
        sep(),
        Span::styled(abbreviated_cwd(&app.session.cwd), tertiary),
    ]);
    let right = if let Some(flash) = app.flash_text() {
        format!("  {flash}  ")
    } else {
        "ctrl+p commands  ".to_string()
    };
    let width = avail as usize;
    let right_w = right.chars().count();
    let max_left = width.saturating_sub(right_w + 1);
    let (left, left_w) = truncate_spans(left, max_left);
    let gap = width.saturating_sub(left_w + right_w);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    let right_style = if app.flash_text().is_some() {
        Style::default().fg(t.accent)
    } else {
        tertiary
    };
    spans.push(Span::styled(right, right_style));
    ui.line(|ui| {
        for span in &spans {
            ui.styled(span.content.as_ref(), span.style);
        }
    });

    // ── Footer hint row (draw()'s footer block) ──
    if let Some(hint) = app.plan_mode.plan_form.hint_line() {
        ui.line(|ui| {
            for span in &hint.spans {
                ui.styled(span.content.as_ref(), span.style);
            }
        });
    } else if app.active_task.is_none() && app.any_task_working() {
        // Main-chat hint while subagents run: how many, and the chord
        // that opens their chats (task 96).
        let n = app
            .task_chats
            .iter()
            .filter(|c| c.outcome.is_none())
            .count();
        let hint = format!(
            "{n} task{} running — Ctrl-N to view",
            if n == 1 { "" } else { "s" }
        );
        ui.styled(hint, Style::default().fg(t.text_tertiary));
    } else {
        // The reserved blank row.
        ui.styled(" ", Style::default().bg(t.bg_app));
    }

    // Publish the composer area for mouse selection confinement (the
    // box container's rect — previous frame's, matching the one-frame
    // handoff cadence the transcript regions use). Overlay layer + 1x1:
    // slt skips deferred draws whose rect collapses to zero, so flow-space
    // bookkeeping regions never run reliably.
    let paint = app.view.paint.clone();
    let rect = box_resp.rect;
    let _ = ui.overlay(|ui| {
        let _ = ui.container().w(1).h(1).draw(move |_buf, _area| {
            if let Ok(mut p) = paint.lock() {
                p.composer_area = crate::tui::geom::Rect::from_slt(rect);
            }
        });
    });
}

/// One row inside the composer surface box: every row carries the accent
/// bar at column 1 (it spans the full box height), then optional content
/// starting after the `▎ ` prefix.
fn box_row(ui: &mut slt::Context, spans: &[Span<'static>]) {
    let t = theme::current();
    let surf = Style::default().bg(t.bg_surface);
    ui.line(|ui| {
        ui.styled(" ", surf);
        ui.styled("▎", Style::default().fg(t.accent).bg(t.bg_surface));
        ui.styled(" ", surf);
        for span in spans {
            ui.styled(span.content.as_ref(), span.style);
        }
    });
}

/// Feed the app's composer state into the render mirror: the widget's
/// lines are the legacy `wrap_rows` output (word-preferring breaks) and
/// the cursor lands on the row containing its char index, as a char
/// offset within that row. The widget clamps both to the actual rows.
fn sync_mirror(state: &mut slt::TextareaState, text: &str, rows: &[(usize, usize)], cursor: usize) {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    state.wrap_width = None; // lines arrive pre-wrapped
    state.lines = rows
        .iter()
        .map(|&(s, e)| chars[s..e].iter().collect())
        .collect();
    if state.lines.is_empty() {
        state.lines.push(String::new());
    }
    // First row whose end reaches the cursor (the legacy `cursor_row_col`
    // tie-break: an edge cursor belongs to the earlier row).
    let mut row = rows.len().saturating_sub(1);
    let mut col = 0usize;
    for (i, &(s, e)) in rows.iter().enumerate() {
        if cursor <= e {
            row = i;
            col = cursor.saturating_sub(s);
            break;
        }
    }
    state.cursor_row = row.min(state.lines.len() - 1);
    state.cursor_col = col;
}

/// Re-apply the craft palette over the textarea's rows (slt styles them
/// from its own theme) and, when given, the bang-prefix syntax colors on
/// the first row. Cells are walked by display width: wide glyphs cover
/// their continuation cells, and the widget's inserted `▎` caret glyph is
/// styled but consumes no span width (it shifts the row by one cell).
fn restyle_text_area(
    buf: &mut slt::Buffer,
    rect: slt::Rect,
    base: Style,
    bang: Option<&[Span<'static>]>,
) {
    let x_end = rect.x.saturating_add(rect.width);
    let y_end = rect.y.saturating_add(rect.height);
    for y in rect.y..y_end {
        for x in rect.x..x_end {
            if let Some(cell) = buf.try_get_mut(x, y) {
                cell.style = base;
            }
        }
    }
    let Some(spans) = bang else {
        return;
    };
    let mut x = rect.x;
    'spans: for span in spans {
        let mut cells_left = UnicodeWidthStr::width(&*span.content);
        while cells_left > 0 {
            if x >= x_end {
                break 'spans;
            }
            let symbol: String = buf.get(x, rect.y).symbol.to_string();
            if let Some(cell) = buf.try_get_mut(x, rect.y) {
                cell.style = span.style;
            }
            x += 1;
            if symbol == "▎" {
                continue; // the inserted caret glyph: width 0 in the text
            }
            let w = if symbol.is_empty() {
                0 // wide-glyph continuation cell
            } else {
                UnicodeWidthStr::width(&*symbol).max(1)
            };
            cells_left = cells_left.saturating_sub(w);
        }
    }
}
