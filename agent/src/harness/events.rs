//! The conversation event contract: animist agent events plus the craft-only
//! events, on one flume channel per conversation.
//!
//! Surfaces match animist events directly; [`CraftEvent`] holds only what the
//! animist taxonomy cannot express. The sink is conversation-scoped: layers
//! publish craft events into it, and the driver forwards each run's animist
//! event channel into the same stream, so the surface reads exactly one
//! stream for a conversation's lifetime. Ordering holds per source only.

use std::collections::HashMap;

use crate::usage::{StoredTokenUsage, TokenUsage};

/// One event on a conversation's stream.
#[derive(Clone, Debug)]
pub enum Event {
    /// An animist runtime event, forwarded from the active run.
    Agent(animist::agent::AgentEvent),
    /// A craft-only event.
    Craft(CraftEvent),
}

/// Events only craft emits; the variants the animist taxonomy cannot express.
#[allow(dead_code)] // forward substrate for 07–09 (guardrails, compaction, recovery)
#[derive(Clone, Debug)]
pub enum CraftEvent {
    /// Human-readable, non-fatal status text.
    Info(String),
    /// The run failed; paired with a terminal [`CraftEvent::Done`] carrying
    /// the reason.
    Error(String),
    /// The run ended.
    Done {
        usage: TokenUsage,
        context_size: u64,
        context_window: u64,
        num_turns: u32,
        reason: DoneReason,
        /// What the run's steps were billed. `None` when no model in the run
        /// is priced, so callers show no cost instead of "$0.000".
        cost: Option<f64>,
        /// Per-model usage with each model's recorded (billed) cost. Unpriced
        /// models carry `None`.
        by_model: HashMap<String, StoredTokenUsage>,
    },
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
    /// The model returned an empty reply after tool calls and was nudged to
    /// continue.
    Nudge,
    /// The doom-loop grace prompt was injected; `similarity` is the doom
    /// score normalized toward the hard-stop threshold.
    StagnationDetected { similarity: f32 },
    /// Authentication failed and the run paused for re-authentication;
    /// `attempt` is 1-based.
    AuthRequired { attempt: u32, message: String },
    /// The post-turn advisor reviewed the run's delta and produced this note.
    AdvisorNote { severity: String, message: String },
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
    /// An event from a `task`-spawned subagent, tagged with the spawning
    /// call's id and description. The child's own craft events nest inside.
    Subagent {
        tool_use_id: String,
        description: String,
        event: Box<Event>,
    },
}

/// Why a run ended, riding the terminal [`CraftEvent::Done`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoneReason {
    /// The model finished without pending tool calls.
    Stop,
    /// The step budget ran out (partial history committed).
    MaxTurns,
    /// Generation hit the output token limit.
    MaxTokens,
    Cancelled,
    Error,
}

/// The conversation's publishing half: cheap to clone, every holder (layer,
/// driver) shares one stream.
#[derive(Clone, Debug)]
pub struct EventSink {
    tx: flume::Sender<Event>,
}

/// The single reader of a conversation's event stream. Not `Clone`: two
/// readers would split the stream and one would starve.
#[derive(Debug)]
pub struct EventStream {
    rx: flume::Receiver<Event>,
}

impl EventSink {
    /// Create a sink paired with its stream.
    pub fn new() -> (Self, EventStream) {
        let (tx, rx) = flume::unbounded();
        (Self { tx }, EventStream { rx })
    }

    /// Fire-and-forget send; a dropped stream swallows the event silently.
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// Publish a craft-only event.
    pub fn craft(&self, event: CraftEvent) {
        self.send(Event::Craft(event));
    }
}

impl Default for EventSink {
    fn default() -> Self {
        Self::new().0
    }
}

impl EventStream {
    /// The next event, or `None` once every sender (the whole conversation)
    /// is gone, forever after.
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv_async().await.ok()
    }
}
