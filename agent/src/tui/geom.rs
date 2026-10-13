//! App-owned terminal geometry: a `u16`-based [`Rect`].
//!
//! The TUI's layout math (scrollback doc rows, hit-test regions, selection
//! anchors, overlay centering) is written against 16-bit coordinates. `slt`'s
//! own `Rect` uses `u32`, so the app keeps this small native type as its
//! internal currency and converts only at the paint boundary
//! ([`Rect::to_slt`] / [`Rect::from_slt`]).

/// A rectangular region of the terminal grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    /// Construct a rect from position and size.
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Cell count of the region.
    #[allow(dead_code)] // foundation API; consumed by the paint pivot
    pub const fn area(&self) -> u16 {
        self.width.saturating_mul(self.height)
    }

    /// One past the rightmost column (clamped; mirrors ratatui semantics).
    #[allow(dead_code)] // foundation API; consumed by the paint pivot
    pub const fn right(&self) -> u16 {
        self.x.saturating_add(self.width)
    }

    /// One past the bottommost row (clamped; mirrors ratatui semantics).
    pub const fn bottom(&self) -> u16 {
        self.y.saturating_add(self.height)
    }

    /// True when the region has no area.
    #[allow(dead_code)] // foundation API; consumed by the paint pivot
    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// True when `(row, col)` lies inside the region (inclusive of origin,
    /// exclusive of the far edge).
    pub const fn contains(&self, row: u16, col: u16) -> bool {
        row >= self.y && row < self.bottom() && col >= self.x && col < self.right()
    }

    /// Shrink by removing `dx` columns on each side and `dy` rows top+bottom,
    /// clamped at zero.
    #[allow(dead_code)] // foundation API; consumed by the paint pivot
    pub fn shrink(&self, dx: u16, dy: u16) -> Self {
        let x = self.x.saturating_add(dx);
        let y = self.y.saturating_add(dy);
        Self {
            x,
            y,
            width: self.width.saturating_sub(dx.saturating_mul(2)),
            height: self.height.saturating_sub(dy.saturating_mul(2)),
        }
    }

    /// Convert to the `slt` (`u32`-field) rect used at the paint boundary.
    pub fn to_slt(self) -> slt::Rect {
        slt::Rect::new(
            self.x as u32,
            self.y as u32,
            self.width as u32,
            self.height as u32,
        )
    }

    /// Convert from an `slt` rect, saturating overflowing coordinates.
    pub fn from_slt(r: slt::Rect) -> Self {
        Self::new(
            r.x.min(u16::MAX as u32) as u16,
            r.y.min(u16::MAX as u32) as u16,
            r.width.min(u16::MAX as u32) as u16,
            r.height.min(u16::MAX as u32) as u16,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_rowcol() {
        let r = Rect::new(5, 2, 10, 4);
        assert!(r.contains(2, 5));
        assert!(r.contains(5, 14));
        assert!(r.contains(2, 5));
        assert!(!r.contains(9, 5));
        assert!(!r.contains(2, 15));
    }

    #[test]
    fn slt_roundtrip() {
        let r = Rect::new(1, 2, 30, 40);
        assert_eq!(Rect::from_slt(r.to_slt()), r);
    }
}
