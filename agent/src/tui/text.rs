//! The TUI's styled-text document model: [`Span`] and [`Line`].
//!
//! These are app-owned types (the same shape as the ratatui types they
//! replaced) used as the *storage* model everywhere: the transcript
//! (`scrollback.rs`), streaming caches, the wrap engine, and tests. Painting
//! is done by the UI layer — `slt` widgets for chrome, the transcript painter
//! for scrollback — neither of which needs ratatui-era text types.

use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

use super::style::Style;

/// A styled run of text.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Span<'a> {
    pub content: Cow<'a, str>,
    pub style: Style,
}

impl<'a> Span<'a> {
    /// An unstyled span.
    pub fn raw<T>(content: T) -> Self
    where
        T: Into<Cow<'a, str>>,
    {
        Self {
            content: content.into(),
            style: Style::default(),
        }
    }

    /// A span with `style`.
    pub fn styled<T>(content: T, style: Style) -> Self
    where
        T: Into<Cow<'a, str>>,
    {
        Self {
            content: content.into(),
            style,
        }
    }

    /// Display width of the span content, in terminal cells.
    pub fn width(&self) -> usize {
        self.content.width()
    }
}

/// A single line of text: a sequence of spans rendered left to right.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Line<'a> {
    pub spans: Vec<Span<'a>>,
}

impl<'a> Line<'a> {
    /// A line from a span list.
    pub fn from_spans(spans: Vec<Span<'a>>) -> Self {
        Self { spans }
    }

    /// Display width of the whole line, in terminal cells.
    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| s.width()).sum()
    }

    /// The line's concatenated content, unstyled.
    pub fn text(&self) -> String {
        let mut s = String::with_capacity(self.spans.iter().map(|s| s.content.len()).sum());
        for span in &self.spans {
            s.push_str(&span.content);
        }
        s
    }

    /// True when the line has no spans or all spans are empty.
    pub fn is_empty(&self) -> bool {
        self.spans.iter().all(|s| s.content.is_empty())
    }
}

impl<'a> From<Vec<Span<'a>>> for Line<'a> {
    fn from(spans: Vec<Span<'a>>) -> Self {
        Self { spans }
    }
}

impl<'a> From<Span<'a>> for Line<'a> {
    fn from(span: Span<'a>) -> Self {
        Self { spans: vec![span] }
    }
}

impl<'a> From<String> for Line<'a> {
    fn from(s: String) -> Self {
        Self::from(vec![Span::raw(s)])
    }
}

impl<'a> From<&'a str> for Line<'a> {
    fn from(s: &'a str) -> Self {
        Self::from(vec![Span::raw(s)])
    }
}

/// Merge consecutive spans that share a style into single runs.
///
/// Used after wrapping/slicing spans so the painter writes fewer, larger
/// runs (this preserves the run-length coalescing the old renderer did when
/// mutating `Cow` contents in place).
pub fn run_length_spans(spans: impl IntoIterator<Item = Span<'static>>) -> Vec<Span<'static>> {
    let mut merged: Vec<Span<'static>> = Vec::new();
    for span in spans {
        match merged.last_mut() {
            Some(last) if last.style == span.style => {
                last.content.to_mut().push_str(&span.content);
            }
            _ => merged.push(span),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::style::{Color, Modifiers, StyleExt};

    #[test]
    fn line_width_sums_spans() {
        let line = Line::from(vec![
            Span::raw("ab"),
            Span::styled("你好", Style::default()),
            Span::raw("c"),
        ]);
        assert_eq!(line.width(), 2 + 4 + 1);
    }

    #[test]
    fn line_text_concatenates() {
        let line = Line::from(vec![Span::raw("a"), Span::raw("b"), Span::raw("c")]);
        assert_eq!(line.text(), "abc");
    }

    #[test]
    fn run_length_merges_same_style() {
        let bold = Style::default().add_modifier(Modifiers::BOLD);
        let spans = vec![
            Span::styled("a", bold),
            Span::styled("b", bold),
            Span::raw("c"),
            Span::styled("d", Style::default().fg(Color::Red)),
        ];
        let merged = run_length_spans(spans);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].content, "ab");
        assert_eq!(merged[0].style, bold);
        assert_eq!(merged[2].content, "d");
    }
}
