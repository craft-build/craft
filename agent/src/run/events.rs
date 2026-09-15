//! Session event broadcast: a single-owner stream that closes only when its
//! guard drops. Ported from the reference's `EventStreamGuard`/`SessionEvents`
//! (`craft-agent/src/types.rs`), on tokio unbounded mpsc instead of flume.
//!
//! Dropping the [`EventStreamGuard`] ends the stream, and it is the only thing
//! that does: handing out [`EventSender`] clones is free, none of them extend
//! the stream. The close marker rides the same FIFO as the events, so
//! everything sent before the guard dropped is delivered and everything sent
//! after it is lost.

use tokio::sync::mpsc;

use super::Event;

/// One event on the wire, stamped with the run that produced it.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub event: Event,
    pub run_id: u64,
}

/// The only way to create a session event stream.
pub fn event_stream() -> (EventStreamGuard, SessionEvents) {
    let (tx, rx) = mpsc::unbounded_channel();
    (EventStreamGuard { tx }, SessionEvents { rx, closed: false })
}

/// Owns the stream's lifetime; see the module docs.
#[derive(Debug)]
pub struct EventStreamGuard {
    tx: mpsc::UnboundedSender<Envelope>,
}

impl EventStreamGuard {
    /// A sender stamping every event with `run_id`; cloning it never extends
    /// the stream.
    pub fn sender(&self, run_id: u64) -> EventSender {
        EventSender {
            tx: self.tx.clone(),
            run_id,
        }
    }
}

impl Drop for EventStreamGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(Envelope {
            event: Event::StreamClosed,
            run_id: 0,
        });
    }
}

/// A cloneable handle for emitting [`Event`]s onto a session stream.
#[derive(Debug, Clone)]
pub struct EventSender {
    tx: mpsc::UnboundedSender<Envelope>,
    run_id: u64,
}

impl EventSender {
    /// Fire-and-forget send; a closed stream swallows the event silently.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(Envelope {
            event,
            run_id: self.run_id,
        });
    }
}

/// The single reader of a session's stream. Not `Clone`: two readers would
/// split the terminal marker and one of them would wait forever.
#[derive(Debug)]
pub struct SessionEvents {
    rx: mpsc::UnboundedReceiver<Envelope>,
    /// Kept `true` after the marker so `next()` stays `None` forever.
    closed: bool,
}

impl SessionEvents {
    /// `None` once the stream closed, forever after. The marker rides the
    /// FIFO, so everything queued before the guard dropped is delivered
    /// first and everything queued after it is never seen.
    pub async fn next(&mut self) -> Option<Envelope> {
        if self.closed {
            return None;
        }
        match self.rx.recv().await {
            Some(envelope) if !matches!(envelope.event, Event::StreamClosed) => Some(envelope),
            _ => {
                self.closed = true;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty stream must not resolve `next()` until the guard drops.
    #[tokio::test]
    async fn empty_stream_resolves_only_after_the_guard_drops() {
        let (guard, mut events) = event_stream();
        let pending = tokio::time::timeout(std::time::Duration::from_millis(50), events.next());
        assert!(pending.await.is_err(), "next() resolved on an open stream");
        drop(guard);
        assert!(events.next().await.is_none());
    }

    /// Events queued before the marker are all delivered, in order, then the
    /// stream closes.
    #[tokio::test]
    async fn events_before_the_marker_are_delivered_in_order() {
        let (guard, mut events) = event_stream();
        let sender = guard.sender(7);
        for text in ["one", "two", "three"] {
            sender.send(Event::TextDelta(text.into()));
        }
        drop(guard);
        let mut ids = Vec::new();
        while let Some(envelope) = events.next().await {
            assert_eq!(envelope.run_id, 7);
            let Event::TextDelta(text) = envelope.event else {
                panic!("unexpected event: {:?}", envelope.event);
            };
            ids.push(text);
        }
        assert_eq!(ids, vec!["one", "two", "three"]);
    }

    /// `next()` stays `None` on every call after the close.
    #[tokio::test]
    async fn closed_stays_none_forever() {
        let (guard, mut events) = event_stream();
        drop(guard);
        assert!(events.next().await.is_none());
        assert!(events.next().await.is_none());
    }

    /// A sender outliving the guard neither panics nor resurrects the stream:
    /// its late events stay invisible.
    #[tokio::test]
    async fn late_events_after_the_marker_are_lost() {
        let (guard, mut events) = event_stream();
        let sender = guard.sender(1);
        drop(guard);
        sender.send(Event::TextDelta("late".into()));
        assert!(events.next().await.is_none());
    }
}
