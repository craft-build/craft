//! The append-only JSONL log: [`SessionLog`], parsing, and full-file writes.
//!
//! The written format is defined by [`records::Record`]; this module owns
//! the cursor state that makes incremental appends safe. Load-time
//! corruption policy lives in [`records`].

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use super::archive::archive_if_shrinking;
use super::index::update_cwd_index;
use super::legacy::remove_legacy_files;
use super::records::Record;
use super::{LOG_FORMAT_VERSION, SessionError, StoredSession, jsonl_path};
use crate::storage::StorageError;
use crate::storage::atomic::sync_parent_dir;

const EPOCH_CHANGED: &str = "messages were rewritten";
const FILE_CHANGED_UNDERNEATH: &str = "file changed underneath";
const CURSOR_AHEAD: &str = "cursor ahead of session";
pub(super) const LOG_BLOATED: &str = "too many stale meta records";
/// Every append leaves a whole meta record behind and only the last one is ever
/// read. Past this many, the log is rewritten and they all go away at once.
pub(super) const MAX_APPENDS: usize = 512;

// -- SessionLog: append-only persistence --

pub struct SessionLog {
    session_id: crate::id::CraftId,
    file: File,
    /// The session's `(epoch, rewrites)` at the last write. Appending is
    /// sound only while both stay the same.
    saved_epoch: u64,
    saved_rewrites: u64,
    /// Length of the file after the last write. Anything else means someone
    /// truncated, deleted or wrote it, and an append would corrupt it.
    saved_len: u64,
    saved_msg_count: usize,
    appends: usize,
    saved_sub_msg_counts: HashMap<String, usize>,
    /// Serialized trailing meta record; lets `append` persist meta-only
    /// changes without rewriting anything else.
    saved_meta: Vec<u8>,
}

fn sub_msg_snapshot(
    map: &HashMap<String, Arc<Vec<animist::history::HistoryEntry>>>,
) -> HashMap<String, usize> {
    map.iter().map(|(k, v)| (k.clone(), v.len())).collect()
}

impl SessionLog {
    /// Starts the file over: writes the whole log through a rename, so a crash
    /// mid-write leaves the old one intact, then claims the cwd index and
    /// sweeps pre-jsonl leftovers. A rewrite that drops messages (compaction,
    /// rewind) parks the previous file under `archive/<id>/` first, keeping the
    /// newest [`ARCHIVE_KEEP`](super::archive::ARCHIVE_KEEP) of them within
    /// [`ARCHIVE_MAX_BYTES`](super::archive::ARCHIVE_MAX_BYTES).
    pub fn rewrite(dir: &Path, session: &StoredSession) -> Result<Self, SessionError> {
        let log = Self::write_canonical(dir, session)?;
        update_cwd_index(dir, &session.cwd, session.id.id())?;
        Ok(log)
    }

    /// [`Self::rewrite`] without claiming the cwd index: migrating a legacy
    /// file on load must not make that session the cwd's latest.
    pub(super) fn write_canonical(
        dir: &Path,
        session: &StoredSession,
    ) -> Result<Self, SessionError> {
        fs::create_dir_all(dir).map_err(StorageError::from)?;
        let path = jsonl_path(dir, session.id.id());
        let tmp = path.with_extension("jsonl.tmp");

        let mut tmp_file = File::create(&tmp).map_err(StorageError::from)?;
        write_full_session(&mut tmp_file, session)?;
        tmp_file.sync_data().map_err(StorageError::from)?;
        // Last thing before the rename: a write that never lands must not
        // spend an archive slot, since taking one prunes the oldest.
        archive_if_shrinking(dir, session);
        fs::rename(&tmp, &path).map_err(StorageError::from)?;
        sync_parent_dir(&path);

        if let Err(e) = remove_legacy_files(dir, session.id.id()) {
            eprintln!("legacy session files remain after rewrite: {e}");
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;
        Ok(Self::cursor_from(session, file))
    }

    pub fn session_id(&self) -> crate::id::CraftId {
        self.session_id
    }

    /// Append one change-feed batch: the five record kinds of the change
    /// vocabulary plus the trailing meta record. The session's message list
    /// must already mirror the changes (see `StoredSession::apply_changes`);
    /// this log's message-count cursor is not advanced, so a feed log is
    /// owned exclusively by its persister and never mixed with [`Self::append`].
    pub fn append_changes(
        &mut self,
        session: &StoredSession,
        changes: &[animist::history::HistoryChange],
    ) -> Result<(), SessionError> {
        self.require_same_id(session)?;
        self.ensure_appendable(session)?;
        let mut buf = Vec::new();
        for change in changes {
            append_record(&mut buf, &super::records::record_of(change))?;
        }
        let meta = meta_record(session)?;
        if buf.is_empty() && meta == self.saved_meta {
            return Ok(());
        }
        buf.extend_from_slice(&meta);
        if let Err(e) = self
            .file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data())
        {
            let _ = self.file.set_len(self.saved_len);
            return Err(StorageError::from(e).into());
        }
        self.saved_len += buf.len() as u64;
        self.appends += 1;
        self.saved_meta = meta;
        Ok(())
    }

    pub fn append(&mut self, session: &StoredSession) -> Result<(), SessionError> {
        self.require_same_id(session)?;
        self.ensure_appendable(session)?;

        let mut buf = Vec::new();
        let mut new_msg_count = self.saved_msg_count;

        for entry in &session.messages[self.saved_msg_count..] {
            // Each saved message is a source-only entry record.
            append_record(
                &mut buf,
                &Record::Entry {
                    entry: entry.clone(),
                },
            )?;
            new_msg_count += 1;
        }

        let mut new_sub_counts: Vec<(String, usize)> = Vec::new();
        for (sub_id, msgs) in &session.subagent_messages {
            let saved = self.saved_sub_msg_counts.get(sub_id).copied().unwrap_or(0);
            for entry in &msgs[saved..] {
                append_record(
                    &mut buf,
                    &Record::SubEntry {
                        sub: sub_id.clone(),
                        entry: entry.clone(),
                    },
                )?;
            }
            if msgs.len() > saved {
                new_sub_counts.push((sub_id.clone(), msgs.len()));
            }
        }

        let meta = meta_record(session)?;
        if buf.is_empty() && meta == self.saved_meta {
            return Ok(());
        }
        buf.extend_from_slice(&meta);

        if let Err(e) = self
            .file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data())
        {
            // A failed write can leave partial bytes; roll back to the last
            // record boundary so the file matches the unadvanced cursors and
            // a retry appends cleanly instead of duplicating records.
            let _ = self.file.set_len(self.saved_len);
            return Err(StorageError::from(e).into());
        }

        self.saved_len += buf.len() as u64;
        self.appends += 1;
        self.saved_msg_count = new_msg_count;
        for (sub_id, count) in new_sub_counts {
            self.saved_sub_msg_counts.insert(sub_id, count);
        }
        self.saved_meta = meta;

        Ok(())
    }

    fn cursor_from(session: &StoredSession, file: File) -> Self {
        let saved_len = file.metadata().map(|m| m.len()).unwrap_or_default();
        Self {
            session_id: session.id.id(),
            file,
            saved_epoch: session.epoch,
            saved_rewrites: session.rewrites,
            saved_len,
            saved_msg_count: session.messages.len(),
            appends: 0,
            saved_sub_msg_counts: sub_msg_snapshot(&session.subagent_messages),
            saved_meta: meta_record(session).unwrap_or_default(),
        }
    }

    fn require_same_id(&self, session: &StoredSession) -> Result<(), SessionError> {
        if session.id.id() != self.session_id {
            return Err(SessionError::IdMismatch {
                log_id: self.session_id,
                given_id: session.id.id(),
            });
        }
        Ok(())
    }

    /// Ok only while every cursor still describes the file, which is what makes
    /// `saved_len` the file length and the rest of the cursors its content, and
    /// while appending is still cheaper than starting the file over.
    fn ensure_appendable(&self, session: &StoredSession) -> Result<(), SessionError> {
        let reason = if session.epoch != self.saved_epoch || session.rewrites != self.saved_rewrites
        {
            EPOCH_CHANGED
        } else if self.file.metadata().map_err(StorageError::from)?.len() != self.saved_len {
            FILE_CHANGED_UNDERNEATH
        } else if self.appends >= MAX_APPENDS {
            LOG_BLOATED
        } else if self.cursor_ahead(session) {
            // Nothing shrinks a session without minting a new epoch, so this
            // should never fire. It stays because the slices in `append` would
            // panic instead of corrupting if it ever does.
            CURSOR_AHEAD
        } else {
            return Ok(());
        };
        Err(SessionError::LogDiverged { reason })
    }

    fn cursor_ahead(&self, session: &StoredSession) -> bool {
        self.saved_msg_count > session.messages.len()
            || self.saved_sub_msg_counts.iter().any(|(sub, &count)| {
                session
                    .subagent_messages
                    .get(sub)
                    .is_none_or(|msgs| count > msgs.len())
            })
    }
}

// -- Record serialization, per section --

/// The meta section is written whole on every append, so its construction is
/// shared between the incremental append and the canonical rewrite.
fn meta_log_record(session: &StoredSession) -> Record {
    Record::Meta {
        title: session.title.clone(),
        token_usage: session.token_usage,
        updated_at: session.updated_at,
        subagents: session.subagents.clone(),
        usage_by_model: session.usage_by_model.clone(),
        meta: session.meta.clone(),
    }
}

fn meta_record(session: &StoredSession) -> Result<Vec<u8>, SessionError> {
    let mut buf = Vec::new();
    append_record(&mut buf, &meta_log_record(session))?;
    Ok(buf)
}

pub(super) fn write_full_session(
    file: &mut File,
    session: &StoredSession,
) -> Result<(), SessionError> {
    let mut buf = Vec::new();
    write_record(
        file,
        &mut buf,
        &Record::Header {
            v: LOG_FORMAT_VERSION,
            id: session.id.clone(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
        },
    )?;
    for entry in session.messages.iter() {
        write_record(
            file,
            &mut buf,
            &Record::Entry {
                entry: entry.clone(),
            },
        )?;
    }
    // `tool_outputs` is not written; records already on disk stay
    // readable through the loader.
    for (sub_id, msgs) in &session.subagent_messages {
        for entry in msgs.iter() {
            write_record(
                file,
                &mut buf,
                &Record::SubEntry {
                    sub: sub_id.clone(),
                    entry: entry.clone(),
                },
            )?;
        }
    }
    write_record(file, &mut buf, &meta_log_record(session))
}

fn write_record(file: &mut File, buf: &mut Vec<u8>, record: &Record) -> Result<(), SessionError> {
    buf.clear();
    append_record(buf, record)?;
    file.write_all(buf).map_err(StorageError::from)?;
    Ok(())
}

fn append_record(buf: &mut Vec<u8>, record: &Record) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}
