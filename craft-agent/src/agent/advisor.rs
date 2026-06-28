//! Always-on lightweight advisor: a distinct, cheaper quality gate that reviews
//! the transcript delta each turn and emits at most one deduped note.
//!
//! Positioning (kept distinct on purpose):
//! - `judge` (`judge.rs`): goal-completion, only when a goal is set.
//! - `review` (`tools/review.rs`): on-demand subagent spawned by the model.
//! - `advisor` (here): always-on, delta-only, one inline note per turn.
//!
//! The advisor sees only the messages added since the last review (the delta),
//! rendered to markdown. Its reply is parsed into a severity (`nit`/`concern`/
//! `blocker`) and a single line of guidance. An `EmissionGuard` normalizes the
//! note, drops content-free phrases, dedupes against a bounded FIFO, and allows
//! at most one note per update. Off by default (`config.advisor.enabled`).

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use craft_config::AdvisorConfig;
use craft_config::model_roles::ModelRole;
use craft_providers::provider::Provider;
use craft_providers::{Message, Model, RequestOptions};

use crate::AgentError as CrateAgentError;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// content-free or a near-exact repeat of one already emitted this session.
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

/// State carried across turns: the index of the last-reviewed message and the
/// emission guard. Reset on compaction or session switch.
#[derive(Debug, Default)]
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

    pub fn reset(&mut self, config: &AdvisorConfig) {
        self.last_reviewed = 0;
        self.guard = EmissionGuard::new(config.dedup_size);
    }
}

/// Review the transcript delta. Returns `Ok(None)` when the advisor is silent,
/// the note is content-free, or a duplicate.
pub async fn review(
    state: &mut AdvisorState,
    history: &[Message],
    config: &AdvisorConfig,
    active_provider: &Arc<dyn Provider>,
    active_model: &Model,
    timeouts: craft_providers::Timeouts,
    session_id: Option<&str>,
) -> Result<Option<AdvisorNote>, CrateAgentError> {
    let delta = build_delta(history, state.last_reviewed);
    state.last_reviewed = history.len();
    if delta.is_empty() {
        return Ok(None);
    }

    let user_msg = format!("# Agent delta\n{delta}\n\nReview this delta. One line, or OK.");
    let messages = vec![Message::user(user_msg)];

    let (provider, model): (Arc<dyn Provider>, Model) = resolve_advisor(
        config,
        Arc::clone(active_provider),
        active_model.clone(),
        timeouts,
    )
    .await;

    let text = collect_text(provider.as_ref(), &model, &messages, session_id).await?;
    let Some(note) = parse_note(&text) else {
        return Ok(None);
    };
    Ok(state.guard.admit(note))
}

async fn resolve_advisor(
    config: &AdvisorConfig,
    fallback_provider: Arc<dyn Provider>,
    fallback_model: Model,
    timeouts: craft_providers::Timeouts,
) -> (Arc<dyn Provider>, Model) {
    if let Some(spec) = config.model.as_deref() {
        let mut model = match Model::from_spec(spec) {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, spec, "advisor model spec invalid; using active model");
                return (fallback_provider, fallback_model);
            }
        };
        match craft_providers::provider::from_model(&mut model, timeouts).await {
            Ok(p) => return (Arc::from(p), model),
            Err(e) => warn!(error = %e, spec, "advisor provider build failed; using active model"),
        }
    }
    let role = craft_providers::roles::resolve_role(
        ModelRole::Advisor,
        fallback_model.clone(),
        Arc::clone(&fallback_provider),
        timeouts,
    )
    .await;
    (role.primary.provider, role.primary.model)
}

async fn collect_text(
    provider: &dyn Provider,
    model: &Model,
    messages: &[Message],
    session_id: Option<&str>,
) -> Result<String, CrateAgentError> {
    let (ptx, _prx) = flume::unbounded();
    let system = ADVISOR_SYSTEM.to_string();
    let tools = Value::Array(vec![]);
    let response = provider
        .stream_message(
            model,
            messages,
            &system,
            &tools,
            &ptx,
            RequestOptions::default(),
            session_id,
        )
        .await?;
    Ok(response.message.user_text().unwrap_or_default().to_string())
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
        let role = match msg.role {
            craft_providers::Role::User => "user",
            craft_providers::Role::Assistant => "assistant",
        };
        out.push_str(&format!("[{role}] "));
        if let Some(t) = msg.user_text() {
            out.push_str(t);
        }
        if out.len() > MAX_DELTA_CHARS {
            let cut = out.floor_char_boundary(MAX_DELTA_CHARS);
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
    let note = AdvisorNote { severity, message };
    info!(severity = note.severity.as_str(), note = %note.message, "advisor note");
    Some(note)
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
    use test_case::test_case;

    fn cfg(dedup: usize) -> AdvisorConfig {
        AdvisorConfig {
            enabled: true,
            model: None,
            dedup_size: dedup,
        }
    }

    #[test_case("BLOCKER: leaks secret", AdvisorSeverity::Blocker, "leaks secret" ; "blocker")]
    #[test_case("concern: missing error handling", AdvisorSeverity::Concern, "missing error handling" ; "concern")]
    #[test_case("nit: extra blank line", AdvisorSeverity::Nit, "extra blank line" ; "nit")]
    #[test_case("BLOCKER: x.", AdvisorSeverity::Blocker, "x" ; "trailing_punct_stripped")]
    fn parse_note_classifies(text: &str, sev: AdvisorSeverity, msg: &str) {
        let note = parse_note(text).unwrap();
        assert_eq!(note.severity, sev);
        assert_eq!(note.message, msg);
    }

    #[test_case("OK", None ; "ok_uppercase")]
    #[test_case("ok", None ; "ok_lowercase")]
    #[test_case("ok.", None ; "ok_with_dot")]
    #[test_case("looks fine", None ; "looks_fine")]
    #[test_case("", None ; "empty")]
    fn parse_note_silent_when_ok_or_empty(text: &str, _expected: Option<()>) {
        assert!(parse_note(text).is_none());
    }

    #[test_case("blocker: real bug", true ; "admits_new")]
    #[test_case("looks good", false ; "drops_content_free")]
    #[test_case("no issues here", false ; "drops_content_free_phrase")]
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
        // After 3 admits at capacity 2, the first should have been evicted.
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
        let history = vec![Message::user("a".into())];
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

    #[test]
    fn advisor_state_reset_clears_history() {
        let mut state = AdvisorState::with_dedup(8);
        state.last_reviewed = 42;
        state.guard.admit(AdvisorNote {
            severity: AdvisorSeverity::Nit,
            message: "x".into(),
        });
        state.reset(&cfg(8));
        assert_eq!(state.last_reviewed, 0);
        assert!(
            state
                .guard
                .admit(AdvisorNote {
                    severity: AdvisorSeverity::Nit,
                    message: "x".into(),
                })
                .is_some()
        );
    }

    #[test]
    fn severity_as_str_roundtrips() {
        assert_eq!(AdvisorSeverity::Blocker.as_str(), "blocker");
        assert_eq!(AdvisorSeverity::Concern.as_str(), "concern");
        assert_eq!(AdvisorSeverity::Nit.as_str(), "nit");
    }
}
