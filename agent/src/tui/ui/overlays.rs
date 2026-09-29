//! Overlays: slash menu, model menu, command palette, confirm dialog.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::theme;
use crate::mcp::config::{McpServerInfo, McpServerStatus};
use crate::tui::app::App;
use crate::tui::modals::Modal;

fn dim(f: &mut Frame, area: Rect) {
    // Clear first: a bg-only Block would leave the underlying characters visible.
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(theme::current().bg_sunken)),
        area,
    );
}

/// Borderless raised-surface block shared by all overlays.
fn boxed(_area: Rect) -> Block<'static> {
    Block::default().style(Style::default().bg(theme::current().bg_raised))
}

fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn render_rows(f: &mut Frame, rows: &[Vec<Span<'static>>], selected: usize, area: Rect) {
    for (i, spans) in rows.iter().enumerate() {
        if i as u16 >= area.height {
            break;
        }
        let bg = if i == selected {
            theme::current().bg_overlay
        } else {
            theme::current().bg_raised
        };
        let mut spans = spans.clone();
        let w: usize = spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        if w < area.width as usize {
            spans.push(Span::styled(
                " ".repeat(area.width as usize - w),
                Style::default().bg(bg),
            ));
        }
        let style = Style::default().bg(bg);
        f.render_widget(
            Paragraph::new(Line::from(spans)).style(style),
            Rect {
                x: area.x,
                y: area.y + i as u16,
                width: area.width,
                height: 1,
            },
        );
    }
}

/// Search modal (F.3, Ctrl-F): fuzzy matches over the transcript's
/// segments, list on top, query row below it.
pub fn render_search(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::tui::search_modal::{self, SearchModal};

    if !app.overlays.search.is_open() {
        return;
    }
    let search: &mut SearchModal = &mut app.overlays.search;
    let t = theme::current();

    let content_rows = if search.matches.is_empty() && !search.query().is_empty() {
        1
    } else {
        search.matches.len().min(16)
    } as u16;
    let width = (area.width / 2).max(20).min(area.width);
    let height = (content_rows + search_modal::search_row_height() + 2)
        .min(area.height * 3 / 5)
        .max(3);
    dim(f, area);
    let rect = centered(width, height, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block.title(search_modal::MODAL_TITLE), rect);

    let [list_area, query_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    search.viewport_height = list_area.height as usize;
    search.ensure_visible();

    if search.matches.is_empty() {
        if !search.query().is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    search_modal::no_matches_label(),
                    Style::default().fg(t.text_tertiary),
                ))),
                list_area,
            );
        }
    } else {
        let max_label_width = list_area.width.saturating_sub(2) as usize; // label indent
        let end = (search.scroll_offset + search.viewport_height).min(search.matches.len());
        let rows: Vec<Vec<Span<'static>>> = (search.scroll_offset..end)
            .map(|i| {
                search_modal::highlighted_row(
                    &search.matches[i],
                    max_label_width,
                    i == search.selected,
                )
            })
            .collect();
        render_rows(f, &rows, search.selected - search.scroll_offset, list_area);
    }

    // Query row with a block cursor over the char at the caret.
    let query = search.query().to_string();
    let cursor_char = search.cursor();
    let before: String = query.chars().take(cursor_char).collect();
    let at: String = query
        .chars()
        .nth(cursor_char)
        .map(String::from)
        .unwrap_or(" ".into());
    let after: String = query.chars().skip(cursor_char + 1).collect();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                search_modal::search_prefix(),
                Style::default().fg(t.text_tertiary),
            ),
            Span::raw(before),
            Span::styled(at, Style::default().bg(t.accent).fg(t.bg_app)),
            Span::raw(after),
        ])),
        query_area,
    );
}

/// File picker (F.3, Ctrl-S): fuzzy matches over the walked paths, list on
/// top, query row below it, "scanning…" title + spinner while walking.
pub fn render_file_picker(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::tui::file_picker::{self, FilePicker};

    if !app.overlays.file_picker.is_open() {
        return;
    }
    let picker: &mut FilePicker = &mut app.overlays.file_picker;
    let t = theme::current();

    let Some(s) = picker.session_mut() else {
        return;
    };
    if !s.visible {
        return;
    }

    let content_rows = if s.matches.is_empty() && !s.query.is_empty() {
        1
    } else {
        // One row extra when the "+N more files" hint sits at the bottom.
        let hint_row = u16::from((s.total_matches > file_picker::max_materialized()) as u8);
        (s.matches.len() as u16).min(16) + hint_row
    };
    let width = (area.width / 2).max(24).min(area.width);
    let height = (content_rows + file_picker::search_row_height() + 2)
        .min(area.height * 4 / 5)
        .max(3);
    dim(f, area);
    let rect = centered(width, height, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    let title = if s.walking {
        file_picker::MODAL_TITLE_WALKING
    } else {
        file_picker::MODAL_TITLE
    };
    f.render_widget(block.title(title), rect);

    let [list_area, query_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    s.viewport_height = list_area.height as usize;
    s.apply_scroll();

    let max_label_width = list_area.width.saturating_sub(2) as usize; // label indent
    let end = (s.scroll_offset + s.viewport_height).min(s.matches.len());
    let rows: Vec<Vec<Span<'static>>> = (s.scroll_offset..end)
        .map(|i| highlighted_path_row(&s.matches[i], max_label_width, i == s.selected))
        .collect();
    render_rows(f, &rows, s.selected - s.scroll_offset, list_area);

    if s.matches.is_empty() {
        if !s.query.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    file_picker::no_matches_label(),
                    Style::default().fg(t.text_tertiary),
                ))),
                list_area,
            );
        }
    } else if s.total_matches > file_picker::max_materialized()
        && s.scroll_offset + s.viewport_height >= s.matches.len()
    {
        let n = s.total_matches - file_picker::max_materialized();
        let y = list_area.y + (end - s.scroll_offset) as u16;
        if y < list_area.bottom() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("{}+{n} more files (not shown)", file_picker::label_indent()),
                    Style::default().fg(t.text_tertiary),
                ))),
                Rect {
                    x: list_area.x,
                    y,
                    width: list_area.width,
                    height: 1,
                },
            );
        }
    }

    // Query row: spinner while walking, block cursor over the caret char.
    let mut spans = vec![Span::styled(
        file_picker::search_prefix(),
        Style::default().fg(t.text_tertiary),
    )];
    if s.walking {
        let ch = file_picker::spinner_frame(s.started_at.elapsed());
        spans.push(Span::styled(
            format!("{ch} "),
            Style::default().fg(t.text_tertiary),
        ));
    }
    let query = s.query.clone();
    let cursor = s.cursor;
    let before: String = query.chars().take(cursor).collect();
    let at: String = query
        .chars()
        .nth(cursor)
        .map(String::from)
        .unwrap_or(" ".into());
    let after: String = query.chars().skip(cursor + 1).collect();
    spans.extend([
        Span::raw(before),
        Span::styled(at, Style::default().bg(t.accent).fg(t.bg_app)),
        Span::raw(after),
    ]);
    f.render_widget(Paragraph::new(Line::from(spans)), query_area);
}

/// One path row: matched chars bolded in the accent color, the selected row
/// on the overlay background, truncated to the display width (unicode-width
/// aware, like the reference's `build_highlighted_line`).
fn highlighted_path_row(
    m: &crate::tui::file_picker::Match,
    max_width: usize,
    is_selected: bool,
) -> Vec<Span<'static>> {
    use crate::tui::file_picker::label_indent;

    let t = theme::current();
    let base_style = Style::default().bg(if is_selected {
        t.bg_overlay
    } else {
        t.bg_raised
    });
    let match_style = base_style.fg(t.accent).add_modifier(Modifier::BOLD);

    let index_set: std::collections::HashSet<u32> = m.indices.iter().copied().collect();
    let mut spans = vec![Span::styled(label_indent(), base_style)];
    let mut current = false;
    let mut run = String::new();
    let mut width = 0usize;
    for (i, ch) in m.path.chars().enumerate() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + cw > max_width {
            break;
        }
        width += cw;
        let is_match = index_set.contains(&(i as u32));
        if is_match != current && !run.is_empty() {
            let style = if current { match_style } else { base_style };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        current = is_match;
        run.push(ch);
    }
    if !run.is_empty() {
        let style = if current { match_style } else { base_style };
        spans.push(Span::styled(run, style));
    }
    spans
}

/// Slash-command popup above the composer. `chat` is the whole chat column and
/// `bottom` the 7-row composer/status/footer region it sits above.
pub fn render_slash(f: &mut Frame, app: &App, chat: Rect, bottom: Rect) {
    let t = theme::current();

    let items = app.slash_matches();
    if items.is_empty() {
        return;
    }
    let n = items.len().min(6) as u16;
    let width = chat.width.saturating_sub(4).min(64);
    let area = Rect {
        x: chat.x + 2,
        y: bottom.y.saturating_sub(n),
        width,
        height: n,
    };
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(t.bg_raised)),
        area,
    );
    // Scroll the window so the highlighted row stays visible past the edge.
    let window = n as usize;
    let start = app
        .overlays
        .slash_selected
        .saturating_sub(window.saturating_sub(1))
        .min(items.len().saturating_sub(window));
    let rows: Vec<Vec<Span<'static>>> = items
        .iter()
        .skip(start)
        .take(window)
        .map(|(cmd, desc)| {
            vec![
                Span::styled(format!(" {cmd}"), Style::default().fg(t.cyan)),
                Span::styled(format!("  {desc}"), Style::default().fg(t.text_tertiary)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        app.overlays.slash_selected.saturating_sub(start),
        area,
    );
}

/// Model picker, opened with ctrl+l / /model / palette.
pub fn render_model_menu(f: &mut Frame, app: &App, chat: Rect, bottom: Rect) {
    let t = theme::current();

    let Modal::ModelMenu(selected) = app.overlays.modal else {
        return;
    };
    if app.session.models.is_empty() {
        return;
    }
    // Large real catalogs would otherwise paint over the chat and composer:
    // cap the menu to the rows above the composer and window it around the
    // selected row.
    let available = bottom.y.saturating_sub(chat.y + 2) as usize;
    let visible = app.session.models.len().min(available.max(1));
    let start = selected.min(app.session.models.len().saturating_sub(visible));
    let n = visible as u16;
    let area = Rect {
        x: chat.x + 2,
        y: bottom.y.saturating_sub(n + 3),
        width: 56.min(chat.width.saturating_sub(4)),
        height: n + 3,
    };
    f.render_widget(Clear, area);
    let block = boxed(area);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows: Vec<Vec<Span<'static>>> = app
        .session
        .models
        .iter()
        .enumerate()
        .map(|(i, choice)| {
            let marker = if i == app.session.model_idx {
                "● "
            } else {
                "  "
            };
            // Held tiers from the global registry; overrides are tier-keyed
            // so a model may hold several ("strong/weak").
            let spec = format!("{}/{}", choice.provider, choice.model);
            let tiers = crate::model_registry::override_tiers(&spec);
            let tier_label = tiers
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join("/");
            let label_style = if tiers.is_empty() {
                Style::default().fg(t.text_primary)
            } else {
                // Highlight rows the user has explicitly assigned.
                Style::default().fg(t.accent)
            };
            vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(if i == app.session.model_idx {
                        t.accent
                    } else {
                        t.bg_raised
                    }),
                ),
                Span::styled(choice.label.clone(), label_style),
                Span::styled(
                    format!("  {}", choice.provider_label),
                    Style::default().fg(t.text_tertiary),
                ),
                Span::styled(format!("  {tier_label}"), Style::default().fg(t.accent)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows[start..start + visible],
        selected - start,
        Rect {
            y: inner.y + 1,
            height: inner.height.saturating_sub(2),
            ..inner
        },
    );
    // Keybind footer: Enter selects; !@#$ (or 1-4) toggle tier assignment.
    let footer = Rect {
        y: inner.y + 1 + visible as u16,
        height: 1,
        ..inner
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Enter select", Style::default().fg(t.cyan)),
            Span::styled("  ! strong", Style::default().fg(t.accent)),
            Span::styled("  @ medium", Style::default().fg(t.accent)),
            Span::styled("  # weak", Style::default().fg(t.accent)),
            Span::styled("  $ compaction", Style::default().fg(t.accent)),
        ])),
        footer,
    );
}

/// ctrl+p command palette, centered near the top.
pub fn render_palette(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::Palette { query, selected } = &app.overlays.modal else {
        return;
    };
    let (query, selected) = (query.clone(), *selected);
    dim(f, area);

    let items = app.palette_items();
    let n = items.len().min(8) as u16;
    let width = 60.min(area.width.saturating_sub(4));
    let height = n + 4; // top pad + query row + divider + items + bottom pad
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + area.height / 6 + 2, // placed two rows below center-top anchor
        width,
        height: height.min(area.height),
    };
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    // query row (after the top padding row)
    let qrow = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: inner.width.saturating_sub(2),
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(t.cyan)),
            Span::styled(
                if query.is_empty() {
                    "Type a command…".to_string()
                } else {
                    query.clone()
                },
                Style::default().fg(if query.is_empty() {
                    t.text_tertiary
                } else {
                    t.text_primary
                }),
            ),
        ])),
        qrow,
    );
    f.set_cursor_position((qrow.x + 2 + query.chars().count() as u16, qrow.y));

    // Content inset by one blank column on each side of the popup.
    let content = Rect {
        x: inner.x + 1,
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    };

    // divider
    let div_y = qrow.y + 1;
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(content.width as usize),
            Style::default().fg(t.border_subtle),
        ))),
        Rect {
            x: content.x,
            y: div_y,
            width: content.width,
            height: 1,
        },
    );

    // Scroll the 8-row window so the highlighted row stays visible once the
    // selection moves past the bottom edge.
    let window = n as usize;
    let start = selected
        .saturating_sub(window.saturating_sub(1))
        .min(items.len().saturating_sub(window));
    let rows: Vec<Vec<Span<'static>>> = items
        .iter()
        .skip(start)
        .take(window)
        .map(|(_, label, hint)| {
            let w = content.width as usize;
            let label = format!(" {label}");
            let hint = format!("{hint} ");
            let gap = w.saturating_sub(label.chars().count() + hint.chars().count());
            vec![
                Span::styled(label, Style::default().fg(t.text_primary)),
                Span::raw(" ".repeat(gap)),
                Span::styled(hint, Style::default().fg(t.text_tertiary)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        selected.saturating_sub(start),
        Rect {
            x: content.x,
            y: div_y + 1,
            width: content.width,
            height: n,
        },
    );
}

/// Shared scrollable sheet renderer for the `/usage` and `/stats` overlays
/// (F.5): boxed title, a body of pre-built lines clipped by `scroll`, and a
/// footer. Body lines are built by pure helpers so layout stays testable.
fn render_usage_sheet(
    f: &mut Frame,
    area: Rect,
    title: &str,
    lines: &[Line<'static>],
    footer: Vec<Span<'static>>,
    scroll: usize,
) -> usize {
    dim(f, area);
    let height = (lines.len() as u16 + 4).min(area.height);
    let width = 64.min(area.width.saturating_sub(4));
    let rect = centered(width, height, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let w = inner.width.saturating_sub(2);
    // Title and footer claim two lines of `inner`; the body must fit in the
    // rest or it would spill over the dimmed chat on short terminals.
    let visible = (inner.height.saturating_sub(3)) as usize;
    let scroll = scroll.min(lines.len().saturating_sub(visible.min(lines.len())));
    let max_scroll = lines.len().saturating_sub(visible.min(lines.len()));
    let line = |l: Line<'static>| Paragraph::new(l);
    f.render_widget(
        line(Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme::current().text_primary)
                .add_modifier(Modifier::BOLD),
        ))),
        Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: w,
            height: 1,
        },
    );
    for (row, body) in lines.iter().skip(scroll).take(visible).enumerate() {
        f.render_widget(
            line(body.clone()),
            Rect {
                x: inner.x + 1,
                y: inner.y + 2 + row as u16,
                width: w,
                height: 1,
            },
        );
    }
    f.render_widget(
        line(Line::from(footer)),
        Rect {
            x: inner.x + 1,
            y: inner.y + inner.height.saturating_sub(1),
            width: w,
            height: 1,
        },
    );
    max_scroll
}

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

/// `/usage`: this session's per-model tokens and cost, plus the live
/// provider quota section (F.5).
pub fn render_usage(f: &mut Frame, app: &mut App, area: Rect) {
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
        lines.push(Line::raw(""));
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
    app.overlays.usage_scroll_max = render_usage_sheet(
        f,
        area,
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

/// `/stats`: cross-session cost totals from the cost ledger, with the
/// by-model table and top-sessions sections (F.5).
pub fn render_stats(f: &mut Frame, app: &mut App, area: Rect) {
    let t = theme::current();

    let Modal::Stats(view) = &app.overlays.modal else {
        return;
    };
    if view.empty {
        render_usage_sheet(
            f,
            area,
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
        lines.push(Line::raw(""));
        lines.push(section_label("Top sessions:"));
        lines.extend(view.by_session.iter().map(session_line));
    }
    let scroll = app.overlays.usage_scroll;
    app.overlays.usage_scroll_max = render_usage_sheet(
        f,
        area,
        "Cost stats",
        &lines,
        vec![
            Span::styled(
                format!(" total {}", crate::usage::format_tokens(view.total_tokens)),
                Style::default().fg(t.text_secondary),
            ),
            Span::styled(
                format!("  {}", crate::storage::stats::format_usd(view.total_cost)),
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
    render_mcp_sheet(f, &reader, *selected, area);
}

/// The `/mcp` sheet itself, drawn from any snapshot reader so tests can
/// feed it a hand-built snapshot. One row per server — name, transport,
/// tool/prompt counts on the left, status (color-coded) on the right —
/// plus a detail line carrying the auth URL of a NeedsAuth server.
fn render_mcp_sheet(
    f: &mut Frame,
    reader: &crate::mcp::McpSnapshotReader,
    selected: usize,
    area: Rect,
) {
    let t = theme::current();
    dim(f, area);
    let infos = reader.load().infos.clone();

    // Row plan: every server's header row (the one the selection marks),
    // plus a detail row for NeedsAuth / Failed servers.
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    let mut header_rows: Vec<usize> = Vec::new();
    for info in &infos {
        header_rows.push(rows.len());
        let (status_label, status_color) = mcp_status_style(info);
        let left = format!(
            " {} · {} · {} tools · {} prompts",
            info.name, info.transport_kind, info.tool_count, info.prompt_count
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
    }

    let width = 78.min(area.width.saturating_sub(4));
    let n = rows.len().clamp(1, 10) as u16;
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "MCP servers — t toggle, r reconnect, esc close",
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

/// `/help`: keybinding sheet, generated from the data-driven `KEYBINDS`
/// table grouped by context (F.1); user overrides show their effective
/// chords, disabled actions are omitted.
pub fn render_help(f: &mut Frame, app: &mut App, area: Rect) {
    let t = theme::current();

    if !matches!(app.overlays.modal, Modal::Help) {
        return;
    }
    let (lines, _) = crate::tui::keybindings::help_lines(&app.overlays.keybinds);
    let scroll = app.overlays.help_scroll;
    app.overlays.help_scroll_max = render_usage_sheet(
        f,
        area,
        "Keybindings",
        &lines,
        vec![Span::styled(
            "↑↓ scroll · esc to close",
            Style::default().fg(t.text_tertiary),
        )],
        scroll,
    );
}

/// `/sessions`: persisted-session picker; Enter loads, Esc closes.
pub fn render_sessions(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::Sessions { entries, selected } = &app.overlays.modal else {
        return;
    };
    dim(f, area);
    let n = entries.len().clamp(1, 8) as u16;
    let width = 60.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Sessions — enter to resume, esc to close",
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
    let content_w = inner.width.saturating_sub(2) as usize;
    let spans: Vec<Vec<Span<'static>>> = if entries.is_empty() {
        vec![vec![Span::styled(
            " no sessions yet".to_string(),
            Style::default().fg(t.text_tertiary),
        )]]
    } else {
        entries
            .iter()
            .map(|entry| {
                let updated = format!("{} ", entry.updated);
                let title_w = content_w.saturating_sub(updated.chars().count() + 1);
                let mut title: String = entry.title.chars().take(title_w).collect();
                if entry.title.chars().count() > title_w && title_w > 1 {
                    title.truncate(title_w - 1);
                    title.push('…');
                }
                let title = format!(" {title}");
                let gap = content_w.saturating_sub(title.chars().count() + updated.chars().count());
                vec![
                    Span::styled(title, Style::default().fg(t.text_primary)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(updated, Style::default().fg(t.text_tertiary)),
                ]
            })
            .collect()
    };
    render_rows(
        f,
        &spans,
        if entries.is_empty() {
            usize::MAX
        } else {
            *selected
        },
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}

/// `/recipe`: recipe picker (J.5); Enter runs the selection or prefills
/// `key=` parameter stubs, Esc closes.
pub fn render_recipes(f: &mut Frame, app: &App, area: Rect) {
    use crate::tui::modals::Modal;
    let t = theme::current();

    let Modal::Recipes { entries, selected } = &app.overlays.modal else {
        return;
    };
    dim(f, area);
    let n = entries.len().clamp(1, 8) as u16;
    let width = 60.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Recipes — enter to run, esc to close",
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
    let content_w = inner.width.saturating_sub(2) as usize;
    let spans: Vec<Vec<Span<'static>>> = if entries.is_empty() {
        vec![vec![Span::styled(
            " no recipes found (.craft/recipes/*.yaml)".to_string(),
            Style::default().fg(t.text_tertiary),
        )]]
    } else {
        entries
            .iter()
            .map(|entry| {
                let params = if entry.params.is_empty() {
                    String::new()
                } else {
                    format!(" {}", entry.params.join(", "))
                };
                let hint = format!("{params} ");
                let name_w = content_w.saturating_sub(hint.chars().count() + 1);
                let mut name: String = entry.name.chars().take(name_w).collect();
                if entry.name.chars().count() > name_w && name_w > 1 {
                    name.truncate(name_w - 1);
                    name.push('…');
                }
                let name = format!(" {name}");
                let gap = content_w.saturating_sub(name.chars().count() + hint.chars().count());
                vec![
                    Span::styled(name, Style::default().fg(t.text_primary)),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(hint, Style::default().fg(t.text_tertiary)),
                ]
            })
            .collect()
    };
    render_rows(
        f,
        &spans,
        if entries.is_empty() {
            usize::MAX
        } else {
            *selected
        },
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
}

/// `/theme`: bundled-theme picker. Arrows preview live, Enter applies +
/// persists, Esc restores the pre-open theme.
pub fn render_theme_picker(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::current();

    let Modal::ThemePicker {
        entries,
        selected,
        original,
    } = &app.overlays.modal
    else {
        return;
    };
    dim(f, area);
    if entries.is_empty() {
        return;
    }
    const MAX_VISIBLE: usize = 15;
    let visible = entries.len().clamp(1, MAX_VISIBLE);
    let start = (*selected).min(entries.len().saturating_sub(visible));
    let n = visible as u16;
    let width = 48.min(area.width.saturating_sub(4));
    let rect = centered(width, n + 4, area);
    f.render_widget(Clear, rect);
    let block = boxed(rect);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "Themes — ↑↓ preview, enter apply, esc cancel",
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
    let rows: Vec<Vec<Span<'static>>> = entries[start..start + visible]
        .iter()
        .map(|name| {
            let marker = if name == original { "● " } else { "  " };
            vec![
                Span::styled(
                    marker.to_string(),
                    Style::default().fg(if name == original {
                        t.accent
                    } else {
                        t.bg_raised
                    }),
                ),
                Span::styled(name.clone(), Style::default().fg(t.text_primary)),
            ]
        })
        .collect();
    render_rows(
        f,
        &rows,
        *selected - start,
        Rect {
            x: inner.x + 1,
            y: inner.y + 2,
            width: inner.width.saturating_sub(2),
            height: n,
        },
    );
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
            generation: 1,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_mcp_sheet(f, &reader, 0, f.area()))
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

    /// An empty snapshot (no servers configured) still renders the sheet
    /// with its placeholder row instead of panicking.
    #[test]
    fn mcp_sheet_renders_an_empty_placeholder() {
        let reader = McpSnapshotReader::empty();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|f| render_mcp_sheet(f, &reader, 0, f.area()))
            .unwrap();
        assert!(buffer_text(&terminal).contains("no MCP servers configured"));
    }
}
