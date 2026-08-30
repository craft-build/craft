use crate::render_worker::RenderWorker;
use crate::theme;

use crate::wrap;

use super::super::code_view::SectionFlags;
use super::super::tool_display::{HighlightRequest, ToolLines};
use ratatui::text::{Line, Span};
use std::cell::Cell;

const INST_SUFFIX: &str = "__inst";

pub fn is_instruction_segment(id: &str) -> bool {
    id.ends_with(INST_SUFFIX)
}

pub fn instruction_id(parent_id: &str) -> String {
    format!("{parent_id}{INST_SUFFIX}")
}

pub fn instruction_parent(id: &str) -> Option<&str> {
    id.strip_suffix(INST_SUFFIX)
}

pub fn is_child_segment(id: &str) -> bool {
    id.contains("__")
}

#[derive(Clone, Copy, Default)]
struct CachedHeight {
    at_width: u16,
    height: u16,
}

#[derive(Default, PartialEq, Eq)]
struct HighlightKey {
    has_output: bool,
    theme_gen: u64,
}

impl HighlightKey {
    /// The generation is read here rather than passed in: a theme only swaps
    /// from `update`, never mid-`view`, and a missed call site would silently
    /// splice old-palette lines back in.
    fn from_request(hl: Option<&HighlightRequest>) -> Self {
        Self {
            has_output: hl.is_some_and(|h| h.output.is_some()),
            theme_gen: theme::generation(),
        }
    }
}

#[derive(Default)]
pub(super) struct Segment {
    lines: Vec<Line<'static>>,
    pub search_text: String,
    pub tool_id: Option<String>,
    pub msg_index: Option<usize>,
    pub truncation: SectionFlags,
    cached_height: Cell<Option<CachedHeight>>,
    pending_highlight: Option<u64>,
    highlight_range: Option<(usize, usize)>,
    highlight_key: HighlightKey,
    pub spinner_lines: Vec<usize>,
    pub content_indent: &'static str,
    pub indent_style: ratatui::style::Style,
    pub has_bar: bool,
    pub hyperlinks: Vec<crate::hyperlink::Hyperlink>,
    pub image: Option<std::sync::Arc<crate::image_render::ImageRenderState>>,
    /// The lines were built for another width or theme, so code blocks and
    /// tables are still wrapped to the old width and spans carry the old
    /// palette. Only the content ages, never the height, so a segment the
    /// reflow window never reaches merely looks dated; it cannot desync the
    /// layout from what is painted.
    ///
    /// Cleared by `set_lines` (whole vector replaced) and up front by
    /// `reflow_segment`; partial splices (`apply_highlight_result`) leave it
    /// set so the segment still reflows later.
    pub(super) stale: bool,
}

impl Segment {
    pub fn with_tool(tool_id: String, msg_index: Option<usize>) -> Self {
        Self {
            tool_id: Some(tool_id),
            msg_index,
            ..Self::default()
        }
    }

    pub fn spacer() -> Self {
        Self {
            lines: vec![Line::default()],
            ..Self::default()
        }
    }

    pub fn with_lines(
        lines: Vec<Line<'static>>,
        search_text: String,
        msg_index: Option<usize>,
    ) -> Self {
        Self {
            lines,
            search_text,
            msg_index,
            ..Self::default()
        }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        self.lines = lines;
        self.stale = false;
        self.invalidate_height();
    }

    pub fn set_image(
        &mut self,
        image: Option<std::sync::Arc<crate::image_render::ImageRenderState>>,
    ) {
        self.image = image;
        self.invalidate_height();
    }

    /// Rows the lines take at `width`, measured at that width whatever
    /// [`Self::stale`] says. Render, [`super::scroll::Layout`], the scrollbar
    /// and the copy path all read this one number, so none of them can
    /// disagree on how tall a segment is.
    pub fn height(&self, width: u16) -> u16 {
        if let Some(c) = self.cached_height.get()
            && c.at_width == width
        {
            return c.height;
        }
        let h = if let Some(img) = &self.image {
            let caption = wrap::total_rows(&self.lines, width);
            caption.saturating_add(img.rows)
        } else {
            wrap::total_rows(&self.lines, width)
        };
        self.cached_height.set(Some(CachedHeight {
            at_width: width,
            height: h,
        }));
        h
    }

    fn invalidate_height(&self) {
        self.cached_height.set(None);
    }

    pub fn update_spinner(&mut self, line_idx: usize, span_idx: usize, span: Span<'static>) {
        if let Some(line) = self.lines.get_mut(line_idx)
            && line.spans.len() > span_idx
        {
            line.spans[span_idx] = span;
        }
    }

    fn reuse_highlight(
        &self,
        key: &HighlightKey,
        new_range: (usize, usize),
    ) -> Option<Vec<Line<'static>>> {
        if self.pending_highlight.is_some() || self.highlight_key != *key {
            return None;
        }
        let (s, e) = self.highlight_range?;
        if s > e || e > self.lines.len() {
            return None;
        }
        if (e - s) != (new_range.1 - new_range.0) {
            return None;
        }
        Some(self.lines[s..e].to_vec())
    }

    pub fn apply_highlight(&mut self, tl: ToolLines, worker: &RenderWorker) {
        self.pending_highlight = tl.send_highlight(worker);
        self.highlight_range = tl.highlight.as_ref().map(|h| h.range);
        self.highlight_key = HighlightKey::from_request(tl.highlight.as_ref());
        self.spinner_lines = tl.spinner_lines;
        self.content_indent = tl.content_indent;
        self.indent_style = tl.indent_style;
        self.has_bar = tl.has_bar;
        self.truncation = tl.truncation;
        self.hyperlinks = tl.hyperlinks;
        self.set_lines(tl.lines);
    }

    pub fn update_with_reuse(&mut self, mut tl: ToolLines, worker: &RenderWorker) {
        let key = HighlightKey::from_request(tl.highlight.as_ref());
        let reused = tl.highlight.as_ref().and_then(|req| {
            let hl_lines = self.reuse_highlight(&key, req.range)?;
            let (s, _) = req.range;
            let new_end = s + hl_lines.len();
            tl.lines.splice(s..req.range.1, hl_lines);
            Some((s, new_end))
        });
        self.truncation = tl.truncation;
        if let Some((s, e)) = reused {
            self.set_lines(tl.lines);
            self.highlight_range = Some((s, e));
            self.pending_highlight = None;
            self.spinner_lines = tl.spinner_lines;
            self.content_indent = tl.content_indent;
            self.indent_style = tl.indent_style;
            self.has_bar = tl.has_bar;
            self.hyperlinks = tl.hyperlinks;
        } else {
            self.apply_highlight(tl, worker);
        }
    }

    pub fn matches_pending_highlight(&self, id: u64) -> bool {
        self.pending_highlight == Some(id)
    }

    pub fn apply_highlight_result(&mut self, lines: Vec<Line<'static>>) {
        if let Some((start, end)) = self.highlight_range {
            let indent = self.content_indent;
            let indent_style = self.indent_style;
            let indented: Vec<Line<'static>> = lines
                .into_iter()
                .map(|mut line| {
                    if !indent.is_empty() {
                        line.spans.insert(0, Span::styled(indent, indent_style));
                    }
                    line
                })
                .collect();
            let new_end = start + indented.len();
            self.lines.splice(start..end, indented);
            self.highlight_range = Some((start, new_end));
            self.invalidate_height();
        }
        self.pending_highlight = None;
    }
}

pub(super) struct SegmentCache {
    segments: Vec<Segment>,
    msg_count: usize,
}

impl SegmentCache {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            msg_count: 0,
        }
    }

    pub fn clear(&mut self) {
        self.segments.clear();
        self.msg_count = 0;
    }

    pub fn push(&mut self, seg: Segment) {
        self.segments.push(seg);
    }

    /// Books the spacer and instruction slots a tool fills in later, when its
    /// output finally arrives. Splicing them in at that point would shift
    /// every segment after them, and plenty of things hold a segment index by
    /// then: the scroll position, the search highlight, the scroll a search
    /// restores, the anchors of a live selection. Empty segments take no rows,
    /// so a pair nobody fills stays invisible.
    pub fn reserve_instructions(&mut self, tool_id: &str, msg_index: Option<usize>) {
        self.segments.push(Segment::default());
        self.segments
            .push(Segment::with_tool(instruction_id(tool_id), msg_index));
    }

    pub fn insert(&mut self, pos: usize, seg: Segment) {
        self.segments.insert(pos, seg);
    }

    pub fn remove(&mut self, idx: usize) -> Segment {
        self.segments.remove(idx)
    }

    pub fn needs_rebuild(&self, msg_len: usize) -> bool {
        self.msg_count != msg_len
    }

    pub fn mark_built(&mut self, count: usize) {
        self.msg_count = count;
    }

    pub fn msg_count(&self) -> usize {
        self.msg_count
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn segments_mut(&mut self) -> &mut [Segment] {
        &mut self.segments
    }

    pub fn get(&self, idx: usize) -> Option<&Segment> {
        self.segments.get(idx)
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Segment> {
        self.segments.get_mut(idx)
    }

    pub fn find_by_tool_id(&self, id: &str) -> Option<usize> {
        self.segments
            .iter()
            .rposition(|s| s.tool_id.as_deref() == Some(id))
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn push_spacer_if_needed(&mut self) {
        if !self.segments.is_empty() {
            self.segments.push(Segment::spacer());
        }
    }

    pub fn search_texts(&self) -> Vec<&str> {
        self.segments
            .iter()
            .map(|s| s.search_text.as_str())
            .collect()
    }

    pub fn mark_all_stale(&mut self) {
        for seg in &mut self.segments {
            seg.stale = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OTHER_THEME: &str = "dracula";

    #[test]
    fn reuse_highlight_keys_on_theme_not_width() {
        use crate::components::code_view::RenderLimits;
        use craft_agent::ToolOutput;
        use std::sync::Arc;

        let output = Arc::new(ToolOutput::ReadCode {
            path: "f.rs".into(),
            start_line: 1,
            lines: vec!["fn main() {}".into()],
            prefix: String::new(),
            total_lines: 1,
            instructions: None,
            no_compress: false,
        });
        let key = || {
            HighlightKey::from_request(Some(&HighlightRequest {
                range: (1, 3),
                input: None,
                output: Some(Arc::clone(&output)),
                limits: RenderLimits {
                    script: 0,
                    output: 0,
                },
            }))
        };
        let seg = Segment {
            highlight_key: key(),
            highlight_range: Some((1, 3)),
            lines: vec![
                Line::raw("h"),
                Line::raw("a"),
                Line::raw("b"),
                Line::raw("t"),
            ],
            ..Segment::default()
        };

        // Highlighted lines are source lines, not wrapped rows (the worker
        // job carries no width), so the key deliberately omits width.
        assert!(
            seg.reuse_highlight(&key(), (1, 3)).is_some(),
            "reuse must fire across a width change; highlight lines are width-independent"
        );

        theme::set(theme::load_by_name(OTHER_THEME).unwrap());
        assert!(
            seg.reuse_highlight(&key(), (1, 3)).is_none(),
            "theme mismatch must force a fresh highlight, not splice old-palette lines"
        );
    }
}
