//! The live-session store: the surface-facing owner of a session's
//! persisted state (meta, drafts, model choices, cost ledger) and, through
//! the change-feed persister, of its entry records.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{SESSIONS_DIR, SessionError, SessionLog, StoredSession};
use crate::id::SessionRef;
use crate::storage::StateDir;
use crate::storage::StorageError;
use crate::storage::stats::{self, CostLedger, CostUsage};

/// The shortest gap between two writes that carry only soft session state
/// (the composer draft). Ported from the reference `SOFT_SAVE_DELAY`.
pub(crate) const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);

/// Owns a session's persisted state for the lifetime of one run. Save
/// failures are warnings, never fatal: a run must finish even when the disk
/// is unhappy.
pub struct SessionStore {
    dir: StateDir,
    /// Cost ledger handle, opened once so per-turn appends don't re-resolve
    /// the path. `None` when the state dir cannot host it.
    ledger: Option<CostLedger>,
    session: StoredSession,
    /// When the last disk write happened, so draft-only changes land at
    /// most once per [`SOFT_SAVE_DELAY`] (each costs a meta record plus an
    /// fsync).
    last_save: Instant,
    /// A soft change is waiting for its write window; flushed by
    /// [`SessionStore::checkpoint_now`].
    pending_soft_save: bool,
}

impl SessionStore {
    /// Open (or create) in the resolved state dir; `None` when the state dir
    /// is unavailable, meaning the session simply will not be persisted.
    pub fn open(session_ref: SessionRef, cwd: &str, model_spec: &str) -> Option<Self> {
        let dir = StateDir::resolve().ok()?;
        Self::open_in(dir, session_ref, cwd, model_spec).ok()
    }

    /// Open (or create) against an explicit dir; a fresh session is saved
    /// immediately so it is loadable before the first turn completes.
    /// Returns `Err` when an existing session cannot be read for any reason
    /// other than absence — the caller must disable persistence for the run
    /// so the unreadable file is never overwritten by a later save.
    pub fn open_in(
        dir: StateDir,
        session_ref: SessionRef,
        cwd: &str,
        model_spec: &str,
    ) -> Result<Self, SessionError> {
        // Opened once and reused for every turn's cost append; a failure here
        // only means no cost records, the session itself still persists.
        let ledger = CostLedger::from_state_dir(&dir).ok();
        match StoredSession::load(session_ref.id(), &dir) {
            Ok(session) => Ok(Self {
                dir,
                ledger,
                session,
                last_save: Instant::now(),
                pending_soft_save: false,
            }),
            Err(SessionError::Storage {
                source: StorageError::NotFound { .. },
            }) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = session_ref;
                let mut store = Self {
                    dir,
                    ledger,
                    session,
                    last_save: Instant::now(),
                    pending_soft_save: false,
                };
                store.save();
                Ok(store)
            }
            Err(e) => Err(e),
        }
    }

    fn save(&mut self) {
        self.last_save = Instant::now();
        self.pending_soft_save = false;
        if let Err(e) = self.session.save(&self.dir) {
            eprintln!("failed to persist session: {e}");
        }
    }

    /// Full canonical write, returning the log whose append cursors describe
    /// the file just written; the change-feed persister resumes from it.
    pub(crate) fn write_log(&mut self) -> Result<SessionLog, SessionError> {
        self.last_save = Instant::now();
        self.pending_soft_save = false;
        let sessions_dir = self.dir.ensure_subdir(SESSIONS_DIR)?;
        self.session.save_to(&sessions_dir)
    }

    /// The persisted session mirror.
    pub fn session(&self) -> &StoredSession {
        &self.session
    }

    /// Mutable access for the persister's mirror updates.
    pub(crate) fn session_mut(&mut self) -> &mut StoredSession {
        &mut self.session
    }

    /// A draft-only change: written at most once per [`SOFT_SAVE_DELAY`],
    /// bounding what a crash takes with it without charging a keystroke an
    /// fsync. Anything a turn produced still writes immediately. An empty
    /// draft clears the field.
    pub fn checkpoint_draft(&mut self, draft: &str) {
        let draft = (!draft.is_empty()).then(|| draft.to_owned());
        if self.session.meta.input_draft.as_deref() == draft.as_deref() {
            return;
        }
        self.session.set_input_draft(draft);
        if self.last_save.elapsed() >= SOFT_SAVE_DELAY {
            self.save();
        } else {
            self.pending_soft_save = true;
        }
    }

    /// Flush a pending soft write: shutdown, or a session swap, where no
    /// later frame will carry the change to disk.
    pub fn checkpoint_now(&mut self) {
        if self.pending_soft_save {
            self.save();
        }
    }

    /// How long until a pending soft write may land, so a caller can arm a
    /// timer instead of waiting for the next keystroke to carry it.
    pub fn soft_save_wait(&self) -> Option<Duration> {
        self.pending_soft_save
            .then(|| SOFT_SAVE_DELAY.saturating_sub(self.last_save.elapsed()))
    }

    /// The loaded session's persisted messages, so a resumed run seeds its
    /// history from the store instead of starting empty — the first save
    /// would otherwise wipe every prior turn.
    pub fn messages(&self) -> &[animist::history::HistoryEntry] {
        self.session.messages()
    }

    /// Persist the post-turn history and the model that served it, deriving
    /// the title from the first user message when it is still the default.
    /// The whole-history rewrite path for surfaces still on the old loop;
    /// the change-feed persister replaces it as the surfaces cut over.
    pub fn record_turn(&mut self, messages: &[crate::history::StoredMessage], model_spec: String) {
        self.session
            .replace_messages(crate::history::ext::entries(messages.to_vec()));
        self.session.set_model(model_spec);
        self.session.update_title_if_default();
        self.save();
    }

    /// Record the currently selected model without a turn, so the choice is
    /// restored on the next launch even when no message followed it.
    pub fn set_model(&mut self, model_spec: String) {
        self.session.set_model(model_spec);
        self.save();
    }

    pub fn thinking(&self) -> Option<crate::thinking::ThinkingConfig> {
        self.session.meta.thinking
    }

    pub fn set_thinking(&mut self, thinking: crate::thinking::ThinkingConfig) {
        self.session.set_thinking(thinking);
        self.save();
    }

    /// Persist the session mode so a resume restores it (`build`/`plan`).
    /// Mode changes are user-initiated and rare, so they write immediately.
    pub fn set_mode(&mut self, plan: bool) {
        let mode = if plan { "plan" } else { "build" };
        if self.session.meta.mode.as_deref() == Some(mode) {
            return;
        }
        self.session.set_mode(Some(mode.to_string()));
        self.save();
    }

    /// Fold a finished run's per-model usage into the session and append one
    /// cost-ledger record per model to `cost.jsonl`. Ledger failures are
    /// warnings, never fatal. A model whose cost could not be resolved records
    /// `cost_usd: null` — the token counts carry the information and callers
    /// must not show "$0.00" as a bill.
    pub fn record_cost(
        &mut self,
        by_model: &HashMap<String, crate::usage::StoredTokenUsage>,
        fast: bool,
    ) {
        if by_model.is_empty() {
            return;
        }
        for (spec, usage) in by_model {
            let stored = *usage;
            self.session.add_model_usage(spec, stored);
            if let Some(ledger) = &self.ledger {
                let (provider, model) = spec.split_once('/').unwrap_or(("", spec.as_str()));
                let record = stats::make_record(
                    self.session.id.id().to_string(),
                    model,
                    provider,
                    CostUsage::from_stored(usage),
                    usage.cost,
                    fast,
                );
                if let Err(e) = ledger.append(&record) {
                    eprintln!("warning: failed to append cost record: {e}");
                }
            }
        }
        self.save();
    }
}
