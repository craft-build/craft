//! macOS sandbox via the stock `sandbox-exec` binary. Generates a Seatbelt SBPL
//! profile at runtime that confines writes to the workspace (+ common writable
//! roots like tmp/cache) and gates network access.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::process::Command;

#[cfg(test)]
use tracing::warn;

use crate::{NetworkPolicy, SandboxError, SandboxMode, SandboxProfile, default_writable_roots, normalize};

const SANDBOX_EXEC: &str = "sandbox-exec";

/// Rewrites `command` in place to run under `sandbox-exec -p '<profile>' -- <cmd>`. A
/// generated profile is only produced for `WorkspaceWrite` and `ReadOnly` modes; all
/// other modes leave the command unwrapped.
pub fn apply(command: &mut Command, profile: &SandboxProfile) -> Result<(), SandboxError> {
    let sbpl = build_sbpl(profile);
    if sbpl.is_empty() {
        return Ok(());
    }

    let argv = collect_argv(command);
    if argv.is_empty() {
        return Err(SandboxError::Profile("empty command".into()));
    }

    let mut wrapped = Command::new(SANDBOX_EXEC);
    wrapped.arg("-p").arg(&sbpl);
    wrapped.arg("--");
    for a in &argv {
        wrapped.arg(a);
    }

    *command = wrapped;
    Ok(())
}

/// Extracts the program + args from a `Command` so we can re-wrap them.
fn collect_argv(cmd: &Command) -> Vec<String> {
    let mut out = Vec::new();
    out.push(cmd.get_program().to_string_lossy().into_owned());
    for a in cmd.get_args() {
        out.push(a.to_string_lossy().into_owned());
    }
    out
}

/// Builds the SBPL profile string. `WorkspaceWrite` denies all writes by default
/// then re-allows the workspace and common writable roots; `ReadOnly` denies all
/// writes. Network is gated by `profile.network`. `DangerFullAccess`/`Off` yield an
/// empty string (no wrapping).
pub(crate) fn build_sbpl(profile: &SandboxProfile) -> String {
    match profile.mode {
        SandboxMode::DangerFullAccess | SandboxMode::Off => return String::new(),
        SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly => {}
    }

    let mut s = String::new();
    s.push_str("(version 1)\n");
    s.push_str("(allow default)\n");
    if profile.network == NetworkPolicy::Denied {
        s.push_str("(deny network*)\n");
    }
    s.push_str("(deny file-write*)\n");

    if profile.mode == SandboxMode::WorkspaceWrite {
        let workspace = normalize(&profile.workspace);
        let mut roots = profile.writable_roots.clone();
        if roots.is_empty() {
            roots = default_writable_roots();
        }
        s.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sbpl_escape(&workspace.to_string_lossy())
        ));
        for r in &roots {
            let r = normalize(r);
            s.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                sbpl_escape(&r.to_string_lossy())
            ));
        }
    }

    s.push_str("(allow process*)\n");
    s.push_str("(allow signal (target children))\n");
    s.push_str("(allow sysctl*)\n");
    s.push_str("(allow mach-lookup)\n");
    s
}

/// Escapes backslash and double-quote so a path is safe to embed in an SBPL
/// string literal. Without this, a workspace/root path containing `"` or `\`
/// could break out of the literal and inject policy directives.
fn sbpl_escape(p: &str) -> String {
    p.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxProfile;

    #[test]
    fn sbpl_denies_network_and_writes_outside_workspace() {
        let profile = SandboxProfile::workspace_write("/Users/test/project");
        let sbpl = build_sbpl(&profile);
        assert!(sbpl.contains("(deny network*)"), "must deny network");
        assert!(
            sbpl.contains("(deny file-write*)"),
            "must deny writes by default"
        );
        assert!(
            sbpl.contains("/Users/test/project"),
            "must allow workspace writes"
        );
    }

    #[test]
    fn danger_full_access_returns_empty_profile() {
        let mut profile = SandboxProfile::workspace_write("/x");
        profile.mode = SandboxMode::DangerFullAccess;
        assert_eq!(build_sbpl(&profile), "");
    }

    #[test]
    fn apply_wraps_command_when_available() {
        if crate::which(SANDBOX_EXEC).is_none() {
            warn!("sandbox-exec not present, skipping apply test");
            return;
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hi");
        let profile = SandboxProfile::workspace_write("/tmp");
        apply(&mut cmd, &profile).unwrap();
        assert_eq!(cmd.get_program(), SANDBOX_EXEC);
    }

    #[test]
    fn network_allowed_omits_deny() {
        let mut profile = SandboxProfile::workspace_write("/Users/test/project");
        profile.network = NetworkPolicy::Allowed;
        let sbpl = build_sbpl(&profile);
        assert!(
            !sbpl.contains("(deny network*)"),
            "network must not be denied when policy allows it"
        );
    }

    #[test]
    fn read_only_denies_all_writes() {
        let mut profile = SandboxProfile::workspace_write("/Users/test/project");
        profile.mode = SandboxMode::ReadOnly;
        let sbpl = build_sbpl(&profile);
        assert!(sbpl.contains("(deny file-write*)"));
        assert!(
            !sbpl.contains("(allow file-write*"),
            "read-only profile must not re-allow any writes"
        );
    }

    #[test]
    fn sbpl_escapes_embedded_quotes() {
        let profile = SandboxProfile::workspace_write(r#"/Users/test/a"b"#);
        let sbpl = build_sbpl(&profile);
        assert!(
            sbpl.contains(r#"a\"b"#),
            "embedded quote must be escaped in the SBPL literal"
        );
    }

    #[test]
    fn collect_argv_preserves_program_and_args() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hi");
        let argv = collect_argv(&cmd);
        assert_eq!(argv, vec!["sh", "-c", "echo hi"]);
    }
}
