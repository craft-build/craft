//! Scrollback engine: the transcript as a segment-addressed document.
//!
//! Ported from the reference (`craft-ui/src/components/messages/{scroll.rs,
//! segment.rs}`), adapted to this repo: lines are pre-wrapped at build
//! width, so a segment's height is simply its line count, but the
//! width-keyed height cache is kept to state the invariant that heights
//! are only valid at the width the lines were wrapped for (and to serve
//! the several walks a frame makes over the same segments).

use std::cell::Cell;

use ratatui::text::Line;

/// Top of the viewport as a place in the document: index into the segment
/// cache plus the row within that segment.
///
/// Nothing here depends on the width, so a resize is not a scroll: the
/// anchor segment survives a re-wrap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScrollPos {
    pub seg: usize,
    pub row: u16,
}

#[derive(Clone, Copy)]
struct CachedHeight {
    at_width: u16,
    height: u16,
}

/// One block of pre-wrapped lines (a message bubble, a tool card, a
/// spacer). Lines are one display row each, so rows == lines and row
/// slicing is line slicing.
pub struct Segment {
    lines: Vec<Line<'static>>,
    cached_height: Cell<Option<CachedHeight>>,
}

impl Segment {
    pub fn with_lines(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            cached_height: Cell::new(None),
        }
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        self.lines = lines;
        self.cached_height.set(None);
    }

    /// Rows the segment takes at `width`. Lines arrive pre-wrapped for the
    /// current width, so this is the line count; the cache exists so the
    /// several walks per frame agree without re-measuring, and so a stale
    /// width (lines wrapped for another width) is never silently trusted.
    pub fn height(&self, width: u16) -> u16 {
        if let Some(c) = self.cached_height.get()
            && c.at_width == width
        {
            return c.height;
        }
        let h = self.lines.len().min(u16::MAX as usize) as u16;
        self.cached_height.set(Some(CachedHeight {
            at_width: width,
            height: h,
        }));
        h
    }
}

/// The document: ordered segments. Refilled by the renderer each frame;
/// stored positions address it by (segment, row) so the refill itself is
/// not a scroll either — segment order is deterministic in message order.
#[derive(Default)]
pub struct SegmentCache {
    segments: Vec<Segment>,
}

impl SegmentCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.segments.clear();
    }

    pub fn push(&mut self, seg: Segment) {
        self.segments.push(seg);
    }

    pub fn len(&self) -> usize {
        self.segments.len()
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn get(&self, idx: usize) -> Option<&Segment> {
        self.segments.get(idx)
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut Segment> {
        self.segments.get_mut(idx)
    }
}

/// One frame's document view: the segment cache at a given width. Every
/// row walk goes through here, so all readers count rows the same way.
pub struct Layout<'a> {
    cache: &'a SegmentCache,
    width: u16,
}

impl<'a> Layout<'a> {
    pub fn new(cache: &'a SegmentCache, width: u16) -> Self {
        Self { cache, width }
    }

    fn height(&self, i: usize) -> u16 {
        self.cache.get(i).map_or(0, |s| s.height(self.width))
    }

    /// One past the last addressable row, so `retreat` from here is "the
    /// last N rows of the document".
    pub fn end(&self) -> ScrollPos {
        ScrollPos {
            seg: self.cache.len(),
            row: 0,
        }
    }

    /// Pulls `row` back inside its segment. A segment can shrink under a
    /// stored position (content update, collapse), and the walkers read a
    /// row past its end as "nothing left", so the position must be kept
    /// in range for the two to agree.
    pub fn clamp(&self, pos: ScrollPos) -> ScrollPos {
        ScrollPos {
            seg: pos.seg.min(self.cache.len()),
            row: pos
                .row
                .min(self.height(pos.seg.min(self.cache.len())).saturating_sub(1)),
        }
    }

    /// Costs the number of segments crossed, not the number of rows, so a
    /// wheel tick stays cheap however tall the transcript is.
    pub fn advance(&self, mut pos: ScrollPos, mut rows: u32) -> ScrollPos {
        while pos.seg < self.cache.len() {
            let left = u32::from(self.height(pos.seg).saturating_sub(pos.row));
            if rows < left {
                pos.row += rows as u16;
                return pos;
            }
            rows -= left;
            pos = ScrollPos {
                seg: pos.seg + 1,
                row: 0,
            };
        }
        self.end()
    }

    pub fn retreat(&self, mut pos: ScrollPos, mut rows: u32) -> ScrollPos {
        while rows > 0 {
            if u32::from(pos.row) >= rows {
                pos.row -= rows as u16;
                return pos;
            }
            rows -= u32::from(pos.row);
            if pos.seg == 0 {
                return ScrollPos::default();
            }
            pos.seg -= 1;
            pos.row = self.height(pos.seg);
        }
        pos
    }

    /// The lowest position that still fills the viewport.
    pub fn bottom(&self, viewport: u16) -> ScrollPos {
        self.retreat(self.end(), u32::from(viewport))
    }

    /// Rows between two positions, or 0 when `to` is not below `from`.
    /// Only the segments in between are walked, so projecting a position
    /// into the viewport costs what is on screen.
    pub fn rows_from(&self, from: ScrollPos, to: ScrollPos) -> u32 {
        if to <= from {
            return 0;
        }
        (from.seg..to.seg.min(self.cache.len()))
            .map(|i| u32::from(self.height(i)))
            .fold(u32::from(to.row), u32::saturating_add)
            .saturating_sub(u32::from(from.row))
    }

    /// O(transcript) row offset of a position from the top; reads cached
    /// heights rather than re-wrapping.
    pub fn doc_row(&self, pos: ScrollPos) -> u32 {
        self.rows_from(ScrollPos::default(), self.clamp(pos))
    }

    /// The scrollbar and future consumers need the whole-document row
    /// count; it reads cached heights rather than re-wrapping.
    #[cfg_attr(not(test), expect(dead_code))]
    pub fn total_rows(&self) -> u32 {
        self.doc_row(self.end())
    }

    /// The inverse of [`Self::doc_row`].
    pub fn at_row(&self, doc_row: u32) -> ScrollPos {
        self.advance(ScrollPos::default(), doc_row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u16 = 80;

    fn cache(heights: &[u16]) -> SegmentCache {
        let mut cache = SegmentCache::new();
        for &h in heights {
            let lines = (0..h).map(|i| Line::raw(format!("l{i}"))).collect();
            cache.push(Segment::with_lines(lines));
        }
        cache
    }

    fn pos(seg: usize, row: u16) -> ScrollPos {
        ScrollPos { seg, row }
    }

    #[test]
    fn height_is_the_line_count_and_caches_per_width() {
        let mut c = SegmentCache::new();
        assert!(c.is_empty());
        c.push(Segment::with_lines(vec![Line::raw("a"), Line::raw("b")]));
        assert_eq!(c.get(0).unwrap().height(80), 2);
        // Same width hits the cache; the number is width-independent here
        // only because lines arrive pre-wrapped for that width.
        assert_eq!(c.get(0).unwrap().height(80), 2);
        c.get_mut(0).unwrap().set_lines(vec![Line::raw("x")]);
        assert_eq!(c.get(0).unwrap().height(80), 1, "set_lines invalidates");
    }

    #[test]
    fn advance_walks_rows() {
        let c = cache(&[3, 1, 2]);
        let l = Layout::new(&c, WIDTH);
        assert_eq!(l.advance(pos(0, 0), 0), pos(0, 0), "zero rows stays");
        assert_eq!(l.advance(pos(0, 0), 2), pos(0, 2), "inside first segment");
        assert_eq!(
            l.advance(pos(0, 0), 3),
            pos(1, 0),
            "boundary lands on next start"
        );
        assert_eq!(l.advance(pos(0, 1), 4), pos(2, 1), "crosses two segments");
        assert_eq!(l.advance(pos(1, 0), 99), pos(3, 0), "clamps at the end");
    }

    #[test]
    fn retreat_walks_rows() {
        let c = cache(&[3, 1, 2]);
        let l = Layout::new(&c, WIDTH);
        assert_eq!(l.retreat(pos(2, 1), 1), pos(2, 0), "inside a segment");
        assert_eq!(
            l.retreat(pos(2, 0), 1),
            pos(1, 0),
            "into the previous segment"
        );
        assert_eq!(
            l.retreat(pos(2, 0), 2),
            pos(0, 2),
            "across a one-row segment"
        );
        assert_eq!(l.retreat(pos(1, 0), 99), pos(0, 0), "clamps at the start");
    }

    #[test]
    fn rows_from_counts_down() {
        let c = cache(&[3, 2]);
        let l = Layout::new(&c, WIDTH);
        assert_eq!(l.rows_from(pos(0, 2), pos(1, 1)), 2, "counts rows between");
        assert_eq!(
            l.rows_from(pos(1, 1), pos(0, 2)),
            0,
            "target above never underflows"
        );
    }

    #[test]
    fn at_row_and_doc_row_round_trip() {
        let c = cache(&[3, 1, 2]);
        let l = Layout::new(&c, WIDTH);
        assert_eq!(l.total_rows(), 6);
        assert_eq!(l.doc_row(pos(1, 0)), 3);
        for row in 0..l.total_rows() {
            assert_eq!(l.doc_row(l.at_row(row)), row, "row {row}");
        }
    }

    #[test]
    fn bottom_fills_the_viewport_from_the_end() {
        let c = cache(&[3, 1, 2]);
        let l = Layout::new(&c, WIDTH);
        assert_eq!(l.bottom(2), pos(2, 0));
        assert_eq!(l.bottom(6), pos(0, 0));
        assert_eq!(l.bottom(99), pos(0, 0), "viewport taller than the document");
    }

    #[test]
    fn clamp_pulls_a_shrunk_position_back_inside() {
        let c = cache(&[3, 2]);
        let l = Layout::new(&c, WIDTH);
        assert_eq!(l.clamp(pos(1, 9)), pos(1, 1));
        assert_eq!(l.clamp(pos(7, 0)), pos(2, 0), "past-the-end segment clamps");
    }

    #[test]
    fn resize_keeps_the_anchor_segment() {
        let mut c = cache(&[3, 1, 2]);
        let stored = ScrollPos { seg: 1, row: 0 };
        // Re-wrap at a narrower width: the anchored segment re-renders
        // taller; the stored position stays on the same segment.
        c.get_mut(1)
            .unwrap()
            .set_lines((0..5).map(|i| Line::raw(format!("n{i}"))).collect());
        let narrow = Layout::new(&c, 40);
        assert_eq!(narrow.clamp(stored).seg, 1);
        assert_eq!(narrow.clamp(stored).row, 0);
    }
}
