//! The change-feed persister: one per conversation, the only writer of the
//! session's entry records. Checkpoints read the history's change feed
//! (`changes_since`), append the batch as session records, and only then
//! advance the cursor; a cursor the feed no longer retains falls back to a
//! full rewrite with an epoch bump through the existing session machinery.

use animist::history::HistoryError;
use animist::history::{History, HistoryEntry, HistoryVersion};
use snafu::ResultExt;

use crate::error::{PersisterOpenSnafu, Result};
use crate::id::SessionRef;
use crate::storage::StateDir;
use crate::storage::sessions::{SessionError, SessionLog, SessionStore};

/// The conversation's persistence owner: the session store for meta and
/// load state, the append log for entry records, and the cursor into the
/// live history's change feed.
pub struct Persister {
    history: History,
    store: SessionStore,
    log: Option<SessionLog>,
    cursor: HistoryVersion,
}

impl Persister {
    /// Open (or create) in the resolved state dir; `None` when the state
    /// dir is unavailable, meaning the conversation simply is not persisted.
    pub async fn open(
        session_ref: SessionRef,
        cwd: &str,
        model_spec: &str,
    ) -> Option<Result<Self, SessionError>> {
        let dir = StateDir::resolve().ok()?;
        Some(Self::open_in(dir, session_ref, cwd, model_spec).await)
    }

    /// Open (or create) against an explicit dir. The restored entries are
    /// written whole once (migrating any legacy shape) and the fresh feed's
    /// cursor is taken from the restored history, whose feed starts empty.
    pub async fn open_in(
        dir: StateDir,
        session_ref: SessionRef,
        cwd: &str,
        model_spec: &str,
    ) -> Result<Self, SessionError> {
        let mut store = SessionStore::open_in(dir, session_ref, cwd, model_spec)?;
        let entries = store.messages().to_vec();
        let history = History::from_entries(entries);
        let cursor = history.snapshot().await.generation();
        let log = store.write_log()?;
        Ok(Self {
            history,
            store,
            log: Some(log),
            cursor,
        })
    }

    /// The conversation's history handle, restored from the loaded entries.
    pub fn history(&self) -> History {
        self.history.clone()
    }

    /// The loaded entries, for surfaces building a view before the first
    /// turn.
    pub fn entries(&self) -> &[HistoryEntry] {
        self.store.messages()
    }

    /// The underlying session store, for the surface's meta duties (drafts,
    /// model, thinking, mode, cost). A full rewrite it performs only
    /// invalidates the log's cursors; the next checkpoint recovers with a
    /// full rewrite of its own.
    pub fn store_mut(&mut self) -> &mut SessionStore {
        &mut self.store
    }

    /// Read access to the session store.
    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Write everything the history gained since the last checkpoint.
    /// Never fails the run for a disk problem: the caller logs and surfaces
    /// the error.
    pub async fn checkpoint(&mut self) -> std::result::Result<(), SessionError> {
        let changes = match self.history.changes_since(&self.cursor).await {
            Ok(changes) => changes,
            // The feed no longer retains this cursor (or the history was
            // swapped): the file must be rewritten whole, exactly the
            // existing epoch mechanism.
            Err(HistoryError::ChangesTooOld { .. } | HistoryError::ForeignVersion { .. }) => {
                return self.full_rewrite().await;
            }
            // `changes_since` reports no other failures; recovering with a
            // full rewrite keeps the file consistent if that ever changes.
            Err(other) => {
                tracing::warn!("change feed read failed ({other}); rewriting whole");
                return self.full_rewrite().await;
            }
        };
        if changes.changes().is_empty() {
            return Ok(());
        }
        // The in-memory mirror must not keep a batch the file rejected:
        // `apply_record` is not idempotent, so a retry after a failed
        // append would duplicate every entry. Snapshot (an `Arc` bump, not
        // a copy), apply, and roll the mirror back unless the append
        // landed.
        let mirror_before = self.store.session().messages_arc();
        let unapplied = self.store.session_mut().apply_changes(changes.changes());
        // A record the mirror could not apply would still be appended and
        // then fail every future load (the loader refuses out-of-range
        // records). Recover from the live history instead: the rewrite
        // carries what the feed actually did, file and mirror agree, and
        // the bad batch is dropped loudly rather than poisoning the log.
        if unapplied > 0 {
            tracing::warn!(
                "dropping {unapplied} change records the session mirror could not apply; \
                 rewriting the session from the live history"
            );
            return self.full_rewrite().await;
        }
        self.store.session_mut().update_title_if_default();
        let appended = match &mut self.log {
            Some(log) => log.append_changes(self.store.session(), changes.changes()),
            None => Ok(()),
        };
        match appended {
            Ok(()) => {
                self.cursor = changes.generation();
                Ok(())
            }
            Err(SessionError::LogDiverged { .. }) => self.full_rewrite().await,
            Err(error) => {
                self.store.session_mut().restore_messages_arc(mirror_before);
                Err(error)
            }
        }
    }

    /// Record one finished subagent's history and roster entry: the
    /// child conversation runs no persistence layer of its own, so its
    /// parent stores what it produced when it ends. The entries land as
    /// sub-entry records through the log's sub cursors; the roster rides
    /// the trailing meta record. A diverged log falls back to the full
    /// rewrite, which carries sub histories too.
    pub async fn record_subagent(
        &mut self,
        subagent: crate::storage::sessions::StoredSubagent,
        entries: Vec<HistoryEntry>,
    ) -> std::result::Result<(), SessionError> {
        let id = subagent.tool_use_id.clone();
        let prior_messages = self.store.session().subagent_messages().get(&id).cloned();
        let prior_roster = self.store.session().subagents().to_vec();
        self.store
            .session_mut()
            .set_subagent_messages(id.clone(), entries);
        self.store.session_mut().upsert_subagent(subagent);
        match &mut self.log {
            Some(log) => match log.append_sub_entries(self.store.session()) {
                Ok(()) => Ok(()),
                Err(SessionError::LogDiverged { .. }) => self.full_rewrite().await,
                // The mirror must not keep records the file rejected (the
                // same rule as `checkpoint`): roll back so a retry re-emits
                // them instead of the session silently diverging.
                Err(error) => {
                    self.store
                        .session_mut()
                        .restore_subagent_messages(&id, prior_messages);
                    self.store.session_mut().set_subagents(prior_roster);
                    Err(error)
                }
            },
            None => Ok(()),
        }
    }

    /// Rewrite the whole session (epoch bump via the session's own rewrite
    /// path) and resume the feed from the current snapshot.
    async fn full_rewrite(&mut self) -> std::result::Result<(), SessionError> {
        let snapshot = self.history.snapshot().await;
        self.store
            .session_mut()
            .replace_messages(snapshot.entries().to_vec());
        self.store.session_mut().update_title_if_default();
        self.log = Some(self.store.write_log()?);
        self.cursor = snapshot.generation();
        Ok(())
    }
}

/// Open a persister or report why the session cannot be persisted, with
/// `None` meaning "no state dir" (persistence silently off, as before).
pub async fn open_persister(
    session_ref: SessionRef,
    cwd: &str,
    model_spec: &str,
) -> Result<Option<Persister>> {
    match Persister::open(session_ref, cwd, model_spec).await {
        Some(result) => result.map(Some).context(PersisterOpenSnafu),
        None => Ok(None),
    }
}
