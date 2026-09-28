//! G.7 self-update & rollback, ported from the reference `src/update.rs`.
//!
//! The reference fetches `install.sh` from `main`, which the comparison
//! notes has no integrity pinning; here the script is fetched from the
//! tag matching the latest release and its SHA-256 verified against the
//! `install.sh.sha256` digest published with that tag. A release without
//! a digest, or a mismatch, refuses to run.

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{InvalidSnafu, Result};
use crate::storage::{StateDir, version};

const RELEASE_BASE: &str = "https://raw.githubusercontent.com/craft-build/craft";
const SCRIPT_NAME: &str = "install.sh";
const DIGEST_SUFFIX: &str = ".sha256";
const BACKUP_FILENAME: &str = "craft_backup";
const INSTALL_DIR_ENV: &str = "CRAFT_INSTALL_DIR";

fn invalid(reason: String) -> crate::error::Error {
    InvalidSnafu { reason }.build()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The digest file is `hex[ whitespace name? ]`; compare only the first
/// token. Any whitespace/case variance in the hex is tolerated.
pub fn digest_matches(script: &str, digest_file: &str) -> bool {
    let Some(published) = digest_file.split_whitespace().next() else {
        return false;
    };
    let computed = sha256_hex(script.as_bytes());
    published.eq_ignore_ascii_case(&computed)
}

fn script_url(tag: &str) -> String {
    format!("{RELEASE_BASE}/v{tag}/{SCRIPT_NAME}")
}

fn digest_url(tag: &str) -> String {
    format!("{RELEASE_BASE}/v{tag}/{SCRIPT_NAME}{DIGEST_SUFFIX}")
}

async fn fetch_text(url: &str) -> Result<String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| invalid(format!("fetch {url}: {e}")))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(invalid(format!("fetch {url}: HTTP {status}")));
    }
    resp.text()
        .await
        .map_err(|e| invalid(format!("read {url}: {e}")))
}

/// Digest pinned at build time (`CRAFT_INSTALL_SHA256`), set by release
/// automation out of band from the tag contents. Mandatory in release
/// builds (the tag's `.sha256` file alone is same-origin and cannot
/// detect a compromised tag); dev builds may omit it.
const BUILD_PIN: Option<&str> = option_env!("CRAFT_INSTALL_SHA256");

#[cfg(not(debug_assertions))]
const _: () = match BUILD_PIN {
    Some(_) => {}
    None => panic!("release builds must set CRAFT_INSTALL_SHA256 to pin the install script"),
};

fn check_build_pin(script: &str, pin: Option<&str>) -> Result<()> {
    let Some(pin) = pin else {
        return if cfg!(debug_assertions) {
            Ok(())
        } else {
            Err(invalid(
                "no install-script digest pinned into this binary; \
                 refusing to run the script"
                    .to_string(),
            ))
        };
    };
    let computed = sha256_hex(script.as_bytes());
    if !pin.eq_ignore_ascii_case(&computed) {
        return Err(invalid(format!(
            "sha256 of {SCRIPT_NAME} does not match the digest pinned into \
             this binary; refusing to run the script"
        )));
    }
    Ok(())
}

/// Fetch the install script pinned to `tag` and verify its digest. A
/// missing digest file is an error, not a pass-through.
async fn fetch_pinned_script(tag: &str) -> Result<String> {
    let script = fetch_text(&script_url(tag)).await?;
    let digest = fetch_text(&digest_url(tag)).await.map_err(|e| {
        invalid(format!(
            "release v{tag} publishes no digest for {SCRIPT_NAME} ({e}); \
             refusing to run an unpinned script"
        ))
    })?;
    if !digest_matches(&script, &digest) {
        return Err(invalid(format!(
            "sha256 mismatch for {SCRIPT_NAME} at tag v{tag}: \
             refusing to run the script"
        )));
    }
    check_build_pin(&script, BUILD_PIN)?;
    Ok(script)
}

/// Where the install script installs to: `$CRAFT_INSTALL_DIR` when set,
/// else the current binary's directory.
fn install_dir(exe_path: &Path, override_dir: Option<PathBuf>) -> Result<PathBuf> {
    override_dir
        .filter(|d| !d.as_os_str().is_empty())
        .or_else(|| Some(exe_path.parent()?.to_path_buf()))
        .ok_or_else(|| invalid("binary path has no parent directory".to_string()))
}

fn backup_binary(exe_path: &Path, storage: &StateDir) -> Result<PathBuf> {
    let backup_path = storage.path().join(BACKUP_FILENAME);
    std::fs::copy(exe_path, &backup_path)
        .map_err(|e| invalid(format!("backup binary to {}: {e}", backup_path.display())))?;
    Ok(backup_path)
}

fn execute_script(script: &str, install_dir: &Path) -> Result<()> {
    let mut tmp = tempfile::NamedTempFile::new()
        .map_err(|e| invalid(format!("write install script: {e}")))?;
    tmp.write_all(script.as_bytes())
        .map_err(|e| invalid(format!("write install script: {e}")))?;
    tmp.flush()
        .map_err(|e| invalid(format!("write install script: {e}")))?;

    let status = std::process::Command::new("sh")
        .arg(tmp.path())
        .env(INSTALL_DIR_ENV, install_dir)
        .status()
        .map_err(|e| invalid(format!("execute install script: {e}")))?;

    if !status.success() {
        return Err(invalid(format!(
            "install script failed with exit code {:?}",
            status.code()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn needs_sudo(path: &Path) -> bool {
    use std::ffi::CString;
    let Some(dir) = path.parent() else {
        return false;
    };
    let Ok(cpath) = CString::new(dir.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    unsafe { libc::access(cpath.as_ptr(), libc::W_OK) != 0 }
}

#[cfg(not(unix))]
fn needs_sudo(_path: &Path) -> bool {
    false
}

/// Copy the backup over the binary via a temp path, sudoing both steps
/// when the install directory is not user-writable.
fn restore_backup(backup_path: &Path, exe_path: &Path) -> Result<()> {
    let err = |e: std::io::Error| {
        invalid(format!(
            "restore backup from {}: {e}",
            backup_path.display()
        ))
    };
    let tmp = exe_path.with_extension("craft_tmp");
    if needs_sudo(exe_path) {
        println!("Restoring to {} (requires sudo)...", exe_path.display());
        let status = std::process::Command::new("sudo")
            .args(["cp", "--"])
            .arg(backup_path)
            .arg(&tmp)
            .status()
            .map_err(err)?;
        if !status.success() {
            return Err(err(std::io::Error::other("sudo cp failed")));
        }
        let status = std::process::Command::new("sudo")
            .args(["mv", "--"])
            .arg(&tmp)
            .arg(exe_path)
            .status()
            .map_err(err)?;
        if !status.success() {
            return Err(err(std::io::Error::other("sudo mv failed")));
        }
    } else {
        std::fs::copy(backup_path, &tmp).map_err(err)?;
        std::fs::rename(&tmp, exe_path).map_err(err)?;
    }
    Ok(())
}

fn prompt_yes(install_dir: &Path) -> bool {
    use std::io::{BufRead, Write};
    eprint!(
        "Install to {} and run this script? [y/N] ",
        install_dir.display()
    );
    let _ = std::io::stderr().flush();
    let mut input = String::new();
    std::io::stdin().lock().read_line(&mut input).is_ok() && input.trim().eq_ignore_ascii_case("y")
}

/// `craft update [--yes] [--no-color]`: fetch the digest-pinned install
/// script for the latest release, back up the current binary, and run it.
pub async fn update(skip_confirm: bool, _no_color: bool) -> Result<()> {
    let latest = version::fetch_latest().await?;
    if !version::is_newer(&latest, version::CURRENT) {
        println!("Already up to date (v{})", version::CURRENT);
        return Ok(());
    }

    println!("Current version: v{}", version::CURRENT);
    println!("Latest version:  v{latest}");
    println!();

    let exe_path = std::env::current_exe()
        .map_err(|e| invalid(format!("determine current binary path: {e}")))?;
    let install_dir = install_dir(
        &exe_path,
        std::env::var_os(INSTALL_DIR_ENV).map(PathBuf::from),
    )?;
    let storage = StateDir::resolve().map_err(|e| invalid(format!("resolve state dir: {e}")))?;

    let script = fetch_pinned_script(&latest).await?;
    // The reference syntax-highlights the script here; this repo has no
    // standalone bash highlighter, so it prints plainly either way.
    println!("{script}");

    if !skip_confirm && !prompt_yes(&install_dir) {
        println!("Aborted.");
        return Ok(());
    }

    let backup_path = backup_binary(&exe_path, &storage)?;
    execute_script(&script, &install_dir)?;

    println!();
    println!("Updated successfully.");
    println!("Previous version saved to: {}", backup_path.display());
    println!("To restore: craft rollback");

    Ok(())
}

/// `craft rollback`: restore the binary backed up by the last update.
pub fn rollback() -> Result<()> {
    let exe_path = std::env::current_exe()
        .map_err(|e| invalid(format!("determine current binary path: {e}")))?;
    let storage = StateDir::resolve().map_err(|e| invalid(format!("resolve state dir: {e}")))?;
    let backup_path = storage.path().join(BACKUP_FILENAME);

    if !backup_path.exists() {
        return Err(invalid(format!(
            "no backup found at {}",
            backup_path.display()
        )));
    }

    restore_backup(&backup_path, &exe_path)?;
    println!("Restored previous version.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_accepts_hex_with_optional_filename() {
        let script = "echo hi\n";
        let hex = sha256_hex(script.as_bytes());
        assert!(digest_matches(script, &hex));
        assert!(digest_matches(script, &format!("{hex}  install.sh\n")));
        assert!(digest_matches(script, &hex.to_uppercase()));
    }

    #[test]
    fn digest_matches_rejects_wrong_or_empty_digests() {
        let script = "echo hi\n";
        assert!(!digest_matches(script, &sha256_hex(b"other")));
        assert!(!digest_matches(script, "  \n"));
        let hex = sha256_hex(script.as_bytes());
        assert!(!digest_matches("echo bye\n", &hex));
    }

    #[test]
    fn build_pin_is_an_independent_check() {
        let script = "echo hi\n";
        let hex = sha256_hex(script.as_bytes());
        assert!(check_build_pin(script, Some(&hex)).is_ok());
        assert!(check_build_pin(script, Some(&hex.to_uppercase())).is_ok());
        let err = check_build_pin(script, Some(&sha256_hex(b"other"))).unwrap_err();
        assert!(err.to_string().contains("pinned into"));
        // No build-time pin: dev builds only; release builds fail in
        // check_build_pin (and at compile time).
        assert!(check_build_pin(script, None).is_ok());
    }

    #[test]
    fn script_and_digest_urls_are_tag_pinned() {
        assert_eq!(
            script_url("0.14.1"),
            "https://raw.githubusercontent.com/craft-build/craft/v0.14.1/install.sh"
        );
        assert!(digest_url("0.14.1").ends_with("/v0.14.1/install.sh.sha256"));
    }

    #[test]
    fn install_dir_prefers_the_env_override() {
        let exe = Path::new("/usr/local/bin/craft");
        assert_eq!(
            install_dir(exe, Some(PathBuf::from("/opt/craft"))).unwrap(),
            Path::new("/opt/craft")
        );
        assert_eq!(install_dir(exe, None).unwrap(), Path::new("/usr/local/bin"));
        assert_eq!(
            install_dir(exe, Some(PathBuf::new())).unwrap(),
            Path::new("/usr/local/bin"),
            "empty override falls through to the binary directory"
        );
    }

    #[test]
    fn backup_and_restore_roundtrip_via_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("craft");
        std::fs::write(&exe, b"new binary").unwrap();
        let dir = StateDir::from_path(tmp.path().to_path_buf());

        let backup = backup_binary(&exe, &dir).unwrap();
        assert_eq!(backup, tmp.path().join(BACKUP_FILENAME));
        std::fs::write(&exe, b"updated binary").unwrap();

        restore_backup(&backup, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new binary");
    }

    #[test]
    fn execute_script_propagates_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(execute_script("exit 0", dir.path()).is_ok());
        let err = execute_script("exit 3", dir.path()).unwrap_err();
        assert!(err.to_string().contains("exit code"));
    }
}
