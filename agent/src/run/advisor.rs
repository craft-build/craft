//! Always-on lightweight advisor (C.12): a distinct, cheaper quality gate
//! that reviews the transcript delta after a terminal reply and emits at most
//! one deduped note.
//!
//! Positioning (kept distinct on purpose):
//! - `review` tool: on-demand subagent spawned by the model.
//! - advisor (here): always-on, delta-only, one inline note per run.
//!
//! The advisor sees only the messages added since the last review (the
//! delta). Its reply is parsed into a severity (`nit`/`concern`/`blocker`)
//! and a single line of guidance. An [`EmissionGuard`] normalizes the note,
//! drops content-free phrases, dedupes against a bounded FIFO, and allows at
//! most one note per update. Off by default (`agent.advisor.enabled`).
//!
//! Ported from the reference `craft-agent/src/agent/advisor.rs` +
//! `agent/run/flow.rs` wiring, re-expressed for this repo's Rig edge: the
//! review call goes through [`crate::edge::to_request`] +
//! `CompletionModel::completion`, like the auto-reviewer. Deviations from the
//! reference: no advisor model-role/spec resolution (the run's model serves
//! the review, which is the reference's own fallback), no cancel-raced
//! provider stream (the caller checks the token and the deadline bounds the
//! call), and state is per-run rather than per-session.

use std::collections::VecDeque;

use rig_core::completion::CompletionModel;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::warn;

use crate::config::{AdvisorAutoAct, AdvisorConfig};
use crate::edge;
use crate::history::Message;

const ADVISOR_SYSTEM: &str = "\
You are a lightweight code reviewer paired with an autonomous coding agent. \
You see only the agent's most recent activity (its delta). Look for real problems the agent \
rushed past: bugs, security issues, broken contracts, missing error handling, wrong assumptions. \
Do NOT comment on style, taste, or trivial formatting. Stay silent if there is nothing worth saying.\n\n\
Reply with exactly one line in this form, or nothing:\n\
SEVERITY: <one-line note>\n\
where SEVERITY is NIT (minor, non-blocking), CONCERN (should be addressed), or BLOCKER (will break). \
If the delta is fine, reply with the single word OK.";

const MAX_DELTA_MESSAGES: usize = 6;
const MAX_DELTA_CHARS: usize = 8_000;
/// Bounds the advisor call; a hung review must never wedge the run.
const REVIEW_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

const BLOCKER: &str = "blocker";
const CONCERN: &str = "concern";
const NIT: &str = "nit";
const OK_TOKEN: &str = "ok";
const CONTENT_FREE: &[&str] = &[
    "looks good",
    "no issues",
    "nothing to report",
    "all good",
    "seems fine",
    "lgtm",
];

pub(crate) const ADVISOR_REVIEWING_INFO: &str = "advisor reviewing recent activity…";

const ADVISOR_FOLLOWUP_PROMPT: &str = "<advisor-note>\nA lightweight advisor reviewed your last turn and flagged a {severity}:\n{note}\n\nAddress this concern before finishing. Make the change; keep it minimal and do not narrate it in comments or prose. Only explain if it does not apply, in one sentence.\n</advisor-note>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdvisorSeverity {
    Nit,
    Concern,
    Blocker,
}

impl AdvisorSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            AdvisorSeverity::Nit => NIT,
            AdvisorSeverity::Concern => CONCERN,
            AdvisorSeverity::Blocker => BLOCKER,
        }
    }

    /// The auto-act threshold this severity satisfies. Mirrors the
    /// declaration order of [`AdvisorAutoAct`] (`Off < Nit < Concern <
    /// Blocker`).
    fn threshold(self) -> AdvisorAutoAct {
        match self {
            AdvisorSeverity::Nit => AdvisorAutoAct::Nit,
            AdvisorSeverity::Concern => AdvisorAutoAct::Concern,
            AdvisorSeverity::Blocker => AdvisorAutoAct::Blocker,
        }
    }
}

/// Whether a note of `severity` should trigger an automatic follow-up turn,
/// given the configured `threshold`. `Off` never acts; a threshold acts on
/// notes at or above its own severity.
pub fn should_act(severity: AdvisorSeverity, threshold: AdvisorAutoAct) -> bool {
    threshold != AdvisorAutoAct::Off && severity.threshold() >= threshold
}

#[derive(Debug, Clone)]
pub struct AdvisorNote {
    pub severity: AdvisorSeverity,
    pub message: String,
}

/// Bounded FIFO that drops content-free phrases and exact duplicates.
#[derive(Debug, Default)]
pub struct EmissionGuard {
    seen: VecDeque<String>,
    capacity: usize,
}

impl EmissionGuard {
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
        }
    }

    /// Normalize, filter, and dedupe. Returns `None` when the note is
    /// content-free or an exact repeat of one already emitted this run.
    pub fn admit(&mut self, note: AdvisorNote) -> Option<AdvisorNote> {
        let normalized = normalize(&note.message);
        if normalized.is_empty() || is_content_free(&normalized) {
            return None;
        }
        let key = format!("{}:{}", note.severity.as_str(), normalized);
        if self.seen.iter().any(|s| *s == key) {
            return None;
        }
        if self.seen.len() >= self.capacity {
            self.seen.pop_front();
        }
        self.seen.push_back(key);
        Some(AdvisorNote {
            severity: note.severity,
            message: normalized,
        })
    }
}

fn normalize(s: &str) -> String {
    s.trim().trim_end_matches(['.', ',']).to_string()
}

fn is_content_free(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    CONTENT_FREE
        .iter()
        .any(|phrase| lower == *phrase || lower.starts_with(phrase))
}

/// State carried across a run: the index of the last-reviewed message and
/// the emission guard.
#[derive(Debug)]
pub struct AdvisorState {
    pub last_reviewed: usize,
    pub guard: EmissionGuard,
}

impl AdvisorState {
    pub fn with_dedup(dedup_size: usize) -> Self {
        Self {
            last_reviewed: 0,
            guard: EmissionGuard::new(dedup_size),
        }
    }
}

/// Review the transcript delta. Returns `Ok(None)` when the advisor is
/// silent, the note is content-free, or a duplicate. Provider failures are
/// logged and swallowed — an advisor problem must never fail the run.
pub async fn review<M: CompletionModel>(
    model: &M,
    state: &mut AdvisorState,
    history: &[Message],
) -> Option<AdvisorNote> {
    let delta = build_delta(history, state.last_reviewed);
    state.last_reviewed = history.len();
    if delta.is_empty() {
        return None;
    }

    let user_msg = format!("# Agent delta\n{delta}\n\nReview this delta. One line, or OK.");
    let messages = vec![Message::user(user_msg)];
    let request = edge::to_request(&messages, &[], Some(ADVISOR_SYSTEM), None, None);
    let response = match timeout(REVIEW_DEADLINE, model.completion(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            warn!(error = %e, "advisor review failed");
            return None;
        }
        Err(_) => {
            warn!("advisor review timed out");
            return None;
        }
    };
    let text = response
        .choice
        .iter()
        .filter_map(|block| match block {
            rig_core::completion::message::AssistantContent::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let note = parse_note(&text)?;
    state.guard.admit(note)
}

/// What the run loop does with an advisor note.
pub(crate) enum AdvisorTurnAction {
    Continue(AdvisorNote),
    Stop,
}

/// The reference's `advisor_turn_action`, minus the Flow goal-approval gate
/// (no pending-approval state exists in this run loop).
pub(crate) fn advisor_turn_action(
    note: Option<AdvisorNote>,
    cfg: &AdvisorConfig,
    continuations: u32,
) -> AdvisorTurnAction {
    let Some(note) = note else {
        return AdvisorTurnAction::Stop;
    };
    if continuations >= cfg.max_act_turns || !should_act(note.severity, cfg.auto_act) {
        return AdvisorTurnAction::Stop;
    }
    AdvisorTurnAction::Continue(note)
}

pub(crate) fn advisor_continuation_info(note: &AdvisorNote, continuation: u32, max: u32) -> String {
    format!(
        "advisor raised a {} ({}); continuing to address it ({continuation}/{max})",
        note.severity.as_str(),
        note.message,
    )
}

pub(crate) fn advisor_followup_message(note: &AdvisorNote) -> Message {
    Message::user(
        ADVISOR_FOLLOWUP_PROMPT
            .replace("{severity}", note.severity.as_str())
            .replace("{note}", &note.message),
    )
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else {
        let mut i = index;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

fn build_delta(history: &[Message], last_reviewed: usize) -> String {
    if history.len() <= last_reviewed {
        return String::new();
    }
    let tail: Vec<&Message> = history
        .iter()
        .skip(last_reviewed)
        .rev()
        .take(MAX_DELTA_MESSAGES)
        .collect();
    let mut out = String::new();
    for msg in tail.into_iter().rev() {
        if !out.is_empty() {
            out.push_str("\n---\n");
        }
        let role = match msg {
            Message::User { .. } => "user",
            _ => "assistant",
        };
        out.push_str(&format!("[{role}] "));
        out.push_str(&msg.text());
        if out.len() > MAX_DELTA_CHARS {
            let cut = floor_char_boundary(&out, MAX_DELTA_CHARS);
            out.truncate(cut);
            out.push_str("\n...(truncated)");
            break;
        }
    }
    out
}

fn parse_note(text: &str) -> Option<AdvisorNote> {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())?
        .trim()
        .to_ascii_lowercase();
    if line == OK_TOKEN || line.starts_with("ok ") || line == "ok." || line.starts_with("ok. ") {
        return None;
    }
    let (severity, rest) = parse_severity(&line)?;
    let message = normalize(rest);
    if message.is_empty() {
        return None;
    }
    Some(AdvisorNote { severity, message })
}

fn parse_severity(line: &str) -> Option<(AdvisorSeverity, &str)> {
    for (prefix, severity) in [
        ("blocker", AdvisorSeverity::Blocker),
        ("concern", AdvisorSeverity::Concern),
        ("nit", AdvisorSeverity::Nit),
    ] {
        if let Some(after) = line.strip_prefix(prefix) {
            let after = after.trim_start_matches([':', '-', ' ', '.']);
            return Some((severity, after));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdvisorAutoAct;
    use rig_core::test_utils::{MockCompletionModel, MockTurn};

    fn cfg(dedup: usize) -> AdvisorConfig {
        AdvisorConfig {
            enabled: true,
            dedup_size: dedup,
            auto_act: AdvisorAutoAct::Concern,
            max_act_turns: 2,
        }
    }

    #[test_case::test_case("BLOCKER: leaks secret", AdvisorSeverity::Blocker, "leaks secret" ; "blocker")]
    #[test_case::test_case("concern: missing error handling", AdvisorSeverity::Concern, "missing error handling" ; "concern")]
    #[test_case::test_case("nit: extra blank line", AdvisorSeverity::Nit, "extra blank line" ; "nit")]
    #[test_case::test_case("BLOCKER: x.", AdvisorSeverity::Blocker, "x" ; "trailing_punct_stripped")]
    fn parse_note_classifies(text: &str, sev: AdvisorSeverity, msg: &str) {
        let note = parse_note(text).unwrap();
        assert_eq!(note.severity, sev);
        assert_eq!(note.message, msg);
    }

    #[test_case::test_case("OK" ; "ok_uppercase")]
    #[test_case::test_case("ok" ; "ok_lowercase")]
    #[test_case::test_case("ok." ; "ok_with_dot")]
    #[test_case::test_case("looks fine" ; "looks_fine")]
    #[test_case::test_case("" ; "empty")]
    #[test_case::test_case("random prose with no severity" ; "no_severity")]
    fn parse_note_silent_when_ok_or_unparseable(text: &str) {
        assert!(parse_note(text).is_none());
    }

    #[test_case::test_case("blocker: real bug", true ; "admits_new")]
    #[test_case::test_case("looks good", false ; "drops_content_free")]
    #[test_case::test_case("no issues here", false ; "drops_content_free_phrase")]
    fn emission_guard_filters(note_text: &str, admitted: bool) {
        let mut guard = EmissionGuard::new(8);
        let note = AdvisorNote {
            severity: AdvisorSeverity::Blocker,
            message: note_text.into(),
        };
        assert_eq!(guard.admit(note).is_some(), admitted);
    }

    #[test]
    fn emission_guard_dedupes_exact_repeat() {
        let mut guard = EmissionGuard::new(8);
        let note = AdvisorNote {
            severity: AdvisorSeverity::Concern,
            message: "off by one".into(),
        };
        assert!(guard.admit(note.clone()).is_some());
        assert!(
            guard.admit(note).is_none(),
            "exact duplicate must be dropped"
        );
    }

    #[test]
    fn emission_guard_evicts_oldest_at_capacity() {
        let mut guard = EmissionGuard::new(2);
        for i in 0..3 {
            guard.admit(AdvisorNote {
                severity: AdvisorSeverity::Nit,
                message: format!("note {i}"),
            });
        }
        assert!(
            guard
                .admit(AdvisorNote {
                    severity: AdvisorSeverity::Nit,
                    message: "note 0".into(),
                })
                .is_some(),
            "evicted note should be re-admitted"
        );
    }

    #[test]
    fn build_delta_empty_when_no_new_messages() {
        let history = vec![Message::user("a")];
        assert_eq!(build_delta(&history, 1), "");
        assert_eq!(build_delta(&history, 5), "");
    }

    #[test]
    fn build_delta_includes_only_new_tail() {
        let history: Vec<Message> = (0..10).map(|i| Message::user(format!("m{i}"))).collect();
        let delta = build_delta(&history, 7);
        assert!(delta.contains("m7"));
        assert!(delta.contains("m9"));
        assert!(!delta.contains("m6"));
    }

    #[test]
    fn build_delta_truncates_at_char_boundary() {
        let big: String = "é".repeat(MAX_DELTA_CHARS + 100);
        let history = vec![Message::user(big)];
        let delta = build_delta(&history, 0);
        assert!(delta.len() <= MAX_DELTA_CHARS + 64);
        assert!(delta.ends_with("...(truncated)"));
    }

    #[test_case::test_case(AdvisorSeverity::Nit, AdvisorAutoAct::Off, false ; "off_never_acts")]
    #[test_case::test_case(AdvisorSeverity::Blocker, AdvisorAutoAct::Off, false ; "off_ignores_blocker")]
    #[test_case::test_case(AdvisorSeverity::Nit, AdvisorAutoAct::Nit, true ; "nit_acts_on_nit")]
    #[test_case::test_case(AdvisorSeverity::Concern, AdvisorAutoAct::Nit, true ; "nit_acts_on_concern")]
    #[test_case::test_case(AdvisorSeverity::Blocker, AdvisorAutoAct::Nit, true ; "nit_acts_on_blocker")]
    #[test_case::test_case(AdvisorSeverity::Nit, AdvisorAutoAct::Concern, false ; "concern_skips_nit")]
    #[test_case::test_case(AdvisorSeverity::Concern, AdvisorAutoAct::Concern, true ; "concern_acts_on_concern")]
    #[test_case::test_case(AdvisorSeverity::Blocker, AdvisorAutoAct::Concern, true ; "concern_acts_on_blocker")]
    #[test_case::test_case(AdvisorSeverity::Concern, AdvisorAutoAct::Blocker, false ; "blocker_skips_concern")]
    #[test_case::test_case(AdvisorSeverity::Blocker, AdvisorAutoAct::Blocker, true ; "blocker_acts_on_blocker")]
    fn should_act_thresholds(severity: AdvisorSeverity, threshold: AdvisorAutoAct, expected: bool) {
        assert_eq!(should_act(severity, threshold), expected);
    }

    #[test_case::test_case("concern", Some(AdvisorAutoAct::Concern) ; "parses_concern")]
    #[test_case::test_case("off", Some(AdvisorAutoAct::Off) ; "parses_off")]
    #[test_case::test_case("blocker", Some(AdvisorAutoAct::Blocker) ; "parses_blocker")]
    #[test_case::test_case("nit", Some(AdvisorAutoAct::Nit) ; "parses_nit")]
    #[test_case::test_case("bogus", None ; "rejects_unknown")]
    #[test_case::test_case("BLOCKER", None ; "rejects_uppercase")]
    fn advisor_auto_act_serde(input: &str, expected: Option<AdvisorAutoAct>) {
        let parsed: Result<AdvisorAutoAct, _> = serde_json::from_str(&format!("\"{input}\""));
        assert_eq!(parsed.ok(), expected);
    }

    #[test]
    fn advisor_turn_action_caps_continuations() {
        let note = || AdvisorNote {
            severity: AdvisorSeverity::Blocker,
            message: "real bug".into(),
        };
        assert!(matches!(
            advisor_turn_action(Some(note()), &cfg(8), 2),
            AdvisorTurnAction::Stop
        ));
        assert!(matches!(
            advisor_turn_action(Some(note()), &cfg(8), 1),
            AdvisorTurnAction::Continue(_)
        ));
        assert!(matches!(
            advisor_turn_action(None, &cfg(8), 0),
            AdvisorTurnAction::Stop
        ));
    }

    #[tokio::test]
    async fn review_returns_none_on_empty_history() {
        let model = MockCompletionModel::new(Vec::<MockTurn>::new());
        let mut state = AdvisorState::with_dedup(8);
        assert!(review(&model, &mut state, &[]).await.is_none());
        assert_eq!(model.request_count(), 0);
    }

    #[tokio::test]
    async fn review_parses_and_admits_a_note() {
        let model = MockCompletionModel::new([MockTurn::text("concern: unchecked unwrap")]);
        let mut state = AdvisorState::with_dedup(8);
        let history = vec![Message::user("do the thing"), Message::assistant("done")];
        let note = review(&model, &mut state, &history).await.unwrap();
        assert_eq!(note.severity, AdvisorSeverity::Concern);
        assert_eq!(note.message, "unchecked unwrap");
        assert_eq!(state.last_reviewed, history.len());
        // Exactly one locked-down call: no tools, advisor preamble.
        assert_eq!(model.request_count(), 1);
        let request = &model.requests()[0];
        assert!(request.tools.is_empty());
        assert_eq!(request.preamble.as_deref(), Some(ADVISOR_SYSTEM));
    }

    #[tokio::test]
    async fn review_swallows_provider_errors_and_dedupes_repeats() {
        let model = MockCompletionModel::new([
            MockTurn::text("blocker: real bug"),
            MockTurn::text("blocker: real bug"),
        ]);
        let mut state = AdvisorState::with_dedup(8);
        let history = vec![Message::user("go")];
        assert!(review(&model, &mut state, &history).await.is_some());
        // Same note again: deduped to silence.
        assert!(review(&model, &mut state, &history).await.is_none());

        let failing = MockCompletionModel::new([MockTurn::error("rate limited")]);
        let mut state = AdvisorState::with_dedup(8);
        assert!(review(&failing, &mut state, &history).await.is_none());
    }
}
