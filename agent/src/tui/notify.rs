//! Bell notifications: urgency-ranked, gated on whether the user is
//! watching the terminal. Ported from the reference `craft-ui`
//! `event_loop.rs` / `app/mod.rs` notification machinery, adapted to our
//! single-session event loop (no queue: a turn "settles" when the provider
//! status leaves Thinking/Running).

use std::time::{Duration, Instant};

/// Craft never turns focus reporting on under Windows, so anything that
/// looks like a focus record there is a guess rather than a report.
const TRUSTS_FOCUS_EVENTS: bool = !cfg!(windows);

/// How long input keeps an unproven terminal counting as watched. Short on
/// purpose: suppressing wrongly hides a finished turn, notifying wrongly only
/// costs a bell.
const INPUT_IMPLIES_WATCHING: Duration = Duration::from_secs(30);

/// Something the agent wants the user's attention for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Notification {
    TurnComplete { response: Option<String> },
    PermissionRequested { tool: Option<String> },
}

impl Notification {
    /// Prompts blocking the agent outrank turn completions.
    pub(crate) fn is_urgent(&self) -> bool {
        !matches!(self, Self::TurnComplete { .. })
    }

    /// Human-readable payload; the bell itself is content-free, so this only
    /// surfaces in tests today — it becomes the toast/OSC-9 text when those
    /// land.
    #[allow(dead_code)]
    pub(crate) fn message(&self) -> String {
        match self {
            Self::TurnComplete { response } => response
                .clone()
                .unwrap_or_else(|| "Agent turn complete".into()),
            Self::PermissionRequested { tool: Some(tool) } => {
                format!("Permission requested: {tool}")
            }
            Self::PermissionRequested { tool: None } => "Permission requested".into(),
        }
    }

    /// The reference's `error_completion`: a failed turn rings like a
    /// completion, but with the error wording.
    pub(crate) fn error_completion() -> Self {
        Self::TurnComplete {
            response: Some("Agent stopped with an error".into()),
        }
    }
}

/// Keep only the first `NOTIFICATION_PREVIEW_CHARS` characters of the last
/// reply, lazily, so a huge response never costs a full clone.
const NOTIFICATION_PREVIEW_CHARS: usize = 200;

fn notification_preview<'a>(chunks: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut preview: String = chunks
        .flat_map(str::split_whitespace)
        .enumerate()
        .flat_map(|(i, word)| (i > 0).then_some(' ').into_iter().chain(word.chars()))
        .take(NOTIFICATION_PREVIEW_CHARS)
        .collect();
    // Collapse the hard cut into an ellipsis so the preview reads as cut,
    // not truncated mid-glyph.
    if preview.chars().count() == NOTIFICATION_PREVIEW_CHARS {
        preview.push('…');
    }
    (!preview.is_empty()).then_some(preview)
}

/// Whether the user is watching this terminal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Focus {
    /// No focus report has arrived yet. Terminals that never send one (GNU
    /// screen, tmux without `focus-events on`) stay here for good, and one
    /// never arrives while the user simply keeps the window focused, so
    /// recent input is the only evidence available.
    Unproven {
        last_input: Option<Instant>,
    },
    Focused,
    Unfocused,
}

impl Default for Focus {
    fn default() -> Self {
        Self::Unproven { last_input: None }
    }
}

impl Focus {
    /// A prompt parks the agent on the user, so it rings even while they
    /// watch. A finished turn they can already see is just noise.
    pub(crate) fn allows(self, notification: &Notification) -> bool {
        notification.is_urgent() || !self.is_watched()
    }

    fn is_watched(self) -> bool {
        match self {
            Self::Focused => true,
            Self::Unfocused => false,
            Self::Unproven { last_input } => {
                last_input.is_some_and(|at| at.elapsed() < INPUT_IMPLIES_WATCHING)
            }
        }
    }

    pub(crate) fn report(&mut self, reported: Self) {
        if TRUSTS_FOCUS_EVENTS {
            *self = reported;
        }
    }

    /// Typing proves the user was here just now. It never latches: without
    /// a report to clear it, a terminal that cannot send `FocusLost` would
    /// go quiet for good, so the evidence expires on its own.
    pub(crate) fn note_input(&mut self) {
        match self {
            Self::Unfocused => *self = Self::Focused,
            Self::Unproven { last_input } => *last_input = Some(Instant::now()),
            Self::Focused => {}
        }
    }

    /// An editor, a shell or a suspend eats the focus reports we would have
    /// seen, so assume the user may have walked away.
    pub(crate) fn on_resume(&mut self) {
        *self = Self::Unproven { last_input: None };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingCompletion {
    WaitingForQueueDrain(Notification),
    Due(Notification),
}

/// Per-turn bell bookkeeping. `reconcile` is the single decision point: it
/// returns the notification to ring, if any.
#[derive(Default)]
pub(crate) struct RunNotificationState {
    response_candidate: Option<String>,
    pending_completion: Option<PendingCompletion>,
    last_attention: Option<Notification>,
}

impl RunNotificationState {
    /// A queued follow-up replaces the in-flight turn, so any pending bell
    /// for the old one is void.
    pub(crate) fn on_new_turn(&mut self) {
        self.response_candidate = None;
        self.pending_completion = None;
    }

    pub(crate) fn on_turn_complete(&mut self, message: &str) {
        self.response_candidate = notification_preview([message].into_iter());
    }

    pub(crate) fn on_done(&mut self, failed: bool) {
        let notification = if failed {
            self.response_candidate = None;
            Notification::error_completion()
        } else {
            Notification::TurnComplete {
                response: self.response_candidate.take(),
            }
        };
        self.pending_completion = Some(PendingCompletion::WaitingForQueueDrain(notification));
    }

    pub(crate) fn on_drain(&mut self) {
        self.pending_completion = match self.pending_completion.take() {
            Some(PendingCompletion::WaitingForQueueDrain(notification)) => {
                Some(PendingCompletion::Due(notification))
            }
            pending => pending,
        };
    }

    pub(crate) fn on_manual_exit(&mut self) {
        self.pending_completion = None;
    }

    /// True between Done/Failed and the turn settling. An exit must not fire
    /// in that window: a queued follow-up may still start a new turn.
    /// (Test-only today: the loop drains immediately, so nothing else asks.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn waiting_for_drain(&self) -> bool {
        matches!(
            self.pending_completion,
            Some(PendingCompletion::WaitingForQueueDrain(_))
        )
    }

    /// Decide what rings now. `attention` is a pending blocking prompt
    /// (permission request); `settled` means the session is idle with no
    /// blocking prompt outstanding.
    pub(crate) fn reconcile(
        &mut self,
        attention: Option<Notification>,
        settled: bool,
    ) -> Option<Notification> {
        let prompt = (attention != self.last_attention)
            .then(|| attention.clone())
            .flatten();
        self.last_attention = attention;

        // A due completion is decided on its first reconcile: fire if the
        // session settled, otherwise drop it for good.
        let completion = match self.pending_completion.take() {
            Some(PendingCompletion::Due(notification)) => settled.then_some(notification),
            waiting => {
                self.pending_completion = waiting;
                None
            }
        };
        // A prompt due in the same reconcile outranks the completion.
        select_notification(prompt, completion)
    }
}

/// An urgent candidate displaces a non-urgent selected one; anything else
/// keeps the current selection.
pub(crate) fn select_notification(
    selected: Option<Notification>,
    candidate: Option<Notification>,
) -> Option<Notification> {
    match (selected, candidate) {
        (Some(current), Some(candidate)) if candidate.is_urgent() && !current.is_urgent() => {
            Some(candidate)
        }
        (selected @ Some(_), _) => selected,
        (None, candidate) => candidate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_complete() -> Notification {
        Notification::TurnComplete { response: None }
    }

    #[test]
    fn completions_are_not_urgent_but_prompts_are() {
        assert!(!turn_complete().is_urgent());
        assert!(!Notification::error_completion().is_urgent());
        assert!(Notification::PermissionRequested { tool: None }.is_urgent());
    }

    #[test]
    fn urgent_candidate_displaces_non_urgent_selection() {
        let prompt = Notification::PermissionRequested { tool: None };
        assert_eq!(
            select_notification(Some(turn_complete()), Some(prompt.clone())),
            Some(prompt.clone())
        );
        // Non-urgent candidate cannot displace anything.
        assert_eq!(
            select_notification(Some(prompt.clone()), Some(turn_complete())),
            Some(prompt)
        );
        assert_eq!(
            select_notification(None, Some(turn_complete())),
            Some(turn_complete())
        );
        assert_eq!(select_notification(None, None), None);
    }

    #[test]
    fn focused_focus_suppresses_completion_but_not_prompts() {
        let prompt = Notification::PermissionRequested { tool: None };
        assert!(!Focus::Focused.allows(&turn_complete()));
        assert!(Focus::Focused.allows(&prompt));
        assert!(Focus::Unfocused.allows(&turn_complete()));
    }

    #[test]
    fn unproven_focus_watches_only_after_recent_input() {
        let fresh = Focus::Unproven {
            last_input: Some(Instant::now()),
        };
        assert!(!fresh.allows(&turn_complete()));
        let stale = Focus::Unproven {
            last_input: Some(Instant::now() - INPUT_IMPLIES_WATCHING - Duration::from_secs(1)),
        };
        assert!(stale.allows(&turn_complete()));
        let never = Focus::Unproven { last_input: None };
        assert!(never.allows(&turn_complete()));
    }

    #[test]
    fn input_latches_focus_and_expires_via_report() {
        let mut focus = Focus::Unfocused;
        focus.note_input();
        assert!(!focus.allows(&turn_complete()));
        focus.report(Focus::Unfocused);
        assert!(focus.allows(&turn_complete()));
        // Resume after an editor: assume the user may have walked away.
        focus.on_resume();
        assert!(focus.allows(&turn_complete()));
    }

    #[test]
    fn done_then_drain_fires_only_when_settled() {
        let mut state = RunNotificationState::default();
        state.on_turn_complete("hello world");
        state.on_done(false);
        assert!(state.waiting_for_drain());
        // Not settled yet: nothing fires, and the pending stays waiting.
        assert_eq!(state.reconcile(None, false), None);
        assert!(state.waiting_for_drain());
        state.on_drain();
        // Settled now: the completion fires exactly once.
        let fired = state.reconcile(None, true).expect("completion rings");
        assert_eq!(
            fired,
            Notification::TurnComplete {
                response: Some("hello world".into())
            }
        );
        assert_eq!(state.reconcile(None, true), None);
    }

    #[test]
    fn due_completion_is_dropped_when_not_settled() {
        let mut state = RunNotificationState::default();
        state.on_done(true);
        state.on_drain();
        assert_eq!(state.reconcile(None, false), None);
        // Dropped for good: settling later does not resurrect it.
        assert_eq!(state.reconcile(None, true), None);
    }

    #[test]
    fn error_completion_carries_error_wording() {
        let mut state = RunNotificationState::default();
        state.on_turn_complete("partial reply");
        state.on_done(true);
        state.on_drain();
        let fired = state.reconcile(None, true).expect("error rings");
        assert_eq!(fired.message(), "Agent stopped with an error");
    }

    #[test]
    fn attention_prompt_fires_on_change_only() {
        let mut state = RunNotificationState::default();
        let prompt = Notification::PermissionRequested {
            tool: Some("bash".into()),
        };
        assert_eq!(
            state.reconcile(Some(prompt.clone()), false),
            Some(prompt.clone())
        );
        // Same attention again: no repeat ring.
        assert_eq!(state.reconcile(Some(prompt.clone()), false), None);
        // Cleared then re-raised: rings again.
        assert_eq!(state.reconcile(None, false), None);
        assert_eq!(state.reconcile(Some(prompt.clone()), false), Some(prompt));
    }

    #[test]
    fn prompt_outranks_completion_in_reconcile() {
        let mut state = RunNotificationState::default();
        state.on_done(false);
        state.on_drain();
        let prompt = Notification::PermissionRequested { tool: None };
        // Both due in the same reconcile: the prompt wins and is returned.
        assert_eq!(state.reconcile(Some(prompt.clone()), true), Some(prompt));
    }

    #[test]
    fn manual_exit_and_new_turn_void_pending() {
        let mut state = RunNotificationState::default();
        state.on_done(false);
        state.on_manual_exit();
        state.on_drain();
        assert_eq!(state.reconcile(None, true), None);

        state.on_turn_complete("stale");
        state.on_done(false);
        state.on_new_turn();
        state.on_drain();
        assert_eq!(state.reconcile(None, true), None);
    }

    #[test]
    fn preview_collapses_whitespace_and_caps_length() {
        let long = "word ".repeat(100);
        state_preview(&long);
    }

    fn state_preview(text: &str) {
        let mut state = RunNotificationState::default();
        state.on_turn_complete(text);
        state.on_done(false);
        state.on_drain();
        match state.reconcile(None, true) {
            Some(Notification::TurnComplete { response: Some(p) }) => {
                assert!(p.chars().count() <= 201); // 200 + ellipsis
                assert!(p.starts_with("word"));
            }
            other => panic!("expected preview completion, got {other:?}"),
        }
    }
}
