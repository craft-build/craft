//! The JSONL record vocabulary and the one loader for every session shape.
//!
//! The current format (log version 4) writes animist [`HistoryEntry`]
//! records — `entry`, `insert`, `revise`, `deactivate`, `replace` — mapping
//! one to one onto animist's `HistoryChange` feed, which the change-feed
//! persister writes. Craft writes only `entry` records; the loader replays
//! all five kinds so a record written by a later build still reconstructs
//! its snapshot.
//!
//! Older shapes stay loadable line by line: a record that fails the current
//! parse is decoded as a legacy record (legacy craft messages,
//! `msg`/`sub_msg`/`out`) and converted, exactly like the `.json` legacy
//! path. Load-time corruption policy: every record that fails both parses
//! is skipped with a stderr warning, wherever it sits in the file, while a
//! record whose index addresses an entry the replayed list does not have
//! fails the load ([`SessionError::RecordOutOfRange`]) — a structurally
//! consistent file cannot produce one, so it is corruption worth refusing,
//! not noise worth skipping.
//!
//! Raw tool-output copies (`out` records here, the `tool_outputs` map of a
//! legacy `.json` file) are folded into the matching tool-result entry's
//! source when that entry holds no richer original, and dropped otherwise
//! (see [`fold_tool_outputs`]).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use animist::history::{ContentBlock, HistoryEntry, StoredMessage, ToolContent};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::history::ext;

use super::Session;
use super::legacy_messages;
use super::legacy_messages::Message as LegacyMessage;
use super::{
    DEFAULT_TITLE, SESSION_VERSION, SessionError, SessionMeta, StoredSubagent, StoredTokenUsage,
    normalize_title,
};
use crate::id::SessionRef;
use crate::storage::StorageError;

/// Records of the current format (log version 4).
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
pub(super) enum Record {
    #[serde(rename = "header")]
    Header {
        v: u32,
        id: SessionRef,
        model: String,
        cwd: String,
        created_at: u64,
    },
    /// [`animist::history::HistoryChange::Appended`].
    #[serde(rename = "entry")]
    Entry { entry: HistoryEntry },
    /// [`animist::history::HistoryChange::Inserted`].
    #[serde(rename = "insert")]
    Insert { index: usize, entry: HistoryEntry },
    /// [`animist::history::HistoryChange::Revised`].
    #[serde(rename = "revise")]
    Revise {
        index: usize,
        message: StoredMessage,
    },
    /// [`animist::history::HistoryChange::Deactivated`].
    #[serde(rename = "deactivate")]
    Deactivate { index: usize },
    /// [`animist::history::HistoryChange::EntryReplaced`].
    #[serde(rename = "replace")]
    Replace { index: usize, entry: HistoryEntry },
    /// Raw tool-output copies from older saves; readable, never written.
    #[serde(rename = "out")]
    Out { id: String, d: ContentBlock },
    #[serde(rename = "sub_entry")]
    SubEntry { sub: String, entry: HistoryEntry },
    #[serde(rename = "meta")]
    Meta {
        title: String,
        #[serde(default, deserialize_with = "null_or_default")]
        token_usage: StoredTokenUsage,
        updated_at: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        subagents: Vec<StoredSubagent>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        usage_by_model: HashMap<String, StoredTokenUsage>,
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

/// Records of the legacy format (log version 3 and older), decoded
/// only when a line fails the current parse.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum LegacyRecord {
    #[serde(rename = "msg")]
    Msg { d: LegacyMessage },
    // Retired with the session `tool_outputs` map: still parsed so
    // old lines load, their payloads folding into entry sources.
    #[serde(rename = "out")]
    Out {
        id: String,
        d: legacy_messages::ToolResult,
    },
    #[serde(rename = "sub_msg")]
    SubMsg { sub: String, d: LegacyMessage },
    #[serde(rename = "meta")]
    Meta {
        title: String,
        #[serde(default, deserialize_with = "null_or_default")]
        token_usage: StoredTokenUsage,
        updated_at: u64,
        #[serde(default)]
        subagents: Vec<StoredSubagent>,
        #[serde(default)]
        usage_by_model: HashMap<String, StoredTokenUsage>,
        #[serde(default, flatten)]
        meta: SessionMeta,
    },
}

/// Older writers spelled an absent ledger as `null`; read it as default.
fn null_or_default<'de, D>(deserializer: D) -> Result<StoredTokenUsage, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<StoredTokenUsage>::deserialize(deserializer)?.unwrap_or_default())
}

/// The record spelling one history change: the inverse of the mapping the
/// loader replays, so writing a change and loading it back round-trips.
pub(crate) fn record_of(change: &animist::history::HistoryChange) -> Record {
    use animist::history::HistoryChange;
    match change {
        HistoryChange::Appended { entry, .. } => Record::Entry {
            entry: entry.clone(),
        },
        HistoryChange::Inserted { index, entry } => Record::Insert {
            index: *index,
            entry: entry.clone(),
        },
        HistoryChange::Revised { index, message } => Record::Revise {
            index: *index,
            message: message.clone(),
        },
        HistoryChange::Deactivated { index } => Record::Deactivate { index: *index },
        HistoryChange::EntryReplaced { index, entry } => Record::Replace {
            index: *index,
            entry: entry.clone(),
        },
    }
}

/// Apply one change record to the entry list. Indices address the list at
/// the time of the record, as animist defines them, so replaying records in
/// order reproduces the snapshot. An out-of-range index is a corrupt
/// record: the caller fails the load with it ([`SessionError::RecordOutOfRange`]).
pub(crate) fn apply_record(entries: &mut Vec<HistoryEntry>, record: &Record) -> Result<(), ()> {
    match record {
        Record::Entry { entry } => {
            entries.push(entry.clone());
            Ok(())
        }
        Record::Insert { index, entry } => {
            if *index > entries.len() {
                return Err(());
            }
            entries.insert(*index, entry.clone());
            Ok(())
        }
        Record::Revise { index, message } => {
            let source = entries.get_mut(*index).ok_or(())?;
            let mut revisions = source.revisions().to_vec();
            revisions.push(message.clone());
            *source = HistoryEntry::builder()
                .source(source.verbatim().clone())
                .revisions(revisions)
                .active(source.is_active())
                .build();
            Ok(())
        }
        Record::Deactivate { index } => {
            let entry = entries.get_mut(*index).ok_or(())?;
            *entry = HistoryEntry::builder()
                .source(entry.verbatim().clone())
                .revisions(entry.revisions().to_vec())
                .active(false)
                .build();
            Ok(())
        }
        Record::Replace { index, entry } => {
            entries.get_mut(*index).ok_or(())?.clone_from(entry);
            Ok(())
        }
        Record::Header { .. }
        | Record::Out { .. }
        | Record::SubEntry { .. }
        | Record::Meta { .. } => Ok(()),
    }
}

/// The entry index a change record addresses, for the out-of-range error;
/// `None` for records that carry none.
fn record_index(record: &Record) -> Option<usize> {
    match record {
        Record::Insert { index, .. }
        | Record::Revise { index, .. }
        | Record::Deactivate { index }
        | Record::Replace { index, .. } => Some(*index),
        _ => None,
    }
}

/// How much output a tool result carries: text characters and image
/// blocks. Older writers stored the raw copy next to a possibly
/// compressed in-message copy, so whichever side carries more content is
/// the original.
fn contents_richness(content: &animist::history::ToolContents) -> (usize, usize) {
    let mut text = 0;
    let mut images = 0;
    for part in &content.0 {
        match part {
            ToolContent::Text { text: chars } => text += chars.len(),
            ToolContent::Image { .. } => images += 1,
        }
    }
    (text, images)
}

/// [`contents_richness`] of a tool-result block; `(0, 0)` for any other
/// block shape.
fn result_richness(block: &ContentBlock) -> (usize, usize) {
    match block {
        ContentBlock::ToolResult { content, .. } => contents_richness(content),
        _ => (0, 0),
    }
}

/// Whether the entry's source holds a tool result for `id`.
fn holds_call(entry: &HistoryEntry, id: &str) -> bool {
    entry.verbatim().message().content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id
        )
    })
}

/// Fold one raw tool-output copy into the matching tool-result entry's
/// source, and return whether `id` matched an entry in this list. The entry
/// keeps its copy when it is at least as rich (it already holds the
/// original, or a later revision model wrote it whole); the raw copy
/// replaces the source's block content when it carries more, keeping the
/// entry's own call name where it has one. Only the matched entry is
/// cloned. A call id no list knows is dropped — its result left the
/// history before the save.
fn fold_tool_output(
    entries: &mut [HistoryEntry],
    id: &str,
    raw: &ContentBlock,
    raw_richness: (usize, usize),
) -> bool {
    let ContentBlock::ToolResult {
        content: raw_content,
        name: raw_name,
        is_error: raw_is_error,
        ..
    } = raw
    else {
        return false;
    };
    let Some(index) = entries.iter().position(|entry| holds_call(entry, id)) else {
        return false;
    };
    let entry = &mut entries[index];
    let existing = entry
        .verbatim()
        .message()
        .content
        .iter()
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == id => Some(contents_richness(content)),
            _ => None,
        })
        .unwrap_or((0, 0));
    if existing >= raw_richness {
        // The entry already holds the equally rich or richer original.
        return true;
    }
    let mut source = entry.verbatim().clone();
    for block in source.message_mut().content.iter_mut() {
        if let ContentBlock::ToolResult {
            tool_use_id,
            name,
            content,
            is_error,
            ..
        } = block
            && tool_use_id == id
        {
            *content = raw_content.clone();
            if name.is_none() {
                *name = raw_name.clone();
            }
            *is_error = *raw_is_error;
            break;
        }
    }
    *entry = HistoryEntry::builder()
        .source(source)
        .revisions(entry.revisions().to_vec())
        .active(entry.is_active())
        .build();
    true
}

/// Fold every collected raw tool-output copy into the entry lists that hold
/// their calls: the main history first, then the subagent histories (the
/// legacy map was keyed by call id across both). One raw copy folds into at
/// most one list — the first that holds the call; the same id in two lists
/// is a cross-agent collision the legacy keying could not disambiguate
/// anyway.
fn fold_tool_outputs(
    messages: &mut [HistoryEntry],
    subagents: &mut HashMap<String, Vec<HistoryEntry>>,
    raws: HashMap<String, ContentBlock>,
) {
    if raws.is_empty() {
        return;
    }
    for (id, raw) in &raws {
        let richness = result_richness(raw);
        if fold_tool_output(messages, id, raw, richness) {
            continue;
        }
        for entries in subagents.values_mut() {
            if fold_tool_output(entries, id, raw, richness) {
                break;
            }
        }
    }
}

/// Tag-only probe used to classify a line that failed both parses:
/// distinguishes a header with a bad id from a genuinely unknown record.
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum RawTag {
    Header {
        id: String,
    },
    #[serde(other)]
    Other,
}

/// One accepted line of a session log, in either format.
enum Parsed {
    Header {
        v: u32,
        id: SessionRef,
        model: String,
        cwd: String,
        created_at: u64,
    },
    Current(Record),
    Legacy(LegacyRecord),
}

fn parse_line(line: &[u8]) -> Result<Parsed, serde_json::Error> {
    if let Ok(record) = serde_json::from_slice::<Record>(line) {
        return Ok(match record {
            Record::Header {
                v,
                id,
                model,
                cwd,
                created_at,
            } => Parsed::Header {
                v,
                id,
                model,
                cwd,
                created_at,
            },
            other => Parsed::Current(other),
        });
    }
    serde_json::from_slice::<LegacyRecord>(line).map(Parsed::Legacy)
}

/// The shared body of every load: parse every line, replay entry records in
/// order, convert legacy records as they appear, and assemble the session.
/// `display_path` names the file in warnings.
pub(super) fn load_jsonl(
    data: &[u8],
    display_path: &str,
) -> Result<Session<HistoryEntry, StoredTokenUsage>, SessionError> {
    let mut line_count = 0usize;

    let mut id: Option<SessionRef> = None;
    let mut model = String::new();
    let mut cwd = String::new();
    let mut created_at = 0u64;
    let mut messages: Vec<HistoryEntry> = Vec::new();
    let mut subagent_messages: HashMap<String, Vec<HistoryEntry>> = HashMap::new();
    let mut tool_outputs: HashMap<String, ContentBlock> = HashMap::new();
    let mut title = DEFAULT_TITLE.to_string();
    let mut token_usage = StoredTokenUsage::default();
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
        let parsed: Parsed = match parse_line(line) {
            Ok(parsed) => parsed,
            Err(e) => {
                // A header whose only defect is an unparseable id fails both
                // strict parses; surface that precisely instead of silently
                // skipping to a misleading NotFound.
                if !got_header
                    && let Ok(RawTag::Header { id: raw_id }) =
                        serde_json::from_slice::<RawTag>(line)
                    && let Err(source) = raw_id.parse::<crate::id::CraftId>()
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
        match parsed {
            Parsed::Header {
                v,
                id: h_id,
                model: h_model,
                cwd: h_cwd,
                created_at: h_created,
            } => {
                if v > super::LOG_FORMAT_VERSION {
                    return Err(SessionError::VersionMismatch {
                        found: v,
                        expected: super::LOG_FORMAT_VERSION,
                    });
                }
                id = Some(h_id);
                model = h_model;
                cwd = h_cwd;
                created_at = h_created;
                got_header = true;
            }
            Parsed::Current(record) => match &record {
                Record::Entry { .. }
                | Record::Insert { .. }
                | Record::Revise { .. }
                | Record::Deactivate { .. }
                | Record::Replace { .. } => {
                    if let Err(()) = apply_record(&mut messages, &record) {
                        return Err(SessionError::RecordOutOfRange {
                            path: display_path.to_string(),
                            line: line_count,
                            index: record_index(&record).unwrap_or_default(),
                            len: messages.len(),
                        });
                    }
                }
                // `Record::Out` raw tool outputs are gone since 08:
                // originals live in entry sources. Copies already on disk
                // fold into those sources at the end of the load.
                Record::Out { id, d } => {
                    tool_outputs.insert(id.clone(), d.clone());
                }
                Record::SubEntry { sub, entry } => {
                    subagent_messages
                        .entry(sub.clone())
                        .or_default()
                        .push(entry.clone());
                }
                Record::Meta {
                    title: m_title,
                    token_usage: m_usage,
                    updated_at: m_updated,
                    subagents: m_subagents,
                    usage_by_model: m_usage_by_model,
                    meta: m_meta,
                } => {
                    title = m_title.clone();
                    token_usage = *m_usage;
                    updated_at = *m_updated;
                    subagents = m_subagents.clone();
                    usage_by_model = m_usage_by_model.clone();
                    meta = m_meta.clone();
                }
                Record::Header { .. } => unreachable!("header parsed as Parsed::Header"),
            },
            Parsed::Legacy(record) => {
                // Legacy messages carry no per-message time; the header's
                // created_at stands in for all of them (an approximation
                // that mis-dates late messages but keeps order positional).
                let timestamp =
                    Timestamp::from_second(created_at as i64).unwrap_or(Timestamp::UNIX_EPOCH);
                match record {
                    LegacyRecord::Msg { d } => {
                        messages.extend(
                            legacy_messages::to_stored(std::slice::from_ref(&d), timestamp)
                                .into_iter()
                                .map(crate::history::ext::entry),
                        );
                    }
                    LegacyRecord::Out { id, d } => {
                        tool_outputs.insert(id.clone(), legacy_messages::tool_result_block(&d));
                    }
                    LegacyRecord::SubMsg { sub, d } => {
                        subagent_messages.entry(sub).or_default().extend(
                            legacy_messages::to_stored(std::slice::from_ref(&d), timestamp)
                                .into_iter()
                                .map(crate::history::ext::entry),
                        );
                    }
                    LegacyRecord::Meta {
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
        }
    }

    let id = id.ok_or(StorageError::NotFound {
        path: display_path.to_string(),
    })?;

    fold_tool_outputs(&mut messages, &mut subagent_messages, tool_outputs);

    Ok(Session {
        version: SESSION_VERSION,
        id,
        title,
        cwd,
        model,
        messages: Arc::new(messages),
        token_usage,
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

pub(super) fn load_session_at(
    path: &Path,
) -> Result<Session<HistoryEntry, StoredTokenUsage>, SessionError> {
    let data = std::fs::read(path).map_err(StorageError::from)?;
    let mut session = if path.extension().is_some_and(|e| e == "jsonl") {
        load_jsonl(&data, &path.display().to_string())?
    } else {
        load_json(&data)?
    };
    session.title = normalize_title(&session.title);
    Ok(session)
}

/// Reject a session whose format version this build cannot read.
fn check_version(
    session: Session<HistoryEntry, StoredTokenUsage>,
) -> Result<Session<HistoryEntry, StoredTokenUsage>, SessionError> {
    if session.version != SESSION_VERSION {
        return Err(SessionError::VersionMismatch {
            found: session.version,
            expected: SESSION_VERSION,
        });
    }
    Ok(session)
}

/// The pre-JSONL single-file shape: the session wrapper around the legacy
/// message and tool-result enums, mirrored field for field so the retired
/// `tool_outputs` map deserializes too. Converted on load; the current
/// shape loads directly.
#[derive(Deserialize)]
struct LegacyJsonSession {
    version: u32,
    id: SessionRef,
    title: String,
    cwd: String,
    model: String,
    messages: Vec<LegacyMessage>,
    #[serde(default, deserialize_with = "null_or_default")]
    token_usage: StoredTokenUsage,
    /// Raw tool-output copies keyed by call id (retired in 08).
    #[serde(default)]
    tool_outputs: HashMap<String, legacy_messages::ToolResult>,
    #[serde(default)]
    subagent_messages: HashMap<String, Vec<LegacyMessage>>,
    #[serde(default)]
    subagents: Vec<StoredSubagent>,
    #[serde(default)]
    usage_by_model: HashMap<String, StoredTokenUsage>,
    #[serde(default, flatten)]
    meta: SessionMeta,
    created_at: u64,
    updated_at: u64,
}

/// The pre-JSONL single-file shape: the same session wrapper around the
/// legacy message and tool-result enums, converted on load. The current
/// shape loads directly.
fn load_json(data: &[u8]) -> Result<Session<HistoryEntry, StoredTokenUsage>, SessionError> {
    // Same policy as the jsonl reader: try the current shape first, then
    // decode the legacy one and convert.
    if let Ok(session) = serde_json::from_slice::<Session<HistoryEntry, StoredTokenUsage>>(data) {
        return check_version(session);
    }
    let legacy: LegacyJsonSession = serde_json::from_slice(data).map_err(StorageError::from)?;
    if legacy.version != SESSION_VERSION {
        return Err(SessionError::VersionMismatch {
            found: legacy.version,
            expected: SESSION_VERSION,
        });
    }
    // Same approximation as the jsonl path: the file stored no per-message
    // time, so the session's created_at stands in for every message.
    let timestamp =
        Timestamp::from_second(legacy.created_at as i64).unwrap_or(Timestamp::UNIX_EPOCH);
    let mut messages = ext::entries(legacy_messages::to_stored(&legacy.messages, timestamp));
    let mut subagent_messages: HashMap<String, Vec<HistoryEntry>> = legacy
        .subagent_messages
        .iter()
        .map(|(id, msgs)| {
            (
                id.clone(),
                legacy_messages::to_stored(msgs, timestamp)
                    .into_iter()
                    .map(ext::entry)
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let tool_outputs: HashMap<String, ContentBlock> = legacy
        .tool_outputs
        .iter()
        .map(|(id, result)| (id.clone(), legacy_messages::tool_result_block(result)))
        .collect();
    fold_tool_outputs(&mut messages, &mut subagent_messages, tool_outputs);
    Ok(Session {
        version: SESSION_VERSION,
        id: legacy.id,
        title: legacy.title,
        cwd: legacy.cwd,
        model: legacy.model,
        messages: Arc::new(messages),
        token_usage: legacy.token_usage,
        subagent_messages: subagent_messages
            .into_iter()
            .map(|(id, msgs)| (id, Arc::new(msgs)))
            .collect(),
        subagents: legacy.subagents,
        usage_by_model: legacy.usage_by_model,
        meta: legacy.meta,
        created_at: legacy.created_at,
        updated_at: legacy.updated_at,
        revision: 0,
        content_revision: 0,
        epoch: super::next_epoch(),
        rewrites: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ext;

    fn stored(text: &str) -> StoredMessage {
        ext::stored_user_text(text)
    }

    fn entry(text: &str) -> HistoryEntry {
        ext::entry(stored(text))
    }

    /// The persister's failed-checkpoint rollback exists because applying
    /// a record is NOT idempotent: the same entry record applied twice
    /// duplicates the entry. This pins that property so the rollback
    /// cannot be "simplified" away.
    #[test]
    fn applying_the_same_record_twice_duplicates_the_entry() {
        let record = Record::Entry {
            entry: entry("once"),
        };
        let mut entries = Vec::new();
        apply_record(&mut entries, &record).unwrap();
        apply_record(&mut entries, &record).unwrap();
        assert_eq!(entries.len(), 2, "record application is not idempotent");
    }

    #[test]
    fn replay_covers_every_record_kind() {
        let mut entries = vec![entry("one"), entry("two")];
        let insert = Record::Insert {
            index: 1,
            entry: entry("inserted"),
        };
        apply_record(&mut entries, &insert).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| ext::message_text(e.verbatim()))
                .collect::<Vec<_>>(),
            vec!["one", "inserted", "two"],
            "an insert shifts later entries right"
        );

        let revise = Record::Revise {
            index: 2,
            message: stored("two, revised"),
        };
        apply_record(&mut entries, &revise).unwrap();
        assert_eq!(ext::message_text(entries[2].latest()), "two, revised");
        assert_eq!(
            ext::message_text(entries[2].verbatim()),
            "two",
            "the source survives a revision"
        );

        let deactivate = Record::Deactivate { index: 0 };
        apply_record(&mut entries, &deactivate).unwrap();
        assert!(!entries[0].is_active());
        assert!(entries[1].is_active());

        let replace = Record::Replace {
            index: 0,
            entry: HistoryEntry::builder()
                .source(stored("replaced"))
                .active(false)
                .build(),
        };
        apply_record(&mut entries, &replace).unwrap();
        assert_eq!(ext::message_text(entries[0].verbatim()), "replaced");
        assert!(!entries[0].is_active(), "replace keeps nothing implicitly");

        apply_record(
            &mut entries,
            &Record::Entry {
                entry: entry("appended"),
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(ext::message_text(entries[3].verbatim()), "appended");
    }

    #[test]
    fn out_of_range_records_are_rejected() {
        let mut entries = vec![entry("one")];
        assert_eq!(
            apply_record(&mut entries, &Record::Deactivate { index: 5 }),
            Err(())
        );
        assert_eq!(
            apply_record(
                &mut entries,
                &Record::Revise {
                    index: 5,
                    message: stored("x")
                }
            ),
            Err(())
        );
        assert_eq!(
            apply_record(
                &mut entries,
                &Record::Replace {
                    index: 5,
                    entry: entry("x")
                }
            ),
            Err(())
        );
        // An index that was valid at write time can point past a list a
        // corrupt earlier record shrank: the replay refuses the record
        // (never panics), and the loader turns the refusal into
        // `RecordOutOfRange` instead of skipping it silently.
        assert_eq!(
            apply_record(
                &mut entries,
                &Record::Insert {
                    index: 5,
                    entry: entry("bad")
                }
            ),
            Err(())
        );
        assert_eq!(
            apply_record(
                &mut entries,
                &Record::Insert {
                    index: 0,
                    entry: entry("ok")
                }
            ),
            Ok(())
        );
        assert_eq!(entries.len(), 2);
    }
}
