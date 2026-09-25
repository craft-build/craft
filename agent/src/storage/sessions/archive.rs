//! Archive rotation: a shrink rewrite parks the old log under
//! `archive/<id>/`, and old archives are pruned by count and bytes.

use std::cmp::Reverse;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::{Session, is_jsonl, jsonl_path};
use crate::storage::atomic::sync_parent_dir;

/// Where a shrink rewrite parks the log it is about to drop, as `archive/<id>/`.
pub(super) const ARCHIVE_DIR: &str = "archive";
/// Archives kept per session. The extra ones go on the next archive, not on a
/// timer.
pub(super) const ARCHIVE_KEEP: usize = 3;
/// Three copies of a log full of tool output add up fast, so the bytes get a
/// budget of their own. The newest archive always survives, whatever it weighs.
pub(super) const ARCHIVE_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// A `msg` line starts with this. Matching the prefix beats parsing the log.
pub(super) const MSG_PREFIX: &[u8] = br#"{"t":"msg""#;

/// Compaction and rewind hand the log a shorter message list, and writing that
/// out would take the dropped turns with it, so the old file gets a second name
/// under `archive/<id>/` first. Its own path is untouched until the rename, so
/// [`SessionLog::rewrite`](super::SessionLog::rewrite) keeps its crash-safety
/// promise.
pub(super) fn archive_if_shrinking<M, U, T>(dir: &Path, session: &Session<M, U, T>) {
    let path = jsonl_path(dir, session.id.id());
    let Some(old_count) = count_msg_lines(&path) else {
        return;
    };
    let new_count = session.messages.len();
    if old_count <= new_count {
        return;
    }

    let archive_dir = dir.join(ARCHIVE_DIR).join(session.id.id().to_string());
    if let Err(e) = fs::create_dir_all(&archive_dir) {
        eprintln!("cannot create session archive dir: {e}");
        return;
    }
    let existing = archives_newest_first(&archive_dir);
    let next = existing.first().map_or(0, |a| a.seq) + 1;
    let archive_path = archive_dir.join(format!("{next}.jsonl"));
    let bytes = match link_archive(&path, &archive_path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("cannot archive session log before shrink rewrite: {e}");
            return;
        }
    };
    prune_archives(existing, bytes);
    // The live log is about to be renamed away. If the archive's directory
    // entry is not durable by then, a crash frees the only inode holding the
    // dropped turns, which is the loss this whole function exists to prevent.
    sync_parent_dir(&archive_path);
}

/// `None` when the file is missing or unreadable, and the rewrite then goes
/// ahead as before: nobody should lose a save because the old file would not
/// count. The scan reads bytes into one reused buffer rather than allocating
/// and UTF-8-validating a `String` per line.
fn count_msg_lines(path: &Path) -> Option<usize> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut count = 0;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return Some(count),
            Ok(_) => {}
        }
        if line.starts_with(MSG_PREFIX) {
            count += 1;
        }
    }
}

/// The archive is a second name for the log's current inode. The rewrite
/// renames a fresh file over the path, so the old bytes stay whole under the
/// new name and not one of them is copied. Filesystems with no links (FAT32 on
/// a stick) fall back to a plain copy. The size is for the byte budget.
fn link_archive(from: &Path, to: &Path) -> Result<u64, std::io::Error> {
    if fs::hard_link(from, to).is_err() {
        fs::copy(from, to).inspect_err(|_| {
            let _ = fs::remove_file(to);
        })?;
    }
    Ok(fs::metadata(to)?.len())
}

/// `<seq>.jsonl`, counting up. A number cannot step back the way a clock does
/// after an NTP fix or a suspend, so the order is always the truth and pruning
/// can never mistake the newest archive for the oldest. The mtime says when.
struct Archive {
    seq: u64,
    size: u64,
    path: PathBuf,
}

/// Newest first: the next name comes off the front, pruning walks to the back.
fn archives_newest_first(archive_dir: &Path) -> Vec<Archive> {
    let Ok(entries) = fs::read_dir(archive_dir) else {
        return Vec::new();
    };
    let mut archives: Vec<Archive> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !is_jsonl(&path) {
                return None;
            }
            Some(Archive {
                seq: path.file_stem()?.to_str()?.parse().ok()?,
                size: entry.metadata().ok()?.len(),
                path,
            })
        })
        .collect();
    archives.sort_unstable_by_key(|a| Reverse(a.seq));
    archives
}

/// Walks from the newest and keeps what both budgets allow, so the rest go.
/// `new_bytes` is the archive we just made: it is not in `existing`, so it can
/// never be the one dropped.
fn prune_archives(existing: Vec<Archive>, new_bytes: u64) {
    let mut total = new_bytes;
    let mut room = ARCHIVE_KEEP.saturating_sub(1);
    for archive in existing {
        total += archive.size;
        if room > 0 && total <= ARCHIVE_MAX_BYTES {
            room -= 1;
            continue;
        }
        let _ = fs::remove_file(&archive.path);
    }
}
