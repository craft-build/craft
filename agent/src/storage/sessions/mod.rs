//! Session persistence with append-only JSONL log format.
//!
//! Ported from the reference `craft-storage/src/sessions.rs` (thiserror →
//! snafu, no `tracing` here so warnings go to stderr, meta trimmed to the
//! concepts this repo ports, and `StoredRule` replaced by
//! `crate::permissions::PermissionRule`).
//!
//! Each session is stored as `{uuid}.jsonl`, one JSON record per line. The
//! format is crash-safe: on load, any trailing run of unparseable lines is
//! discarded (a partial flush may corrupt multiple trailing records).
//! [`SessionLog`](log::SessionLog) tracks cursor state to enable O(delta)
//! incremental saves.
//!
//! Legacy `.json` files are loaded transparently and converted to `.jsonl` on
//! next save.
//!
//! Module layout: the [`Session`] model and its API live here, while the
//! append-only log (`log`), archive rotation (`archive`), the cwd index
//! (`index`), header scanning for the picker (`scan`), and legacy-file
//! migration/removal (`legacy`) live in sibling modules.

mod archive;
mod index;
mod legacy;
mod log;
mod scan;

#[cfg(test)]
mod tests;

pub use log::SessionLog;

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use snafu::Snafu;

use crate::id::{CraftId, SessionRef};
use crate::permissions::PermissionRule;
use crate::storage::{StateDir, StorageError};

use archive::ARCHIVE_DIR;
use index::{load_cwd_index, remove_from_cwd_index};
use legacy::{locate_session_file, remove_legacy_files, try_remove};
use log::load_session_at;
use scan::scan_headers;

#[cfg(test)]
use archive::{ARCHIVE_KEEP, ARCHIVE_MAX_BYTES, MSG_PREFIX};
#[cfg(test)]
use index::{CWD_INDEX_FILE, update_cwd_index};
#[cfg(test)]
use legacy::json_path;
#[cfg(test)]
use log::{LOG_BLOATED, MAX_APPENDS, write_full_session};

const SESSION_VERSION: u32 = 1;
const LOG_FORMAT_VERSION: u32 = 3;
pub const SESSIONS_DIR: &str = "sessions";
const DEFAULT_TITLE: &str = "New session";
const MAX_TITLE_LEN: usize = 60;

/// Hands out the token that tags one append-only run of a message list.
/// Process wide, so two runs never pick the same number.
static EPOCH: AtomicU64 = AtomicU64::new(1);

pub fn next_epoch() -> u64 {
    EPOCH.fetch_add(1, Ordering::Relaxed)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[derive(Debug, Snafu)]
pub enum SessionError {
    #[snafu(display("storage error"))]
    #[snafu(context(false))]
    Storage { source: StorageError },

    #[snafu(display("incompatible session version {found} (expected {expected})"))]
    VersionMismatch { found: u32, expected: u32 },

    #[snafu(display("session ID mismatch: log owns {log_id}, got {given_id}"))]
    IdMismatch { log_id: CraftId, given_id: CraftId },

    #[snafu(display("session log diverged ({reason}); rewrite required"))]
    LogDiverged { reason: &'static str },

    #[snafu(display(
        "session log {path} has header id {raw_id:?} that is not a valid id: {source}"
    ))]
    CorruptHeaderId {
        path: String,
        raw_id: String,
        source: crate::id::CraftIdParseError,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StoredTokenUsage {
    #[serde(default)]
    pub input: u32,
    #[serde(default)]
    pub output: u32,
    #[serde(default)]
    pub cache_creation: u32,
    #[serde(default)]
    pub cache_read: u32,
    /// What the turns billed, in USD. Prices move (some providers by the
    /// hour), so re-pricing these counters later would be fiction. `None` on
    /// unpriced models, and on entries written before we recorded it until the
    /// next load settles an estimate into them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

/// From the in-memory accounting type of `usage.rs` (what `Event::Done`
/// carries) into this persisted shape.
impl From<crate::usage::StoredTokenUsage> for StoredTokenUsage {
    fn from(u: crate::usage::StoredTokenUsage) -> Self {
        Self {
            input: u.input,
            output: u.output,
            cache_creation: u.cache_creation,
            cache_read: u.cache_read,
            cost: u.cost,
        }
    }
}

impl StoredTokenUsage {
    pub fn total_input(&self) -> u32 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    pub fn total(&self) -> u32 {
        self.total_input().saturating_add(self.output)
    }
}

impl std::ops::AddAssign for StoredTokenUsage {
    fn add_assign(&mut self, rhs: Self) {
        self.input = self.input.saturating_add(rhs.input);
        self.output = self.output.saturating_add(rhs.output);
        self.cache_creation = self.cache_creation.saturating_add(rhs.cache_creation);
        self.cache_read = self.cache_read.saturating_add(rhs.cache_read);
        add_cost(&mut self.cost, rhs.cost);
    }
}

/// The one way costs are summed, so every running total agrees: `None` until
/// the first priced turn shows up, and from there it only grows.
pub fn add_cost(total: &mut Option<f64>, addend: Option<f64>) {
    if let Some(addend) = addend {
        *total = Some(total.unwrap_or_default() + addend);
    }
}

/// The part of a session the owner mirrors from its own live state and hands
/// over whole on every checkpoint. `subagents` and `usage_by_model` live on
/// [`Session`] directly because the session maintains them itself, so a
/// checkpoint would otherwise copy them out and back in only to compare them
/// against themselves.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub session_rules: Vec<PermissionRule>,
    #[serde(default)]
    pub context_size: u32,
}

/// Messages plus the token of the run they belong to. Comparing tokens tells
/// an append from a rewrite, with no need to diff the lists.
#[derive(Clone)]
pub struct HistorySnapshot<M> {
    pub epoch: u64,
    pub messages: Arc<Vec<M>>,
}

impl<M> HistorySnapshot<M> {
    pub fn new(messages: Vec<M>) -> Self {
        Self {
            epoch: next_epoch(),
            messages: Arc::new(messages),
        }
    }
}

impl<M> Default for HistorySnapshot<M> {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// The conversation collections are private so every change goes through a
/// mutator that classifies itself: `revision` says "this needs writing",
/// `content_revision` says "it can wait", and `epoch` says "append cursors
/// into the log are void". The other fields stay public because the meta
/// record is rewritten in full on every append, so they hold no cursor to
/// spoil. `cwd` and `model` are the exception: they live in the header record,
/// which only a rewrite touches, so changes must go through their setters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session<M, U, T> {
    pub version: u32,
    pub id: SessionRef,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub(super) messages: Arc<Vec<M>>,
    pub token_usage: U,
    #[serde(default = "HashMap::new")]
    pub(super) tool_outputs: HashMap<String, Arc<T>>,
    #[serde(default = "HashMap::new", skip_serializing_if = "HashMap::is_empty")]
    pub(super) subagent_messages: HashMap<String, Arc<Vec<M>>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) subagents: Vec<StoredSubagent>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) usage_by_model: HashMap<String, StoredTokenUsage>,
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub created_at: u64,
    pub updated_at: u64,
    /// Bumped by every mutation, so a checkpoint knows if there is anything
    /// to write.
    #[serde(skip)]
    pub(super) revision: u64,
    /// Bumped by every mutation except `meta`, so a checkpoint can tell a tool
    /// result, which has to reach disk now, from a keystroke in the draft,
    /// which can wait for the keystrokes behind it.
    #[serde(skip)]
    pub(super) content_revision: u64,
    /// The append-only run `messages` belongs to, adopted from the producer's
    /// snapshot or minted fresh when this session rewrites them itself. Once
    /// it changes, every append cursor into the log is void.
    #[serde(skip, default = "next_epoch")]
    pub(super) epoch: u64,
    /// Bumped when this session rewrites a collection in place (replaced
    /// messages, tool outputs or subagent histories). Kept apart from
    /// `epoch` so `set_history` adopting a producer's snapshot can never
    /// erase a locally minted void: cursor validity is the pair.
    #[serde(skip)]
    pub(super) rewrites: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionRef,
    pub title: String,
    pub cwd: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

pub trait TitleSource {
    fn first_user_text(&self) -> Option<&str>;
}

impl TitleSource for crate::history::Message {
    fn first_user_text(&self) -> Option<&str> {
        match self {
            Self::User { content } => content.iter().find_map(|block| match block {
                crate::history::UserContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            }),
            _ => None,
        }
    }
}

/// A pasted code block bakes `\n` into a title and skews width-based padding
/// in single-line UI like the picker, so every title entry point calls this.
pub fn normalize_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn generate_title<M: TitleSource>(messages: &[M]) -> String {
    let first_user_text = messages.iter().find_map(|m| m.first_user_text());

    let Some(text) = first_user_text.map(str::trim).filter(|t| !t.is_empty()) else {
        return DEFAULT_TITLE.into();
    };
    let text = normalize_title(text);

    if text.len() <= MAX_TITLE_LEN {
        return text;
    }

    let boundary = text.floor_char_boundary(MAX_TITLE_LEN);
    let truncated = &text[..boundary];
    match truncated.rfind(' ') {
        Some(pos) if pos > MAX_TITLE_LEN / 2 => format!("{}…", &truncated[..pos]),
        _ => format!("{truncated}…"),
    }
}

fn jsonl_path(dir: &Path, id: CraftId) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn is_jsonl(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "jsonl")
}

// -- Session impl --

impl<M, U, T> Session<M, U, T>
where
    M: Serialize + DeserializeOwned + TitleSource + Clone,
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
            messages: Arc::default(),
            token_usage: U::default(),
            tool_outputs: HashMap::new(),
            subagent_messages: HashMap::new(),
            subagents: Vec::new(),
            usage_by_model: HashMap::new(),
            meta: SessionMeta::default(),
            created_at: now,
            updated_at: now,
            revision: 0,
            content_revision: 0,
            epoch: next_epoch(),
            rewrites: 0,
        }
    }

    pub fn messages(&self) -> &[M] {
        &self.messages
    }

    pub fn take_messages(self) -> Vec<M> {
        Arc::unwrap_or_clone(self.messages)
    }

    pub fn tool_outputs(&self) -> &HashMap<String, Arc<T>> {
        &self.tool_outputs
    }

    pub fn subagent_messages(&self) -> &HashMap<String, Arc<Vec<M>>> {
        &self.subagent_messages
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    fn touch(&mut self) {
        self.content_revision += 1;
        self.touch_soft();
    }

    /// Only UI state moved, so the write can wait for company. Everything a
    /// crash would lose for good goes through `touch`, which is the default a
    /// new mutator gets by not thinking about it.
    fn touch_soft(&mut self) {
        self.updated_at = now_epoch();
        self.revision += 1;
    }

    /// Every append cursor into the log is void from here on. Counted in
    /// `rewrites`, which snapshot adoption never touches, so a same-frame
    /// `set_history` cannot erase the void before the writer sees it.
    fn rewrite(&mut self) {
        self.rewrites += 1;
        self.touch();
    }

    /// [`Self::rewrite`] for local changes to `messages`: they also leave the
    /// producer's run, so the epoch is minted fresh. Once this state is
    /// saved, re-adopting a stale run snapshot keeps diverging instead of
    /// splicing its tail onto a rewound log.
    fn rewrite_messages(&mut self) {
        self.epoch = next_epoch();
        self.rewrite();
    }

    pub fn push_message(&mut self, msg: M) {
        Arc::make_mut(&mut self.messages).push(msg);
        self.touch();
    }

    pub fn replace_messages(&mut self, messages: Vec<M>) {
        self.messages = Arc::new(messages);
        self.rewrite_messages();
    }

    pub fn truncate_messages(&mut self, len: usize) {
        if len >= self.messages.len() {
            return;
        }
        Arc::make_mut(&mut self.messages).truncate(len);
        self.rewrite_messages();
    }

    /// Adopting a producer's snapshot inherits its run token, so the log's
    /// cursors survive exactly when the snapshot was an append.
    fn set_history(&mut self, snapshot: &HistorySnapshot<M>) {
        self.messages = Arc::clone(&snapshot.messages);
        self.epoch = snapshot.epoch;
        self.touch();
    }

    /// Applies everything the owner mirrors from live state. It takes an `Arc`
    /// and checks for a real change first because `Arc::make_mut` deep-copies
    /// the whole session while the writer still holds the last snapshot, and an
    /// idle session should not pay for that every frame.
    pub fn checkpoint(
        this: &mut Arc<Self>,
        history: Option<&HistorySnapshot<M>>,
        meta: SessionMeta,
        token_usage: U,
    ) where
        M: Clone,
        U: PartialEq + Clone,
        T: Clone,
    {
        let history = history.filter(|h| !Arc::ptr_eq(&this.messages, &h.messages));
        if history.is_none() && this.meta == meta && this.token_usage == token_usage {
            return;
        }
        let session = Arc::make_mut(this);
        if let Some(snapshot) = history {
            session.set_history(snapshot);
            // The title comes from the messages, so it goes stale exactly when
            // they move.
            session.update_title_if_default();
        }
        session.set_meta(meta);
        session.set_token_usage(token_usage);
    }

    /// A change under an existing id is not expressible as an append, so it
    /// voids the cursors; a new id is a pure append.
    pub fn insert_tool_output(&mut self, id: String, output: Arc<T>) {
        if self.tool_outputs.insert(id, output).is_some() {
            self.rewrite();
        } else {
            self.touch();
        }
    }

    /// A change under an existing id is not expressible as an append, so it
    /// voids the cursors; a new id is a pure append.
    pub fn set_subagent_messages(&mut self, id: String, msgs: Vec<M>) {
        if self.subagent_messages.insert(id, Arc::new(msgs)).is_some() {
            self.rewrite();
        } else {
            self.touch();
        }
    }

    fn set_token_usage(&mut self, usage: U)
    where
        U: PartialEq,
    {
        if self.token_usage == usage {
            return;
        }
        self.token_usage = usage;
        self.touch();
    }

    fn set_meta(&mut self, meta: SessionMeta) {
        if self.meta == meta {
            return;
        }
        self.meta = meta;
        self.touch_soft();
    }

    pub fn subagents(&self) -> &[StoredSubagent] {
        &self.subagents
    }

    pub fn set_subagents(&mut self, subagents: Vec<StoredSubagent>) {
        if self.subagents == subagents {
            return;
        }
        self.subagents = subagents;
        self.touch();
    }

    pub fn usage_by_model(&self) -> &HashMap<String, StoredTokenUsage> {
        &self.usage_by_model
    }

    /// For settling costs on load; every other write goes through
    /// [`Self::add_model_usage`].
    pub fn usage_by_model_mut(&mut self) -> &mut HashMap<String, StoredTokenUsage> {
        self.touch();
        &mut self.usage_by_model
    }

    pub fn set_title(&mut self, title: String) {
        if self.title == title {
            return;
        }
        self.title = title;
        self.touch();
    }

    /// Header field: appends never rewrite the header, so the change voids
    /// the cursors to force a full rewrite.
    pub fn set_cwd(&mut self, cwd: String) {
        if self.cwd == cwd {
            return;
        }
        self.cwd = cwd;
        self.rewrite();
    }

    /// Header field, see [`Self::set_cwd`].
    pub fn set_model(&mut self, model: String) {
        if self.model == model {
            return;
        }
        self.model = model;
        self.rewrite();
    }

    pub fn add_model_usage(&mut self, model: &str, usage: StoredTokenUsage) {
        *self.usage_by_model.entry(model.to_owned()).or_default() += usage;
        self.touch();
    }

    /// After `messages` is truncated (rewind), state keyed by tool_use_id can
    /// point at calls that no longer exist. On restore that shows up as ghost
    /// subagent tabs and leaked tool outputs, so this drops everything not
    /// reachable from `messages`.
    pub fn prune_orphans(&mut self, tool_ids: impl Fn(&M) -> Vec<String>) {
        let main_ids: HashSet<String> = self.messages.iter().flat_map(&tool_ids).collect();
        self.subagent_messages.retain(|id, _| main_ids.contains(id));
        self.subagents
            .retain(|sa| main_ids.contains(&sa.tool_use_id));

        let live: HashSet<String> = self
            .subagent_messages
            .values()
            .flat_map(|msgs| msgs.iter())
            .flat_map(&tool_ids)
            .chain(main_ids)
            .collect();
        self.tool_outputs.retain(|id, _| live.contains(id));
        self.rewrite();
    }

    pub fn save(&mut self, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        self.save_to(&sessions_dir)
    }

    pub fn save_to(&mut self, dir: &Path) -> Result<(), SessionError> {
        self.updated_at = now_epoch();
        SessionLog::rewrite(dir, self)?;
        Ok(())
    }

    pub fn load(id: CraftId, dir: &StateDir) -> Result<Self, SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::load_from(id, &sessions_dir)
    }

    pub fn load_from(id: CraftId, dir: &Path) -> Result<Self, SessionError> {
        let Some(path) = locate_session_file(dir, id) else {
            return Err(StorageError::NotFound {
                path: id.to_string(),
            }
            .into());
        };
        let session = load_session_at::<M, U, T>(&path)?;
        if path != jsonl_path(dir, id)
            && let Err(e) = SessionLog::write_canonical(dir, &session)
        {
            eprintln!("failed migrate to canonical jsonl; keeping legacy file: {e}");
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
                    Err(e) => {
                        eprintln!("indexed session missing on disk; rescanning: {e}");
                    }
                },
                Err(e) => eprintln!("indexed session id unparseable; rescanning: {e}"),
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
            self.set_title(generate_title(&self.messages));
        }
    }

    pub fn delete(id: CraftId, dir: &StateDir) -> Result<(), SessionError> {
        let sessions_dir = dir.ensure_subdir(SESSIONS_DIR)?;
        Self::delete_from(id, &sessions_dir)
    }

    pub fn delete_from(id: CraftId, dir: &Path) -> Result<(), SessionError> {
        let mut removed = try_remove(&jsonl_path(dir, id))?;
        removed |= remove_legacy_files(dir, id)?;
        // Backups, not the session: failing to sweep them must not fail a
        // delete whose log is already gone, and their presence alone does not
        // make a session exist.
        if let Err(e) = fs::remove_dir_all(dir.join(ARCHIVE_DIR).join(id.to_string()))
            && e.kind() != ErrorKind::NotFound
        {
            eprintln!("session archives remain after delete: {e}");
        }
        if !removed {
            return Err(StorageError::NotFound {
                path: id.to_string(),
            }
            .into());
        }
        remove_from_cwd_index(dir, id)?;
        Ok(())
    }
}
