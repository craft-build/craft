//! Fuzzy query overlays: the transcript search modal and the file picker.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use super::super::theme;
use super::{boxed, centered, dim, render_rows};
use crate::tui::app::App;

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
