//! Compaction trigger engine: runs configured stages when the estimated
//! context fill crosses each stage's threshold, with an effectiveness score
//! that disarms a stage whose last run barely shrank the context (avoiding
//! cyclical compaction when the context cannot shrink further).
//!
//! The engine works on the conversation's `History` handle through
//! `Compactor` plans: each stage builds its plan against a snapshot, the
//! commit runs under animist's generation check (one retry on `Conflict`,
//! never re-running the summarizer), and the original entries stay in the
//! snapshot as inactive sources. Edits run only between runs — the driver
//! owns that sequencing — and [`CompactionEngine::maybe_compact`] is the one
//! seam `RecoveryLayer::recover` calls for overflow compaction (the
//! agent re-takes its history snapshot per attempt, so compacting inside
//! `recover` cannot conflict with the run's own commit).

use std::collections::HashSet;
use std::sync::Arc;

use animist::history::{History, HistoryError, HistorySnapshot, StoredMessage};

use crate::compaction::SharedCompactionState;
use crate::config::{
    CompactionBuffer, CompactionConfig, CompactionKind,
    DEFAULT_COMPACTION_BUFFER as DEFAULT_ENGINE_BUFFER,
};
use crate::harness::EventSink;
use crate::harness::events::CraftEvent;
use crate::harness::guardrails::GuardrailState;
use crate::provider::ModelHandle;

use super::estimate::{TokenEstimator, estimate_tokens};
use super::llm::{PlanCompactor, build_llm_plan};
use super::vcc::build_vcc_plan;

/// A stage whose last run saved less than this fraction is disarmed until
/// another stage compacts effectively.
pub const INEFFECTIVE_SAVINGS: f32 = 0.1;
/// Ordered stage list (ascending by context ratio).
#[derive(Debug, Clone)]
pub struct CompactionEngine {
    stages: Vec<CompactionConfig>,
    buffer: CompactionBuffer,
    /// Rendering surface for the summarizer's system prompt.
    prompt_format: crate::prompt::PromptFormat,
}

/// Per-session effectiveness state, persisted across turns.
#[derive(Debug, Default, Clone)]
pub struct CompactionState {
    disarmed: HashSet<CompactionKind>,
    /// Calibrated token estimator: thresholds are checked against scaled
    /// estimates once the provider proves the raw ones too low.
    pub estimator: TokenEstimator,
    /// Reset when a run rewrites history: the compacted conversation no
    /// longer describes the calls that tripped the counters or the cache.
    /// A run that declines leaves it intact.
    guardrails: Option<Arc<GuardrailState>>,
    /// Where the unanswered input starts, as a snapshot-entry index:
    /// everything from here on is held out of the summary and kept
    /// verbatim, so input no turn has answered yet is never summarized away.
    /// `None` (the default) protects nothing, which
    /// is the correct state between turns.
    carry_from: Option<usize>,
    /// Tokens the request carries besides its messages (system preamble +
    /// serialized tool schemas), refreshed by the request layer each step.
    /// Fill and overflow checks run against
    /// `estimate_tokens(history) + request_overhead` so a large preamble or
    /// MCP schema set triggers compaction before the provider overflows.
    request_overhead: u64,
    /// The provider-measured input of the last completed stream (prompt +
    /// cache read). The fill check never trusts an estimate below it: the
    /// provider already proved the real size.
    measured_prompt: u64,
}

impl CompactionState {
    /// Share the session's guardrail state (doom tracker, counters, dedup
    /// cache) so a run that rewrites history can reset it.
    pub fn with_guardrail_state(mut self, guardrails: Arc<GuardrailState>) -> Self {
        self.guardrails = Some(guardrails);
        self
    }

    /// Mark `index` as where the unanswered input starts (a snapshot-entry
    /// index); compaction holds everything from there on out of the summary.
    pub fn protect_from(&mut self, index: usize) {
        self.carry_from = Some(index);
    }

    /// All input has been answered: nothing to protect (call when a turn
    /// completes).
    pub fn mark_answered(&mut self) {
        self.carry_from = None;
    }

    /// Where the protected (unanswered) input currently starts, if any.
    pub fn carry_from(&self) -> Option<usize> {
        self.carry_from
    }

    /// Record the current request overhead (preamble + tool schema tokens);
    /// call whenever the tool set or preamble may have changed.
    pub fn set_request_overhead(&mut self, tokens: u64) {
        self.request_overhead = tokens;
    }

    /// The request overhead the fill/overflow checks currently account for.
    pub fn request_overhead(&self) -> u64 {
        self.request_overhead
    }

    /// Record the provider-measured prompt size of the last completed
    /// stream; the fill check treats it as a floor. Zero is the sentinel
    /// for "no measurement" — overflow recovery clears it because the
    /// pre-compaction measurement no longer describes the compacted
    /// context.
    pub fn set_measured_prompt(&mut self, tokens: u64) {
        self.measured_prompt = tokens;
    }

    /// The provider-measured prompt floor, zero when unmeasured.
    pub fn measured_prompt(&self) -> u64 {
        self.measured_prompt
    }

    /// The overhead-inclusive estimate the engine's thresholds run against,
    /// never below what the provider last measured (the fill check must not
    /// trust an estimate the provider already proved low).
    fn scaled_estimate(&self, history: &[StoredMessage]) -> u64 {
        self.scaled_estimate_unfloored(history)
            .max(self.measured_prompt)
    }

    /// The same estimate without the measured floor: the after-side of the
    /// effectiveness score and the `CompactionDone` gauge. The floor is the
    /// last request's size — roughly the *pre*-compaction size — so flooring
    /// the after-side would score every effective pass as a no-op and
    /// disarm the stage for the rest of the session. Recovery's doom score
    /// reads the same currency.
    pub(crate) fn scaled_estimate_unfloored(&self, history: &[StoredMessage]) -> u64 {
        self.estimator
            .scale(estimate_tokens(history).saturating_add(self.request_overhead))
    }

    /// The position in a snapshot's active view where the protected
    /// (unanswered) input starts; `usize::MAX` protects nothing.
    fn summarize_end(&self, snapshot: &HistorySnapshot) -> usize {
        let Some(carry_from) = self.carry_from else {
            return usize::MAX;
        };
        snapshot
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_active())
            .position(|(index, _)| index >= carry_from)
            .unwrap_or(usize::MAX)
    }

    /// The number of entries from the carry anchor on, for re-anchoring
    /// after a stage inserts its summary.
    fn carry_len(&self, entries: usize) -> usize {
        entries.saturating_sub(self.carry_from.unwrap_or(entries))
    }

    /// Recalibrate the estimator after an overflow: `actual` is the
    /// provider-reported prompt size for a request estimated at
    /// `estimated` tokens.
    pub fn recalibrate(&mut self, actual: u64, estimated: u64) -> bool {
        self.estimator.recalibrate(actual, estimated)
    }
}

/// The active view of a snapshot: each active entry's latest stored message.
fn active_view(snapshot: &HistorySnapshot) -> Vec<StoredMessage> {
    snapshot
        .entries()
        .iter()
        .filter_map(|entry| entry.current())
        .cloned()
        .collect()
}

impl CompactionEngine {
    pub fn new(mut stages: Vec<CompactionConfig>) -> Self {
        stages.sort_by(|a, b| {
            a.context
                .partial_cmp(&b.context)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self {
            stages,
            buffer: DEFAULT_ENGINE_BUFFER,
            prompt_format: crate::prompt::PromptFormat::default(),
        }
    }

    /// Set the compaction buffer reserved below the context window
    /// (defaults to 20% of the window).
    pub fn with_buffer(mut self, buffer: CompactionBuffer) -> Self {
        self.buffer = buffer;
        self
    }

    /// Set the rendering surface for the summarizer's system prompt (the
    /// conversation's `agent.prompt_format`; defaults to markdown).
    pub fn with_prompt_format(mut self, prompt_format: crate::prompt::PromptFormat) -> Self {
        self.prompt_format = prompt_format;
        self
    }

    /// Run every armed stage whose threshold is crossed, lowest first.
    /// `history` is rewritten through compaction plans; originals stay as
    /// entry sources. Returns whether any stage ran.
    ///
    /// A stage runs when either its proactive fill threshold is crossed or
    /// the estimated context has already overflowed the buffer-subtracted
    /// window — overflow forces every armed stage in order (VCC first, then
    /// LLM) regardless of its ratio threshold, until a pass leaves the
    /// history under its stage target. This is the seam
    /// `RecoveryLayer::recover` calls for overflow compaction.
    pub async fn maybe_compact(
        &self,
        state: &SharedCompactionState,
        model: &ModelHandle,
        history: &History,
        context_length: Option<u32>,
        sink: &EventSink,
    ) -> bool {
        self.compact_impl(state, model, history, context_length, sink, false)
            .await
    }

    /// Manual compaction (`/compact`): the forcing applies until a stage
    /// runs, regardless of fill; once one has, the overflow cascade rule
    /// decides the heavier stages. Stages whose mechanics decline to run
    /// (too-short history) still leave the history untouched, so an
    /// already-compact history returns `false`.
    pub async fn force_compact(
        &self,
        state: &SharedCompactionState,
        model: &ModelHandle,
        history: &History,
        context_length: Option<u32>,
        sink: &EventSink,
    ) -> bool {
        self.compact_impl(state, model, history, context_length, sink, true)
            .await
    }

    async fn compact_impl(
        &self,
        shared: &SharedCompactionState,
        model: &ModelHandle,
        history: &History,
        context_length: Option<u32>,
        sink: &EventSink,
        force: bool,
    ) -> bool {
        let Some(context_length) = context_length.filter(|length| *length > 0) else {
            return false;
        };
        let mut state = shared.lock().await;
        // The buffer is headroom the model needs for its reply, so the fill
        // checks run against the window minus the buffer — never less than
        // `MIN_OUTPUT_TOKENS`, so the output-cap floor and the compaction
        // threshold agree on the same reservation.
        let reserved = u64::from(self.buffer.resolve(context_length))
            .max(crate::harness::request::MIN_OUTPUT_TOKENS);
        let usable = u64::from(context_length).saturating_sub(reserved);
        let mut ran = false;
        // Overflow forcing cascades: a VCC pass that leaves the history over
        // its limit forces the next (heavier) stage even below its threshold.
        let mut forced = force;
        for stage in &self.stages {
            let threshold = (context_length as f64 * stage.context) as u64;
            if threshold == 0 {
                continue;
            }
            let snapshot = history.snapshot().await;
            let view = active_view(&snapshot);
            let before = state.scaled_estimate(&view);
            let overflow = forced || before >= usable;
            if !overflow && before < threshold {
                continue;
            }
            if state.disarmed.contains(&stage.kind) {
                continue;
            }
            // Compaction can only shrink history: the stage's budget must
            // leave room for the overhead the request still carries, so the
            // summary coexists with it inside the stage's threshold.
            let stage_target = threshold.saturating_sub(state.request_overhead);
            let summarize_end = state.summarize_end(&snapshot);
            let carry_len = state.carry_len(snapshot.entries().len());
            sink.craft(CraftEvent::AutoCompacting {
                context_size: before,
                context_window: u64::from(context_length),
            });
            let plan = match stage.kind {
                CompactionKind::Vcc => build_vcc_plan(&snapshot, summarize_end).await,
                CompactionKind::Llm => {
                    build_llm_plan(model, &snapshot, summarize_end, None, self.prompt_format).await
                }
            };
            let Some(plan) = plan else {
                // A stage that declined to run (too-short history, empty
                // head, nothing compactable) leaves the history untouched;
                // that is not an ineffective run, so the stage stays armed —
                // and the guardrail state, which describes the
                // pre-compaction history, stays intact too.
                continue;
            };
            // Commit under the generation check; one retry with the same
            // plan on a conflict, never re-running the summarizer.
            if !commit(history, plan).await {
                tracing::warn!("compaction commit conflicted twice; stage skipped");
                continue;
            }
            if let Some(guardrails) = &state.guardrails {
                guardrails.reset();
            }
            ran = true;
            // The carried input survived verbatim at the tail (both stages
            // hold it out of the summary); re-anchor the protection to where
            // it now starts.
            let after_snapshot = history.snapshot().await;
            if carry_len > 0 {
                state.carry_from = Some(after_snapshot.entries().len().saturating_sub(carry_len));
            }
            let after_view = active_view(&after_snapshot);
            let after = state.scaled_estimate_unfloored(&after_view);
            sink.craft(CraftEvent::CompactionDone {
                context_size_before: before,
                context_size_after: after,
                context_window: u64::from(context_length),
            });
            let savings = if before > 0 {
                1.0 - (after as f32 / before as f32)
            } else {
                0.0
            };
            if savings >= INEFFECTIVE_SAVINGS {
                // An effective run re-arms every stage.
                state.disarmed.clear();
            } else {
                state.disarmed.insert(stage.kind);
            }
            // A VCC pass that left the history over its limit is not a
            // sufficient pass: force the heavier stages to run too. Any
            // other outcome ends the forcing — a manual /compact whose
            // first pass fit stops there. The check uses the raw estimate
            // against the stage target.
            forced =
                stage.kind == CompactionKind::Vcc && estimate_tokens(&after_view) > stage_target;
        }
        ran
    }
}

/// Commit a plan through `History::compact`, retrying once on a conflict
/// (a concurrent mutation between plan and commit; the driver's
/// between-runs sequencing makes this rare). Other rejections — a plan
/// whose indices no longer match — leave the history untouched and the
/// stage is reported as not run. Returns whether the plan's edits landed.
async fn commit(history: &History, plan: animist::history::CompactionPlan) -> bool {
    let compactor = PlanCompactor::new(plan.clone());
    match history.compact(compactor).await {
        Ok(()) => true,
        Err(HistoryError::Conflict { .. }) => {
            let retry = PlanCompactor::new(plan);
            matches!(history.compact(retry).await, Ok(()))
        }
        Err(error) => {
            tracing::warn!("compaction commit rejected: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compaction::llm::LLM_SUMMARY_PREFIX;
    use crate::compaction::test_support::{
        stored_assistant_text, stored_assistant_tool_args, stored_tool_result_of, stored_user,
    };
    use crate::history::ext;

    fn stage(kind: CompactionKind, context: f64) -> CompactionConfig {
        CompactionConfig { kind, context }
    }

    fn tool_round(i: usize, output: &str) -> Vec<StoredMessage> {
        let id = format!("t{i}");
        vec![
            stored_assistant_tool_args(
                &id,
                "bash",
                serde_json::json!({"command": format!("echo {i}")}),
            ),
            stored_tool_result_of(&id, output),
        ]
    }

    /// `long_history` repeated `times` over with globally unique tool-call
    /// ids: replay validation rejects duplicates, and animist's VCC cut
    /// spans tool exchanges by id.
    fn repeated_history(times: usize) -> Vec<StoredMessage> {
        let mut messages = Vec::new();
        for repeat in 0..times {
            for i in 0..8 {
                let id = format!("t{repeat}-{i}");
                messages.push(stored_user(&format!(
                    "do task {i} with a fairly long instruction"
                )));
                messages.push(stored_assistant_tool_args(
                    &id,
                    "bash",
                    serde_json::json!({"command": format!("echo {i}")}),
                ));
                messages.push(stored_tool_result_of(&id, &"x".repeat(200)));
            }
            messages.push(stored_user("second task begins here"));
            for i in 8..14 {
                let id = format!("t{repeat}-{i}");
                messages.push(stored_user(&format!(
                    "do subtask {i} with a fairly long instruction"
                )));
                messages.push(stored_assistant_tool_args(
                    &id,
                    "bash",
                    serde_json::json!({"command": format!("echo {i}")}),
                ));
                messages.push(stored_tool_result_of(&id, &"y".repeat(200)));
            }
        }
        messages
    }

    /// A compaction model scripting `reply` for every call it serves.
    fn summary_model(reply: &str) -> crate::provider::ModelHandle {
        summary_script(reply).0
    }

    /// [`summary_model`] with the provider recording its calls.
    fn summary_script(
        reply: &str,
    ) -> (
        crate::provider::ModelHandle,
        crate::testing::ScriptedProvider,
    ) {
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        for _ in 0..8 {
            provider.push(crate::testing::text_turn(reply));
        }
        (
            crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
                &provider,
                crate::testing::model_info("summary", 1_000_000, 8_192, false, false),
            )),
            provider,
        )
    }

    /// A shared state cell, the way the conversation holds it.
    fn shared(state: CompactionState) -> SharedCompactionState {
        std::sync::Arc::new(tokio::sync::Mutex::new(state))
    }

    /// A history seeded from stored messages.
    fn history_of(messages: Vec<StoredMessage>) -> History {
        History::from_entries(ext::entries(messages))
    }

    /// The history's active view.
    async fn view(history: &History) -> Vec<StoredMessage> {
        active_view(&history.snapshot().await)
    }

    /// Long history: alternating prompts and large tool rounds.
    fn long_history() -> Vec<StoredMessage> {
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(stored_user(&format!(
                "do task {i} with a fairly long instruction"
            )));
            messages.extend(tool_round(i, &"x".repeat(200)));
        }
        messages.push(stored_user("second task begins here"));
        for i in 8..14 {
            messages.push(stored_user(&format!(
                "do subtask {i} with a fairly long instruction"
            )));
            messages.extend(tool_round(i, &"y".repeat(200)));
        }
        messages
    }

    /// A history whose compactable head is tiny relative to its kept tail,
    /// so a VCC pass saves less than [`INEFFECTIVE_SAVINGS`] and disarms:
    /// three short entries in front of eight heavy ones (VCC keeps the
    /// recent eight and cuts at the user boundary).
    fn tight_history() -> Vec<StoredMessage> {
        let mut messages = vec![
            stored_user("short task"),
            stored_assistant_text("short reply"),
            stored_assistant_text("short followup"),
        ];
        // A heavy tail VCC must keep inside the recent window: with 11
        // entries the cut limit (len - 8) reaches the user boundary after
        // the tiny head.
        messages.push(stored_user(&"z".repeat(4_000)));
        for i in 0..3 {
            messages.push(stored_assistant_text(&format!(
                "{}{}",
                "w".repeat(4_000),
                i
            )));
            messages.push(stored_user(&format!("{}{}", "y".repeat(4_000), i)));
        }
        messages.push(stored_assistant_text(&"v".repeat(4_000)));
        messages
    }

    #[tokio::test]
    async fn force_compact_runs_regardless_of_fill() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(long_history());
        let before = view(&history).await.len();
        // A window far above the fill: `maybe_compact` would not run.
        let context_length = 1_000_000;
        let ran = engine
            .force_compact(
                &state,
                &summary_model("x"),
                &history,
                Some(context_length),
                &sink,
            )
            .await;
        assert!(ran);
        assert!(
            view(&history).await.len() < before,
            "forced compaction shrinks a long history even below the fill threshold"
        );
    }

    #[tokio::test]
    async fn force_compact_on_short_history_is_a_no_op() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(vec![stored_user("hi"), stored_user("there")]);
        // The VCC stage declines a too-short history even when forced, so
        // callers get `false` and can report "already compact".
        let ran = engine
            .force_compact(
                &state,
                &summary_model("x"),
                &history,
                Some(1_000_000),
                &sink,
            )
            .await;
        assert!(!ran);
    }

    #[tokio::test]
    async fn force_compact_without_a_window_is_a_no_op() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(long_history());
        let ran = engine
            .force_compact(&state, &summary_model("x"), &history, None, &sink)
            .await;
        assert!(!ran);
    }

    #[tokio::test]
    async fn forced_compact_stops_once_a_pass_fits_the_context() {
        // /compact on a wide window: the forced VCC pass shrinks the history
        // below every threshold, so the heavier LLM stage must not run —
        // one compaction, no summarizer call, nothing disarmed.
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.6),
            stage(CompactionKind::Llm, 0.8),
        ]);
        let state = shared(CompactionState::default());
        let (model, calls) = summary_script("llm summary text");
        let (sink, _stream) = EventSink::new();
        let history = history_of(long_history());
        let ran = engine
            .force_compact(&state, &model, &history, Some(1_000_000), &sink)
            .await;
        assert!(ran, "the forced VCC pass runs");
        let view = view(&history).await;
        assert!(ext::message_text(&view[0]).contains("Earlier conversation"));
        assert!(
            !view
                .iter()
                .any(|m| ext::message_text(m).contains(LLM_SUMMARY_PREFIX)),
            "a sufficient forced pass must not cascade into the LLM stage"
        );
        assert!(calls.requests().is_empty(), "no summarizer call was made");
        let state = state.lock().await;
        assert!(
            !state.disarmed.contains(&CompactionKind::Llm),
            "a skipped stage stays armed"
        );
    }

    #[tokio::test]
    async fn vcc_pass_left_over_target_forces_the_heavier_stage() {
        // The VCC pass barely shrinks the tight history, leaving it over the
        // VCC stage's target while under both the LLM threshold and the
        // usable window: only the cascade can bring the LLM stage in.
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.1),
            stage(CompactionKind::Llm, 0.99),
        ])
        .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = tight_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        let ran = engine
            .maybe_compact(
                &state,
                &summary_model("llm summary text"),
                &history,
                Some(context_length),
                &sink,
            )
            .await;
        assert!(ran);
        assert!(
            ext::message_text(&view(&history).await[0]).contains(LLM_SUMMARY_PREFIX),
            "an insufficient VCC pass must cascade into the LLM stage"
        );
    }

    #[tokio::test]
    async fn runs_vcc_stage_when_threshold_crossed() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // Window sized so 60% of it is below the current estimate.
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        let ran = engine
            .maybe_compact(
                &state,
                &summary_model("x"),
                &history,
                Some(context_length),
                &sink,
            )
            .await;
        assert!(ran);
        let view = view(&history).await;
        assert!(!view.is_empty());
        // VCC summary present (deterministic, no LLM call): the system
        // preamble, tagged as a VCC summary.
        assert!(ext::message_text(&view[0]).contains("Earlier conversation"));
        assert_eq!(
            view[0].tags().get("craft.compaction").map(String::as_str),
            Some("vcc")
        );
    }

    #[tokio::test]
    async fn runs_llm_stage_at_higher_threshold() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.8)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // Window sized so 80% of it is below the current estimate.
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        let model = summary_model("llm summary text");
        let ran = engine
            .maybe_compact(&state, &model, &history, Some(context_length), &sink)
            .await;
        assert!(ran);
        assert!(ext::message_text(&view(&history).await[0]).contains(LLM_SUMMARY_PREFIX));
    }

    #[tokio::test]
    async fn ineffective_vcc_is_not_run_twice() {
        // The tight history's head is two short messages; VCC's summary of
        // them saves less than 10% of the context, so the stage disarms.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = tight_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.99) as u32).max(1);
        let history = history_of(seeded);
        let model = summary_model("x");
        let ran = engine
            .maybe_compact(&state, &model, &history, Some(context_length), &sink)
            .await;
        assert!(ran);
        assert!(state.lock().await.disarmed.contains(&CompactionKind::Vcc));
        // Second call: nothing runs (stage disarmed) even though crossed.
        let again = engine
            .maybe_compact(&state, &model, &history, Some(context_length), &sink)
            .await;
        assert!(!again, "disarmed stage must not run again");
    }

    #[tokio::test]
    async fn overflow_forcing_respects_disarmed_stages() {
        // Disarm VCC on the tight history first.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(tight_history());
        let model = summary_model("x");
        let tokens = estimate_tokens(&tight_history());
        let tight = ((tokens as f64 / 0.99) as u32).max(1);
        assert!(
            engine
                .maybe_compact(&state, &model, &history, Some(tight), &sink)
                .await
        );
        // A second run with a wide window but a huge buffer: fill is over the
        // buffer-subtracted usable window (overflow), under the threshold.
        let after = estimate_tokens(&view(&history).await);
        let overflowing = ((after as f64 / 0.9) as u32).max(1);
        let engine = engine.with_buffer(crate::config::CompactionBuffer::Percent(50));
        assert!(
            !engine
                .maybe_compact(&state, &model, &history, Some(overflowing), &sink)
                .await,
            "a disarmed stage must not run even on overflow"
        );
    }

    #[tokio::test]
    async fn declined_stage_stays_armed() {
        // A single-message history crosses the threshold but both compactors
        // decline; neither stage may be disarmed by the no-op.
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.6),
            stage(CompactionKind::Llm, 0.8),
        ]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = vec![stored_user(&"x".repeat(4000))];
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        let model = summary_model("x");
        let ran = engine
            .maybe_compact(&state, &model, &history, Some(context_length), &sink)
            .await;
        assert!(!ran);
        assert!(
            state.lock().await.disarmed.is_empty(),
            "declined runs must not disarm"
        );
    }

    #[tokio::test]
    async fn calibrated_multiplier_tightens_thresholds() {
        // The raw estimate sits under the threshold, but a multiplier
        // recalibrated after an overflow pushes it over: the stage runs.
        // The history is repeated so the 20% buffer clears the
        // MIN_OUTPUT_TOKENS floor while the (5x-capped) multiplier can
        // still carry the scaled estimate past the usable window.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = repeated_history(8);
        let tokens = estimate_tokens(&seeded);
        // Fill ≈ 0.35: under the 0.6 threshold, under the usable window.
        let context_length = ((tokens as f64 / 0.35) as u32).max(1);
        let history = history_of(seeded);
        assert!(
            !engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await,
            "raw estimate sits under the threshold"
        );
        state
            .lock()
            .await
            .recalibrate(u64::from(context_length) * 2, tokens);
        let seeded = repeated_history(8);
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
    }

    #[tokio::test]
    async fn compaction_clears_dedup_cache() {
        use crate::harness::dedup::ToolDedupCache;

        let guardrails = std::sync::Arc::new(GuardrailState::new(None));
        let input = serde_json::json!({"path": "/x.rs"});
        let key = ToolDedupCache::key("read", &input);
        guardrails.with(|inner| {
            inner.dedup.insert(
                key,
                &crate::history::ext::tool_result("c1", Some("read"), "x", false),
                None,
                "read",
                &input,
            );
        });
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default().with_guardrail_state(guardrails.clone()));
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        assert!(
            guardrails.with(|inner| inner.dedup.get(key, "read", &input).is_none()),
            "a compaction run must clear the dedup cache"
        );
    }

    #[tokio::test]
    async fn declined_compaction_keeps_dedup_cache() {
        use crate::harness::dedup::ToolDedupCache;

        let guardrails = std::sync::Arc::new(GuardrailState::new(None));
        let input = serde_json::json!({"path": "/x.rs"});
        let key = ToolDedupCache::key("read", &input);
        guardrails.with(|inner| {
            inner.dedup.insert(
                key,
                &crate::history::ext::tool_result("c1", Some("read"), "x", false),
                None,
                "read",
                &input,
            );
        });
        // A short history that crosses a tiny threshold but both compactors
        // decline: the dedup cache must survive repeated runs.
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.1),
            stage(CompactionKind::Llm, 0.1),
        ]);
        let state = shared(CompactionState::default().with_guardrail_state(guardrails.clone()));
        let (sink, _stream) = EventSink::new();
        let seeded = vec![stored_user(&"x".repeat(4000))];
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        for _ in 0..2 {
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink,
                )
                .await;
        }
        assert!(
            guardrails.with(|inner| inner.dedup.get(key, "read", &input).is_some()),
            "a declined compaction must not clear the dedup cache"
        );
    }

    #[tokio::test]
    async fn no_compaction_without_context_length() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(long_history());
        let ran = engine
            .maybe_compact(&state, &summary_model("x"), &history, None, &sink)
            .await;
        assert!(!ran);
        assert_eq!(view(&history).await.len(), long_history().len());
    }

    #[tokio::test]
    async fn carry_protection_keeps_unanswered_input_verbatim() {
        // The last two messages are unanswered input; the LLM stage may
        // summarize everything before them, but must keep them verbatim.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let mut seeded = long_history();
        let (prompt, followup) = (
            stored_user("unanswered prompt"),
            stored_user("queued followup"),
        );
        seeded.push(prompt.clone());
        seeded.push(followup.clone());
        let mut state = CompactionState::default();
        state.protect_from(seeded.len() - 2);
        let state = shared(state);
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let (model, calls) = summary_script("summary");
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(&state, &model, &history, Some(context_length), &sink)
                .await
        );
        // Summary replaced the head, protected tail is intact and last.
        let view = view(&history).await;
        assert!(ext::message_text(&view[0]).contains(LLM_SUMMARY_PREFIX));
        assert_eq!(view[view.len() - 2], prompt);
        assert_eq!(*view.last().unwrap(), followup);
        // The protection re-anchored to where the carried input now starts
        // (an entries index: the summary insert shifted it by one).
        let entries = history.snapshot().await.entries().to_vec();
        let carry_from = state.lock().await.carry_from().expect("re-anchored");
        assert_eq!(carry_from, entries.len() - 2);
        assert_eq!(entries[carry_from].current().cloned(), Some(prompt));
        // The summarizer never saw the carried input.
        let request = &calls.requests()[0];
        let carried = &request
            .messages
            .iter()
            .filter(|m| crate::history::ext::is_user(m))
            .any(|m| {
                crate::history::ext::text(m).contains("unanswered prompt")
                    || crate::history::ext::text(m).contains("queued followup")
            });
        assert!(!carried, "carried input must not be sent to the summarizer");
    }

    #[tokio::test]
    async fn mark_answered_clears_protection() {
        let mut state = CompactionState::default();
        state.protect_from(3);
        assert_eq!(state.carry_from(), Some(3));
        state.mark_answered();
        assert_eq!(state.carry_from(), None);
        assert_eq!(CompactionState::default().carry_from(), None);
    }

    #[tokio::test]
    async fn fully_protected_history_declines_instead_of_summarizing_input() {
        // Everything unanswered: the LLM stage must decline rather than
        // summarize the only input there is.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.1)]);
        let seeded = long_history();
        let len = seeded.len();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.5) as u32).max(1);
        let mut state = CompactionState::default();
        state.protect_from(0);
        let state = shared(state);
        // No model calls: the protected tail leaves nothing compactable.
        let provider = crate::testing::ScriptedProvider::new(Vec::new());
        let model = crate::provider::ModelHandle::scripted(crate::testing::scripted_model(
            &provider,
            crate::testing::model_info("summary", 1_000_000, 8_192, false, false),
        ));
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            !engine
                .maybe_compact(&state, &model, &history, Some(context_length), &sink)
                .await
        );
        assert_eq!(view(&history).await.len(), len);
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn vcc_carry_protection_keeps_unanswered_input_verbatim() {
        // The queued prompt is unanswered; the VCC stage must hold it out of
        // the summary and keep it verbatim.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut seeded = long_history();
        let prompt = stored_user("unanswered queued prompt");
        seeded.push(prompt.clone());
        let mut state = CompactionState::default();
        state.protect_from(seeded.len() - 1);
        let state = shared(state);
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        // The prompt survived verbatim at the tail and was never summarized.
        let view = view(&history).await;
        assert_eq!(*view.last().unwrap(), prompt);
        let summary = ext::message_text(&view[0]);
        assert!(summary.contains("Earlier conversation"));
        assert!(!summary.contains("unanswered queued prompt"));
        // The protection re-anchored to where the carried input now starts.
        let entries = history.snapshot().await.entries().to_vec();
        let carry_from = state.lock().await.carry_from().expect("re-anchored");
        assert_eq!(carry_from, entries.len() - 1);
        assert_eq!(entries[carry_from].current().cloned(), Some(prompt));
    }

    #[tokio::test]
    async fn vcc_carry_reanchor_points_at_carried_messages() {
        // After a VCC compaction, the re-anchored index must fall exactly on
        // the start of the carried (unanswered) messages.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let mut seeded = long_history();
        let (a, b) = (
            stored_user("first unanswered"),
            stored_user("second unanswered"),
        );
        seeded.push(a.clone());
        seeded.push(b);
        let mut state = CompactionState::default();
        state.protect_from(seeded.len() - 2);
        let state = shared(state);
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        let entries = history.snapshot().await.entries().to_vec();
        let carry_from = state
            .lock()
            .await
            .carry_from()
            .expect("protection re-anchored");
        assert_eq!(carry_from, entries.len() - 2);
        assert_eq!(entries[carry_from].current().cloned(), Some(a));
    }

    #[tokio::test]
    async fn vcc_over_limit_forces_llm_stage() {
        // VCC runs at its threshold but cannot get under it; the LLM stage's
        // own threshold is not crossed, so only the cascaded forcing runs it.
        let engine = CompactionEngine::new(vec![
            stage(CompactionKind::Vcc, 0.6),
            stage(CompactionKind::Llm, 0.8),
        ])
        .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let seeded = vec![
            stored_user(&"a".repeat(100)),
            stored_assistant_text(&"b".repeat(100)),
            stored_user(&"c".repeat(100)),
            stored_assistant_text(&"d".repeat(100)),
            stored_user(&"e".repeat(100)),
            stored_assistant_text(&"f".repeat(100)),
            stored_user(&"g".repeat(100)),
            stored_assistant_text(&"h".repeat(100)),
        ];
        let tokens = estimate_tokens(&seeded);
        // Fill ratio 0.7: over VCC's 0.6 threshold, under LLM's 0.8 and the
        // usable window, so the LLM stage only runs via the cascade.
        let context_length = ((tokens as f64 / 0.7) as u32).max(1);
        let (model, calls) = summary_script("summarized");
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(&state, &model, &history, Some(context_length), &sink)
                .await
        );
        assert!(
            !calls.requests().is_empty(),
            "the LLM stage must run after an over-limit VCC pass"
        );
        assert!(ext::message_text(&view(&history).await[0]).contains(LLM_SUMMARY_PREFIX));
        assert!(
            estimate_tokens(&view(&history).await) < u64::from(context_length),
            "context must end under the usable window"
        );
    }

    #[tokio::test]
    async fn large_request_overhead_triggers_compaction_history_alone_would_not() {
        // A large preamble/MCP schema set counts against the fill: a history
        // far under the threshold crosses it once the overhead rides along.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Percent(20));
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // Window with plenty of headroom for the history alone (fill ≈ 0.01,
        // buffer comfortably above the MIN_OUTPUT_TOKENS floor).
        let context_length = (tokens * 100).max(1) as u32;
        let history = history_of(seeded);
        assert!(
            !engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await,
            "history alone sits under the threshold"
        );
        // A schema set 70x the history pushes the fill over 0.6 (and still
        // under the usable window, so the proactive threshold — not the
        // overflow path — is what fires).
        state.lock().await.set_request_overhead(tokens * 70);
        let seeded = long_history();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await,
            "history + request overhead must cross the threshold"
        );
    }

    #[tokio::test]
    async fn changed_overhead_moves_the_trigger() {
        // Tool-set change between turns: the same history/window that did not
        // compact under a small overhead compacts after the tool set grows.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Percent(20));
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = (tokens * 100).max(1) as u32;
        state.lock().await.set_request_overhead(tokens / 10);
        let history = history_of(seeded);
        assert!(
            !engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        state.lock().await.set_request_overhead(tokens * 70);
        let seeded = long_history();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
    }

    #[tokio::test]
    async fn overflow_of_buffer_subtracted_window_forces_stage_below_threshold() {
        // Fill is under the 0.6 proactive threshold but over the window minus
        // the buffer: the stage must still run.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)])
            .with_buffer(crate::config::CompactionBuffer::Percent(50));
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // Window: fill ratio 0.5 (< 0.6 threshold), usable = 0.5 * window
        // (= tokens) so estimate >= usable -> overflow.
        let context_length = ((tokens as f64 / 0.5) as u32).max(1);
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await,
            "overflow must force the stage despite the proactive threshold"
        );
    }

    #[tokio::test]
    async fn buffer_larger_than_window_saturates_and_forces() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.99)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(u32::MAX));
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // Fill ratio far below the 0.99 threshold, but the buffer eats the
        // whole window: the usable limit saturates at 0 and the stage runs.
        let context_length = ((tokens as f64 / 0.1) as u32).max(1);
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await,
            "a buffer covering the whole window must force compaction, not underflow"
        );
    }

    #[tokio::test]
    async fn overflow_boundary_is_the_usable_window() {
        // `before == usable` overflows (forces the stage); `usable - 1` with
        // the fill under the proactive threshold does not.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.99)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // `usable = window - max(0, MIN_OUTPUT_TOKENS)`; sit exactly on it.
        let usable = tokens.max(crate::harness::request::MIN_OUTPUT_TOKENS);
        let window = (usable + crate::harness::request::MIN_OUTPUT_TOKENS).max(1) as u32;
        let state = shared(CompactionState::default());
        let history = history_of(seeded);
        // Scale the overhead so the scaled estimate lands exactly on usable.
        state.lock().await.set_request_overhead(usable - tokens);
        let (sink, _stream) = EventSink::new();
        assert!(
            engine
                .maybe_compact(&state, &summary_model("x"), &history, Some(window), &sink)
                .await,
            "an estimate equal to the usable window is an overflow"
        );
    }

    #[tokio::test]
    async fn below_usable_window_does_not_overflow_force() {
        // One token under the usable window, fill under the proactive
        // threshold: nothing forces the stage to run.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.99)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let usable = tokens.max(crate::harness::request::MIN_OUTPUT_TOKENS);
        // Window so `usable - 1` remains below MIN_OUTPUT_TOKENS-free space.
        let window = (usable + crate::harness::request::MIN_OUTPUT_TOKENS + 1) as u32;
        let state = shared(CompactionState::default());
        state.lock().await.set_request_overhead(usable - tokens - 1);
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            !engine
                .maybe_compact(&state, &summary_model("x"), &history, Some(window), &sink)
                .await,
            "one token under the usable window must not force compaction"
        );
    }

    #[tokio::test]
    async fn small_window_reserves_the_min_output_floor() {
        // Buffer below MIN_OUTPUT_TOKENS on a small window: the reservation
        // is the floor, so usable = window - MIN_OUTPUT_TOKENS. An estimate
        // in (usable, window) must force compaction even though the 1%-style
        // buffer would have left room.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.99)])
            .with_buffer(crate::config::CompactionBuffer::Tokens(0));
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        // Window big enough that usable > 0 but estimate sits above it.
        let window = (tokens + crate::harness::request::MIN_OUTPUT_TOKENS / 2) as u32;
        let usable = u64::from(window).saturating_sub(crate::harness::request::MIN_OUTPUT_TOKENS);
        assert!(tokens > usable, "test premise: estimate over usable");
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(&state, &summary_model("x"), &history, Some(window), &sink)
                .await,
            "the MIN_OUTPUT_TOKENS floor must be reserved below the threshold"
        );
    }

    #[tokio::test]
    async fn plan_indices_survive_prior_inactive_entries() {
        // Compact twice: the second pass plans against a snapshot whose head
        // is inactive, so its indices must still name the right entries.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let history = history_of(long_history());
        let model = summary_model("x");
        let tokens = estimate_tokens(&long_history());
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        assert!(
            engine
                .maybe_compact(&state, &model, &history, Some(context_length), &sink)
                .await
        );
        let snapshot = history.snapshot().await;
        assert!(
            snapshot.entries().iter().any(|entry| !entry.is_active()),
            "the first pass leaves inactive sources behind"
        );
        // Grow the history so the threshold crosses again, then compact: the
        // plan's indices (over entries, inactive included) must land the
        // summary before the first kept active entry, never resurrect the
        // inactive head, and keep replay valid.
        // Fresh ids for the appended rounds: duplicate tool-call ids would
        // both fail replay validation and glue VCC's cut spans together.
        let mut more = repeated_history(1);
        more.push(stored_user("a later task"));
        for message in more {
            history.add_stored(message).await;
        }
        let tokens = estimate_tokens(&view(&history).await);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        assert!(
            engine
                .maybe_compact(&state, &model, &history, Some(context_length), &sink)
                .await
        );
        let view = view(&history).await;
        assert!(ext::message_text(&view[0]).contains("Earlier conversation"));
        let messages: Vec<_> = view.iter().map(StoredMessage::message).cloned().collect();
        animist::protocol::validate_replay(&messages, &[])
            .expect("the compacted view stays replay-valid");
    }

    #[tokio::test]
    async fn conflict_between_plan_and_commit_retries_once_without_resummarizing() {
        // An append racing the LLM stage's commit conflicts; the retry
        // commits the same plan, and the summarizer is called exactly once.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Llm, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let (model, calls) = summary_script("summary");
        let history = history_of(seeded);
        // Append between the plan and the commit, deterministically: the
        // summarizer's request is recorded inside build_llm_plan (after the
        // engine's snapshot, before the commit), so appending once it lands
        // always loses the commit's generation check.
        let racer = {
            let history = history.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                // Bounded wait: the assertions below fail the test anyway if
                // the summarizer is never called; this must not hang.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                while calls.requests().is_empty() && std::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                history.add_stored(stored_user("racing append")).await;
            })
        };
        let ran = engine
            .maybe_compact(&state, &model, &history, Some(context_length), &sink)
            .await;
        racer.await.expect("racer");
        assert!(ran, "the retried commit lands");
        assert_eq!(
            calls.requests().len(),
            1,
            "the summarizer is never called twice after a conflict"
        );
        // The racing append survived.
        assert!(
            view(&history)
                .await
                .iter()
                .any(|stored| ext::message_text(stored) == "racing append")
        );
    }

    #[tokio::test]
    async fn measured_prompt_tokens_floor_the_fill_check() {
        // The raw estimate sits under the threshold, but the provider
        // measured an input above it: the stage must run.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = (tokens * 100).max(1) as u32;
        let history = history_of(seeded);
        assert!(
            !engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        let seeded = long_history();
        let history = history_of(seeded);
        state.lock().await.set_measured_prompt(tokens * 70);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await,
            "a measured prompt above the estimate must drive the fill check"
        );
    }

    #[tokio::test]
    async fn measured_floor_does_not_disarm_an_effective_stage() {
        // The provider's measured prompt rides along from the previous
        // request (roughly the pre-compaction size). It floors the fill
        // check — never the effectiveness score, or every effective pass
        // would look like a no-op and disarm the stage for the session.
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        state
            .lock()
            .await
            .set_measured_prompt(estimate_tokens(&long_history()));
        let (sink, _stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        assert!(
            state.lock().await.disarmed.is_empty(),
            "an effective pass must not disarm under the measured floor"
        );
    }

    #[tokio::test]
    async fn compaction_events_ride_the_sink() {
        let engine = CompactionEngine::new(vec![stage(CompactionKind::Vcc, 0.6)]);
        let state = shared(CompactionState::default());
        let (sink, mut stream) = EventSink::new();
        let seeded = long_history();
        let tokens = estimate_tokens(&seeded);
        let context_length = ((tokens as f64 / 0.9) as u32).max(1);
        let history = history_of(seeded);
        assert!(
            engine
                .maybe_compact(
                    &state,
                    &summary_model("x"),
                    &history,
                    Some(context_length),
                    &sink
                )
                .await
        );
        // The stream only ends once every sender is gone, so drop the test's
        // sink before draining (the engine call above borrowed it, never
        // stored it).
        drop(sink);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            crate::harness::Event::Craft(CraftEvent::AutoCompacting { .. })
        )));
        let done = events
            .iter()
            .find_map(|event| match event {
                crate::harness::Event::Craft(CraftEvent::CompactionDone {
                    context_size_before,
                    context_size_after,
                    ..
                }) => Some((*context_size_before, *context_size_after)),
                _ => None,
            })
            .expect("CompactionDone after the commit");
        assert!(done.1 < done.0, "the compaction shrank the context");
    }
}
