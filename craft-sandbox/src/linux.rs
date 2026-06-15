//! Linux sandbox via `bubblewrap` (`bwrap`). Requires `bwrap` on PATH. Wraps the
//! argv so the process gets a read-only view of the root filesystem with the
//! workspace bind-mounted read-write, and (when network is denied) an unshared
//! network namespace with only loopback.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::process::Command;

#[cfg(test)]
use tracing::warn;

use crate::{NetworkPolicy, SandboxError, SandboxMode, SandboxProfile, default_writable_roots, normalize, which};

const BWRAP: &str = "bwrap";

/// Rewrites `command` to run under bwrap. The workspace is bind-mounted
/// read-write for `WorkspaceWrite` and read-only for `ReadOnly`.
pub fn apply(command: &mut Command, profile: &SandboxProfile) -> Result<(), SandboxError> {
    if matches!(profile.mode, SandboxMode::DangerFullAccess | SandboxMode::Off) {
        return Ok(());
    }
    if which(BWRAP).is_none() {
        return Err(SandboxError::BinaryMissing(BWRAP));
    }

    let argv = collect_argv(command);
    if argv.is_empty() {
        return Err(SandboxError::Profile("empty command".into()));
    }

    let mut wrapped = Command::new(BWRAP);
    let workspace = normalize(&profile.workspace);
    let read_only = profile.mode == SandboxMode::ReadOnly;

    if profile.network == NetworkPolicy::Denied {
        wrapped.arg("--unshare-net");
    }

    if !read_only {
        let mut roots = profile.writable_roots.clone();
        if roots.is_empty() {
            roots = default_writable_roots();
        }
        for r in &roots {
            let r = normalize(r);
            wrapped.arg("--bind").arg(&r).arg(&r);
        }
    }

    let ws_flag = if read_only { "--ro-bind" } else { "--bind" };
    wrapped
        .arg(ws_flag)
        .arg(&workspace)
        .arg(&workspace)
        .arg("--ro-bind")
        .arg("/")
        .arg("/")
        .arg("--dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--ro-bind")
        .arg("/run")
        .arg("/run");

    wrapped.arg("--");
    for a in &argv {
        wrapped.arg(a);
    }

    *command = wrapped;
    Ok(())
}

fn collect_argv(cmd: &Command) -> Vec<String> {
    let mut out = Vec::new();
    out.push(cmd.get_program().to_string_lossy().into_owned());
    for a in cmd.get_args() {
        out.push(a.to_string_lossy().into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_skips_when_bwrap_missing() {
        if which(BWRAP).is_some() {
            return;
        }
        let mut cmd = Command::new("echo");
        let profile = SandboxProfile::workspace_write("/tmp");
        let err = apply(&mut cmd, &profile).unwrap_err();
        assert!(matches!(err, SandboxError::BinaryMissing(BWRAP)));
    }

    #[test]
    fn apply_skips_for_full_access() {
        let mut cmd = Command::new("echo");
        let mut profile = SandboxProfile::workspace_write("/tmp");
        profile.mode = SandboxMode::DangerFullAccess;
        apply(&mut cmd, &profile).unwrap();
        assert_eq!(cmd.get_program(), "echo");
    }

    #[test]
    fn collect_argv_preserves_program_and_args() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hi");
        let argv = collect_argv(&cmd);
        assert_eq!(argv, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn read_only_uses_ro_bind_for_workspace() {
        if which(BWRAP).is_none() {
            warn!("bwrap not present, skipping read-only test");
            return;
        }
        let mut cmd = Command::new("echo");
        let mut profile = SandboxProfile::workspace_write("/tmp/craft-sandbox-ro-test");
        profile.mode = SandboxMode::ReadOnly;
        apply(&mut cmd, &profile).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "--bind" && w[1] == "/tmp/craft-sandbox-ro-test"),
            "read-only mode must not writable-bind the workspace"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--ro-bind" && w[1] == "/tmp/craft-sandbox-ro-test"),
            "read-only mode must ro-bind the workspace"
        );
    }

    #[test]
    fn apply_wires_bwrap_when_present() {
        if which(BWRAP).is_none() {
            warn!("bwrap not present, skipping wiring test");
            return;
        }
        let mut cmd = Command::new("echo");
        let profile = SandboxProfile::workspace_write("/tmp");
        apply(&mut cmd, &profile).unwrap();
        assert_eq!(cmd.get_program(), BWRAP);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--unshare-net".to_string()));
    }
}
