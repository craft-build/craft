//! Continuation layer (09): the two in-run continuations that live at step
//! boundaries — a truncated (`MaxTokens`) reply continues from a
//! continuation observation, and a stalled turn (an empty reply while tool
//! results sit in the recent window) is nudged — both as durable
//! `step_end` appends that set `continue_run`, so the run never re-runs
//! from the driver. Each continuation consumes a step from `max_steps`
//! (sized per README decision 12); the budgets reset in `turn_start`
//! except on driver follow-up runs.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use animist::agent::{Agent, LayerImpl, Result, RunContext, RunStatus, StepAction, StepOutcome};
use animist::history::{ContentBlock, Role, StoredMessage};

use crate::history::{ext, marker};

use super::events::{CraftEvent, EventSink};

/// Observation appended to continue a truncated reply: the cut-off message
/// is already committed, and the next request resumes from it.
pub(crate) const CONTINUATION_PROMPT: &str =
    "[The reply hit the output limit; continue it from exactly where it stopped.]";

/// Stands in for an assistant turn with no text, so history readers see the
/// stall recorded instead of nothing.
pub(crate) const EMPTY_RESPONSE_MARKER: &str = "(empty)";

/// The stalled-turn nudge (reference `run/turn.rs` `recover_stalled_turn`).
pub(crate) const NUDGE_PROMPT: &str = "You just executed tool calls but returned an empty response. Please process the tool results above and continue with the task.";

/// A model that stalls once often stalls again on the retry, so it gets
/// plenty of chances before the turn ends empty handed.
pub(crate) const MAX_NUDGES: u32 = 20;

/// Counted over real (non-marker) messages.
pub(crate) const RECENT_TOOL_WINDOW: usize = 5;

/// Reference default (`DEFAULT_MAX_CONTINUATION_TURNS`).
pub(crate) const DEFAULT_MAX_CONTINUATION_TURNS: usize = 3;

/// Per-run continuation state, shared as a typed handle so the driver can
/// mark follow-up runs (advisor re-runs continue the same turn's budgets).
#[derive(Default)]
pub(crate) struct ContinuationRunState {
    /// Continuations spent on truncated replies this turn.
    continuations: usize,
    /// Nudges spent since the last real progress; a step whose response
    /// carried tool calls resets it.
    nudges: u32,
    /// Set by the driver before a follow-up run: `turn_start` skips the
    /// reset and consumes the mark.
    pub(crate) follow_up: bool,
}

/// The continuation state's typed handle.
pub(crate) type ContinuationHandle = Arc<Mutex<ContinuationRunState>>;

pub(crate) struct ContinuationLayer {
    /// Bound on continuations of truncated replies per turn.
    max_continuation_turns: usize,
    state: ContinuationHandle,
    sink: EventSink,
}

impl ContinuationLayer {
    pub(crate) fn new(
        max_continuation_turns: usize,
        state: ContinuationHandle,
        sink: EventSink,
    ) -> Self {
        Self {
            max_continuation_turns,
            state,
            sink,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ContinuationRunState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Whether any of the last `depth` real messages carries a tool result.
/// `synthetic_tail` counts the marker+nudge pairs this run already
/// appended; they are skipped so repeated nudges cannot push the results
/// out of the window, as are the committed empty replies a stall leaves
/// behind (the old loop replaced them with markers; on this substrate they
/// commit, but they carry nothing and must not consume window depth
/// either).
pub(crate) fn has_recent_tool_results(
    entries: &[StoredMessage],
    depth: usize,
    synthetic_tail: usize,
) -> bool {
    let real = &entries[..entries.len().saturating_sub(synthetic_tail)];
    real.iter()
        .rev()
        .filter(|stored| {
            marker::of(stored).is_none()
                && !(stored.message().role == Role::Assistant
                    && ext::text(stored.message()).trim().is_empty())
        })
        .take(depth)
        .any(|stored| {
            stored.message().role == Role::User
                && stored
                    .message()
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
}

#[async_trait]
impl LayerImpl for ContinuationLayer {
    async fn turn_start(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        message: &StoredMessage,
    ) -> Result<StoredMessage> {
        let mut state = self.lock();
        if state.follow_up {
            state.follow_up = false;
        } else {
            *state = ContinuationRunState::default();
        }
        Ok(message.clone())
    }

    async fn step_end(
        &self,
        agent: &Agent,
        _run: &RunContext,
        outcome: &StepOutcome,
    ) -> Result<StepAction> {
        // Real progress resets the nudge budget (the old loop cleared it
        // whenever a step dispatched tool calls).
        if outcome
            .response
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
        {
            self.lock().nudges = 0;
        }
        match outcome.terminal {
            // A truncated reply continues while the budget lasts: the
            // model's cut-off message is already committed, so the next
            // request resumes from it behind the continuation observation.
            Some(RunStatus::MaxTokens) => {
                let continue_budget = {
                    let mut state = self.lock();
                    if state.continuations < self.max_continuation_turns {
                        state.continuations += 1;
                        true
                    } else {
                        false
                    }
                };
                if continue_budget {
                    return Ok(StepAction::builder()
                        .messages(vec![marker::observation(
                            marker::CONTINUATION,
                            CONTINUATION_PROMPT,
                        )])
                        .continue_run(true)
                        .build());
                }
                Ok(StepAction::default())
            }
            // A completed reply with no text would end the turn with
            // nothing to show for the tool results the model just received.
            // Nudge while a tool result sits in the recent window and the
            // budget lasts; the run records the stall either way (the
            // committed empty reply is its own record — no marker is
            // appended when there is nothing to nudge past).
            Some(RunStatus::Completed) if ext::text(&outcome.response).trim().is_empty() => {
                let view: Vec<StoredMessage> = agent
                    .history()
                    .snapshot()
                    .await
                    .entries()
                    .iter()
                    .filter_map(|entry| entry.current())
                    .cloned()
                    .collect();
                let nudges = self.lock().nudges;
                let nudge = nudges < MAX_NUDGES
                    && has_recent_tool_results(&view, RECENT_TOOL_WINDOW, 2 * nudges as usize);
                if !nudge {
                    return Ok(StepAction::default());
                }
                self.lock().nudges += 1;
                self.sink.craft(CraftEvent::Nudge);
                Ok(StepAction::builder()
                    .messages(vec![
                        marker::observation(marker::NUDGE, EMPTY_RESPONSE_MARKER),
                        marker::observation(marker::NUDGE, NUDGE_PROMPT),
                    ])
                    .continue_run(true)
                    .build())
            }
            _ => Ok(StepAction::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::test_support as ts;

    #[test]
    fn has_recent_tool_results_respects_depth_and_synthetic_tail() {
        let result = ts::stored_tool_result_of("t1", "content");
        let prompt = ts::stored_user("now what");
        let marker = |text| marker::observation(marker::NUDGE, text);
        let real = vec![result.clone(), prompt.clone()];
        assert!(has_recent_tool_results(&real, 2, 0));
        assert!(!has_recent_tool_results(&real, 1, 0));
        // Marker observations never consume window depth.
        let marked = vec![result.clone(), marker(EMPTY_RESPONSE_MARKER), prompt];
        assert!(has_recent_tool_results(&marked, 2, 0));
        // The synthetic streak is skipped entirely.
        let streak = vec![
            result,
            ts::stored_user("what next"),
            marker(EMPTY_RESPONSE_MARKER),
            marker(NUDGE_PROMPT),
            marker(EMPTY_RESPONSE_MARKER),
            marker(NUDGE_PROMPT),
        ];
        assert!(has_recent_tool_results(&streak, 2, 4));
        assert!(!has_recent_tool_results(&streak, 1, 4));
    }

    #[test]
    fn a_verbatim_user_prompt_is_a_real_message() {
        // A user typing the nudge prompt verbatim consumes window depth
        // like any real message; the marker text alone does not carry a
        // tool result.
        let verbatim = vec![
            ts::stored_tool_result_of("t1", "content"),
            ts::stored_user(NUDGE_PROMPT),
        ];
        assert!(has_recent_tool_results(&verbatim, 2, 0));
        assert!(!has_recent_tool_results(&verbatim, 1, 0));
        assert!(!has_recent_tool_results(
            &[ts::stored_user(EMPTY_RESPONSE_MARKER)],
            1,
            0
        ));
    }
}
