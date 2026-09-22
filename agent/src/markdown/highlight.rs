//! Minimal stand-in for the reference `craft-highlight` crate, until
//! syntect-based highlighting lands (plan task 61). Emits one plain,
//! uncolored segment per line, but keeps the two behavioral contracts the
//! markdown renderer depends on: `CodeHighlighter::update` is incremental
//! (only lines that newly completed are re-tokenized) and `theme_generation`
//! gates cache flushing.

/// Resolved foreground color of a highlighted segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentColor {
    Default,
    Rgb(u8, u8, u8),
}

/// One highlighted run of text within a code line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledSegment {
    pub text: String,
    pub fg: SegmentColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl StyledSegment {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            fg: SegmentColor::Default,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

/// Bumped when the theme changes so cached highlight state can be flushed.
/// The facade has no themes, so this is constant.
pub fn theme_generation() -> u64 {
    0
}

fn tokenize_line(line: &str) -> Vec<StyledSegment> {
    vec![StyledSegment::plain(line)]
}

/// Logical lines of a code block: a trailing newline does not start a new
/// (empty) line, matching the reference's `LinesWithEndings` iteration.
fn logical_lines(code: &str) -> Vec<&str> {
    if code.is_empty() {
        return Vec::new();
    }
    let body = code.strip_suffix('\n').unwrap_or(code);
    body.split('\n').collect()
}

/// One-shot highlight of a whole code block.
pub fn highlight_block(_lang: &str, code: &str) -> Vec<Vec<StyledSegment>> {
    logical_lines(code).into_iter().map(tokenize_line).collect()
}

/// Segments and source byte length for every line ending before the last
/// `\n` (all complete lines; a still-streaming final line is excluded).
fn complete_lines(code: &str) -> (Vec<Vec<StyledSegment>>, usize) {
    match code.rfind('\n') {
        Some(nl) => (
            logical_lines(&code[..nl + 1])
                .into_iter()
                .map(tokenize_line)
                .collect(),
            nl + 1,
        ),
        None => (Vec::new(), 0),
    }
}

/// Stateful highlighter for a single growing code block. `update` re-emits
/// the segments of already-completed lines from cache and tokenizes only
/// the tail after the last newline.
pub struct CodeHighlighter {
    lang: String,
    completed: Vec<Vec<StyledSegment>>,
    completed_bytes: usize,
}

impl CodeHighlighter {
    pub fn new(lang: &str) -> Self {
        Self {
            lang: lang.to_owned(),
            completed: Vec::new(),
            completed_bytes: 0,
        }
    }

    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// Feed the current full text of the block; returns segments for every
    /// line, including the (possibly still-streaming) final line.
    pub fn update(&mut self, code: &str) -> Vec<Vec<StyledSegment>> {
        // Only re-tokenize when a new line completed (or the block shrank).
        let bytes = code.rfind('\n').map_or(0, |nl| nl + 1);
        if bytes != self.completed_bytes {
            let (complete, bytes) = complete_lines(code);
            self.completed = complete;
            self.completed_bytes = bytes;
        }
        // Still clones the completed prefix each call; the renderer wraps
        // this in per-block state so the cost is one Vec copy, not a
        // re-tokenization. True tail-only emission returns with syntect.
        let mut all = self.completed.clone();
        let tail = &code[self.completed_bytes.min(code.len())..];
        if !tail.is_empty() {
            all.push(tokenize_line(tail));
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_matches_incremental_final_update() {
        let code = "fn a() {}\nlet x = 1;\n";
        let mut h = CodeHighlighter::new("rust");
        assert_eq!(h.update(code), highlight_block("rust", code));
    }

    #[test]
    fn update_over_prefixes_converges() {
        let code = "fn main() {\n    let x = 1;\n}";
        let mut h = CodeHighlighter::new("rust");
        for end in 1..code.len() {
            if code.is_char_boundary(end) {
                let _ = h.update(&code[..end]);
            }
        }
        assert_eq!(h.update(code), highlight_block("rust", code));
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(highlight_block("x", "").is_empty());
        assert!(CodeHighlighter::new("x").update("").is_empty());
    }

    #[test]
    fn trailing_newline_has_no_streaming_line() {
        let lines = CodeHighlighter::new("x").update("a\nb\n");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn segments_are_plain_until_syntect_lands() {
        let lines = highlight_block("rust", "let x = 1;");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0][0].fg, SegmentColor::Default);
        assert!(!lines[0][0].bold);
    }

    #[test]
    fn theme_generation_is_stable() {
        assert_eq!(theme_generation(), theme_generation());
    }
}
