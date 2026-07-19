//! Session persistence with append-only JSONL log format.
//!
//! Each session is stored as `{uuid}.jsonl`, one JSON record per line. The format is
//! crash-safe: on load, any trailing run of unparseable lines is discarded (a partial
//! flush may corrupt multiple trailing records). `SessionLog` tracks cursor state to
//! enable O(delta) incremental saves.
//!
//! Legacy `.json` files are loaded transparently and converted to `.jsonl` on next save.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::UNIX_EPOCH;

use tracing::warn;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::id::{CraftId, CraftIdParseError, SessionRef};
use crate::{StateDir, StorageError, atomic_write, now_epoch};

const SESSION_VERSION: u32 = 1;
const LOG_FORMAT_VERSION: u32 = 3;
pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const SESSIONS_DIR: &str = "sessions";
const CWD_INDEX_FILE: &str = "cwd_latest.json";
const CWD_INDEX_STEM: &str = "cwd_latest";
const SCAN_CACHE_FILE: &str = "scan_cache.json";
const SCAN_CACHE_STEM: &str = "scan_cache";
const NON_SESSION_STEMS: [&str; 2] = [CWD_INDEX_STEM, SCAN_CACHE_STEM];
const DEFAULT_TITLE: &str = "New session";
const MAX_TITLE_LEN: usize = 60;
const TAIL_BUF: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("incompatible session version {found} (expected {expected})")]
    VersionMismatch { found: u32, expected: u32 },
    #[error("session ID mismatch: log owns {log_id}, got {given_id}")]
    IdMismatch { log_id: CraftId, given_id: CraftId },
    #[error("cursor ahead of session (log has {saved}, session has {actual}); compact required")]
    CursorAhead { saved: usize, actual: usize },
    #[error("session log {path} has header id {raw_id:?} that is not a valid id: {source}")]
    CorruptHeaderId {
        path: String,
        raw_id: String,
        source: CraftIdParseError,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_creation: u32,
    #[serde(default)]
    pub cache_read: u32,
}

impl StoredTokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input + self.cache_read + self.cache_creation
    }

    pub fn total(&self) -> u32 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

impl std::ops::AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_creation += rhs.cache_creation;
        self.cache_read += rhs.cache_read;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub mode: Option<StoredMode>,
    #[serde(default)]
    pub plan_path: Option<String>,
    #[serde(default)]
    pub plan_written: bool,
    #[serde(default)]
    pub session_rules: Vec<StoredRule>,
    #[serde(default)]
    pub context_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_draft: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<StoredSubagent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<StoredThinking>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goal_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub usage_by_model: HashMap<String, StoredTokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_workstream_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_stage: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub flow_chunks: BTreeMap<String, StoredFlowChunk>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub context_window_overrides: HashMap<String, u32>,
}

/// Persisted per-chunk state for Flow mode; mirrors `craft_flow::Chunk`'s
/// render-relevant fields. Stage iteration counts are not tracked here (they
/// live in craft-flow's documents), keeping the session record small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredFlowChunk {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session<M, U, T> {
    pub version: u32,
    pub id: SessionRef,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub messages: Vec<M>,
    pub token_usage: U,
    #[serde(default = "HashMap::new")]
    pub tool_outputs: HashMap<String, T>,
    #[serde(default = "HashMap::new", skip_serializing_if = "HashMap::is_empty")]
    pub subagent_messages: HashMap<String, Vec<M>>,
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionRef,
    pub title: String,
    pub cwd: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredMode {
    Build,
    Plan,
    Flow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub effect: StoredEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThinkingParseError {
    #[error(
        "unknown thinking value {0:?} (use off, adaptive, minimal, low, medium, high, xhigh, max, or a token budget)"
    )]
    Unknown(String),
    #[error("thinking budget must be greater than zero")]
    BudgetZero,
}

/// Floor for every token budget sent to a provider; some APIs reject smaller values.
pub const MIN_THINKING_BUDGET: u32 = 1024;

/// Thinking effort level. Declaration order is intensity order: the `Ord`
/// derive and [`Effort::ALL`] rely on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Percentage of the model's max thinking budget this level spends.
    pub const fn percent(self) -> u32 {
        match self {
            Self::Minimal => 10,
            Self::Low => 20,
            Self::Medium => 40,
            Self::High => 60,
            Self::XHigh => 80,
            Self::Max => 100,
        }
    }

    /// `percent` of `max`, clamped to `[MIN_THINKING_BUDGET, max]`.
    /// A `max` below the floor is raised to it.
    pub fn budget(self, max: u32) -> u32 {
        let max = max.max(MIN_THINKING_BUDGET);
        let tokens = (u64::from(max) * u64::from(self.percent()) / 100) as u32;
        tokens.clamp(MIN_THINKING_BUDGET, max)
    }

    /// Inverse of [`Self::budget`]: the lowest level whose percentage covers
    /// `n` tokens out of `max`. Budgets at or above `max` map to `Max`.
    pub fn from_budget(n: u32, max: u32) -> Self {
        let pct = u64::from(n).saturating_mul(100) / u64::from(max.max(1));
        Self::ALL
            .into_iter()
            .find(|e| u64::from(e.percent()) >= pct)
            .unwrap_or(Self::Max)
    }

    /// Nearest level a provider accepts: exact match keeps `self`, otherwise
    /// the closest lower supported level, otherwise the lowest supported.
    /// An empty `supported` list returns `self` unchanged (dynamic model
    /// listings may not declare supported efforts).
    pub fn snap(self, supported: &[Self]) -> Self {
        if supported.is_empty() || supported.contains(&self) {
            return self;
        }
        supported
            .iter()
            .rev()
            .find(|&&e| e < self)
            .copied()
            .unwrap_or(supported[0])
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = ThinkingParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|e| e.as_str() == s)
            .ok_or_else(|| ThinkingParseError::Unknown(s.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum StoredThinking {
    Off,
    Adaptive,
    Effort { level: Effort },
    Budget { tokens: u32 },
}

impl StoredThinking {
    /// The one string-to-thinking parser: `/thinking`, `always_thinking`
    /// config, and the Lua agent API all delegate here.
    pub fn parse_setting(input: &str) -> Result<Self, ThinkingParseError> {
        match input.trim() {
            "off" => Ok(Self::Off),
            "adaptive" => Ok(Self::Adaptive),
            other => {
                if let Ok(level) = other.parse::<Effort>() {
                    return Ok(Self::Effort { level });
                }
                match other.parse::<u32>() {
                    Ok(0) => Err(ThinkingParseError::BudgetZero),
                    Ok(n) => Ok(Self::Budget { tokens: n }),
                    Err(_) => Err(ThinkingParseError::Unknown(other.to_string())),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSubagent {
    pub tool_use_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_mode: Option<String>,
}

#[derive(Deserialize)]
struct LegacyHeader {
    version: u32,
    id: SessionRef,
    title: String,
    cwd: String,
    updated_at: u64,
}

pub trait TitleSource {
    fn first_user_text(&self) -> Option<&str>;
}

pub fn generate_title<M: TitleSource>(messages: &[M]) -> String {
    let first_user_text = messages.iter().find_map(|m| m.first_user_text());

    let Some(text) = first_user_text.map(str::trim).filter(|t| !t.is_empty()) else {
        return DEFAULT_TITLE.into();
    };

    if text.len() <= MAX_TITLE_LEN {
        return text.to_string();
    }

    let boundary = text.floor_char_boundary(MAX_TITLE_LEN);
    let truncated = &text[..boundary];
    match truncated.rfind(' ') {
        Some(pos) if pos > MAX_TITLE_LEN / 2 => format!("{}…", &truncated[..pos]),
        _ => format!("{truncated}…"),
    }
}

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
        #[serde(flatten)]
        meta: SessionMeta,
    },
}

// -- SessionLog: append-only persistence --

pub struct SessionLog {
    session_id: CraftId,
    file: File,
    saved_msg_count: usize,
    saved_tool_ids: HashSet<String>,
    saved_sub_msg_counts: HashMap<String, usize>,
    saved_meta: Vec<u8>,
}

fn sub_msg_snapshot<M>(map: &HashMap<String, Vec<M>>) -> HashMap<String, usize> {
    map.iter().map(|(k, v)| (k.clone(), v.len())).collect()
}

impl SessionLog {
    pub fn create<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<Self, SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        let log = Self::write_canonical(dir, session)?;
        update_cwd_index(dir, &session.cwd, session.id.id())?;
        Ok(log)
    }

    /// Writes the canonical `{id}.jsonl` without touching the cwd→latest index.
    /// Used by read-path migration, where merely opening an old session must not
    /// repoint "latest" at it.
    fn write_canonical<M, U, T>(
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
        let mut file = File::create(&path).map_err(StorageError::from)?;
        write_full_session(&mut file, session)?;
        file.sync_data().map_err(StorageError::from)?;

        Ok(Self::cursor_from(session, file))
    }

    pub fn open<M, U, T>(
        dir: &Path,
        session_id: CraftId,
    ) -> Result<(Session<M, U, T>, Self), SessionError>
    where
        M: Serialize + DeserializeOwned,
        U: Serialize + DeserializeOwned + Default,
        T: Serialize + DeserializeOwned,
    {
        let path = jsonl_path(dir, session_id);
        let display_path = path.display().to_string();
        let mut file = File::open(&path).map_err(StorageError::from)?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(StorageError::from)?;
        drop(file);

        // A torn tail (partial flush) can corrupt multiple trailing records;
        // truncate at the last record boundary so the file matches the
        // cursors we are about to compute.
        let valid = bytes
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        if valid != bytes.len() {
            warn!(
                path = %display_path,
                dropped = bytes.len() - valid,
                "truncating torn jsonl tail on open",
            );
            let f = OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(StorageError::from)?;
            f.set_len(valid as u64).map_err(StorageError::from)?;
            bytes.truncate(valid);
        }

        let session = load_jsonl::<M, U, T>(&bytes, &display_path)?;

        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;

        let log = Self::cursor_from(&session, file);
        Ok((session, log))
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

        if self.cursor_ahead(session) {
            return Err(SessionError::CursorAhead {
                saved: self.saved_msg_count,
                actual: session.messages.len(),
            });
        }

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

        let start = self.file.metadata().map_err(StorageError::from)?.len();
        if let Err(e) = self
            .file
            .write_all(&buf)
            .and_then(|()| self.file.sync_data())
        {
            // A failed write can leave partial bytes; roll back to the last
            // record boundary so the file matches the unadvanced cursors and
            // a retry appends cleanly instead of duplicating records.
            let _ = self.file.set_len(start);
            return Err(StorageError::from(e).into());
        }

        self.saved_msg_count = new_msg_count;
        self.saved_tool_ids.extend(new_tool_ids);
        for (sub_id, count) in new_sub_counts {
            self.saved_sub_msg_counts.insert(sub_id, count);
        }
        self.saved_meta = meta;

        Ok(())
    }

    pub fn compact<M, U, T>(
        &mut self,
        dir: &Path,
        session: &Session<M, U, T>,
    ) -> Result<(), SessionError>
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        self.require_same_id(session)?;

        let path = jsonl_path(dir, session.id.id());
        let tmp = path.with_extension("jsonl.tmp");

        let mut tmp_file = File::create(&tmp).map_err(StorageError::from)?;
        write_full_session(&mut tmp_file, session)?;
        tmp_file.sync_data().map_err(StorageError::from)?;

        fs::rename(&tmp, &path).map_err(StorageError::from)?;

        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(StorageError::from)?;
        *self = Self::cursor_from(session, file);

        Ok(())
    }

    fn cursor_from<M, U, T>(session: &Session<M, U, T>, file: File) -> Self
    where
        M: Serialize,
        U: Serialize,
        T: Serialize,
    {
        Self {
            session_id: session.id.id(),
            file,
            saved_msg_count: session.messages.len(),
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

fn write_session_file<M, U, T>(dir: &Path, session: &Session<M, U, T>) -> Result<File, SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    fs::create_dir_all(dir).map_err(StorageError::from)?;
    let path = jsonl_path(dir, session.id.id());
    let mut file = File::create(&path).map_err(StorageError::from)?;
    write_full_session(&mut file, session)?;
    file.sync_data().map_err(StorageError::from)?;
    Ok(file)
}

fn meta_record<M, U, T>(session: &Session<M, U, T>) -> Result<Vec<u8>, SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    append_record(
        &mut buf,
        &LogRecord::<&M, &U, &T>::Meta {
            title: session.title.clone(),
            token_usage: &session.token_usage,
            updated_at: session.updated_at,
            meta: session.meta.clone(),
        },
    )?;
    Ok(buf)
}

fn write_full_session<M, U, T>(
    file: &mut File,
    session: &Session<M, U, T>,
) -> Result<(), SessionError>
where
    M: Serialize,
    U: Serialize,
    T: Serialize,
{
    let mut buf = Vec::new();
    write_record(
        file,
        &mut buf,
        &LogRecord::<&M, &U, &T>::Header {
            v: LOG_FORMAT_VERSION,
            id: session.id.clone(),
            model: session.model.clone(),
            cwd: session.cwd.clone(),
            created_at: session.created_at,
        },
    )?;
    for msg in &session.messages {
        write_record(file, &mut buf, &LogRecord::<&M, &U, &T>::Msg { d: msg })?;
    }
    for (id, output) in &session.tool_outputs {
        write_record(
            file,
            &mut buf,
            &LogRecord::<&M, &U, &T>::Out {
                id: id.clone(),
                d: output,
            },
        )?;
    }
    for (sub_id, msgs) in &session.subagent_messages {
        for msg in msgs {
            write_record(
                file,
                &mut buf,
                &LogRecord::<&M, &U, &T>::SubMsg {
                    sub: sub_id.clone(),
                    d: msg,
                },
            )?;
        }
    }
    write_record(
        file,
        &mut buf,
        &LogRecord::<&M, &U, &T>::Meta {
            title: session.title.clone(),
            token_usage: &session.token_usage,
            updated_at: session.updated_at,
            meta: session.meta.clone(),
        },
    )
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
                // silently skipping to a misleading NotFound. The header is
                // always the first line, so only probe until we have one.
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
                warn!(
                    path = display_path,
                    error = %e,
                    line = line_count,
                    "skipping unrecognized JSONL record",
                );
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
                tool_outputs.insert(out_id, d);
            }
            LogRecord::SubMsg { sub, d } => {
                subagent_messages.entry(sub).or_default().push(d);
            }
            LogRecord::Meta {
                title: m_title,
                token_usage: m_usage,
                updated_at: m_updated,
                meta: m_meta,
            } => {
                title = m_title;
                token_usage = m_usage;
                updated_at = m_updated;
                meta = m_meta;
            }
        }
    }

    let id = id.ok_or(StorageError::NotFound(display_path.to_string()))?;

    Ok(Session {
        version: SESSION_VERSION,
        id,
        title,
        cwd,
        model,
        messages,
        token_usage,
        tool_outputs,
        subagent_messages,
        meta,
        created_at,
        updated_at,
    })
}

// -- CWD index --

fn load_cwd_index(dir: &Path) -> HashMap<String, String> {
    fs::read(dir.join(CWD_INDEX_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn update_cwd_index(dir: &Path, cwd: &str, session_id: CraftId) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    index.insert(cwd.to_string(), session_id.to_string());
    atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)
}

fn jsonl_path(dir: &Path, id: CraftId) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn json_path(dir: &Path, id: CraftId) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

fn remove_legacy_files(dir: &Path, id: CraftId) -> Result<bool, SessionError> {
    let mut removed = try_remove(&json_path(dir, id))?;
    for legacy in find_legacy_files(dir, id) {
        removed |= try_remove(&legacy)?;
    }
    Ok(removed)
}

fn try_remove(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn remove_from_cwd_index(dir: &Path, session_id: CraftId) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    let before = index.len();
    index.retain(|_, v| v.parse::<CraftId>() != Ok(session_id));
    if index.len() != before {
        atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)?;
    }
    Ok(())
}

// -- Header scanning for session list --

#[derive(Deserialize)]
struct JsonlHeader {
    v: u32,
    id: SessionRef,
    cwd: String,
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ScanRecord {
    Meta {
        title: String,
        updated_at: u64,
    },
    #[serde(other)]
    Other,
}

#[derive(Serialize, Deserialize)]
struct ScanCacheEntry {
    size: u64,
    mtime_ms: u64,
    header: Option<ScannedHeader>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ScannedHeader {
    id: SessionRef,
    cwd: String,
    title: String,
    updated_at: u64,
}

type ScanCache = HashMap<String, ScanCacheEntry>;

fn load_scan_cache(dir: &Path) -> ScanCache {
    fs::read(dir.join(SCAN_CACHE_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn file_signature(path: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)?;
    Some((meta.len(), mtime_ms))
}

fn scan_headers(cwd: Option<&str>, dir: &Path) -> Result<Vec<SessionSummary>, StorageError> {
    let mut cache = load_scan_cache(dir);
    let mut fresh = ScanCache::new();
    let mut dirty = false;
    let mut out = Vec::new();

    for path in session_entries(dir)? {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((size, mtime_ms)) = file_signature(&path) else {
            continue;
        };
        let entry = match cache.remove(name) {
            Some(e) if e.size == size && e.mtime_ms == mtime_ms => e,
            _ => {
                dirty = true;
                let header = if is_jsonl(&path) {
                    scan_jsonl_header(&path)
                } else {
                    scan_legacy_header(&path)
                };
                ScanCacheEntry {
                    size,
                    mtime_ms,
                    header,
                }
            }
        };
        if let Some(h) = &entry.header
            && cwd.is_none_or(|filter| h.cwd == filter)
        {
            out.push(SessionSummary {
                id: h.id.clone(),
                title: h.title.clone(),
                cwd: h.cwd.clone(),
                updated_at: h.updated_at,
            });
        }
        fresh.insert(name.to_owned(), entry);
    }

    if (dirty || !cache.is_empty())
        && let Ok(data) = serde_json::to_vec(&fresh)
        && let Err(e) = atomic_write(&dir.join(SCAN_CACHE_FILE), &data)
    {
        warn!(error = %e, "failed to write session scan cache");
    }

    Ok(out)
}

fn scan_jsonl_header(path: &Path) -> Option<ScannedHeader> {
    let mut file = File::open(path).ok()?;
    let mut reader = BufReader::new(&mut file);

    let mut first_line = String::new();
    reader.read_line(&mut first_line).ok()?;
    let header: JsonlHeader = serde_json::from_str(first_line.trim_end()).ok()?;
    if header.v > LOG_FORMAT_VERSION {
        return None;
    }

    let (title, updated_at) =
        read_last_meta(&mut file).unwrap_or_else(|| (DEFAULT_TITLE.to_string(), 0));

    Some(ScannedHeader {
        id: header.id,
        cwd: header.cwd,
        title,
        updated_at,
    })
}

/// Walks the file tail backwards to find the last `Meta` record without scanning every line.
fn read_last_meta(file: &mut File) -> Option<(String, u64)> {
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }

    let start = len.saturating_sub(TAIL_BUF);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(len - start).read_to_end(&mut buf).ok()?;

    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        if let Ok(ScanRecord::Meta { title, updated_at }) = serde_json::from_str(line) {
            return Some((title, updated_at));
        }
    }
    None
}

fn scan_legacy_header(path: &Path) -> Option<ScannedHeader> {
    let data = fs::read(path).ok()?;
    let h: LegacyHeader = serde_json::from_slice(&data).ok()?;
    if h.version != SESSION_VERSION {
        return None;
    }
    Some(ScannedHeader {
        id: h.id,
        cwd: h.cwd,
        title: h.title,
        updated_at: h.updated_at,
    })
}

fn session_entries(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    Ok(fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|p| is_session_file(p))
        .collect())
}

fn is_session_file(p: &Path) -> bool {
    p.file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| !NON_SESSION_STEMS.contains(&s))
        && p.extension().is_some_and(|e| e == "json" || e == "jsonl")
}

fn find_legacy_files(dir: &Path, id: CraftId) -> Vec<PathBuf> {
    let canonical = id.to_string();
    session_entries(dir)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s != canonical && s.parse::<CraftId>() == Ok(id))
        })
        .collect()
}

fn locate_session_file(dir: &Path, id: CraftId) -> Option<PathBuf> {
    for ext in ["jsonl", "json"] {
        let path = dir.join(format!("{id}.{ext}"));
        if path.exists() {
            return Some(path);
        }
    }
    let legacy = find_legacy_files(dir, id);
    legacy
        .iter()
        .find(|p| is_jsonl(p))
        .or_else(|| legacy.first())
        .cloned()
}

fn load_session_at<M, U, T>(path: &Path) -> Result<Session<M, U, T>, SessionError>
where
    M: DeserializeOwned,
    U: DeserializeOwned + Default,
    T: DeserializeOwned,
{
    if path.extension().is_some_and(|e| e == "jsonl") {
        let data = fs::read(path).map_err(StorageError::from)?;
        return load_jsonl(&data, &path.display().to_string());
    }
    let data = fs::read(path).map_err(StorageError::from)?;
    let session: Session<M, U, T> = serde_json::from_slice(&data).map_err(StorageError::from)?;
    if session.version != SESSION_VERSION {
        return Err(SessionError::VersionMismatch {
            found: session.version,
            expected: SESSION_VERSION,
        });
    }
    Ok(session)
}

// -- Session impl --

impl<M, U, T> Session<M, U, T>
where
    M: Serialize + DeserializeOwned + TitleSource,
    U: Serialize + DeserializeOwned + Default,
    T: Serialize + DeserializeOwned,
{
    pub fn new(model: &str, cwd: &str) -> Self {
        let now = now_epoch();
        Self {
            version: SESSION_VERSION,
            id: SessionRef::generate(),
            title: DEFAULT_TITLE.into(),
            cwd: cwd.into(),
            model: model.into(),
            messages: Vec::new(),
            token_usage: U::default(),
            tool_outputs: HashMap::new(),
            subagent_messages: HashMap::new(),
            meta: SessionMeta {
                mode: Some(StoredMode::Build),
                ..Default::default()
            },
            created_at: now,
            updated_at: now,
        }
    }

    /// After `messages` is truncated (rewind), state keyed by tool_use_id can
    /// point at calls that no longer exist. On restore that shows up as ghost
    /// subagent tabs and leaked tool outputs, so this drops everything not
    /// reachable from `messages`.
    ///
    /// If you add another field keyed by tool_use_id, prune it here too.
    pub fn prune_orphans(&mut self, tool_ids: impl Fn(&M) -> Vec<String>) {
        let main_ids: HashSet<String> = self.messages.iter().flat_map(&tool_ids).collect();
        self.subagent_messages.retain(|id, _| main_ids.contains(id));
        self.meta
            .subagents
            .retain(|sa| main_ids.contains(&sa.tool_use_id));

        let live: HashSet<String> = self
            .subagent_messages
            .values()
            .flatten()
            .flat_map(&tool_ids)
            .chain(main_ids)
            .collect();
        self.tool_outputs.retain(|id, _| live.contains(id));
    }

    pub fn save(&mut self, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        self.save_to(&sessions_dir)
    }

    pub fn save_to(&mut self, dir: &Path) -> Result<(), SessionError> {
        self.updated_at = now_epoch();
        write_session_file(dir, self)?;
        update_cwd_index(dir, &self.cwd, self.id.id())?;
        Ok(())
    }

    pub fn load(id: CraftId, dir: &StateDir) -> Result<Self, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::load_from(id, &sessions_dir)
    }

    pub fn load_from(id: CraftId, dir: &Path) -> Result<Self, SessionError> {
        let Some(path) = locate_session_file(dir, id) else {
            return Err(StorageError::NotFound(id.to_string()).into());
        };
        let session = load_session_at::<M, U, T>(&path)?;
        let canonical = jsonl_path(dir, id);
        if path != canonical {
            if let Err(e) = SessionLog::write_canonical(dir, &session) {
                warn!(error = %e, "failed migrate to canonical jsonl; keeping legacy file");
            } else if let Err(e) = remove_legacy_files(dir, id) {
                warn!(error = %e, "legacy files remain after migration");
            }
        }
        Ok(session)
    }

    pub fn list(cwd: Option<&str>, dir: &StateDir) -> Result<Vec<SessionSummary>, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::list_in(cwd, &sessions_dir)
    }

    pub fn list_in(cwd: Option<&str>, dir: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        let mut summaries = scan_headers(cwd, dir)?;
        summaries.sort_unstable_by_key(|s| Reverse(s.updated_at));
        Ok(summaries)
    }

    pub fn latest(cwd: &str, dir: &StateDir) -> Result<Option<Self>, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::latest_in(cwd, &sessions_dir)
    }

    pub fn latest_in(cwd: &str, dir: &Path) -> Result<Option<Self>, SessionError> {
        if let Some(id) = load_cwd_index(dir).get(cwd).map(|s| s.parse::<CraftId>()) {
            match id {
                Ok(id) => match Self::load_from(id, dir) {
                    Ok(s) => return Ok(Some(s)),
                    Err(e) => warn!(error = %e, cwd, "indexed session missing on disk; rescanning"),
                },
                Err(e) => warn!(error = %e, cwd, "indexed session id unparseable; rescanning"),
            }
        }

        // The indexed entry is stale or corrupt; fall back to scanning disk.
        scan_headers(Some(cwd), dir)?
            .into_iter()
            .max_by_key(|s| s.updated_at)
            .map(|s| Self::load_from(s.id.id(), dir).map(Some))
            .unwrap_or(Ok(None))
    }

    pub fn update_title_if_default(&mut self) {
        if self.title == DEFAULT_TITLE {
            self.title = generate_title(&self.messages);
        }
    }

    pub fn delete(id: CraftId, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::delete_from(id, &sessions_dir)
    }

    pub fn delete_from(id: CraftId, dir: &Path) -> Result<(), SessionError> {
        let mut removed = try_remove(&jsonl_path(dir, id))?;
        removed |= remove_legacy_files(dir, id)?;
        if !removed {
            return Err(StorageError::NotFound(id.to_string()).into());
        }
        remove_from_cwd_index(dir, id)?;
        Ok(())
    }

    pub fn migrate_to_jsonl(dir: &Path, session: &Self) -> Result<SessionLog, SessionError> {
        let log = SessionLog::create(dir, session)?;
        remove_legacy_files(dir, session.id.id())?;
        Ok(log)
    }
}

#[cfg(test)]
mod tests {
    use super::Effort;
    use super::StoredThinking;
    use super::ThinkingParseError;
    use super::{
        CWD_INDEX_FILE, DEFAULT_TITLE, MAX_TITLE_LEN, SESSION_VERSION, StoredSubagent,
        generate_title, json_path, jsonl_path, load_cwd_index, update_cwd_index,
        write_full_session,
    };
    use super::{Session, SessionError, SessionLog, StorageError, TitleSource};
    use crate::id::CraftId;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;
    use test_case::test_case;

    type TestSession = Session<Value, Value, Value>;

    const LEGACY_HEX_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    impl TitleSource for Value {
        fn first_user_text(&self) -> Option<&str> {
            if self.get("role")?.as_str()? != "user" {
                return None;
            }
            self.get("content")?.as_array()?.iter().find_map(|b| {
                if b.get("type")?.as_str()? == "text" {
                    let text = b.get("text")?.as_str()?;
                    (!text.is_empty()).then_some(text)
                } else {
                    None
                }
            })
        }
    }

    fn user_message(text: &str) -> Value {
        text_message("user", text)
    }

    fn assistant_message(text: &str) -> Value {
        text_message("assistant", text)
    }

    fn text_message(role: &str, text: &str) -> Value {
        serde_json::json!({
            "role": role,
            "content": [{"type": "text", "text": text}]
        })
    }

    fn write_legacy_jsonl(path: &Path, session: &TestSession) {
        let mut file = std::fs::File::create(path).unwrap();
        write_full_session(&mut file, session).unwrap();
    }

    fn append_raw_msg(path: &Path, message: Value) {
        let record = serde_json::to_string(&serde_json::json!({"t":"msg","d": message})).unwrap();
        let mut file = OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(record.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
    }

    #[test]
    fn prune_orphans_drops_unreachable_tool_state() {
        fn ids(m: &Value) -> Vec<String> {
            vec![m.as_str().unwrap().to_owned()]
        }
        fn subagent(id: &str) -> StoredSubagent {
            StoredSubagent {
                tool_use_id: id.into(),
                name: "sub".into(),
                prompt: None,
                model: None,
                lifecycle: None,
                context_mode: None,
            }
        }

        let mut session: TestSession = Session::new("model", "/p");
        session.messages.push("task-live".into());
        session
            .subagent_messages
            .insert("task-live".into(), vec!["sub-tool".into()]);
        session
            .subagent_messages
            .insert("task-stale".into(), vec!["stale-sub-tool".into()]);
        session.meta.subagents = vec![subagent("task-live"), subagent("task-stale")];
        for id in ["task-live", "sub-tool", "stale-sub-tool", "orphan"] {
            session.tool_outputs.insert(id.into(), Value::Null);
        }

        session.prune_orphans(ids);

        assert_eq!(
            session.subagent_messages.keys().collect::<Vec<_>>(),
            ["task-live"]
        );
        let subagent_ids: Vec<_> = session
            .meta
            .subagents
            .iter()
            .map(|sa| sa.tool_use_id.as_str())
            .collect();
        assert_eq!(subagent_ids, ["task-live"]);
        let mut outputs: Vec<_> = session.tool_outputs.keys().cloned().collect();
        outputs.sort();
        assert_eq!(outputs, ["sub-tool", "task-live"]);
    }

    #[test]
    fn roundtrip_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession =
            Session::new("anthropic/claude-sonnet-4", "/home/test/project");
        session.messages.push(user_message("hello"));
        session.subagent_messages.insert(
            "tool-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.model, "anthropic/claude-sonnet-4");
        assert_eq!(loaded.cwd, "/home/test/project");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.version, SESSION_VERSION);
        assert_eq!(loaded.subagent_messages["tool-1"].len(), 2);
    }

    #[test]
    fn roundtrip_jsonl_incremental() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));

        let mut log = SessionLog::create(dir, &session).unwrap();

        session.messages.push(assistant_message("reply"));
        session.messages.push(user_message("second"));
        session
            .tool_outputs
            .insert("tool-1".into(), serde_json::json!({"result": "ok"}));
        session
            .subagent_messages
            .insert("sub-1".into(), vec![user_message("sub-prompt")]);
        log.append(&session).unwrap();

        session
            .subagent_messages
            .get_mut("sub-1")
            .unwrap()
            .push(assistant_message("sub-reply"));
        session
            .subagent_messages
            .insert("sub-2".into(), vec![user_message("sub-2-prompt")]);
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 3);
        assert_eq!(loaded.tool_outputs.len(), 1);
        assert!(loaded.tool_outputs.contains_key("tool-1"));
        assert_eq!(loaded.subagent_messages["sub-1"].len(), 2);
        assert_eq!(loaded.subagent_messages["sub-2"].len(), 1);
    }

    #[test]
    fn append_wrong_session_returns_id_mismatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let session_a: TestSession = Session::new("m", "/project");
        let session_b: TestSession = Session::new("m", "/project");
        let mut log = SessionLog::create(dir, &session_a).unwrap();

        let err = log.append(&session_b).unwrap_err();
        assert!(matches!(err, SessionError::IdMismatch { .. }));
    }

    #[test]
    fn crash_recovery_truncated_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("survives"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id.id());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"msg\",\"d\":{\"trun").unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn crash_recovery_skips_corrupt_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id.id());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"CORRUPT_LINE\n").unwrap();
        drop(file);
        append_raw_msg(&path, user_message("after"));

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn rewind_compact() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        for i in 0..10 {
            session.messages.push(user_message(&format!("msg-{i}")));
        }
        session.subagent_messages.insert(
            "sub-1".into(),
            vec![user_message("sub-prompt"), assistant_message("sub-reply")],
        );
        let mut log = SessionLog::create(dir, &session).unwrap();

        session.messages.truncate(5);
        session.tool_outputs.clear();
        session.subagent_messages.remove("sub-1");
        log.compact(dir, &session).unwrap();

        session.messages.push(user_message("after-compact-1"));
        session.messages.push(user_message("after-compact-2"));
        session.messages.push(user_message("after-compact-3"));
        log.append(&session).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 8);
        assert!(loaded.subagent_messages.is_empty());
    }

    #[test]
    fn migration_json_to_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("legacy"));

        let json_path = json_path(dir, session.id.id());
        fs::write(&json_path, serde_json::to_vec(&session).unwrap()).unwrap();
        update_cwd_index(dir, &session.cwd, session.id.id()).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);

        let _log = TestSession::migrate_to_jsonl(dir, &loaded).unwrap();

        assert!(!json_path.exists());
        assert!(jsonl_path(dir, session.id.id()).exists());

        let reloaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(reloaded.messages.len(), 1);
        assert_eq!(reloaded.model, "m");
    }

    #[test]
    fn load_nonexistent_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let id = CraftId::generate();
        let err = TestSession::load_from(id, tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test_case("550e8400-e29b-41d4-a716-446655440000")]
    #[test_case("550e8400e29b41d4a716446655440000")]
    fn load_legacy_hex_filename_migrates_to_canonical(legacy: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: CraftId = legacy.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id.into();
        session.messages.push(user_message("legacy"));
        let legacy_path = dir.join(format!("{legacy}.jsonl"));
        write_legacy_jsonl(&legacy_path, &session);

        let loaded = TestSession::load_from(id, dir).unwrap();
        assert_eq!(loaded.id.id(), id);
        assert_eq!(loaded.messages.len(), 1);

        assert!(!legacy_path.exists());
        let canonical = jsonl_path(dir, id);
        assert!(canonical.exists());
    }

    #[test]
    fn list_filters_by_cwd() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project-a");
        let mut s2: TestSession = Session::new("m", "/project-b");
        let mut s3: TestSession = Session::new("m", "/project-a");
        s1.save_to(dir).unwrap();
        s2.save_to(dir).unwrap();
        s3.save_to(dir).unwrap();

        let list = TestSession::list_in(Some("/project-a"), dir).unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|s| s.id != s2.id));
    }

    fn save_with_time(session: &mut TestSession, dir: &Path, time: u64) {
        session.updated_at = time;
        SessionLog::create(dir, session).unwrap();
        update_cwd_index(dir, &session.cwd, session.id.id()).unwrap();
    }

    #[test]
    fn latest_returns_most_recent_for_cwd() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project");
        s1.title = "first".into();
        save_with_time(&mut s1, dir, 1000);

        let mut s2: TestSession = Session::new("m", "/other");
        save_with_time(&mut s2, dir, 2000);

        let mut s3: TestSession = Session::new("m", "/project");
        s3.title = "latest".into();
        save_with_time(&mut s3, dir, 3000);

        let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
        assert_eq!(latest.title, "latest");
    }

    #[test]
    fn latest_falls_back_when_index_stale() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.save_to(dir).unwrap();

        let index_path = dir.join(CWD_INDEX_FILE);
        let stale: HashMap<String, String> = [("/project".into(), "deleted-id".into())].into();
        fs::write(&index_path, serde_json::to_vec(&stale).unwrap()).unwrap();

        let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
        assert_eq!(latest.id, session.id);
    }

    #[test_case("short title", "short title" ; "short_passthrough")]
    #[test_case("", DEFAULT_TITLE ; "empty_defaults")]
    #[test_case(
        "This is a very long title that exceeds the sixty character limit and should be truncated at a word boundary",
        "This is a very long title that exceeds the sixty character…"
        ; "long_truncates_at_word"
    )]
    fn title_extraction(input: &str, expected: &str) {
        let messages: Vec<Value> = if input.is_empty() {
            vec![]
        } else {
            vec![user_message(input)]
        };
        assert_eq!(generate_title(&messages), expected);
    }

    #[test]
    fn delete_removes_file_and_cwd_index() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut s1: TestSession = Session::new("m", "/project");
        s1.save_to(dir).unwrap();
        let mut s2: TestSession = Session::new("m", "/other");
        s2.save_to(dir).unwrap();

        TestSession::delete_from(s1.id.id(), dir).unwrap();
        assert!(!jsonl_path(dir, s1.id.id()).exists());
        let index = load_cwd_index(dir);
        assert!(!index.values().any(|v| *v == s1.id.to_string()));
        assert_eq!(index.get("/other"), Some(&s2.id.to_string()));
    }

    #[test]
    fn delete_nonexistent_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let id = CraftId::generate();
        let err = TestSession::delete_from(id, tmp.path()).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test_case("550e8400-e29b-41d4-a716-446655440000")]
    #[test_case("550e8400e29b41d4a716446655440000")]
    fn delete_legacy_hex_filename_removes_file(legacy: &str) {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: CraftId = legacy.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id.into();
        session.messages.push(user_message("legacy"));
        let legacy_path = dir.join(format!("{legacy}.jsonl"));
        write_legacy_jsonl(&legacy_path, &session);

        TestSession::delete_from(id, dir).unwrap();
        assert!(!legacy_path.exists());
        let canonical = jsonl_path(dir, id);
        assert!(!canonical.exists());
    }

    #[test]
    fn delete_removes_coexisting_json_and_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("hi"));

        let jsonl_file = jsonl_path(dir, session.id.id());
        write_legacy_jsonl(&jsonl_file, &session);
        let json_file = json_path(dir, session.id.id());
        fs::write(&json_file, serde_json::to_vec(&session).unwrap()).unwrap();

        TestSession::delete_from(session.id.id(), dir).unwrap();
        assert!(!jsonl_file.exists());
        assert!(!json_file.exists());
    }

    #[test]
    fn load_picks_jsonl_when_legacy_dual_file_exists() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
        let mut jsonl_session: TestSession = Session::new("m", "/project");
        jsonl_session.id = id.into();
        jsonl_session.messages.push(user_message("newer"));

        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &jsonl_session);

        let mut json_session: TestSession = Session::new("m", "/project");
        json_session.id = id.into();
        json_session.messages.push(user_message("older"));
        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

        let loaded = TestSession::load_from(id, dir).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0], user_message("newer"));
    }

    #[test]
    fn load_dual_legacy_files_does_not_leave_duplicate_in_list() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
        let mut jsonl_session: TestSession = Session::new("m", "/project");
        jsonl_session.id = id.into();
        jsonl_session.messages.push(user_message("newer"));
        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &jsonl_session);

        let mut json_session: TestSession = Session::new("m", "/project");
        json_session.id = id.into();
        json_session.messages.push(user_message("older"));
        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&json_session).unwrap()).unwrap();

        TestSession::load_from(id, dir).unwrap();

        assert!(!legacy_json.exists(), "legacy .json sibling left behind");
        let list = TestSession::list_in(Some("/project"), dir).unwrap();
        assert_eq!(
            list.len(),
            1,
            "session shows up more than once in the picker"
        );
    }

    #[test]
    fn delete_drains_coexisting_legacy_json_and_jsonl() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id.into();
        session.messages.push(user_message("legacy"));

        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &session);

        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

        TestSession::delete_from(id, dir).unwrap();
        assert!(!legacy_jsonl.exists());
        assert!(!legacy_json.exists());
    }

    #[test]
    fn migrate_to_jsonl_removes_legacy_named_files() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let id: CraftId = LEGACY_HEX_ID.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id.into();
        session.messages.push(user_message("legacy"));

        let legacy_jsonl = dir.join(format!("{LEGACY_HEX_ID}.jsonl"));
        write_legacy_jsonl(&legacy_jsonl, &session);

        let legacy_json = dir.join(format!("{LEGACY_HEX_ID}.json"));
        fs::write(&legacy_json, serde_json::to_vec(&session).unwrap()).unwrap();

        let _log = TestSession::migrate_to_jsonl(dir, &session).unwrap();

        assert!(!legacy_jsonl.exists());
        assert!(!legacy_json.exists());
        assert!(jsonl_path(dir, id).exists());
    }

    #[test]
    fn load_migration_does_not_steal_latest_pointer() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut newest: TestSession = Session::new("m", "/project");
        newest.title = "newest".into();
        save_with_time(&mut newest, dir, 3000);

        let mut older: TestSession = Session::new("m", "/project");
        older.title = "older".into();
        older.updated_at = 1000;
        let json_path = json_path(dir, older.id.id());
        fs::write(&json_path, serde_json::to_vec(&older).unwrap()).unwrap();

        // Opening the older session migrates it to canonical jsonl, but must not
        // repoint cwd->latest at it.
        let loaded = TestSession::load_from(older.id.id(), dir).unwrap();
        assert_eq!(loaded.title, "older");
        assert!(!json_path.exists());

        let latest = TestSession::latest_in("/project", dir).unwrap().unwrap();
        assert_eq!(latest.title, "newest");
    }

    #[test]
    fn load_surfaces_corrupt_header_id() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id = CraftId::generate();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id.into();

        let path = jsonl_path(dir, id);
        write_legacy_jsonl(&path, &session);

        let corrupted =
            fs::read_to_string(&path)
                .unwrap()
                .replacen(&id.to_string(), "not-a-valid-id", 1);
        fs::write(&path, corrupted).unwrap();

        let err = TestSession::load_from(id, dir).unwrap_err();
        assert!(matches!(err, SessionError::CorruptHeaderId { .. }));
    }

    #[test]
    fn remove_from_cwd_index_matches_legacy_hex_value() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let legacy = "550e8400-e29b-41d4-a716-446655440000";
        let id: CraftId = legacy.parse().unwrap();
        let mut session: TestSession = Session::new("m", "/project");
        session.id = id.into();

        let mut index: HashMap<String, String> = HashMap::new();
        index.insert("/project".into(), legacy.to_string());
        fs::write(
            dir.join(CWD_INDEX_FILE),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();

        super::remove_from_cwd_index(dir, session.id.id()).unwrap();
        let after = load_cwd_index(dir);
        assert!(!after.contains_key("/project"));
    }

    #[test]
    fn title_unicode_safe() {
        let input = "あ".repeat(100);
        let title = generate_title(&[user_message(&input)]);
        assert!(title.len() <= MAX_TITLE_LEN * 4);
        assert!(title.is_char_boundary(title.len()));
    }

    #[test]
    fn scan_headers_reads_both_formats() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut s1: TestSession = Session::new("m", "/project");
        s1.title = "jsonl-session".into();
        s1.save_to(dir).unwrap();

        let mut s2: TestSession = Session::new("m", "/project");
        s2.title = "json-session".into();
        let json_path = json_path(dir, s2.id.id());
        fs::write(&json_path, serde_json::to_vec(&s2).unwrap()).unwrap();

        let list = TestSession::list_in(Some("/project"), dir).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn scan_jsonl_reads_trailing_meta_from_tail() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut session: TestSession = Session::new("m", "/project");
        session.save_to(dir).unwrap();
        let path = jsonl_path(dir, session.id.id());

        let header_line = fs::read_to_string(&path).unwrap();
        let mut buf = header_line;
        for i in 0..5000u64 {
            buf.push_str(&format!("{{\"t\":\"out\",\"id\":\"tool-{i}\"}}\n"));
        }
        buf.push_str("{\"t\":\"meta\",\"title\":\"final-title\",\"updated_at\":99999}\n");
        fs::write(&path, buf).unwrap();

        let list = TestSession::list_in(Some("/project"), dir).unwrap();
        let summary = list.iter().find(|s| s.id == session.id).unwrap();
        assert_eq!(summary.title, "final-title");
        assert_eq!(summary.updated_at, 99999);
    }

    #[test]
    fn load_wrong_version_legacy_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("test/model", "/tmp");
        session.version = 999;
        let path = json_path(dir, session.id.id());
        fs::write(&path, serde_json::to_vec(&session).unwrap()).unwrap();

        let err = TestSession::load_from(session.id.id(), dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }

    #[test]
    fn open_roundtrip_resumes_append() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));

        let mut log = SessionLog::create(dir, &session).unwrap();
        session.messages.push(assistant_message("reply"));
        log.append(&session).unwrap();
        drop(log);

        let (loaded, mut log) =
            SessionLog::open::<Value, Value, Value>(dir, session.id.id()).unwrap();
        assert_eq!(loaded.messages.len(), 2);

        session.messages.push(user_message("second"));
        log.append(&session).unwrap();
        drop(log);

        let reloaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(reloaded.messages.len(), 3);
    }

    #[test]
    fn load_wrong_version_jsonl_returns_error() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let bad_header = serde_json::json!({
            "t": "header",
            "v": 999,
            "id": "01965087-4c71-7f00-8000-000000000000",
            "model": "m",
            "cwd": "/tmp",
            "created_at": 0
        });
        let id: CraftId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let path = jsonl_path(dir, id);
        fs::write(&path, format!("{}\n", bad_header)).unwrap();

        let err = TestSession::load_from(id, dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::VersionMismatch { found: 999, .. }
        ));
    }

    #[test_case(StoredThinking::Off ; "off")]
    #[test_case(StoredThinking::Adaptive ; "adaptive")]
    #[test_case(StoredThinking::Effort { level: Effort::XHigh } ; "effort")]
    #[test_case(StoredThinking::Budget { tokens: 4096 } ; "budget")]
    fn stored_thinking_serde_round_trip(variant: StoredThinking) {
        let json = serde_json::to_string(&variant).unwrap();
        let parsed: StoredThinking = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, variant);
    }

    #[test_case("off", Ok(StoredThinking::Off) ; "off")]
    #[test_case("adaptive", Ok(StoredThinking::Adaptive) ; "adaptive")]
    #[test_case(" adaptive ", Ok(StoredThinking::Adaptive) ; "trims_whitespace")]
    #[test_case("4096", Ok(StoredThinking::Budget { tokens: 4096 }) ; "valid_budget")]
    #[test_case("1", Ok(StoredThinking::Budget { tokens: 1 }) ; "minimum_budget")]
    #[test_case("0", Err(ThinkingParseError::BudgetZero) ; "budget_zero")]
    #[test_case("fast", Err(ThinkingParseError::Unknown("fast".into())) ; "garbage")]
    #[test_case("high", Ok(StoredThinking::Effort { level: Effort::High }) ; "effort_level")]
    fn parse_setting(input: &str, expected: Result<StoredThinking, ThinkingParseError>) {
        assert_eq!(StoredThinking::parse_setting(input), expected);
    }

    // Six ascending values in a six-variant enum also proves ALL is complete.
    #[test]
    fn effort_all_ascending_with_increasing_percent() {
        for pair in Effort::ALL.windows(2) {
            assert!(pair[0] < pair[1], "ALL must be ascending");
            assert!(
                pair[0].percent() < pair[1].percent(),
                "percent must be strictly increasing"
            );
        }
    }

    #[test]
    fn effort_wire_strings_round_trip() {
        let expected = ["minimal", "low", "medium", "high", "xhigh", "max"];
        for (e, s) in Effort::ALL.into_iter().zip(expected) {
            assert_eq!(e.as_str(), s);
            assert_eq!(s.parse::<Effort>(), Ok(e));
        }
    }

    #[test_case(Effort::High, &[Effort::Low, Effort::Medium, Effort::High], Effort::High ; "exact_match")]
    #[test_case(Effort::Max, &[Effort::Low, Effort::Medium, Effort::High], Effort::High ; "downgrade_to_nearest_lower")]
    #[test_case(Effort::Minimal, &[Effort::Low, Effort::Medium], Effort::Low ; "below_lowest_takes_lowest")]
    #[test_case(Effort::Medium, &[], Effort::Medium ; "empty_supported_keeps_self")]
    #[test_case(Effort::Max, &[Effort::High, Effort::XHigh], Effort::XHigh ; "glm_max_snaps_to_xhigh")]
    fn effort_snap(level: Effort, supported: &[Effort], expected: Effort) {
        assert_eq!(level.snap(supported), expected);
    }

    #[test_case(Effort::Minimal, 32_768, 3_276 ; "minimal_ten_percent")]
    #[test_case(Effort::Medium, 32_768, 13_107 ; "medium_forty_percent")]
    #[test_case(Effort::Max, 32_768, 32_768 ; "max_full_budget")]
    #[test_case(Effort::Minimal, 4_096, 1_024 ; "small_max_floors_at_min")]
    #[test_case(Effort::Max, 512, 1_024 ; "tiny_max_raised_to_floor")]
    fn effort_budget(level: Effort, max: u32, expected: u32) {
        assert_eq!(level.budget(max), expected);
    }

    #[test_case(32_768, 32_768, Effort::Max ; "full_budget_is_max")]
    #[test_case(64_000, 32_768, Effort::Max ; "above_max_is_max")]
    #[test_case(0, 32_768, Effort::Minimal ; "zero_is_minimal")]
    #[test_case(13_107, 32_768, Effort::Medium ; "forty_percent_is_medium")]
    #[test_case(1_024, 0, Effort::Max ; "zero_max_saturates")]
    fn effort_from_budget(n: u32, max: u32, expected: Effort) {
        assert_eq!(Effort::from_budget(n, max), expected);
    }

    #[test]
    fn effort_budget_round_trips_at_realistic_max() {
        const MAX: u32 = 32_768;
        for e in Effort::ALL {
            assert_eq!(Effort::from_budget(e.budget(MAX), MAX), e);
        }
    }

    #[test]
    fn session_meta_thinking_backward_compat() {
        let json = r#"{"mode":"build"}"#;
        let meta: super::SessionMeta = serde_json::from_str(json).unwrap();
        assert!(meta.thinking.is_none());
    }

    #[test]
    fn session_thinking_persists_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.meta.thinking = Some(StoredThinking::Budget { tokens: 8192 });
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(
            loaded.meta.thinking,
            Some(StoredThinking::Budget { tokens: 8192 })
        );
    }

    #[test]
    fn session_meta_fast_backward_compat() {
        let json = r#"{"mode":"build"}"#;
        let meta: super::SessionMeta = serde_json::from_str(json).unwrap();
        assert!(!meta.fast);
    }

    #[test]
    fn session_fast_persists_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.meta.fast = true;
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert!(loaded.meta.fast);
    }

    #[test]
    fn session_meta_context_window_overrides_backward_compat() {
        let json = r#"{"mode":"build"}"#;
        let meta: super::SessionMeta = serde_json::from_str(json).unwrap();
        assert!(meta.context_window_overrides.is_empty());
    }

    #[test]
    fn session_context_window_overrides_persist_through_save_load() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session
            .meta
            .context_window_overrides
            .insert("anthropic/claude-opus-4-8".into(), 120_000);
        session.save_to(dir).unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(
            loaded
                .meta
                .context_window_overrides
                .get("anthropic/claude-opus-4-8"),
            Some(&120_000)
        );
    }

    #[test]
    fn session_meta_context_window_overrides_serde_round_trip() {
        let mut meta = super::SessionMeta::default();
        meta.context_window_overrides
            .insert("anthropic/claude-opus-4-8".into(), 200_000);
        let json = serde_json::to_string(&meta).unwrap();
        let back: super::SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.context_window_overrides, meta.context_window_overrides);
    }

    #[test]
    fn crash_recovery_preserves_tool_outputs_around_corrupt_line() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        session
            .tool_outputs
            .insert("t1".into(), serde_json::json!({"result": "ok"}));
        let mut log = SessionLog::create(dir, &session).unwrap();
        log.append(&session).unwrap();

        let path = jsonl_path(dir, session.id.id());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"CORRUPT\n").unwrap();
        file.write_all(
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert!(loaded.tool_outputs.contains_key("t1"));
    }

    #[test]
    fn corrupt_header_line_only_returns_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let id: CraftId = "01965087-4c71-7f00-8000-000000000000".parse().unwrap();
        let path = jsonl_path(dir, id);
        fs::write(&path, "NOT_A_HEADER\n").unwrap();

        let err = TestSession::load_from(id, dir).unwrap_err();
        assert!(matches!(
            err,
            SessionError::Storage(StorageError::NotFound(_))
        ));
    }

    #[test]
    fn empty_lines_in_jsonl_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("msg"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id.id());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\n\n\n").unwrap();
        file.write_all(
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("after")}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn unknown_record_type_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut session: TestSession = Session::new("m", "/project");
        session.messages.push(user_message("first"));
        session.save_to(dir).unwrap();

        let path = jsonl_path(dir, session.id.id());
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"t\":\"future_type\",\"d\":{}}\n")
            .unwrap();
        file.write_all(
            serde_json::to_string(&serde_json::json!({"t":"msg","d": user_message("second")}))
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let loaded = TestSession::load_from(session.id.id(), dir).unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }
}
