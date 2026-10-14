//! Composer (input box), model/mode/context row, activity row and footer
//! hint row.

use unicode_width::UnicodeWidthStr;

use super::theme;
use crate::markdown::highlight::{Highlighter, SegmentColor};
use crate::tui::app::App;
use crate::tui::provider::Status;
use crate::tui::shell::parse_shell_prefix;
use crate::tui::style::{Modifier, Style, StyleExt};
use crate::tui::text::Span;

/// Max text rows the composer grows to before it scrolls instead of expanding.
pub const MAX_TEXT_ROWS: usize = 12;

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

/// Turn activity for the status bar: spinner, status word, the running
/// timer and the interrupt hint while a turn is live; the failure mark
/// after a failed one; nothing while idle.
fn status_indicator(app: &App, with_esc_hint: bool) -> Vec<Span<'static>> {
    let t = theme::current();

    let tertiary = Style::default().fg(t.text_tertiary);
    let dim = Style::default().fg(t.text_disabled);
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
            let mut spans = vec![
                Span::styled(format!("{frame} "), Style::default().fg(t.accent)),
                Span::styled(format!("{word} {elapsed}s"), tertiary),
            ];
            if with_esc_hint {
                spans.push(Span::styled(" · esc interrupt", dim));
            }
            spans
        }
        Status::Failed => vec![
            Span::styled("✖ ", Style::default().fg(t.danger)),
            Span::styled("failed", tertiary),
        ],
        Status::Done => Vec::new(),
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
// The slt composer panel
// ─────────────────────────────────────────────────────────────────────────

/// Outer horizontal margin of the composer column (box, widget, rows).
pub const SIDE_MARGIN: u32 = 2;
/// Cells left of the text inside the box: bar, space, `>`, space.
const PROMPT_W: usize = 4;
/// The right-aligned `enter send` hint plus its gutter; dropped when the
/// box is too narrow to spare it.
const SEND_HINT: &str = "enter send";
const SEND_HINT_W: usize = 12; // 1 gap + hint + 1 pad
/// Below this box width the hint is dropped (text keeps a 1-cell pad).
const SEND_HINT_MIN_BOX: usize = 36;

/// Width of the composer box for a terminal `term_w` cells wide.
pub(crate) fn box_width(term_w: u32) -> usize {
    (term_w.saturating_sub(SIDE_MARGIN * 2) as usize).max(1)
}

/// Whether the box has room for the `enter send` hint.
fn shows_send_hint(box_w: usize) -> bool {
    box_w >= SEND_HINT_MIN_BOX
}

/// Wrap width of the composer text for a terminal `term_w` cells wide.
pub(crate) fn text_width(term_w: u32) -> usize {
    let box_w = box_width(term_w);
    let right = if shows_send_hint(box_w) {
        SEND_HINT_W
    } else {
        1
    };
    box_w.saturating_sub(PROMPT_W + right).max(1)
}

/// Visible text rows of the composer box (wrapped, capped).
pub(crate) fn view_rows(app: &App, term_w: u32) -> usize {
    super::messages::wrap_rows(&app.composer.text, text_width(term_w))
        .len()
        .clamp(1, MAX_TEXT_ROWS)
}

/// Blank padding rows above and below the composer text inside the box.
const BOX_PAD_ROWS: usize = 1;

/// Rows the composer block occupies under the box: the status bar and the
/// optional footer hint row.
fn rows_below(app: &App) -> u32 {
    1 + u32::from(footer_hint(app).is_some())
}

/// Height of the composer box: the text rows plus the padding rows.
fn box_height(view_rows: usize) -> u32 {
    (view_rows + 2 * BOX_PAD_ROWS) as u32
}

/// Screen row of the composer box's top edge; popups anchored to the
/// composer (slash completion, model menu) float above it.
pub(crate) fn box_top(app: &App, term_w: u32, term_h: u32) -> u32 {
    term_h.saturating_sub(box_height(view_rows(app, term_w)) + rows_below(app))
}

/// Height the whole composer block occupies under the box: the box
/// itself (text rows plus padding) and the rows below it. Pure in app
/// state so the transcript painter can budget for it synchronously (see
/// `messages::render_slt`). Only valid while the box is shown — while a
/// form (permission/question/plan) takes its place, the caller falls
/// back to the stale handoff.
pub(crate) fn block_height(app: &App, term_w: u32) -> u32 {
    box_height(view_rows(app, term_w)) + rows_below(app)
}

/// The footer hint row, when there is one: the dismissed plan form's
/// `Plan Ctrl+T` reminder, or the running-subagents hint (task 96).
fn footer_hint(app: &App) -> Option<Vec<Span<'static>>> {
    let t = theme::current();
    if let Some(hint) = app.plan_mode.plan_form.hint_line() {
        return Some(hint.spans);
    }
    if app.active_task.is_none() && app.any_task_working() {
        let n = app
            .task_chats
            .iter()
            .filter(|c| c.outcome.is_none())
            .count();
        return Some(vec![Span::styled(
            format!(
                " {n} task{} running — Ctrl-N to view",
                if n == 1 { "" } else { "s" }
            ),
            Style::default().fg(t.text_tertiary),
        )]);
    }
    None
}

/// Parse the provider's token label (`"44.8K/1M (4%)"`) into the used /
/// budget text and the percentage for the context meter.
fn parse_token_label(label: &str) -> Option<(String, u8)> {
    let (used, rest) = label.split_once(" (")?;
    let pct = rest.strip_suffix("%)")?.trim().parse::<f32>().ok()?;
    let used = used.replacen('/', " / ", 1);
    Some((used, pct.clamp(0.0, 100.0).round() as u8))
}

/// Cells in the context meter.
const METER_CELLS: usize = 10;

/// Right side of the model row: `▰▰▰▱… used / budget pct%`, or the raw
/// label when the provider's string has another shape.
fn context_meter(label: &str) -> Vec<Span<'static>> {
    let t = theme::current();
    let tertiary = Style::default().fg(t.text_tertiary);
    match parse_token_label(label) {
        Some((used, pct)) => {
            let filled = (usize::from(pct) * METER_CELLS)
                .div_ceil(100)
                .min(METER_CELLS);
            vec![
                Span::styled("▰".repeat(filled), Style::default().fg(t.accent)),
                Span::styled(
                    "▰".repeat(METER_CELLS - filled),
                    Style::default().fg(t.border_subtle),
                ),
                Span::styled(format!(" {used} "), tertiary),
                Span::styled(format!("{pct}%"), Style::default().fg(t.border_strong)),
            ]
        }
        None => vec![Span::styled(label.to_owned(), tertiary)],
    }
}

/// Left side of the status bar: `● model · thinking [images] │ BUILD ·
/// tab to switch`, then the turn activity (spinner) and any flash. The
/// chip is the active mode only; Tab switches it.
fn status_left_spans(app: &App, with_tab_hint: bool, with_esc_hint: bool) -> Vec<Span<'static>> {
    let t = theme::current();
    let dim = Style::default().fg(t.text_disabled);
    let sep = || Span::styled(" · ", dim);
    let (model, _provider) = app.model();
    let mut spans = vec![
        Span::styled("● ", Style::default().fg(t.accent)),
        Span::styled(model.to_string(), Style::default().fg(t.text_tertiary)),
        sep(),
        Span::styled(app.thinking_label(), Style::default().fg(t.text_tertiary)),
    ];
    // Attached images (F.6): a count chip, so staged attachments are
    // visible before the message is sent.
    let n_images = app.images.attached.len();
    if n_images > 0 {
        spans.push(sep());
        spans.push(Span::styled(
            format!("[{n_images} image{}]", if n_images == 1 { "" } else { "s" }),
            Style::default().fg(t.accent),
        ));
    }
    spans.push(Span::styled("  │  ", Style::default().fg(t.border_subtle)));
    let chip = app.mode.label().trim_matches(['[', ']']);
    spans.push(Span::styled(
        format!(" {chip} "),
        Style::default()
            .fg(t.bg_app)
            .bg(app.mode.color())
            .add_modifier(Modifier::BOLD),
    ));
    if with_tab_hint {
        spans.push(Span::styled(
            " · tab to switch",
            Style::default().fg(t.border_strong),
        ));
    }
    let activity = status_indicator(app, with_esc_hint);
    if !activity.is_empty() {
        spans.push(Span::raw("   "));
        spans.extend(activity);
    }
    if let Some(flash) = app.flash_text() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            flash.to_owned(),
            Style::default().fg(t.accent),
        ));
    }
    spans
}

/// Lay `left` and `right` out across `width` cells: left clipped to leave
/// room for right, the gap padded with spaces.
fn split_row(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();
    let (left, left_w) = truncate_spans(left, width.saturating_sub(right_w + 1));
    let gap = width.saturating_sub(left_w + right_w);
    let mut spans = left;
    spans.push(Span::raw(" ".repeat(gap)));
    spans.extend(right);
    spans
}

/// Render the composer column's bottom block into the slt frame (the box
/// is skipped when `show_box` is false — a form has taken its place):
///
/// ```text
///   ▎
///   ▎ > message text…                                          enter send
///   ▎
///   ● model · thinking  │   BUILD  · tab to switch   ⠙ running 3s · esc interrupt   ▰▰▰▱▱ 44.8K / 1M 4%
///   [footer hint row — dismissed plan form / running subagents]
/// ```
///
/// The text rows render through `slt::TextareaState` (real caret — `▎`
/// glyph plus the hardware cursor position). The state is a render
/// *mirror*: `App`'s composer owns text + cursor (history gating, word
/// ops, submit), and the closure consumes every key it dispatches, so the
/// widget never edits. Its `lines` are the `wrap_rows` output (word
/// preferring) rather than the widget's own hard wrap; `wrap_width` stays
/// `None` so each line renders as-is.
pub(crate) fn render_slt(
    ui: &mut slt::Context,
    app: &App,
    state: &mut slt::TextareaState,
    show_box: bool,
) {
    let t = theme::current();
    let term_w = ui.width().max(1);
    let box_w = box_width(term_w);
    let text_w = text_width(term_w);
    let rows = super::messages::wrap_rows(&app.composer.text, text_w);
    let view_rows = rows.len().clamp(1, MAX_TEXT_ROWS);
    let send_hint = shows_send_hint(box_w);

    // Sync the mirror before the widget renders: pre-wrapped rows plus the
    // cursor mapped onto them (the app cursor is a char index into the
    // whole text; the widget wants (row, col-in-row)).
    sync_mirror(state, &app.composer.text, &rows, app.composer.cursor);

    // Bang-prefix spans for the first wrapped row (painted by the overlay
    // pass below — the widget renders one style per row).
    let bang_spans = if !app.composer.text.is_empty() {
        let (s, e) = rows[0];
        let line: String = app.composer.text.chars().skip(s).take(e - s).collect();
        shell_spans(&line, Style::default().fg(t.text_primary).bg(t.bg_surface))
    } else {
        None
    };
    let empty = app.composer.text.is_empty();

    // The question form and plan decision menu take the box's place
    // (frame.rs paints them); the status bar below stays.
    let box_rect: Option<slt::Rect> = if show_box {
        // ── Composer box: surface panel, bar + `>` prompt, the textarea ──
        let surf = Style::default().bg(t.bg_surface);
        let box_resp = ui
            .container()
            .bg(t.bg_surface)
            .ml(SIDE_MARGIN)
            .w(box_w as u32)
            .h(box_height(view_rows))
            .col(|ui| {
                // The bar, `>` and hint are painted by the overlay pass (the
                // widget owns its rows, so they cannot flow inline through
                // them); the flow reserves their cells. The textarea sits at a
                // fixed width so its rect is predictable. Blank rows above
                // and below give the input its height.
                for _ in 0..BOX_PAD_ROWS {
                    ui.styled(" ", surf);
                }
                ui.line(|ui| {
                    ui.styled(" ".repeat(PROMPT_W), surf);
                    #[allow(unused)]
                    ui.container().w(text_w as u32).col(|ui| {
                        let _ = ui.textarea(state, view_rows as u32);
                    });
                });
                for _ in 0..BOX_PAD_ROWS {
                    ui.styled(" ", surf);
                }
            });

        // ── Overlay pass over the finished box ──
        //
        // The textarea renders with slt's own row styles, so the craft theme
        // is re-applied on top: the accent bar down the box's left edge, the
        // `>` prompt and `enter send` hint on the first row, the text cells
        // restyled to the craft palette, bang-prefix syntax colors on the
        // first wrapped row, and the placeholder on an empty composer.
        //
        // The textarea rect is COMPUTED, not read from `Response::rect`:
        // that is a retained per-id lookup, and a todo/files toggle (ctrl+b
        // / ctrl+t) changes the widget call order, so the id misses and slt
        // hands back the full-screen fallback rect — the overlay then
        // painted a ▎ bar down column 0 and the placeholder at row 0 for a
        // frame. The box is bottom-anchored with fixed insets, so the rect
        // is exact synchronously; when the terminal is too short for the
        // box (clipped flow), skip the pass like before.
        let term_h = ui.height().max(1) as u32;
        let text_rect: Option<slt::Rect> = if term_h > box_height(view_rows) + rows_below(app) {
            Some(slt::Rect {
                x: SIDE_MARGIN + PROMPT_W as u32,
                y: box_top(app, term_w, term_h) + BOX_PAD_ROWS as u32,
                width: text_w as u32,
                height: view_rows as u32,
            })
        } else {
            None
        };
        if let Some(rect) = text_rect {
            let base = Style::default().fg(t.text_primary).bg(t.bg_surface);
            let placeholder_style = Style::default().fg(t.text_disabled).bg(t.bg_surface);
            let bar_style = Style::default().fg(t.accent).bg(t.bg_surface);
            let prompt_style = bar_style.add_modifier(Modifier::BOLD);
            let hint_style = Style::default().fg(t.text_disabled).bg(t.bg_surface);
            let bang = bang_spans.clone();
            let bar_x = rect.x.saturating_sub(PROMPT_W as u32);
            let pad = BOX_PAD_ROWS as u32;
            let _ = ui.overlay(|ui| {
                ui.container().grow(1).draw(move |buf, _area| {
                    // The bar runs the box's full height, padding rows included.
                    for y in rect.y.saturating_sub(pad)..rect.y + rect.height + pad {
                        buf.set_string(bar_x, y, "▎", bar_style);
                    }
                    buf.set_string(bar_x + 2, rect.y, ">", prompt_style);
                    if send_hint {
                        let hint_x = rect.x + rect.width + 1;
                        buf.set_string(hint_x, rect.y, SEND_HINT, hint_style);
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

        Some(box_resp.rect)
    } else {
        None
    };

    let paint_row = |ui: &mut slt::Context, spans: Vec<Span<'static>>| {
        ui.line(|ui| {
            for span in &spans {
                ui.styled(span.content.as_ref(), span.style);
            }
        });
    };

    // ── Status bar: model · thinking │ mode chip · tab hint  activity …
    //    context meter, starting in the box's bar column ──
    let mut right = context_meter(&app.session.token_label);
    right.push(Span::raw(" "));
    let right_w: usize = right.iter().map(|s| s.content.chars().count()).sum();
    // Narrow terminals shed the interrupt hint, then the tab hint, before
    // the row is clipped.
    let mut left = Vec::new();
    for (tab_hint, esc_hint) in [(true, true), (true, false), (false, false)] {
        left = status_left_spans(app, tab_hint, esc_hint);
        let left_w: usize = left.iter().map(|s| s.content.chars().count()).sum();
        if left_w + right_w < box_w {
            break;
        }
    }
    left.insert(0, Span::raw(" ".repeat(SIDE_MARGIN as usize)));
    paint_row(ui, split_row(left, right, box_w + SIDE_MARGIN as usize));

    // ── Footer hint row (only when there is something to say) ──
    if let Some(mut hint) = footer_hint(app) {
        hint.insert(0, Span::raw(" ".repeat(SIDE_MARGIN as usize)));
        paint_row(ui, hint);
    }

    // Publish the composer area for mouse selection confinement (the
    // box container's rect — previous frame's, matching the one-frame
    // handoff cadence the transcript regions use). Overlay layer + 1x1:
    // slt skips deferred draws whose rect collapses to zero, so flow-space
    // bookkeeping regions never run reliably.
    let paint = app.view.paint.clone();
    let area = box_rect
        .map(crate::tui::geom::Rect::from_slt)
        .unwrap_or_default();
    let _ = ui.overlay(|ui| {
        ui.container().w(1).h(1).draw(move |_buf, _area| {
            if let Ok(mut p) = paint.lock() {
                p.composer_area = area;
            }
        });
    });
}

/// Paint a form (permission prompt, question form, plan decision menu) in
/// the composer box's place and style: the surface panel with a left bar
/// in `bar`, a padding row above and below, and the form's rows where the
/// text field would be. Leading/trailing blank lines are dropped (the
/// panel pads instead) and one leading space comes off every row (the
/// panel's bar + space inset replaces the forms' own one-cell margin).
pub(crate) fn render_form_panel(
    ui: &mut slt::Context,
    lines: &[crate::tui::text::Line<'static>],
    bar: crate::tui::style::Color,
) {
    use crate::tui::text::Line;

    let t = theme::current();
    let box_w = box_width(ui.width());
    let inner_w = box_w.saturating_sub(3).max(1); // bar + space + right pad
    let surf = Style::default().bg(t.bg_surface);
    let bar_style = Style::default().fg(bar).bg(t.bg_surface);
    let is_blank = |l: &Line<'static>| l.spans.iter().all(|s| s.content.trim().is_empty());
    let first = lines
        .iter()
        .position(|l| !is_blank(l))
        .unwrap_or(lines.len());
    let last = lines
        .iter()
        .rposition(|l| !is_blank(l))
        .map_or(first, |i| i + 1);

    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    for line in &lines[first..last] {
        let mut line = line.clone();
        if let Some(span) = line.spans.first_mut()
            && let Some(rest) = span.content.strip_prefix(' ')
        {
            span.content = rest.to_owned().into();
        }
        // Wrapped rows hang under the line's text: its leading indent plus
        // two cells (past a `▸ `-style marker).
        let lead: usize = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .take_while(|c| *c == ' ')
            .count();
        let hang = (lead + 2).min(inner_w / 2);
        let wrapped = crate::tui::permission_prompt::wrap_line_rows(
            &line,
            inner_w.saturating_sub(hang).max(1),
            surf,
        );
        for (i, mut row) in wrapped.into_iter().enumerate() {
            if i > 0 {
                row.insert(0, Span::styled(" ".repeat(hang), surf));
            }
            rows.push(row);
        }
    }

    let _ = ui
        .container()
        .bg(t.bg_surface)
        .ml(SIDE_MARGIN)
        .w(box_w as u32)
        .col(|ui| {
            let paint = |ui: &mut slt::Context, spans: &[Span<'static>]| {
                ui.line(|ui| {
                    ui.styled("▎", bar_style);
                    ui.styled(" ", surf);
                    for span in spans {
                        ui.styled(span.content.as_ref(), span.style);
                    }
                });
            };
            for _ in 0..BOX_PAD_ROWS {
                paint(ui, &[]);
            }
            for row in &rows {
                paint(ui, row);
            }
            for _ in 0..BOX_PAD_ROWS {
                paint(ui, &[]);
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
