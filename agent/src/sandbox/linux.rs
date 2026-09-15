//! Linux sandbox via `bubblewrap` (`bwrap`). Requires `bwrap` on PATH. Wraps the
//! argv so the process gets a read-only view of the root filesystem with the
//! workspace bind-mounted read-write, and (when network is denied) an unshared
//! network namespace with only loopback. Ported from reference
//! `craft-sandbox/src/linux.rs`.

use std::process::Command;

use crate::sandbox::{
    NetworkPolicy, SandboxError, SandboxMode, SandboxProfile, collect_argv, default_writable_roots,
    normalize, which,
};

pub(crate) const BWRAP: &str = "bwrap";

/// Rewrites `command` to run under bwrap. The workspace is bind-mounted
/// read-write for `WorkspaceWrite` and read-only for `ReadOnly`.
pub fn apply(command: &mut Command, profile: &SandboxProfile) -> Result<(), SandboxError> {
    if matches!(
        profile.mode,
        SandboxMode::DangerFullAccess | SandboxMode::Off
    ) {
        return Ok(());
    }
    if which(BWRAP).is_none() {
        return Err(SandboxError::BinaryMissing { binary: BWRAP });
    }

    let argv = collect_argv(command);
    if argv.is_empty() {
        return Err(SandboxError::Profile {
            reason: "empty command".into(),
        });
    }

    let mut wrapped = Command::new(BWRAP);
    let workspace = normalize(&profile.workspace);
    let read_only = profile.mode == SandboxMode::ReadOnly;

    if profile.network == NetworkPolicy::Denied {
        wrapped.arg("--unshare-net");
    }

    // Read-only base first — later --bind mounts override it for their paths.
    wrapped
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

    if !read_only {
        let ws = normalize(&workspace);
        wrapped.arg("--bind").arg(&ws).arg(&ws);
        let mut roots = profile.writable_roots.clone();
        if roots.is_empty() {
            roots = default_writable_roots();
        }
        for r in &roots {
            let r = normalize(r);
            wrapped.arg("--bind").arg(&r).arg(&r);
        }
    } else {
        wrapped.arg("--ro-bind").arg(&workspace).arg(&workspace);
    }

    wrapped.arg("--");
    for a in &argv {
        wrapped.arg(a);
    }

    *command = wrapped;
    Ok(())
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
        assert!(matches!(err, SandboxError::BinaryMissing { .. }));
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
    fn read_only_uses_ro_bind_for_workspace() {
        if which(BWRAP).is_none() {
            // bwrap not present, skipping read-only test
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
            // bwrap not present, skipping wiring test
            return;
        }
        let mut cmd = Command::new("echo");
        let mut profile = SandboxProfile::workspace_write("/tmp");
        profile.network = NetworkPolicy::Denied;
        apply(&mut cmd, &profile).unwrap();
        assert_eq!(cmd.get_program(), BWRAP);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(
            args.windows(2).any(|w| w[0] == "--dev" && w[1] == "/dev"),
            "a fresh /dev must be mounted so /dev/null stays writable despite the read-only root"
        );
    }

    #[test]
    fn ro_bind_root_precedes_writable_binds() {
        if which(BWRAP).is_none() {
            // bwrap not present, skipping ordering test
            return;
        }
        let mut cmd = Command::new("echo");
        let profile = SandboxProfile::workspace_write("/tmp/craft-order-test");
        apply(&mut cmd, &profile).unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let ro_root = args
            .iter()
            .position(|a| a == "--ro-bind")
            .and_then(|i| args.get(i + 1).filter(|v| *v == "/"));
        let ws_bind = args
            .iter()
            .position(|a| a == "--bind")
            .and_then(|i| args.get(i + 1).filter(|v| *v == "/tmp/craft-order-test"));
        assert!(
            ro_root.is_some() && ws_bind.is_some() && ro_root.unwrap() < ws_bind.unwrap(),
            "--ro-bind / must come before --bind <workspace> so writable mounts override the read-only root"
        );
    }
}
