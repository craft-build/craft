//! Code-highlight caching and code-block layout: per-block `CodeHighlighter`
//! state for streaming renders, span coalescing for token stability, and
//! code-bar line wrapping.

use std::mem;

use unicode_width::UnicodeWidthStr;

use super::super::highlight::{CodeHighlighter, StyledSegment};
use super::{CODE_BAR, CODE_BAR_WRAP, Line, LineKind, Span, StyleToken, wrap_spans_impl};

impl super::RenderState<'_> {
    /// A streaming block keeps growing, so it advances its own highlighter and
    /// pays only for the lines that just arrived. Everyone else renders text
    /// that is already finished, where the shared content cache turns a
    /// re-render at a new width into a lookup.
    pub(super) fn code_segments(&mut self, lang: &str, code: &str) -> Vec<Vec<StyledSegment>> {
        if !self.incremental {
            return super::super::highlight::highlight_block(lang, code);
        }
        if self.code_idx >= self.highlighters.len() {
            self.highlighters.push(CodeHighlighter::new(lang));
        }
        self.highlighters[self.code_idx].update(code)
    }
}

/// Streaming can split tokens differently than a oneshot render because the
/// highlighter sees partial input. Merging identical neighbours keeps the
/// span shape stable.
pub(super) fn coalesce_adjacent_spans(spans: &mut Vec<Span>) {
    if spans.len() < 2 {
        return;
    }
    let mut write = 0;
    for read in 1..spans.len() {
        if spans[write].style == spans[read].style && spans[write].emphasis == spans[read].emphasis
        {
            let tail = mem::take(&mut spans[read].text);
            spans[write].text.push_str(&tail);
        } else {
            write += 1;
            if write != read {
                spans.swap(write, read);
            }
        }
    }
    spans.truncate(write + 1);
}

pub(super) fn wrap_code_lines(lines: &mut Vec<Line>, start: usize, width: u16) {
    let width = width as usize;
    if width == 0 {
        return;
    }
    let tail = lines.split_off(start);
    for line in tail {
        if line.width() <= width {
            lines.push(line);
        } else {
            lines.extend(split_line_with_bar(line, width));
        }
    }
}

fn split_line_with_bar(line: Line, width: usize) -> Vec<Line> {
    if line.spans.is_empty() {
        return vec![line];
    }

    let bar_span = line.spans[0].clone();
    let content_spans: Vec<Span> = line.spans[1..].to_vec();
    let first_avail = width.saturating_sub(CODE_BAR.width());
    let cont_avail = width.saturating_sub(CODE_BAR_WRAP.width());

    let rows = wrap_spans_impl(content_spans, first_avail, cont_avail, false, false);

    let mut result: Vec<Line> = Vec::new();
    for (i, mut row) in rows.into_iter().enumerate() {
        let mut spans = Vec::with_capacity(row.len() + 1);
        if i == 0 {
            spans.push(bar_span.clone());
        } else {
            spans.push(Span::new(CODE_BAR_WRAP, StyleToken::CodeBar));
        }
        spans.append(&mut row);
        result.push(Line {
            kind: LineKind::Code,
            spans,
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::super::Emphasis;
    use super::*;

    #[test]
    fn coalesce_merges_same_style_and_splits_different() {
        let mut spans = vec![
            Span::new("aa", StyleToken::Text),
            Span::new("bb", StyleToken::Text),
            Span::new("cc", StyleToken::InlineCode),
            Span::new("dd", StyleToken::InlineCode),
            Span::new("ee", StyleToken::Text),
        ];
        coalesce_adjacent_spans(&mut spans);
        assert_eq!(spans.len(), 3, "three groups after coalesce");
        assert_eq!(spans[0].text, "aabb");
        assert_eq!(spans[0].style, StyleToken::Text);
        assert_eq!(spans[1].text, "ccdd");
        assert_eq!(spans[1].style, StyleToken::InlineCode);
        assert_eq!(spans[2].text, "ee");
        assert_eq!(spans[2].style, StyleToken::Text);
    }

    #[test]
    fn coalesce_does_not_merge_different_emphasis() {
        let mut spans = vec![
            Span::new("plain", StyleToken::Text),
            Span::with_emphasis("bold", StyleToken::Text, Emphasis::BOLD),
        ];
        coalesce_adjacent_spans(&mut spans);
        assert_eq!(spans.len(), 2, "different emphasis must not merge");
    }
}
