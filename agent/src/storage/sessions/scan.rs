//! Header scanning for the session list: reads only the header line and the
//! trailing meta record of each log, with an on-disk cache keyed by file
//! signature.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::SessionSummary;
use super::index::CWD_INDEX_STEM;
use super::legacy::scan_legacy_header;
use super::{DEFAULT_TITLE, LOG_FORMAT_VERSION, StorageError, normalize_title};
use crate::id::SessionRef;
use crate::storage::atomic_write;

const SCAN_CACHE_FILE: &str = "scan_cache.json";
const SCAN_CACHE_STEM: &str = "scan_cache";
const NON_SESSION_STEMS: [&str; 2] = [CWD_INDEX_STEM, SCAN_CACHE_STEM];
const TAIL_BUF: u64 = 64 * 1024;

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
pub(super) struct ScannedHeader {
    pub(super) id: SessionRef,
    pub(super) cwd: String,
    pub(super) title: String,
    pub(super) updated_at: u64,
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

pub(super) fn scan_headers(
    cwd: Option<&str>,
    dir: &Path,
) -> Result<Vec<SessionSummary>, StorageError> {
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
                let header = if super::is_jsonl(&path) {
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
                title: normalize_title(&h.title),
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
        eprintln!("failed to write session scan cache: {e}");
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

/// Walks the file tail backwards to find the last `Meta` record without
/// scanning every line.
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

pub(super) fn session_entries(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
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
