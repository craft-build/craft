//! When the UI paints.
//!
//! Three ideas, and keeping them apart is what keeps the loop cheap:
//!
//! - [`Dirty`]: something changed what is on screen, so a frame is owed.
//!   Every event the loop handles implies it.
//! - [`Watch`]: how a poller notices a background thread writing to a shared
//!   slot, the one kind of change nothing else announces.
//! - [`Cadence`]: how soon the loop has to come back when no event will wake
//!   it, and whether the clock alone owes a frame when it does.
//!
//! Ported from the reference craft-ui `repaint.rs`, adapted to this repo's
//! tokio-based loop: the loop sleeps one cadence frame per turn instead of
//! selecting over every channel inside a single selector.
//
// Some primitives (Watch, SMOOTH, PENDING) are ported ahead of their first
// consumers — markdown streaming, scrollback, usage polling land in later
// Phase 7 tasks — so dead-code lints stay quiet until they arrive.
#![allow(dead_code)]

use std::ops::{BitOr, BitOrAssign};
use std::sync::Arc;
use std::time::Duration;

/// One spinner glyph every 80ms — faster redraws paint the same glyph.
/// Matches the status spinner in `ui/composer.rs`.
pub const SPINNER_FRAME: Duration = Duration::from_millis(80);

/// How long the loop may sleep when nothing is animating. Real events wake it
/// immediately, so this only bounds how stale a polled result can get.
pub const IDLE_POLL: Duration = Duration::from_millis(100);

/// Fast enough that per-character reveals and colour fades look continuous.
const SMOOTH_FRAME: Duration = Duration::from_millis(16);

/// What every poller is tested against, so the two invariants read the same
/// wherever they are checked.
#[cfg(test)]
pub(crate) mod expect {
    pub(crate) const OWED: &str = "the change must owe a frame or the screen stays stale";
    pub(crate) const QUIET: &str = "nothing changed, so no frame may be owed";
}

/// Something changed what is on screen, so a frame is owed.
///
/// `#[must_use]` is the whole point of the newtype: a poller added to the
/// loop a year from now cannot quietly skip its repaint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct Dirty(bool);

impl Dirty {
    pub const NO: Self = Self(false);
    pub const YES: Self = Self(true);

    /// Runs every element. Pollers have side effects, so short-circuiting
    /// would stop draining channels as soon as one reported a change.
    pub fn any(flags: impl IntoIterator<Item = Self>) -> Self {
        flags.into_iter().fold(Self::NO, Self::bitor)
    }

    /// Clears the flag as it reports, so the debt is paid exactly once.
    pub fn take(&mut self) -> bool {
        std::mem::take(self).0
    }
}

impl From<bool> for Dirty {
    fn from(changed: bool) -> Self {
        Self(changed)
    }
}

impl BitOr for Dirty {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for Dirty {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

/// The last value seen in a shared slot a background thread publishes to, so
/// `view` renders from here and never loads the slot itself.
///
/// Every publish stores a fresh `Arc`, and holding the last one alive is what
/// stops its address being reused, so comparing pointers is exact. That asks
/// nothing of the slot: no `PartialEq`, no generation to remember to bump,
/// and no stand-in like a length, which misses a same-sized republish.
pub struct Watch<T>(Option<Arc<T>>);

impl<T> Watch<T> {
    /// Starts on what the slot already holds, so the first poll reports an
    /// arrival and not the watcher's own birth.
    pub fn seeded(latest: impl Into<Option<Arc<T>>>) -> Self {
        Self(latest.into())
    }

    pub fn poll(&mut self, latest: impl Into<Option<Arc<T>>>) -> Dirty {
        let latest = latest.into();
        if self.0.as_ref().map(Arc::as_ptr) == latest.as_ref().map(Arc::as_ptr) {
            return Dirty::NO;
        }
        self.0 = latest;
        Dirty::YES
    }

    pub fn get(&self) -> Option<&T> {
        self.0.as_deref()
    }
}

impl<T> Default for Watch<T> {
    fn default() -> Self {
        Self(None)
    }
}

/// How soon the loop has to come back when nothing is due to wake it.
///
/// Pixels that move on their own ([`Cadence::SPINNER`], [`Cadence::SMOOTH`])
/// owe a frame once the time is up. A worker mid-answer ([`Cadence::PENDING`])
/// owes none: looking again is the only way to find the result, and the frame
/// follows only if the poll came back [`Dirty`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cadence {
    frame: Option<Duration>,
    moves: bool,
}

impl Cadence {
    /// Nothing to come back for. The loop sleeps until something happens.
    pub const IDLE: Self = Self {
        frame: None,
        moves: false,
    };
    /// Spinner glyphs, which advance once per [`SPINNER_FRAME`]. Painting any
    /// faster redraws the same glyph.
    pub const SPINNER: Self = Self {
        frame: Some(SPINNER_FRAME),
        moves: true,
    };
    /// Typewriter reveals and colour fades, which change every frame.
    pub const SMOOTH: Self = Self {
        frame: Some(SMOOTH_FRAME),
        moves: true,
    };
    /// A worker mid-answer, looked at again on the smooth frame so a result
    /// landing between two frames is on screen in the next one.
    pub const PENDING: Self = Self {
        frame: Some(SMOOTH_FRAME),
        moves: false,
    };

    /// `cadence` while `applies`, else [`Cadence::IDLE`].
    pub fn when(applies: bool, cadence: Self) -> Self {
        if applies { cadence } else { Self::IDLE }
    }

    /// Combines both axes: the soonest anyone has to be looked at again, and
    /// motion if anyone moves. A single scale would let a pending poll outrank
    /// a typewriter and freeze it.
    pub fn any(cadences: impl IntoIterator<Item = Self>) -> Self {
        cadences.into_iter().fold(Self::IDLE, |a, b| Self {
            frame: match (a.frame, b.frame) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            moves: a.moves | b.moves,
        })
    }

    /// How long the loop may sleep, or `None` to sleep until something wakes
    /// it.
    pub fn frame(self) -> Option<Duration> {
        self.frame
    }

    /// Whether the next frame differs from this one on the clock alone.
    pub fn moves(self) -> bool {
        self.moves
    }
}

#[cfg(test)]
mod tests {
    use super::expect::{OWED, QUIET};
    use super::{Cadence, Dirty, Watch};
    use std::sync::Arc;

    const EXPECT_SETTLED: &str = "nothing to combine must leave the loop asleep";
    const EXPECT_SOONEST: &str = "the soonest source must win over the whole list";
    const EXPECT_MOTION_KEPT: &str = "a pending poll must not swallow another source's motion";
    const EXPECT_UNSEEN: &str = "an unpolled watch has nothing to render";

    #[test]
    fn any_runs_every_poller() {
        let mut ran = 0;
        let dirty = Dirty::any(
            [Dirty::YES, Dirty::NO, Dirty::NO]
                .into_iter()
                .inspect(|_| ran += 1),
        );
        assert_eq!(dirty, Dirty::YES);
        assert_eq!(ran, 3, "a reported change must not stop later pollers");
    }

    #[test]
    fn take_pays_the_debt_once() {
        let mut dirty = Dirty::YES;
        assert!(dirty.take());
        assert!(!dirty.take());
    }

    /// The empty fold is the common case: a settled session takes it on every
    /// loop turn.
    #[test]
    fn any_takes_the_soonest_frame_and_keeps_the_motion() {
        assert_eq!(Dirty::any([]), Dirty::NO, "{EXPECT_SETTLED}");
        assert_eq!(Cadence::any([]), Cadence::IDLE, "{EXPECT_SETTLED}");

        let picked = Cadence::any([Cadence::IDLE, Cadence::SMOOTH, Cadence::SPINNER]);
        assert_eq!(picked.frame(), Cadence::SMOOTH.frame(), "{EXPECT_SOONEST}");

        let mixed = Cadence::any([Cadence::SPINNER, Cadence::PENDING]);
        assert_eq!(mixed.frame(), Cadence::PENDING.frame(), "{EXPECT_SOONEST}");
        assert!(mixed.moves(), "{EXPECT_MOTION_KEPT}");
    }

    /// Identity asks nothing of the publisher, and an equal value republished
    /// is still a change worth a frame.
    #[test]
    fn watch_reports_each_arrival_once() {
        let mut watch = Watch::default();
        assert_eq!(watch.get(), None, "{EXPECT_UNSEEN}");
        assert_eq!(watch.poll(None), Dirty::NO, "{QUIET}");

        let first = Arc::new(7);
        assert_eq!(watch.poll(Arc::clone(&first)), Dirty::YES, "{OWED}");
        assert_eq!(watch.get(), Some(&7));
        assert_eq!(watch.poll(first), Dirty::NO, "{QUIET}");

        assert_eq!(watch.poll(Arc::new(7)), Dirty::YES, "{OWED}");
        assert_eq!(watch.poll(None), Dirty::YES, "{OWED}");
        assert_eq!(watch.get(), None, "{EXPECT_UNSEEN}");
    }
}
