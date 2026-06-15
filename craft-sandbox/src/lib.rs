//! OS-level command sandboxing. An enforcement layer that sits *under* craft's
//! existing logical permission manager: once a command is approved, the sandbox
//! confines it to the workspace (with network gated) regardless of its contents.
//!
//! Platform support:
//! - **macOS**: shells out to the stock `sandbox-exec` binary with a generated
//!   SBPL profile (`workspace_write` = write the workspace + system temp/cache,
//!   read the rest; `read_only` = read everything, write nothing; network off).
//! - **Linux**: wraps the argv with `bubblewrap` (`bwrap`) when present.
//!   `bwrap` must be on PATH (documented prerequisite). Network is gated via
//!   `--unshare-net`; the workspace is bind-mounted read-write while the rest of
//!   the filesystem is read-only.
//! - **Windows**: no-op in v1. `transform` returns the argv unchanged and a
//!   warning is logged the first time. Full ACL/restricted-token impl is a
//!   follow-up.

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use std::sync::OnceLock;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
use tracing::warn;

mod mac;
mod linux;

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox binary '{0}' not found on PATH")]
    BinaryMissing(&'static str),
    #[error("sandbox profile generation failed: {0}")]
    Profile(String),
}

/// Confinement policy. `Off` disables the sandbox entirely (the `/yolo` path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    #[default]
    WorkspaceWrite,
    ReadOnly,
    DangerFullAccess,
    Off,
}

impl SandboxMode {
    pub fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }
}

/// Network access inside the sandbox. Defaults to off (gated).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    #[default]
    Denied,
    Allowed,
}

/// Resolved sandbox configuration. Built from the `[sandbox]` config section.
#[derive(Debug, Clone)]
pub struct SandboxProfile {
    pub mode: SandboxMode,
    pub network: NetworkPolicy,
    pub workspace: PathBuf,
    pub writable_roots: Vec<PathBuf>,
}

impl SandboxProfile {
    pub fn workspace_write(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: SandboxMode::WorkspaceWrite,
            network: NetworkPolicy::Denied,
            workspace: workspace.into(),
            writable_roots: Vec::new(),
        }
    }
}

/// Wraps `argv` so the spawned process is confined by the profile. When the mode
/// is `Off` (or the platform has no implementation) the command is returned
/// unchanged. `command` is the original `Command` (already configured with cwd,
/// env, stdio); the manager rewrites its program + args in place.
pub fn apply(command: &mut Command, profile: &SandboxProfile) -> Result<(), SandboxError> {
    if profile.mode.is_off() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        mac::apply(command, profile)
    }
    #[cfg(target_os = "linux")]
    {
        linux::apply(command, profile)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        warn_once_unsupported();
        Ok(())
    }
}

/// True iff the backing binary for this platform is available. Used by callers
/// that want to degrade gracefully (warn + run unsandboxed) when missing.
pub fn available() -> bool {
    #[cfg(target_os = "macos")]
    {
        which("sandbox-exec").is_some()
    }
    #[cfg(target_os = "linux")]
    {
        which("bwrap").is_some()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Best-effort PATH lookup without pulling in a dependency. Checks the
/// executable bit on Unix so a non-executable file by the right name does not
/// count as available.
pub(crate) fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    meta.is_file() && meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn warn_once_unsupported() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_ok() {
        warn!("OS sandboxing is not implemented on this platform; commands run unsandboxed");
    }
}

/// Common writable roots every profile grants so builds and standard tools work:
/// the system temp dir and the user's cache home.
pub fn default_writable_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(tmp) = std::env::temp_dir().canonicalize() {
        roots.push(tmp);
    }
    if let Some(cache) = etcetera_cache_dir() {
        roots.push(cache);
    }
    roots
}

fn etcetera_cache_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache"))
        })
        .and_then(|p| p.canonicalize().ok())
}

/// Normalize a path for inclusion in a profile. Canonicalizes when possible so
/// symlinks resolve, falling back to the original on failure.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
