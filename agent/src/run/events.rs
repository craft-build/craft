//! Session event broadcast: a single-owner stream that closes only when its
//! guard drops. Ported from the reference's `EventStreamGuard`/`SessionEvents`
//! (`craft-agent/src/types.rs`), on tokio unbounded mpsc instead of flume.
//!
//! Dropping the [`EventStreamGuard`] ends the stream, and it is the only thing
//! that does: handing out [`EventSender`] clones is free, none of them extend
//! the stream. The close marker rides the same FIFO as the events, so
//! everything sent before the guard dropped is delivered and everything sent
//! after it is lost.

use std::collections::HashMap;

use tokio::sync::mpsc;

use super::RunOutcome;
use crate::history::{self, Message};

/// Events emitted as the run progresses; consumed by the TUI and ACP
/// surfaces. Ported from the reference's `AgentEvent` taxonomy
/// (`craft-agent/src/types.rs`): variants whose backing subsystem is not yet
/// ported (retry ladder, stagnation tracker, auto-review plumbing, live tool
/// buffers) are forward substrate — defined but never emitted here.
#[allow(dead_code)] // forward substrate for C.2/C.8/auto-review/LiveToolBuf tasks
#[derive(Clone, Debug)]
pub enum Event {
    /// A streamed chunk of the assistant reply.
    TextDelta(String),
    /// A streamed chunk of the model's reasoning.
    ThinkingDelta(String),
    /// A tool call is queued but not yet running.
    ToolPending { id: String, name: String },
    /// The model issued a tool call.
    ToolStart {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// `content` is the full accumulated output so far, not a delta.
    ToolOutput { id: String, content: String },
    /// A tool call finished (ran, failed, or was skipped).
    ToolDone {
        id: String,
        name: String,
        arguments: serde_json::Value,
        result: history::ToolResult,
    },
    /// A wave of tool results was appended to the turn; `message` carries
    /// every result of the wave in call order.
    ToolResultsSubmitted { message: Message },
    /// One model call completed: its usage report and the estimated size of
    /// the context the model just saw.
    TurnComplete {
        usage: history::Usage,
        context_size: u64,
    },
    /// The run ended. `context_window` is a `0` sentinel until window sizes
    /// reach the run seam (model-registry work).
    Done {
        usage: history::Usage,
        context_size: u64,
        context_window: u64,
        num_turns: u32,
        reason: DoneReason,
        /// What the run's turns were billed (H.6). `None` when no model in
        /// the run is priced, so callers show no cost instead of "$0.000".
        cost: Option<f64>,
        /// Per-model usage with each model's recorded (billed) cost, for the
        /// session ledger and `cost.jsonl`. Unpriced models carry `None`.
        by_model: HashMap<String, crate::usage::StoredTokenUsage>,
    },
    /// Human-readable, non-fatal status text.
    Info(String),
    /// The post-turn advisor (C.12) reviewed the run's delta and produced
    /// this note. Emitted whether or not the run continues on it.
    AdvisorNote { severity: String, message: String },
    /// The run failed; paired with a terminal `Done` carrying the reason.
    Error(String),
    /// A recoverable stream failure is being retried (attempt is 1-based).
    Retry {
        attempt: u32,
        message: String,
        delay_ms: u64,
    },
    AutoCompacting {
        context_size: u64,
        context_window: u64,
    },
    CompactionDone {
        context_size_before: u64,
        context_size_after: u64,
        context_window: u64,
    },
    /// The doom-loop grace prompt was injected (fires exactly once per
    /// run, at the grace threshold). `similarity` is reference-taxonomy
    /// residue: here it carries the doom score normalized toward
    /// `HARD_STOP_THRESHOLD` (1.0 = about to hard-stop).
    StagnationDetected { similarity: f32 },
    AutoReviewStart {
        id: String,
        tool: String,
        scopes: Vec<String>,
    },
    AutoReviewDecision {
        id: String,
        tool: String,
        scopes: Vec<String>,
        verdict: String,
        risk: String,
        rationale: String,
    },
    /// The model returned an empty reply after tool calls and was nudged
    /// to continue.
    Nudge,
    /// Authentication failed (401) and the run paused for re-authentication
    /// (E.10); `attempt` is 1-based. The run resumes after the responder
    /// succeeds or fails with the message.
    AuthRequired { attempt: u32, message: String },
    /// End-of-stream marker; emitted only by [`EventStreamGuard::drop`] and
    /// swallowed by [`SessionEvents::next`].
    StreamClosed,
    /// An event from a `task`-spawned subagent (A.5), tagged with the
    /// spawning call's id and description. `tool_use_id` is the key the
    /// task-chats view routes by; the child's own `Done`, `Error`,
    /// `ToolOutput`, and `ToolPending` events are filtered before this.
    Subagent {
        tool_use_id: String,
        description: String,
        event: Box<Event>,
    },
}

/// Why a run ended, riding the terminal [`Event::Done`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoneReason {
    /// The model finished without pending tool calls.
    Stop,
    /// The turn budget ran out (partial history committed).
    MaxTurns,
    /// Every continuation of a truncated reply was spent.
    MaxTokens,
    Cancelled,
    Error,
    /// The doom-loop score reached the hard-stop threshold; the sanitized
    /// partial history (with an end marker) was committed.
    DoomStop,
}

impl From<&RunOutcome> for DoneReason {
    fn from(outcome: &RunOutcome) -> Self {
        match outcome {
            RunOutcome::Done { .. } => Self::Stop,
            RunOutcome::MaxTurns => Self::MaxTurns,
            RunOutcome::MaxTokens { .. } => Self::MaxTokens,
            RunOutcome::Cancelled => Self::Cancelled,
            RunOutcome::Failed(_) => Self::Error,
            RunOutcome::DoomStop => Self::DoomStop,
        }
    }
}

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
