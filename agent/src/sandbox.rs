//! OS-level command sandboxing. An enforcement layer that sits *under* the
//! logical permission manager: once a command is approved, the sandbox confines
//! it to the workspace (with network optionally gated) regardless of its
//! contents. Ported from reference `craft-sandbox/`.
//!
//! Policy resolution ([`SandboxPolicy::resolve`]): `[sandbox]` config, `--yolo`
//! ⇒ off, `CRAFT_SANDBOX=off` env escape (highest precedence). Enforcement
//! ([`enforce`]) is fail-closed on supported platforms — a missing backend
//! binary is an error, not a silent pass-through — and degrades to unsandboxed
//! **with a visible note** on platforms that have no implementation.
//!
//! Platform support:
//! - **macOS**: shells out to the stock `sandbox-exec` binary with a generated
//!   SBPL profile (`workspace_write` = write the workspace + system temp/cache,
//!   read the rest; `read_only` = read everything, write nothing except
//!   explicit `writable_roots` grants such as the plan file).
//! - **Linux**: wraps the argv with `bubblewrap` (`bwrap`) when present (documented
//!   prerequisite). Network is gated via `--unshare-net`; the workspace is
//!   bind-mounted read-write while the rest of the filesystem is read-only.
//! - **Other platforms**: unsandboxed with a warning ([`SandboxOutcome::Unavailable`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};

use snafu::Snafu;

#[derive(Debug, Snafu)]
pub enum SandboxError {
    #[snafu(display("sandbox binary '{binary}' not found on PATH"))]
    BinaryMissing { binary: &'static str },

    /// Fail-closed: the platform requires this backend, so refusing to run
    /// unsandboxed is safer than silently dropping confinement.
    #[snafu(display(
        "sandbox backend '{binary}' is required on this platform but not installed; \
         install it or opt out via sandbox.mode = \"off\" (or CRAFT_SANDBOX=off)"
    ))]
    RequiredBackendMissing { binary: &'static str },

    #[snafu(display("sandbox profile generation failed: {reason}"))]
    Profile { reason: String },
}

/// Confinement policy. `Off` disables the sandbox entirely (the `/yolo` path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Resolved, session-scoped sandbox policy: mode + network + explicit
/// per-turn write grants (e.g. the plan file under `ReadOnly`). This is the
/// configurable decision layer; [`SandboxProfile`] is the per-command
/// enforcement input built from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub network: NetworkPolicy,
    /// Exact-path write grants honored in every mode (used to keep the
    /// allocated plan file writable under `ReadOnly`).
    pub writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    pub fn new(mode: SandboxMode, network: NetworkPolicy) -> Self {
        Self {
            mode,
            network,
            writable_roots: Vec::new(),
        }
    }

    pub fn off() -> Self {
        Self::new(SandboxMode::Off, NetworkPolicy::Allowed)
    }

    /// Resolve from config. Precedence: `CRAFT_SANDBOX=off` env (documented
    /// debug/CI escape) > `--yolo` > config. `!enabled`, `mode = "off"`, and
    /// `danger_full_access` all resolve to `Off` (no wrapping).
    pub fn resolve(config: &crate::config::Config, yolo: bool) -> Self {
        Self::resolve_with(std::env::var_os("CRAFT_SANDBOX"), config, yolo)
    }

    /// Test seam for [`resolve`]: the env value is injected so precedence
    /// tests never mutate process-wide state.
    pub fn resolve_with(
        env: Option<std::ffi::OsString>,
        config: &crate::config::Config,
        yolo: bool,
    ) -> Self {
        if env.is_some_and(|v| v == "off") {
            return Self::off();
        }
        if yolo {
            return Self::off();
        }
        let sc = &config.sandbox;
        if !sc.enabled || sc.mode.is_off() || sc.mode == SandboxMode::DangerFullAccess {
            return Self::off();
        }
        Self {
            mode: sc.mode,
            network: match sc.network {
                true => NetworkPolicy::Allowed,
                false => NetworkPolicy::Denied,
            },
            writable_roots: Vec::new(),
        }
    }

    /// Whether this policy demands confinement (i.e. a backend is required).
    pub fn enforced(&self) -> bool {
        matches!(
            self.mode,
            SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly
        )
    }
}

/// Effective result of an enforcement decision, surfaced to the caller so
/// degradation is visible instead of silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxOutcome {
    Applied,
    /// Deliberately not sandboxed (yolo / config `off` / danger mode).
    Disabled {
        reason: String,
    },
    /// No backend can exist here; commands run unsandboxed with a warning.
    Unavailable {
        reason: String,
    },
}

/// True on platforms that have a sandbox backend implementation.
pub fn platform_supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

/// Availability of the platform's sandbox backend, so the enforcement
/// decision (fail-closed vs degrade) can be made — and tested — explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendStatus {
    Available,
    /// The backend binary for a supported platform is not on PATH.
    BinaryMissing(&'static str),
    /// No sandbox implementation exists for this OS.
    UnsupportedPlatform,
}

impl BackendStatus {
    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            if which(mac::SANDBOX_EXEC).is_some() {
                Self::Available
            } else {
                Self::BinaryMissing(mac::SANDBOX_EXEC)
            }
        }
        #[cfg(target_os = "linux")]
        {
            if which(linux::BWRAP).is_some() {
                Self::Available
            } else {
                Self::BinaryMissing(linux::BWRAP)
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            Self::UnsupportedPlatform
        }
    }
}

/// Decide and apply sandboxing for one command. Pure decision + command
/// rewrite: the backend status is injected so tests never touch PATH.
///
/// - Non-enforcing profile (`Off` / `DangerFullAccess`) ⇒ `Disabled`.
/// - Unsupported platform ⇒ `Unavailable` (visible degradation, runs on).
/// - Missing backend on a supported platform ⇒ **Err**: the command must not
///   run unsandboxed when confinement was requested.
/// - Available ⇒ wrap and report `Applied`.
pub fn enforce(
    command: &mut Command,
    profile: &SandboxProfile,
    status: &BackendStatus,
) -> Result<SandboxOutcome, SandboxError> {
    if !matches!(
        profile.mode,
        SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly
    ) {
        return Ok(SandboxOutcome::Disabled {
            reason: format!("sandbox mode {:?} does not confine commands", profile.mode),
        });
    }
    match status {
        BackendStatus::UnsupportedPlatform => Ok(SandboxOutcome::Unavailable {
            reason: format!("sandboxing is unsupported on {}", std::env::consts::OS),
        }),
        BackendStatus::BinaryMissing(binary) => {
            Err(SandboxError::RequiredBackendMissing { binary })
        }
        BackendStatus::Available => {
            apply(command, profile)?;
            Ok(SandboxOutcome::Applied)
        }
    }
}

/// Shared session state: the resolved policy plus the one-time
/// "running unsandboxed" note flag. Frozen per turn by cloning the cell
/// into the tool table (like every other per-turn decision).
#[derive(Debug)]
pub struct SandboxState {
    pub policy: SandboxPolicy,
    pub note_shown: bool,
}

impl Default for SandboxState {
    fn default() -> Self {
        Self {
            // Env-aware default: `CRAFT_SANDBOX=off` honored without config.
            policy: SandboxPolicy::resolve(&crate::config::Config::default(), false),
            note_shown: false,
        }
    }
}

pub type SandboxPolicyCell = Arc<RwLock<SandboxState>>;

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

/// True iff the backing binary for this platform is available. Prefer
/// [`BackendStatus::detect`] + [`enforce`] at enforcement sites so a missing
/// backend on a supported platform fails closed instead of passing through.
pub fn available() -> bool {
    matches!(BackendStatus::detect(), BackendStatus::Available)
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

    // --- policy resolution + enforcement ---

    fn test_config(sandbox: crate::config::SandboxConfig) -> crate::config::Config {
        crate::config::Config {
            sandbox,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_defaults_to_workspace_write_with_network() {
        let policy = SandboxPolicy::resolve_with(None, &test_config(Default::default()), false);
        assert_eq!(policy.mode, SandboxMode::WorkspaceWrite);
        assert_eq!(policy.network, NetworkPolicy::Allowed);
    }

    #[test]
    fn resolve_config_overrides_and_opt_outs() {
        let read_only = crate::config::SandboxConfig {
            mode: SandboxMode::ReadOnly,
            network: false,
            ..Default::default()
        };
        let policy = SandboxPolicy::resolve_with(None, &test_config(read_only), false);
        assert_eq!(policy.mode, SandboxMode::ReadOnly);
        assert_eq!(policy.network, NetworkPolicy::Denied);

        for sc in [
            crate::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            crate::config::SandboxConfig {
                mode: SandboxMode::Off,
                ..Default::default()
            },
            crate::config::SandboxConfig {
                mode: SandboxMode::DangerFullAccess,
                ..Default::default()
            },
        ] {
            assert_eq!(
                SandboxPolicy::resolve(&test_config(sc), false).mode,
                SandboxMode::Off,
                "expected opt-out config {sc:?} to resolve to Off"
            );
        }

        // yolo beats an enforcing config.
        assert_eq!(
            SandboxPolicy::resolve_with(None, &test_config(Default::default()), true).mode,
            SandboxMode::Off
        );
    }

    #[test]
    fn resolve_env_escape_wins_over_everything() {
        let env = Some(std::ffi::OsString::from("off"));
        let policy = SandboxPolicy::resolve_with(env, &test_config(Default::default()), true);
        assert_eq!(policy.mode, SandboxMode::Off);
        // Any other env value does not disable.
        let policy = SandboxPolicy::resolve_with(
            Some(std::ffi::OsString::from("1")),
            &test_config(Default::default()),
            false,
        );
        assert_eq!(policy.mode, SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn enforce_missing_required_backend_fails_closed() {
        let mut cmd = Command::new("bash");
        let profile = SandboxProfile::workspace_write("/tmp");
        let err = enforce(&mut cmd, &profile, &BackendStatus::BinaryMissing("bwrap")).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("bwrap"), "{message}");
        assert!(message.contains("sandbox.mode"), "{message}");
        // The command must be left unwrapped: nothing spawned.
        assert_eq!(cmd.get_program(), "bash");
    }

    #[test]
    fn enforce_unsupported_platform_degrades_visibly() {
        let mut cmd = Command::new("bash");
        let profile = SandboxProfile::workspace_write("/tmp");
        let outcome = enforce(&mut cmd, &profile, &BackendStatus::UnsupportedPlatform).unwrap();
        assert!(matches!(outcome, SandboxOutcome::Unavailable { .. }));
        assert_eq!(cmd.get_program(), "bash");
    }

    #[test]
    fn enforce_off_policy_is_disabled() {
        let mut cmd = Command::new("bash");
        let mut profile = SandboxProfile::workspace_write("/tmp");
        profile.mode = SandboxMode::Off;
        let outcome = enforce(&mut cmd, &profile, &BackendStatus::BinaryMissing("bwrap")).unwrap();
        assert!(matches!(outcome, SandboxOutcome::Disabled { .. }));
        assert_eq!(cmd.get_program(), "bash");
    }

    #[test]
    fn platform_supported_matches_status_fallback() {
        // On supported platforms detect() never reports UnsupportedPlatform.
        if platform_supported() {
            assert_ne!(BackendStatus::detect(), BackendStatus::UnsupportedPlatform);
        }
    }
}
