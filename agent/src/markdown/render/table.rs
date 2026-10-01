//! Table layout: cell measurement, column-width constraint, and boxed /
//! compact table rendering.

use unicode_width::UnicodeWidthStr;

use super::super::{Emphasis, InlineSpan, SpanKind, parse_inline};
use super::{Line, LineKind, Span, StyleToken, wrap_spans_impl};

const MIN_COL_WIDTH: usize = 5;

fn cell_display_width(cell: &str) -> usize {
    parse_inline(cell).iter().map(|s| s.text.width()).sum()
}

fn constrain_col_widths(col_widths: &mut [usize], available: usize) {
    let total: usize = col_widths.iter().sum();
    if total <= available {
        return;
    }
    for w in col_widths.iter_mut() {
        *w = (*w * available / total).max(MIN_COL_WIDTH).min(*w);
    }
    let mut excess = col_widths.iter().sum::<usize>().saturating_sub(available);
    while excess > 0 {
        let max_w = col_widths.iter().copied().max().unwrap_or(0);
        if max_w <= MIN_COL_WIDTH {
            break;
        }
        for w in col_widths.iter_mut() {
            if excess == 0 {
                break;
            }
            if *w == max_w && *w > MIN_COL_WIDTH {
                *w -= 1;
                excess -= 1;
            }
        }
    }
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.width()).sum()
}

fn cell_spans(cell: &str, header: bool) -> Vec<Span> {
    parse_inline(cell)
        .into_iter()
        .map(
            |InlineSpan {
                 text,
                 kind,
                 emphasis,
             }| {
                let mut emphasis = emphasis;
                if header {
                    emphasis.bold = true;
                }
                let style = if kind == SpanKind::Code {
                    StyleToken::InlineCode
                } else {
                    StyleToken::Text
                };
                Span::with_emphasis(text, style, emphasis)
            },
        )
        .collect()
}

pub(super) fn render_table(
    rows: &[Vec<String>],
    header_end: usize,
    width: u16,
    persistent_widths: &mut Vec<usize>,
) -> Vec<Line> {
    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        return Vec::new();
    }

    let overhead = col_count * 3 + 1;
    let min_box_width = overhead + col_count * MIN_COL_WIDTH;
    if (width as usize) < min_box_width {
        return render_table_compact(rows, header_end, width);
    }

    let mut col_widths = vec![0usize; col_count];
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(cell_display_width(cell));
        }
    }

    let available = (width as usize) - overhead;

    persistent_widths.resize(persistent_widths.len().max(col_count), 0);
    for (i, w) in col_widths.iter_mut().enumerate() {
        persistent_widths[i] = persistent_widths[i].max(*w);
        *w = persistent_widths[i];
    }

    constrain_col_widths(&mut col_widths, available);

    let mut lines = Vec::new();

    let border = |left: &str, mid: &str, right: &str, fill: &str| -> Line {
        let mut spans = vec![Span::new(left, StyleToken::TableBorder)];
        for (i, &w) in col_widths.iter().enumerate() {
            spans.push(Span::new(fill.repeat(w + 2), StyleToken::TableBorder));
            if i < col_count - 1 {
                spans.push(Span::new(mid, StyleToken::TableBorder));
            }
        }
        spans.push(Span::new(right, StyleToken::TableBorder));
        Line {
            kind: LineKind::TableBorder,
            spans,
        }
    };

    lines.push(border("╭", "┬", "╮", "─"));

    for (ri, row) in rows.iter().enumerate() {
        let header = ri < header_end;

        let wrapped_cells: Vec<Vec<Vec<Span>>> = (0..col_count)
            .map(|c| {
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                wrap_spans_impl(
                    cell_spans(cell, header),
                    col_widths[c],
                    col_widths[c],
                    true,
                    true,
                )
            })
            .collect();

        let row_height = wrapped_cells.iter().map(|c| c.len()).max().unwrap_or(1);
        let row_emphasis = if header {
            Emphasis::BOLD
        } else {
            Emphasis::default()
        };

        for line_idx in 0..row_height {
            let mut spans = vec![Span::new("│ ", StyleToken::TableBorder)];
            for (c, &w) in col_widths.iter().enumerate() {
                let sub_line = wrapped_cells[c].get(line_idx);
                let content_width = sub_line.map_or(0, |sl| spans_width(sl));

                let pad = w.saturating_sub(content_width);

                if let Some(sl) = sub_line {
                    spans.extend(sl.iter().cloned());
                }
                spans.push(Span::with_emphasis(
                    " ".repeat(pad + 1),
                    StyleToken::Text,
                    row_emphasis,
                ));
                if c < col_count - 1 {
                    spans.push(Span::new("│ ", StyleToken::TableBorder));
                } else {
                    spans.push(Span::new("│", StyleToken::TableBorder));
                }
            }
            lines.push(Line {
                kind: LineKind::TableRow,
                spans,
            });
        }

        if ri + 1 < rows.len() {
            lines.push(border("├", "┼", "┤", "─"));
        }
    }

    lines.push(border("╰", "┴", "╯", "─"));

    lines
}

/// Fallback when the terminal is too narrow for box-drawing borders.
fn render_table_compact(rows: &[Vec<String>], header_end: usize, width: u16) -> Vec<Line> {
    const CELL_SEP: &str = " | ";
    let mut lines = Vec::new();
    for (ri, row) in rows.iter().enumerate() {
        let header = ri < header_end;
        let mut spans: Vec<Span> = Vec::new();
        for (c, cell) in row.iter().enumerate() {
            if c > 0 {
                spans.push(Span::new(CELL_SEP, StyleToken::TableBorder));
            }
            spans.extend(cell_spans(cell, header));
        }
        for row_spans in wrap_spans_impl(spans, width as usize, width as usize, true, true) {
            lines.push(Line {
                kind: LineKind::TableRow,
                spans: row_spans,
            });
        }
    }
    lines
}
