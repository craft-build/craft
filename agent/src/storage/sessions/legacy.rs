//! Legacy `.json`-era session files: the pre-jsonl header shape, files whose
//! name spells the id in a different encoding, and their migration/removal.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::SessionError;
use super::scan::{ScannedHeader, session_entries};
use super::{StorageError, is_jsonl};
use crate::id::{CraftId, SessionRef};

#[derive(Deserialize)]
struct LegacyHeader {
    version: u32,
    id: SessionRef,
    title: String,
    cwd: String,
    updated_at: u64,
}

pub(super) fn scan_legacy_header(path: &Path) -> Option<ScannedHeader> {
    let data = fs::read(path).ok()?;
    let h: LegacyHeader = serde_json::from_slice(&data).ok()?;
    if h.version != super::SESSION_VERSION {
        return None;
    }
    Some(ScannedHeader {
        id: h.id,
        cwd: h.cwd,
        title: h.title,
        updated_at: h.updated_at,
    })
}

pub(super) fn json_path(dir: &Path, id: CraftId) -> PathBuf {
    dir.join(format!("{id}.json"))
}

pub(super) fn remove_legacy_files(dir: &Path, id: CraftId) -> Result<bool, SessionError> {
    let mut removed = try_remove(&json_path(dir, id))?;
    for legacy in find_legacy_files(dir, id) {
        removed |= try_remove(&legacy)?;
    }
    Ok(removed)
}

pub(super) fn try_remove(path: &Path) -> Result<bool, StorageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
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

pub(super) fn locate_session_file(dir: &Path, id: CraftId) -> Option<PathBuf> {
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
