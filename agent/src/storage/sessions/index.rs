//! The cwd index: `cwd_latest.json`, one cwd → latest session id entry.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::id::CraftId;
use crate::storage::{StorageError, atomic_write};

pub(super) const CWD_INDEX_FILE: &str = "cwd_latest.json";
pub(super) const CWD_INDEX_STEM: &str = "cwd_latest";

pub(super) fn load_cwd_index(dir: &Path) -> HashMap<String, String> {
    fs::read(dir.join(CWD_INDEX_FILE))
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

pub(super) fn update_cwd_index(
    dir: &Path,
    cwd: &str,
    session_id: CraftId,
) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    index.insert(cwd.to_string(), session_id.to_string());
    atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)
}

pub(super) fn remove_from_cwd_index(dir: &Path, session_id: CraftId) -> Result<(), StorageError> {
    let mut index = load_cwd_index(dir);
    let before = index.len();
    index.retain(|_, v| v.parse::<CraftId>() != Ok(session_id));
    if index.len() != before {
        atomic_write(&dir.join(CWD_INDEX_FILE), &serde_json::to_vec(&index)?)?;
    }
    Ok(())
}
