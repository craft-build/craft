//! macOS sandbox via the stock `sandbox-exec` binary. Generates a Seatbelt SBPL
//! profile at runtime that confines writes to the workspace (+ common writable
//! roots like tmp/cache) and gates network access. Ported from reference
//! `craft-sandbox/src/mac.rs`.

use std::process::Command;

use crate::sandbox::{
    NetworkPolicy, SandboxError, SandboxMode, SandboxProfile, collect_argv, default_writable_roots,
    normalize,
};

pub(crate) const SANDBOX_EXEC: &str = "sandbox-exec";

/// Rewrites `command` in place to run under `sandbox-exec -p '<profile>' -- <cmd>`.
/// A generated profile is only produced for `WorkspaceWrite` and `ReadOnly`
/// modes; all other modes leave the command unwrapped.
pub fn apply(command: &mut Command, profile: &SandboxProfile) -> Result<(), SandboxError> {
    let sbpl = build_sbpl(profile);
    if sbpl.is_empty() {
        return Ok(());
    }

    let argv = collect_argv(command);
    if argv.is_empty() {
        return Err(SandboxError::Profile {
            reason: "empty command".into(),
        });
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

/// Builds the SBPL profile string. `WorkspaceWrite` denies all writes by default
/// then re-allows the workspace and common writable roots; `ReadOnly` denies all
/// writes. Network is gated by `profile.network`. `DangerFullAccess`/`Off` yield
/// an empty string (no wrapping).
pub(crate) fn build_sbpl(profile: &SandboxProfile) -> String {
    if matches!(
        profile.mode,
        SandboxMode::DangerFullAccess | SandboxMode::Off
    ) {
        return String::new();
    }

    let mut s = String::new();
    s.push_str("(version 1)\n");
    s.push_str("(allow default)\n");
    if profile.network == NetworkPolicy::Denied {
        s.push_str("(deny network*)\n");
    }
    s.push_str("(deny file-write*)\n");
    s.push_str("(allow file-write* (literal \"/dev/null\"))\n");

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
    use crate::sandbox::SandboxProfile;

    #[test]
    fn sbpl_denies_network_and_writes_outside_workspace() {
        let mut profile = SandboxProfile::workspace_write("/Users/test/project");
        profile.network = NetworkPolicy::Denied;
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
        if crate::sandbox::which(SANDBOX_EXEC).is_none() {
            // sandbox-exec not present, skipping apply test
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
    fn sbpl_allows_dev_null() {
        let profile = SandboxProfile::workspace_write("/Users/test/project");
        let sbpl = build_sbpl(&profile);
        assert!(
            sbpl.contains("(allow file-write* (literal \"/dev/null\"))"),
            "profile must allow writes to /dev/null"
        );
    }

    #[test]
    fn read_only_denies_workspace_writes() {
        let mut profile = SandboxProfile::workspace_write("/Users/test/project");
        profile.mode = SandboxMode::ReadOnly;
        let sbpl = build_sbpl(&profile);
        assert!(sbpl.contains("(deny file-write*)"));
        assert!(
            !sbpl.contains("/Users/test/project"),
            "read-only profile must not allow writes to the workspace"
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
    fn sbpl_escapes_embedded_backslash() {
        let profile = SandboxProfile::workspace_write(r"/Users/test/a\b");
        let sbpl = build_sbpl(&profile);
        assert!(
            sbpl.contains(r"a\\b"),
            "embedded backslash must be escaped in the SBPL literal"
        );
    }

    #[test]
    fn custom_writable_roots_override_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let mut profile = SandboxProfile::workspace_write("/Users/test/project");
        profile.writable_roots = vec![dir.path().to_path_buf()];
        let sbpl = build_sbpl(&profile);
        let root = normalize(dir.path());
        assert!(sbpl.contains(&format!("(subpath \"{}\"))", root.display())));
        assert!(
            !sbpl.contains("(subpath \"/tmp\"))"),
            "default roots must not be granted when custom roots are set"
        );
    }

    #[test]
    fn sandboxed_process_cannot_write_outside_workspace() {
        if crate::sandbox::which(SANDBOX_EXEC).is_none() {
            // sandbox-exec not present, skipping enforcement test
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Target the HOME dir, which is not among the default writable roots.
        let outside = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .expect("HOME required")
            .join(".craft_sb_probe_outside");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!(
            "echo x > {} 2>/dev/null; echo blocked_rc=$?; echo ok > {}/inside 2>/dev/null; echo inside_rc=$?",
            outside.display(),
            dir.path().display(),
        ));
        let profile = SandboxProfile::workspace_write(dir.path());
        apply(&mut cmd, &profile).unwrap();
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let _ = std::fs::remove_file(&outside); // cleanup if enforcement failed
        if stderr.contains("sandbox_apply: Operation not permitted") {
            // Nested sandboxing is blocked (we are already inside a sandbox);
            // profile construction is covered by the build_sbpl tests.
            return;
        }
        assert!(
            stdout.contains("blocked_rc=1"),
            "write outside the workspace must fail under the sandbox, got stdout: {stdout:?} stderr: {stderr:?}"
        );
        assert!(
            stdout.contains("inside_rc=0"),
            "write inside the workspace must succeed under the sandbox, got: {stdout}"
        );
    }
}
