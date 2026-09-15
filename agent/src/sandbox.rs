//! OS-level command sandboxing. An enforcement layer that sits *under* the
//! logical permission manager: once a command is approved, the sandbox confines
//! it to the workspace (with network optionally gated) regardless of its
//! contents. Ported from reference `craft-sandbox/`.
//!
//! Platform support:
//! - **macOS**: shells out to the stock `sandbox-exec` binary with a generated
//!   SBPL profile (`workspace_write` = write the workspace + system temp/cache,
//!   read the rest; `read_only` = read everything, write nothing).
//! - **Linux**: wraps the argv with `bubblewrap` (`bwrap`) when present (documented
//!   prerequisite). Network is gated via `--unshare-net`; the workspace is
//!   bind-mounted read-write while the rest of the filesystem is read-only.
//! - **Other platforms**: no-op; `apply` returns the argv unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;

use snafu::Snafu;

#[derive(Debug, Snafu)]
pub enum SandboxError {
    #[snafu(display("sandbox binary '{binary}' not found on PATH"))]
    BinaryMissing { binary: &'static str },

    #[snafu(display("sandbox profile generation failed: {reason}"))]
    Profile { reason: String },
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

/// Network access inside the sandbox. Defaults to allowed so standard build
/// tools and network pulls into the workspace/temp work without reconfiguration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    #[default]
    Allowed,
    Denied,
}

/// Resolved sandbox configuration.
#[derive(Debug, Clone, Default)]
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
            workspace: workspace.into(),
            ..Default::default()
        }
    }
}

/// Wraps `argv` so the spawned process is confined by the profile. When the
/// mode is `Off` (or the platform has no implementation) the command is left
/// unchanged; the caller's cwd/env/stdio are preserved because only the program
/// and args are rewritten.
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
        let _ = (command, profile);
        Ok(())
    }
}

/// True iff the backing binary for this platform is available. Used by callers
/// that want to degrade gracefully (warn + run unsandboxed) when missing.
pub fn available() -> bool {
    #[cfg(target_os = "macos")]
    {
        which(mac::SANDBOX_EXEC).is_some()
    }
    #[cfg(target_os = "linux")]
    {
        which(linux::BWRAP).is_some()
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

/// Common writable roots every profile grants so builds and standard package
/// managers work without per-tool configuration: the system temp dir, the user
/// cache home(s), and the per-tool data homes used by cargo, rustup, go, npm,
/// yarn, gradle and maven. Env overrides are honored where tools define them.
pub fn default_writable_roots() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|h| h.canonicalize().ok());

    let mut roots = Vec::new();
    if let Ok(tmp) = std::env::temp_dir().canonicalize() {
        roots.push(tmp);
    }

    // On Unix, /tmp is a common writable root for build tools.
    #[cfg(unix)]
    {
        roots.push(PathBuf::from("/tmp"));
    }
    roots.extend(cache_dirs());

    for (env_var, fallback) in BUILD_TOOL_HOMES {
        if let Some(p) = resolve_tool_home(*env_var, fallback, home.as_deref()) {
            roots.push(p);
        }
    }
    for env_var in ENV_ONLY_ROOTS {
        if let Some(p) = std::env::var_os(env_var)
            .map(PathBuf::from)
            .and_then(|p| p.canonicalize().ok())
        {
            roots.push(p);
        }
    }

    dedup_preserving_order(roots)
}

/// Per-tool data homes. The env override wins over the default subdir under
/// `$HOME`; a `None` env var means the tool has no standard override and always
/// uses its default subdir.
const BUILD_TOOL_HOMES: &[(Option<&str>, &str)] = &[
    (Some("CARGO_HOME"), ".cargo"),
    (Some("RUSTUP_HOME"), ".rustup"),
    (Some("GOPATH"), "go"),
    (Some("GRADLE_USER_HOME"), ".gradle"),
    (Some("YARN_CACHE_FOLDER"), ".yarn"),
    (None, ".npm"),
    (None, ".m2"),
];

/// Roots with no fixed default under `$HOME`: their default already lives inside
/// an allowed root (the workspace or `GOPATH`), so they only matter when the env
/// var points them elsewhere (e.g. a shared `CARGO_TARGET_DIR`).
const ENV_ONLY_ROOTS: &[&str] = &["CARGO_TARGET_DIR", "GOMODCACHE"];

fn resolve_tool_home(
    env_var: Option<&str>,
    fallback: &str,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let path = match (env_var.and_then(std::env::var_os), home) {
        (Some(v), _) => PathBuf::from(v),
        (None, Some(h)) => h.join(fallback),
        (None, None) => return None,
    };
    path.canonicalize().ok()
}

/// User cache homes across platforms: `$XDG_CACHE_HOME`, `~/.cache` (Linux/XDG)
/// and `~/Library/Caches` (macOS; Homebrew, pip, cocoapods, yarn, etc.).
fn cache_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
        && let Ok(c) = p.canonicalize()
    {
        dirs.push(c);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [".cache", "Library/Caches"] {
            if let Ok(c) = home.join(sub).canonicalize() {
                dirs.push(c);
            }
        }
    }
    dirs
}

fn dedup_preserving_order(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|p| seen.insert(p.clone()))
        .collect()
}

/// Normalize a path for inclusion in a profile. Canonicalizes when possible so
/// symlinks resolve, falling back to the original on failure.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Extracts the program + args from a `Command` so we can re-wrap them.
pub(crate) fn collect_argv(cmd: &Command) -> Vec<String> {
    let mut out = Vec::new();
    out.push(cmd.get_program().to_string_lossy().into_owned());
    for a in cmd.get_args() {
        out.push(a.to_string_lossy().into_owned());
    }
    out
}

#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roots_include_temp_dir_without_duplicates() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let roots = default_writable_roots();
        assert!(roots.contains(&tmp), "system temp dir must be writable");
        let mut sorted = roots.clone();
        sorted.sort();
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(sorted, deduped, "roots must not contain duplicates");
    }

    #[test]
    fn resolve_tool_home_falls_back_under_home_when_env_unset() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        let resolved = resolve_tool_home(
            Some("CRAFT_SANDBOX_DEFINITELY_UNSET_ENV_42"),
            ".cargo",
            Some(&home),
        )
        .expect("must resolve when the default subdir exists under home");
        assert_eq!(resolved, home.join(".cargo"));
    }

    #[test]
    fn resolve_tool_home_returns_none_when_path_missing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        assert!(
            resolve_tool_home(
                Some("CRAFT_SANDBOX_DEFINITELY_UNSET_ENV_43"),
                ".does-not-exist",
                Some(&home),
            )
            .is_none()
        );
    }

    #[test]
    fn dedup_preserves_first_occurrence_order() {
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let input = vec![a.clone(), b.clone(), a, b];
        assert_eq!(
            dedup_preserving_order(input),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[test]
    fn off_mode_leaves_command_unchanged() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hi");
        let mut profile = SandboxProfile::workspace_write("/tmp");
        profile.mode = SandboxMode::Off;
        apply(&mut cmd, &profile).unwrap();
        assert_eq!(cmd.get_program(), "sh");
    }

    #[test]
    fn collect_argv_preserves_program_and_args() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hi");
        let argv = collect_argv(&cmd);
        assert_eq!(argv, vec!["sh", "-c", "echo hi"]);
    }
}
