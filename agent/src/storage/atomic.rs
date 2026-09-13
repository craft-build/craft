//! Atomic file replacement: write to a tempfile in the same directory, then
//! rename over the destination so readers see either the old file or the
//! complete new one.
//!
//! `atomic_write_permissions` sets the file mode before the rename, for
//! secrets that must land at 0600. Ported from the reference
//! `craft-storage/src/lib.rs`.

use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tempfile::NamedTempFile;

use super::{StorageError, StorageResult};

/// Atomically replace `path` with `data`. The parent directory must exist.
/// Readers observe either the old file or the complete new file. Existing
/// file permissions are preserved; new files use mode 0600.
pub fn atomic_write(path: &Path, data: &[u8]) -> StorageResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(tmp.path(), metadata.permissions())?;
    }
    tmp.as_file().sync_data()?;
    persist(tmp, path)
}

/// Like [`atomic_write`], but forces the destination to `mode` instead of
/// preserving the previous permissions.
pub fn atomic_write_permissions(path: &Path, data: &[u8], mode: u32) -> StorageResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    #[cfg(unix)]
    fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode))?;
    #[cfg(not(unix))]
    let _ = mode;
    tmp.as_file().sync_all()?;
    persist(tmp, path)
}

/// `into_parts` drops the auto-cleanup-on-drop guarantee, but we need the
/// File handle closed (the temp lives in the same dir) and a rename must
/// reach the directory entry before we report success. On failure, we
/// manually clean up the temp file.
fn persist(tmp: NamedTempFile, path: &Path) -> StorageResult<()> {
    let (_, tmp_path) = tmp.into_parts();
    fs::rename(&tmp_path, path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        StorageError::Io { source: e }
    })?;
    sync_parent_dir(path);
    Ok(())
}

/// A rename is durable only once the directory entry reaches disk; without
/// this a freshly created file can vanish after power loss even though the
/// write returned Ok. Best effort: not every filesystem accepts a directory
/// fsync.
pub(crate) fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(f) = fs::File::open(dir)
    {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &[u8] = b"original";
    const OWNER_ONLY_FILE_MODE: u32 = 0o600;
    const REPLACEMENT: &[u8] = b"replacement";
    #[cfg(unix)]
    const FILE_MODE_MASK: u32 = 0o777;

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[test]
    fn atomic_write_permissions_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();

        atomic_write_permissions(&path, REPLACEMENT, OWNER_ONLY_FILE_MODE).unwrap();

        assert_eq!(fs::read(path).unwrap(), REPLACEMENT);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");

        atomic_write(&path, ORIGINAL).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            OWNER_ONLY_FILE_MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_permissions_forces_mode() {
        const MODE: u32 = 0o600;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        fs::write(&path, ORIGINAL).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write_permissions(&path, REPLACEMENT, MODE).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_destination_permissions() {
        const MODE: u32 = 0o640;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        fs::write(&path, ORIGINAL).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(MODE)).unwrap();

        atomic_write(&path, REPLACEMENT).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & FILE_MODE_MASK,
            MODE
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_cleans_up_temp_after_replacement_failure() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("destination");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write(&destination, REPLACEMENT).is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
