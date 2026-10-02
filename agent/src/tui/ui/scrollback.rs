//! Scrollback engine: the transcript as a segment-addressed document.
//!
//! Ported from the reference (`craft-ui/src/components/messages/{scroll.rs,
//! segment.rs}`), adapted to this repo: lines are pre-wrapped at build
//! width, so a segment's height is its line count; the width-keyed height
//! cache states the invariant that heights are only valid at the width the
//! lines were wrapped for (and serves the several walks a frame makes).

use std::cell::Cell;
use std::sync::Arc;

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

/// Immutable line chunks shared by cached rendering and scrollback segments.
#[derive(Clone, Debug, Default)]
pub struct SharedLines {
    stable: Arc<StableLines>,
    tail: Option<Arc<[Line<'static>]>>,
}

#[derive(Clone, Debug, Default)]
struct StableLines {
    chunks: Vec<IndexedChunk>,
    len: usize,
}

#[derive(Clone, Debug)]
struct IndexedChunk {
    lines: Arc<[Line<'static>]>,
    end: usize,
}

impl SharedLines {
    pub fn from_chunks(chunks: Vec<Arc<[Line<'static>]>>) -> Self {
        let mut lines = Self::default();
        for chunk in chunks {
            lines.push_chunk(chunk);
        }
        lines
    }

    /// Replace only the streaming tail, sharing the stable chunk index in O(1).
    pub fn with_tail(&self, tail: Arc<[Line<'static>]>) -> Self {
        Self {
            stable: Arc::clone(&self.stable),
            tail: Some(tail),
        }
    }

    /// Append stable rows without copying lines. A shared index is copied only
    /// when necessary to preserve an existing snapshot.
    pub fn push_chunk(&mut self, chunk: Arc<[Line<'static>]>) {
        if chunk.is_empty() {
            return;
        }
        let stable = Arc::make_mut(&mut self.stable);
        stable.len += chunk.len();
        stable.chunks.push(IndexedChunk {
            lines: chunk,
            end: stable.len,
        });
    }

    pub fn len(&self) -> usize {
        self.stable.len + self.tail.as_ref().map_or(0, |tail| tail.len())
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[cfg(test)]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.stable, &other.stable)
            && match (&self.tail, &other.tail) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Line<'static>> {
        self.stable
            .chunks
            .iter()
            .flat_map(|chunk| chunk.lines.iter())
            .chain(self.tail.iter().flat_map(|tail| tail.iter()))
    }

    /// Locate the first stable chunk by binary search, then walk only the
    /// requested rows (including the tail). Out-of-bounds ranges stop at EOF.
    pub fn range(&self, start: usize, count: usize) -> impl Iterator<Item = &Line<'static>> {
        let chunks = &self.stable.chunks;
        let first = chunks.partition_point(|chunk| chunk.end <= start);
        let preceding = if first == 0 { 0 } else { chunks[first - 1].end };
        chunks[first..]
            .iter()
            .flat_map(|chunk| chunk.lines.iter())
            .chain(self.tail.iter().flat_map(|tail| tail.iter()))
            .skip(start - preceding)
            .take(count)
    }
}

impl From<Vec<Line<'static>>> for SharedLines {
    fn from(lines: Vec<Line<'static>>) -> Self {
        Self::from_chunks(vec![lines.into()])
    }
}

impl PartialEq for SharedLines {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

/// One block of pre-wrapped lines (a message bubble, a tool card, a
/// spacer). Lines are one display row each, so row slicing is line slicing.
pub struct Segment {
    lines: SharedLines,
    /// Inline image rendered below the lines (F.6): occupies `rows` extra
    /// display rows, tracked by the same height cache.
    pub image: Option<std::sync::Arc<crate::tui::ui::image::ImageRenderState>>,
    cached_height: Cell<Option<CachedHeight>>,
}

impl Segment {
    pub fn with_lines(lines: Vec<Line<'static>>) -> Self {
        Self::with_shared_lines(lines.into())
    }

    /// Reuse immutable pre-wrapped lines without cloning their spans or text.
    pub fn with_shared_lines(lines: SharedLines) -> Self {
        Self {
            lines,
            image: None,
            cached_height: Cell::new(None),
        }
    }

    pub fn set_image(
        &mut self,
        image: Option<std::sync::Arc<crate::tui::ui::image::ImageRenderState>>,
    ) {
        self.image = image;
        self.cached_height.set(None);
    }

    pub fn lines(&self) -> &SharedLines {
        &self.lines
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub fn set_lines(&mut self, lines: Vec<Line<'static>>) {
        self.lines = lines.into();
        self.cached_height.set(None);
    }

    /// Rows the segment takes at `width`: the line count (lines arrive
    /// pre-wrapped), plus image rows. Cached so the several walks per frame
    /// agree, and so a stale width is never silently trusted.
    pub fn height(&self, width: u16) -> u16 {
        if let Some(c) = self.cached_height.get()
            && c.at_width == width
        {
            return c.height;
        }
        let h = self.lines.len().min(u16::MAX as usize) as u16;
        let h = self
            .image
            .as_ref()
            .map_or(h, |img| h.saturating_add(img.rows));
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

    /// Pulls `row` back inside its segment: a segment can shrink under a
    /// stored position, and the walkers read a row past its end as
    /// "nothing left", so the two must agree.
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
    fn shared_lines_reuse_allocation_and_invalidate_height() {
        use base64::Engine;

        let lines: Arc<[Line<'static>]> =
            vec![Line::raw(String::from("a")), Line::raw(String::from("b"))].into();
        let shared = SharedLines::from_chunks(vec![Arc::clone(&lines)]);
        let mut segment = Segment::with_shared_lines(shared.clone());
        let other = Segment::with_shared_lines(shared);
        assert!(Arc::ptr_eq(&lines, &segment.lines.stable.chunks[0].lines));
        assert!(segment.lines.ptr_eq(&other.lines));
        assert_eq!(segment.height(WIDTH), 2);
        assert_eq!(segment.height(WIDTH / 2), 2);

        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let source = base64::engine::general_purpose::STANDARD.encode(png.into_inner());
        let image = Arc::new(
            crate::tui::ui::image::ImagePicker::new()
                .render_state(&source, WIDTH)
                .unwrap(),
        );
        let image_rows = image.rows;
        segment.set_image(Some(image));
        assert!(segment.cached_height.get().is_none());
        assert_eq!(segment.height(WIDTH), 2 + image_rows);
        assert!(Arc::ptr_eq(&lines, &segment.lines.stable.chunks[0].lines));

        segment.set_lines(vec![Line::raw("replacement")]);
        assert!(segment.cached_height.get().is_none());
        assert_eq!(segment.height(WIDTH), 1 + image_rows);
        assert_eq!(other.height(WIDTH), 2);
        assert!(Arc::ptr_eq(&lines, &other.lines.stable.chunks[0].lines));

        segment.set_image(None);
        assert!(segment.cached_height.get().is_none());
        assert_eq!(segment.height(WIDTH), 1);
    }

    #[test]
    fn shared_line_ranges_cross_chunks_and_skip_empty_chunks() {
        let first: Arc<[Line<'static>]> = vec![Line::raw("a"), Line::raw("b")].into();
        let last: Arc<[Line<'static>]> = vec![Line::raw("c"), Line::raw("d")].into();
        let shared = SharedLines::from_chunks(vec![first.clone(), Arc::from([]), last.clone()]);
        assert_eq!(shared.len(), 4);
        assert_eq!(shared.iter().count(), 4);
        let range: Vec<_> = shared.range(1, 2).collect();
        assert!(std::ptr::eq(range[0], &first[1]));
        assert!(std::ptr::eq(range[1], &last[0]));
        assert_eq!(shared.range(3, usize::MAX).count(), 1);
        assert_eq!(shared.range(4, 1).count(), 0);
        assert_eq!(shared.range(usize::MAX, 1).count(), 0);
        assert_eq!(shared.range(0, 0).count(), 0);
        assert!(SharedLines::default().is_empty());
        assert_eq!(
            shared,
            SharedLines::from(vec![
                Line::raw("a"),
                Line::raw("b"),
                Line::raw("c"),
                Line::raw("d"),
            ])
        );
        assert_eq!(Segment::with_shared_lines(shared).height(WIDTH), 4);
    }

    #[test]
    fn shared_tails_reuse_index_and_ranges_cross_into_tail() {
        let stable = SharedLines::from(vec![Line::raw("a"), Line::raw("b")]);
        let tail: Arc<[Line<'static>]> = vec![Line::raw("c"), Line::raw("d")].into();
        let first = stable.with_tail(tail.clone());
        let replacement = first.with_tail(vec![Line::raw("e")].into());
        assert!(Arc::ptr_eq(&stable.stable, &first.stable));
        assert!(Arc::ptr_eq(&first.stable, &replacement.stable));
        assert!(first.ptr_eq(&first.clone()));
        assert!(!first.ptr_eq(&replacement));
        assert_eq!(first.len(), 4);
        assert_eq!(replacement.len(), 3);
        let range: Vec<_> = first.range(1, 3).collect();
        assert!(std::ptr::eq(range[0], &stable.stable.chunks[0].lines[1]));
        assert!(std::ptr::eq(range[1], &tail[0]));
        assert!(std::ptr::eq(range[2], &tail[1]));
        assert_eq!(first.range(3, usize::MAX).count(), 1);
        assert_eq!(first.range(4, 1).count(), 0);
        assert_eq!(first.range(usize::MAX, 1).count(), 0);
        assert_eq!(first.range(0, 0).count(), 0);
        assert_eq!(first.with_tail(Arc::from([])), stable);
        let tail_only = SharedLines::default().with_tail(tail);
        assert_eq!(tail_only.range(1, 3).count(), 1);
        assert_eq!(
            first,
            SharedLines::from(vec![
                Line::raw("a"),
                Line::raw("b"),
                Line::raw("c"),
                Line::raw("d"),
            ])
        );
    }

    #[test]
    fn shared_chunk_append_preserves_snapshots_and_reuses_unique_index() {
        let mut stable = SharedLines::from(vec![Line::raw("a")]);
        let index = Arc::as_ptr(&stable.stable);
        stable.push_chunk(vec![Line::raw("b")].into());
        assert_eq!(index, Arc::as_ptr(&stable.stable));
        let snapshot = stable.with_tail(vec![Line::raw("tail")].into());
        let chunk: Arc<[Line<'static>]> = vec![Line::raw("c")].into();
        stable.push_chunk(chunk.clone());
        assert!(!Arc::ptr_eq(&stable.stable, &snapshot.stable));
        assert!(Arc::ptr_eq(&chunk, &stable.stable.chunks[2].lines));
        assert_eq!(stable.len(), 3);
        assert_eq!(
            snapshot,
            SharedLines::from(vec![Line::raw("a"), Line::raw("b"), Line::raw("tail"),])
        );
        assert_eq!(
            stable
                .stable
                .chunks
                .iter()
                .map(|c| c.end)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
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
