//! Guardrail layer (07): the doom loop, the per-tool guardrails and the
//! dedup cache sit here, behind the conversation's
//! [`GuardrailState`](crate::run::GuardrailState).
//!
//! The chain runs the policy layer first, so guardrails judge exactly the
//! calls a human or the rules already allowed — a user-approved call still
//! cannot loop unproductively (the pre-migration order). `pre_tool_exec`
//! decides doom hard stop → doom repeat → guardrail warn/block → dedup;
//! `post_tool_exec` feeds the counters and the cache and appends any
//! `[guardrail]` warning to the model-visible result; `step_end` fires the
//! one-shot doom grace prompt.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use animist::agent::tool::{ToolContext, ToolOutput};
use animist::agent::{Agent, LayerImpl, Result, RunStatus, StepAction, StepOutcome, ToolDecision};
use animist::history::{ContentBlock, StoredMessage};

use crate::history::{ext, marker};
use crate::run::GuardrailState;
use crate::run::dedup::{self, ToolDedupCache};
use crate::run::doom;
use crate::run::guardrails::{
    GUARDRAIL_BLOCK_MESSAGE, GUARDRAIL_WARN_PREFIX, GuardrailDecision, GuardrailInner,
};

use super::events::{CraftEvent, EventSink};

pub(crate) struct GuardrailLayer {
    state: Arc<GuardrailState>,
    sink: EventSink,
    /// Calls this run answered from the dedup cache, so `post_tool_exec`
    /// re-runs the guardrail counters on the replayed result (the old loop
    /// did) but never re-records it into the cache.
    responded: Mutex<HashSet<String>>,
    /// Calls whose pre-exec guardrail check warned; `post_tool_exec`
    /// consumes the mark to append the warning the counters may not repeat.
    pre_warned: Mutex<HashSet<String>>,
    /// Calls denied as a doom-loop repeat this run: the old loop routed
    /// them straight to the result, never through the wave commit's error
    /// count, so the block contributes exactly `SCORE_DOOM_LOOP` — no
    /// extra error point from its denied result.
    doom_blocked: Mutex<HashSet<String>>,
}

impl GuardrailLayer {
    pub(crate) fn new(state: Arc<GuardrailState>, sink: EventSink) -> Self {
        Self {
            state,
            sink,
            responded: Mutex::new(HashSet::new()),
            pre_warned: Mutex::new(HashSet::new()),
            doom_blocked: Mutex::new(HashSet::new()),
        }
    }

    /// Record an executed result and invalidate what the call may have
    /// written; mirrors the bookkeeping the dispatch table did around every
    /// execution. Runs inside the state lock — keep it sync.
    fn record(inner: &mut GuardrailInner, context: &ToolContext<'_>, output: &ToolOutput) {
        let name = context.call.name.as_str();
        let arguments = &context.call.input;
        let read_only = ToolDedupCache::is_read_only(name);
        let dedup_key = read_only.then(|| ToolDedupCache::key(name, arguments));
        let result = ext::tool_result(
            context.call.id.clone(),
            Some(name),
            output.content.clone(),
            output.is_error,
        );
        // Cache inserts run on the raw, pre-transform result so a
        // call-specific transform cannot poison the cache.
        if !output.is_error
            && let Some(key) = dedup_key
            && !output
                .content
                .0
                .iter()
                .any(|item| matches!(item, animist::history::ToolContent::Image { .. }))
        {
            let path = dedup::extract_file_path(arguments);
            inner
                .dedup
                .insert(key, &result, path.as_deref(), name, arguments);
        }
        // Invalidation runs even on a failed result: apply_patch and delete
        // apply hunks iteratively, so a call that errors midway may still
        // have written. Clearing a cache entry that did not change only
        // costs one re-read.
        if ToolDedupCache::is_write(name) {
            let mut paths = dedup::extract_write_paths(name, arguments);
            if name == "move_file" {
                // `move_file` also rewrites imports elsewhere; those files
                // appear only in the result text.
                let text = ext::tool_content_text(&output.content);
                paths.extend(crate::tools::move_file::rewritten_files(&text));
            }
            if paths.is_empty() {
                inner.dedup.invalidate_pathless();
            } else {
                for path in paths {
                    inner.dedup.invalidate_path(&path);
                }
            }
        } else if ToolDedupCache::clears_whole_cache(name) {
            inner.dedup.clear();
        }
    }
}

/// One pre-exec pass over the shared state, atomic against the concurrent
/// pre-hooks of sibling calls.
enum Verdict {
    /// The doom score crossed the hard-stop threshold; stop the run.
    HardStop,
    /// The identical call repeated; denied with the doom-loop message.
    DoomLoop,
    /// The guardrail counters tripped; denied with the block message.
    GuardrailBlock,
    /// The call may run; `cached` holds a dedup hit to replay instead, and
    /// `warned` carries the pre-call guardrail warning for the result.
    Proceed {
        cached: Option<ContentBlock>,
        warned: bool,
    },
}

#[async_trait]
impl LayerImpl for GuardrailLayer {
    async fn turn_start(
        &self,
        _agent: &Agent,
        _run: &animist::agent::RunContext,
        message: &StoredMessage,
    ) -> Result<StoredMessage> {
        // A new user turn resets the volatile doom signals (the score and
        // the grace flag) while the recent-call window survives, so a loop
        // that spans prompts is still detected. The guardrail counters and
        // the dedup cache are conversation state and stay.
        self.state.reset_for_turn();
        for set in [&self.responded, &self.pre_warned, &self.doom_blocked] {
            set.lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clear();
        }
        Ok(message.clone())
    }

    async fn pre_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
    ) -> Result<ToolDecision> {
        let call = context.call;
        let name = call.name.as_str();
        let input = &call.input;
        let read_only = ToolDedupCache::is_read_only(name);
        let verdict = self.state.with(|inner| {
            if inner.doom.should_hard_stop() {
                return Verdict::HardStop;
            }
            if inner.doom.recent_calls.is_doom_loop(name, input) {
                // The call is blocked, not executed, and the window is wiped
                // so the model gets a clean slate to try a different
                // approach: without this, the saturated window would
                // re-trigger on the very next identical retry.
                inner.doom.note_doom_loop();
                inner.doom.recent_calls.clear();
                inner.doom.recent_calls.record(name.to_owned(), input);
                inner.answered.insert(call.id.clone());
                return Verdict::DoomLoop;
            }
            inner.doom.recent_calls.record(name.to_owned(), input);
            match inner.guardrails.check_before_call(name, input, read_only) {
                GuardrailDecision::Block => {
                    inner.answered.insert(call.id.clone());
                    Verdict::GuardrailBlock
                }
                decision => {
                    let cached = read_only.then(|| {
                        let key = ToolDedupCache::key(name, input);
                        inner.dedup.get(key, name, input).cloned()
                    });
                    Verdict::Proceed {
                        cached: cached.flatten(),
                        warned: decision == GuardrailDecision::Warn,
                    }
                }
            }
        });
        match verdict {
            Verdict::HardStop => Ok(ToolDecision::Stop {
                reason: doom::DOOM_STOP_REASON.to_owned(),
            }),
            Verdict::DoomLoop => {
                self.doom_blocked
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(call.id.clone());
                Ok(ToolDecision::Deny {
                    reason: doom::DOOM_LOOP_MESSAGE.to_owned(),
                })
            }
            Verdict::GuardrailBlock => {
                // The user-facing status mirrors the old wave commit's
                // `Event::Info`, reason stripped of the model-visible lead.
                self.sink.craft(CraftEvent::Info(format!(
                    "guardrail blocked {name}: {}",
                    GUARDRAIL_BLOCK_MESSAGE.trim_start_matches("blocked by guardrails: ")
                )));
                Ok(ToolDecision::Deny {
                    reason: GUARDRAIL_BLOCK_MESSAGE.to_owned(),
                })
            }
            Verdict::Proceed { cached, warned } => {
                if warned {
                    self.pre_warned
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .insert(call.id.clone());
                }
                if let Some(ContentBlock::ToolResult {
                    content, is_error, ..
                }) = cached
                {
                    self.responded
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .insert(call.id.clone());
                    return Ok(ToolDecision::Respond {
                        output: ToolOutput {
                            content,
                            is_error,
                            loaded_tools: Vec::new(),
                        },
                    });
                }
                Ok(ToolDecision::Run)
            }
        }
    }

    async fn post_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput> {
        let call = context.call;
        let name = call.name.as_str();
        let mut output = output;
        let responded = self
            .responded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&call.id);
        let warned = self
            .pre_warned
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&call.id);
        let answered = self.state.take_answered(&call.id);
        let doom_blocked = self
            .doom_blocked
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&call.id);
        let read_only = ToolDedupCache::is_read_only(name);
        let text = ext::tool_content_text(&output.content);
        // One pass: doom accounting runs for every closed result (the old
        // wave commit counted denials too — except doom blocks, which never
        // rode a wave), the guardrail counters and the cache only for calls
        // that really executed or replayed.
        let reason = self.state.with(|inner| {
            if !doom_blocked {
                if output.is_error {
                    inner.doom.note_tool_error();
                } else {
                    inner.doom.note_tool_success();
                }
            }
            if answered {
                return None;
            }
            if !responded {
                Self::record(inner, context, &output);
            }
            inner
                .guardrails
                .record_result(name, &call.input, &text, output.is_error, read_only)
                .map(|warning| warning.reason)
                .or_else(|| {
                    warned.then(|| {
                        format!("{name} is repeating unproductively; consider a different approach or tool")
                    })
                })
        });
        if let Some(reason) = reason {
            self.sink.craft(CraftEvent::Info(format!(
                "guardrail warning on {name}: {reason}"
            )));
            output.content.0.insert(
                0,
                animist::history::ToolContent::Text {
                    text: format!("{GUARDRAIL_WARN_PREFIX}{reason}"),
                },
            );
        }
        Ok(output)
    }

    async fn step_end(
        &self,
        _agent: &Agent,
        _run: &animist::agent::RunContext,
        outcome: &StepOutcome,
    ) -> Result<StepAction> {
        // The grace prompt fires once per run, after a step's results
        // commit and before the next inference attempt — a durable append
        // inside the same run, where the old loop pushed the prompt
        // mid-run. `mark_grace_called` inside the lock keeps it exactly
        // once even with concurrent step ends.
        let score = self.state.with(|inner| {
            if inner.doom.should_grace() {
                inner.doom.mark_grace_called();
                Some(inner.doom.score())
            } else {
                None
            }
        });
        let Some(score) = score else {
            return Ok(StepAction::default());
        };
        self.sink.craft(CraftEvent::StagnationDetected {
            similarity: score as f32 / doom::HARD_STOP_THRESHOLD as f32,
        });
        let message = marker::observation(marker::GRACE, doom::GRACE_CALL_PROMPT);
        // A step with tool calls keeps the run going on its own; a terminal
        // step needs `continue_run` for the model to ever see the prompt.
        let continue_run = outcome.terminal == Some(RunStatus::Completed);
        Ok(StepAction::builder()
            .messages(vec![message])
            .continue_run(continue_run)
            .build())
    }
}
