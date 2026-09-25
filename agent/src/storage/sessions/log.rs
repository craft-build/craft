//! The append-only JSONL log: record types, [`SessionLog`], parsing, and
//! full-file writes.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::Session;
use super::archive::archive_if_shrinking;
use super::index::update_cwd_index;
use super::legacy::remove_legacy_files;
use super::{
    DEFAULT_TITLE, LOG_FORMAT_VERSION, SESSION_VERSION, SessionError, SessionMeta, StoredSubagent,
    StoredTokenUsage, jsonl_path, normalize_title,
};
use crate::id::{CraftId, SessionRef};
use crate::storage::StorageError;
use crate::storage::atomic::sync_parent_dir;

const EPOCH_CHANGED: &str = "messages were rewritten";
const FILE_CHANGED_UNDERNEATH: &str = "file changed underneath";
const CURSOR_AHEAD: &str = "cursor ahead of session";
pub(super) const LOG_BLOATED: &str = "too many stale meta records";
/// Every append leaves a whole meta record behind and only the last one is ever
/// read. Past this many, the log is rewritten and they all go away at once.
pub(super) const MAX_APPENDS: usize = 512;

// -- JSONL record types --

#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
#[allow(clippy::large_enum_variant)]
enum LogRecord<M, U, T> {
    #[serde(rename = "header")]
    Header {
        v: u32,
        id: SessionRef,
        model: String,
        cwd: String,
        created_at: u64,
    },
    #[serde(rename = "msg")]
    Msg { d: M },
    #[serde(rename = "out")]
    Out { id: String, d: T },
    #[serde(rename = "sub_msg")]
    SubMsg { sub: String, d: M },
    #[serde(rename = "meta")]
    Meta {
        title: String,
        token_usage: U,
        updated_at: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        subagents: Vec<StoredSubagent>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        usage_by_model: HashMap<String, StoredTokenUsage>,
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

// -- SessionLog: append-only persistence --

pub struct SessionLog {
    session_id: CraftId,
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
    saved_tool_ids: HashSet<String>,
    saved_sub_msg_counts: HashMap<String, usize>,
    /// Serialized trailing meta record; lets `append` persist meta-only
    /// changes without rewriting anything else.
    saved_meta: Vec<u8>,
}

fn sub_msg_snapshot<M>(map: &HashMap<String, Arc<Vec<M>>>) -> HashMap<String, usize> {
    map.iter().map(|(k, v)| (k.clone(), v.len())).collect()
}

impl SessionLog {
    /// Starts the file over: writes the whole log through a rename, so a crash
    /// mid-write leaves the old one intact, then claims the cwd index and
    /// sweeps pre-jsonl leftovers. A rewrite that drops messages (compaction,
    /// rewind) parks the previous file under `archive/<id>/` first, keeping the
    /// newest [`ARCHIVE_KEEP`](super::archive::ARCHIVE_KEEP) of them within
    /// [`ARCHIVE_MAX_BYTES`](super::archive::ARCHIVE_MAX_BYTES).
    pub fn rewrite<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let log = Self::write_canonical(dir, session)?;
        update_cwd_index(dir, &session.cwd, session.id.id())?;
        Ok(log)
    }

    /// [`Self::rewrite`] without claiming the cwd index: migrating a legacy
    /// file on load must not make that session the cwd's latest.
    pub(super) fn write_canonical<M, U, T>(
        dir: &Path,
        session: &Session<M, U, T>,
    ) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
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

    pub fn session_id(&self) -> CraftId {
        self.session_id
    }

    pub fn append<M, U, T>(&mut self, session: &Session<M, U, T>) -> Result<(), SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        self.require_same_id(session)?;
        self.ensure_appendable(session)?;

        let mut buf = Vec::new();
        let mut new_msg_count = self.saved_msg_count;
        let mut new_tool_ids = Vec::new();

        for msg in &session.messages[self.saved_msg_count..] {
            append_record(&mut buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
            new_msg_count += 1;
        }

        for (id, output) in &session.tool_outputs {
            if !self.saved_tool_ids.contains(id) {
                append_record(
                    &mut buf,
                    &LogRecord::<&M, &U, &T>::Out {
                        id: id.clone(),
                        d: output,
                    },
                )?;
                new_tool_ids.push(id.clone());
            }
        }

        let mut new_sub_counts: Vec<(String, usize)> = Vec::new();
        for (sub_id, msgs) in &session.subagent_messages {
            let saved = self.saved_sub_msg_counts.get(sub_id).copied().unwrap_or(0);
            for msg in &msgs[saved..] {
                append_record(
                    &mut buf,
                    &LogRecord::<&M, &U, &T>::SubMsg {
                        sub: sub_id.clone(),
                        d: msg,
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
        self.saved_tool_ids.extend(new_tool_ids);
        for (sub_id, count) in new_sub_counts {
            self.saved_sub_msg_counts.insert(sub_id, count);
        }
        self.saved_meta = meta;

        Ok(())
    }

    fn cursor_from<M, U, T>(session: &Session<M, U, T>, file: File) -> Self
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let saved_len = file.metadata().map(|m| m.len()).unwrap_or_default();
        Self {
            session_id: session.id.id(),
            file,
            saved_epoch: session.epoch,
            saved_rewrites: session.rewrites,
            saved_len,
            saved_msg_count: session.messages.len(),
            appends: 0,
            saved_tool_ids: session.tool_outputs.keys().cloned().collect(),
            saved_sub_msg_counts: sub_msg_snapshot(&session.subagent_messages),
            saved_meta: meta_record(session).unwrap_or_default(),
        }
    }

    fn require_same_id<M, U, T>(&self, session: &Session<M, U, T>) -> Result<(), SessionError> {
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
    fn ensure_appendable<M, U, T>(&self, session: &Session<M, U, T>) -> Result<(), SessionError> {
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

    fn cursor_ahead<M, U, T>(&self, session: &Session<M, U, T>) -> bool {
        self.saved_msg_count > session.messages.len()
            || self
                .saved_tool_ids
                .iter()
                .any(|id| !session.tool_outputs.contains_key(id))
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
fn meta_log_record<M, U, T>(session: &Session<M, U, T>) -> LogRecord<&M, &U, &T>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    LogRecord::Meta {
        title: session.title.clone(),
        token_usage: &session.token_usage,
        updated_at: session.updated_at,
        subagents: session.subagents.clone(),
        usage_by_model: session.usage_by_model.clone(),
        meta: session.meta.clone(),
    }
}

fn meta_record<M, U, T>(session: &Session<M, U, T>) -> Result<Vec<u8>, SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    append_record(&mut buf, &meta_log_record(session))?;
    Ok(buf)
}

pub(super) fn write_full_session<M, U, T>(
    file: &mut File,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    write_header_record(file, &mut buf, session)?;
    write_message_records(file, &mut buf, session)?;
    write_tool_output_records(file, &mut buf, session)?;
    write_subagent_records(file, &mut buf, session)?;
    write_meta_record(file, &mut buf, session)
}

fn write_header_record<M, U, T>(
    file: &mut File,
    buf: &mut Vec<u8>,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    write_record(
        file,
        buf,
        &LogRecord::<&M, &U, &T>::Header {
            v: LOG_FORMAT_VERSION,
            id: session.id.clone(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
        },
    )
}

fn write_message_records<M, U, T>(
    file: &mut File,
    buf: &mut Vec<u8>,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    for msg in session.messages.iter() {
        write_record(file, buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
    }
    Ok(())
}

fn write_tool_output_records<M, U, T>(
    file: &mut File,
    buf: &mut Vec<u8>,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    for (id, output) in &session.tool_outputs {
        write_record(
            file,
            buf,
            &LogRecord::<&M, &U, &T>::Out {
                id: id.clone(),
                d: output,
            },
        )?;
    }
    Ok(())
}

fn write_subagent_records<M, U, T>(
    file: &mut File,
    buf: &mut Vec<u8>,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    for (sub_id, msgs) in &session.subagent_messages {
        for msg in msgs.iter() {
            write_record(
                file,
                buf,
                &LogRecord::<&M, &U, &T>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
        }
    }
    Ok(())
}

fn write_meta_record<M, U, T>(
    file: &mut File,
    buf: &mut Vec<u8>,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    write_record(file, buf, &meta_log_record(session))
}

fn write_record<R: Serialize>(
    file: &mut File,
    buf: &mut Vec<u8>,
    record: &R,
) -> Result<(), SessionError> {
    buf.clear();
    append_record(buf, record)?;
    file.write_all(buf).map_err(StorageError::from)?;
    Ok(())
}

fn append_record<R: Serialize>(buf: &mut Vec<u8>, record: &R) -> Result<(), SessionError> {
    serde_json::to_writer(&mut *buf, record).map_err(StorageError::from)?;
    buf.push(b'\n');
    Ok(())
}

// -- Parsing --

/// Tag-only probe used to classify a line that failed the strict `LogRecord`
/// parse: distinguishes a header with a bad id from a genuinely unknown record.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum RawTag {
    Header {
        id: String,
    },
    #[serde(other)]
    Other,
}

fn load_jsonl<M, U, T>(data: &[u8], display_path: &str) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let mut line_count = 0usize;

    let mut id: Option<SessionRef> = None;
    let mut model = String::new();
    let mut cwd = String::new();
    let mut created_at = 0u64;
    let mut messages = Vec::new();
    let mut tool_outputs = HashMap::new();
    let mut subagent_messages: HashMap<String, Vec<M>> = HashMap::new();
    let mut title = DEFAULT_TITLE.to_string();
    let mut token_usage = U::default();
    let mut updated_at = 0u64;
    let mut subagents = Vec::new();
    let mut usage_by_model = HashMap::new();
    let mut meta = SessionMeta::default();
    let mut got_header = false;

    for line in data.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        line_count += 1;
        let record: LogRecord<M, U, T> = match serde_json::from_slice(line) {
            Ok(r) => r,
            Err(e) => {
                // A header whose only defect is an unparseable id fails the
                // strict LogRecord parse; surface that precisely instead of
                // silently skipping to a misleading NotFound.
                if !got_header
                    && let Ok(RawTag::Header { id: raw_id }) =
                        serde_json::from_slice::<RawTag>(line)
                    && let Err(source) = raw_id.parse::<CraftId>()
                {
                    return Err(SessionError::CorruptHeaderId {
                        path: display_path.to_string(),
                        raw_id,
                        source,
                    });
                }
                eprintln!("skipping unrecognized JSONL record at {display_path}:{line_count}: {e}");
                continue;
            }
        };
        match record {
            LogRecord::Header {
                v,
                id: h_id,
                model: h_model,
                cwd: h_cwd,
                created_at: h_created,
            } => {
                if v > LOG_FORMAT_VERSION {
                    return Err(SessionError::VersionMismatch {
                        found: v,
                        expected: LOG_FORMAT_VERSION,
                    });
                }
                id = Some(h_id);
                model = h_model;
                cwd = h_cwd;
                created_at = h_created;
                got_header = true;
            }
            LogRecord::Msg { d } => messages.push(d),
            LogRecord::Out { id: out_id, d } => {
                tool_outputs.insert(out_id, Arc::new(d));
            }
            LogRecord::SubMsg { sub, d } => {
                subagent_messages.entry(sub).or_default().push(d);
            }
            LogRecord::Meta {
                title: m_title,
                token_usage: m_usage,
                updated_at: m_updated,
                subagents: m_subagents,
                usage_by_model: m_usage_by_model,
                meta: m_meta,
            } => {
                title = m_title;
                token_usage = m_usage;
                updated_at = m_updated;
                subagents = m_subagents;
                usage_by_model = m_usage_by_model;
                meta = m_meta;
            }
        }
    }

    let id = id.ok_or(StorageError::NotFound {
        path: display_path.to_string(),
    })?;

    Ok(Session {
        version: SESSION_VERSION,
        id,
        title,
        cwd,
        model,
        messages: Arc::new(messages),
        token_usage,
        tool_outputs,
        subagent_messages: subagent_messages
            .into_iter()
            .map(|(id, msgs)| (id, Arc::new(msgs)))
            .collect(),
        subagents,
        usage_by_model,
        meta,
        created_at,
        updated_at,
        revision: 0,
        content_revision: 0,
        epoch: super::next_epoch(),
        rewrites: 0,
    })
}

pub(super) fn load_session_at<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    let data = fs::read(path).map_err(StorageError::from)?;
    let mut session: Session<M, U, T> = if path.extension().is_some_and(|e| e == "jsonl") {
        load_jsonl(&data, &path.display().to_string())?
    } else {
        let session: Session<M, U, T> =
            serde_json::from_slice(&data).map_err(StorageError::from)?;
        if session.version != SESSION_VERSION {
            return Err(SessionError::VersionMismatch {
                found: session.version,
                expected: SESSION_VERSION,
            });
        }
        session
    };
    session.title = normalize_title(&session.title);
    Ok(session)
}
