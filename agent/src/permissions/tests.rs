//! Tests for the permission rule engine.

use super::rules::{PermissionsFileConfig, build_permissions, read_permissions_file};
use super::*;

fn allow_rule(tool: &str, scope: Option<&str>) -> PermissionRule {
    PermissionRule {
        tool: ToolKey::native(tool),
        scope: scope.map(str::to_string),
        effect: Effect::Allow,
    }
}

fn deny_rule(tool: &str, scope: Option<&str>) -> PermissionRule {
    PermissionRule {
        tool: ToolKey::native(tool),
        scope: scope.map(str::to_string),
        effect: Effect::Deny,
    }
}

fn mgr_with(cwd: &Path, rules: Vec<PermissionRule>) -> PermissionManager {
    PermissionManager::new(
        PermissionsConfig {
            rules,
            ..Default::default()
        },
        cwd.to_path_buf(),
    )
}

fn needs_prompt(check: &PermissionCheck) -> bool {
    matches!(check, PermissionCheck::NeedsPrompt { .. })
}

#[test]
fn auto_review_toggles_and_forks() {
    let mgr = mgr_with(std::env::temp_dir().as_ref(), Vec::new());
    assert!(!mgr.is_auto_review());
    assert!(mgr.toggle_auto_review());
    assert!(mgr.is_auto_review());
    assert!(!mgr.toggle_auto_review());
    assert!(!mgr.is_auto_review());
    mgr.toggle_auto_review();
    assert!(mgr.fork().is_auto_review(), "fork carries the mode");
}

#[test]
fn apply_auto_review_allow_records_allow_rule() {
    let mgr = mgr_with(std::env::temp_dir().as_ref(), Vec::new());
    let tool = ToolKey::native("write");
    let scopes = vec!["/tmp/a".to_string()];
    assert!(mgr.apply_auto_review(&tool, &scopes, true));
    assert!(
        matches!(mgr.check(&tool, &scopes), PermissionCheck::Allowed),
        "an allow decision must not prompt again"
    );
}

#[test]
fn apply_auto_review_deny_blocks_later_calls() {
    let mgr = mgr_with(std::env::temp_dir().as_ref(), Vec::new());
    let tool = ToolKey::native("bash");
    let scopes = vec!["rm -rf /".to_string()];
    assert!(!mgr.apply_auto_review(&tool, &scopes, false));
    assert!(matches!(mgr.check(&tool, &scopes), PermissionCheck::Denied));
}

#[test]
fn check_multi_force_prompt_skips_allow_rules() {
    let mgr = mgr_with(
        Path::new("/tmp"),
        vec![
            allow_rule("bash", Some("cargo *")),
            allow_rule("bash", Some("git *")),
        ],
    );
    let tool = ToolKey::native("bash");
    let scopes = vec!["cargo test".to_string(), "git push".to_string()];
    assert!(matches!(
        mgr.check_multi(&tool, &scopes, false),
        PermissionCheck::Allowed
    ));
    match mgr.check_multi(&tool, &scopes, true) {
        PermissionCheck::NeedsPrompt {
            scopes: s,
            force_prompt,
            ..
        } => {
            assert_eq!(s, vec!["cargo test", "git push"]);
            assert!(force_prompt);
        }
        other => panic!("expected NeedsPrompt, got {other:?}"),
    }
}

#[test]
fn check_multi_deny_wins_over_force_prompt() {
    let mgr = mgr_with(Path::new("/tmp"), vec![deny_rule("bash", Some("rm *"))]);
    assert!(matches!(
        mgr.check_multi(&ToolKey::native("bash"), &["rm -rf /".to_string()], true),
        PermissionCheck::Denied
    ));
}

#[test]
fn check_multi_partial_coverage_prompts_uncovered() {
    let mgr = mgr_with(Path::new("/tmp"), vec![allow_rule("bash", Some("cargo *"))]);
    match mgr.check_multi(
        &ToolKey::native("bash"),
        &[
            "cargo test".to_string(),
            "git push".to_string(),
            "ls".to_string(),
        ],
        false,
    ) {
        PermissionCheck::NeedsPrompt { scopes, .. } => {
            assert_eq!(scopes, vec!["git push", "ls"]);
        }
        other => panic!("expected NeedsPrompt, got {other:?}"),
    }
}

#[test]
fn scope_matches_prefix_token_boundary() {
    assert!(scope_matches("git *", "git diff"));
    assert!(scope_matches("git *", "git"));
    assert!(!scope_matches("pwd *", "pwdx /"));
    assert!(scope_matches("prefix*", "prefix-thing"));
    assert!(scope_matches("exact", "exact"));
    assert!(!scope_matches("exact", "exactly"));
}

#[test]
fn universal_scopes_match_everything() {
    for pattern in ["*", "**", "/*", "/**"] {
        assert!(is_universal_scope(pattern), "{pattern}");
        assert!(scope_matches(pattern, "/anything/at/all"), "{pattern}");
        assert!(scope_matches(pattern, "git push"), "{pattern}");
    }
    assert!(!is_universal_scope("/tmp/**"));
    assert!(scope_matches("/tmp/**", "/tmp/a/b"));
    assert!(!scope_matches("/tmp/**", "/var/tmp"));
}

#[test]
fn dir_glob_matches_before_dir_exists() {
    // Absolutization happens before symlink resolution, so a rule for a
    // directory that does not exist yet still matches paths under it.
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("dist");
    assert!(scope_matches(
        &format!("{}/**", base.display()),
        &base.join("foo.txt").display().to_string()
    ));
}

#[test]
fn deny_beats_allow_and_session_beats_default() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = mgr_with(
        tmp.path(),
        vec![
            allow_rule("bash", Some("git *")),
            deny_rule("bash", Some("git push *")),
        ],
    );
    let tool = ToolKey::native("bash");
    assert!(matches!(
        mgr.check(&tool, &["git diff".to_string()]),
        PermissionCheck::Allowed
    ));
    assert!(matches!(
        mgr.check(&tool, &["git push origin main".to_string()]),
        PermissionCheck::Denied
    ));

    // Unknown scope falls to the default (prompt).
    assert!(needs_prompt(&mgr.check(&tool, &["rm -rf /".to_string()])));
}

#[test]
fn tool_default_changes_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = PermissionManager::new(
        PermissionsConfig {
            tool_defaults: HashMap::from([(ToolKey::native("glob"), DefaultEffect::Allow)]),
            ..Default::default()
        },
        tmp.path().to_path_buf(),
    );
    let tool = ToolKey::native("glob");
    assert!(matches!(
        mgr.check(&tool, &["**/*.rs".to_string()]),
        PermissionCheck::Allowed
    ));
}

#[test]
fn session_rules_allow_without_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = mgr_with(tmp.path(), vec![]);
    let tool = ToolKey::native("write");
    let scopes = vec!["src/lib.rs".to_string()];
    assert!(needs_prompt(&mgr.check(&tool, &scopes)));

    mgr.apply_decision(&tool, &scopes, &PermissionAnswer::AllowSession);
    assert!(matches!(
        mgr.check(&tool, &scopes),
        PermissionCheck::Allowed
    ));

    // A different directory still prompts: the session grant generalized
    // to the parent dir (`src/**`), not to every file.
    assert!(needs_prompt(
        &mgr.check(&tool, &["docs/other.rs".to_string()])
    ));
}

#[test]
fn apply_decision_persists_always_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = mgr_with(tmp.path(), vec![]);
    let tool = ToolKey::native("write");
    let scopes = vec!["/abs/proj/src/lib.rs".to_string()];

    let persist = mgr.apply_decision(&tool, &scopes, &PermissionAnswer::AllowAlwaysGlobal);
    assert_eq!(persist.len(), 1);
    assert_eq!(persist[0].0, tool);
    assert_eq!(persist[0].2, Effect::Allow);
    assert!(matches!(persist[0].3, PermissionTarget::Global));

    // The generalization for write tools is the parent dir glob.
    assert_eq!(persist[0].1.as_deref(), Some("/abs/proj/src/**"));

    let deny = mgr.apply_decision(&tool, &scopes, &PermissionAnswer::DenyAlwaysLocal);
    assert_eq!(deny.len(), 1);
    assert!(matches!(deny[0].3, PermissionTarget::Project(_)));
    assert_eq!(deny[0].2, Effect::Deny);

    let once = mgr.apply_decision(&tool, &scopes, &PermissionAnswer::AllowOnce);
    assert!(once.is_empty());
}

#[test]
fn answer_encode_decode_roundtrip() {
    let answers = [
        PermissionAnswer::AllowOnce,
        PermissionAnswer::AllowSession,
        PermissionAnswer::AllowAlwaysLocal,
        PermissionAnswer::AllowAlwaysGlobal,
        PermissionAnswer::Deny,
        PermissionAnswer::DenyWithGuidance("use git instead".into()),
        PermissionAnswer::DenyAlwaysLocal,
        PermissionAnswer::DenyAlwaysGlobal,
    ];
    for a in &answers {
        assert_eq!(PermissionAnswer::decode(&a.encode()).as_ref(), Some(a));
    }
    assert_eq!(PermissionAnswer::decode("nope"), None);
    assert_eq!(
        PermissionAnswer::decode("deny:"),
        Some(PermissionAnswer::Deny)
    );
}

#[test]
fn bash_generalization_respects_shell_keywords() {
    let tool = ToolKey::native("bash");
    assert_eq!(
        generalized_scopes(&tool, &["git push origin main".to_string()]),
        vec!["git *".to_string()]
    );
    // Shell keywords stay literal: no blank cheque over every loop body.
    assert_eq!(
        generalized_scopes(&tool, &["for f in *.rs; do wc -l $f; done".to_string()]),
        vec!["for f in *.rs; do wc -l $f; done".to_string()]
    );
    // A command word with odd-but-legal chars still generalizes.
    assert_eq!(
        generalized_scopes(&tool, &["cargo-nextest run".to_string()]),
        vec!["cargo-nextest *".to_string()]
    );
}

#[test]
fn boundary_check_detects_escapes_and_unresolvable_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let inside = tmp.path().join("src/lib.rs");
    assert_eq!(physical_boundary_check(tmp.path(), &inside), Some(true));
    let outside = std::env::temp_dir().join("elsewhere.txt");
    assert_eq!(physical_boundary_check(tmp.path(), &outside), Some(false));

    // An empty parent path cannot be resolved: the boundary is
    // unverifiable, which is the hard-block case.
    assert_eq!(physical_boundary_check(Path::new(""), &inside), None);

    // Resolvable boundaries produce a verdict rather than a block; an
    // outside-cwd path flows through the normal prompt instead.
    let mgr = mgr_with(tmp.path(), vec![]);
    assert_eq!(mgr.boundary_block_reason(&inside), None);
    assert_eq!(mgr.boundary_block_reason(&outside), None);
}

#[test]
fn write_back_preserves_comments_and_appends_unique() {
    let tmp = tempfile::tempdir().unwrap();
    let project = PermissionTarget::Project(tmp.path().to_path_buf());
    let path = tmp.path().join(PROJECT_DIR).join(PERMISSIONS_FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "# my carefully written comment\n[write]\nallow = [\n    \"src/**\",\n]\n",
    )
    .unwrap();

    append_permission_rule(
        &ToolKey::native("write"),
        Some("docs/**"),
        Effect::Allow,
        &project,
    )
    .unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("# my carefully written comment"),
        "{content}"
    );
    assert!(content.contains("\"docs/**\""), "{content}");

    // Appending an existing scope is a no-op.
    append_permission_rule(
        &ToolKey::native("write"),
        Some("src/**"),
        Effect::Allow,
        &project,
    )
    .unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(content.matches("\"src/**\"").count(), 1, "{content}");
}

#[test]
fn write_back_creates_missing_file_and_denies() {
    let tmp = tempfile::tempdir().unwrap();
    append_permission_rule(
        &ToolKey::native("bash"),
        Some("git *"),
        Effect::Deny,
        &PermissionTarget::Project(tmp.path().to_path_buf()),
    )
    .unwrap();
    let content =
        std::fs::read_to_string(tmp.path().join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap();
    assert!(content.contains("[bash]"), "{content}");
    assert!(content.contains("deny = [\n    \"git *\",\n]"), "{content}");
}

#[test]
fn write_back_never_writes_wildcard() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        append_permission_rule(
            &ToolKey::Wildcard,
            None,
            Effect::Allow,
            &PermissionTarget::Project(tmp.path().to_path_buf()),
        ),
        Err(PermissionWriteError::NotATable { .. })
    ));
}

#[test]
fn load_permissions_reads_global_and_project_files() {
    let tmp = tempfile::tempdir().unwrap();
    let craft_dir = tmp.path().join(PROJECT_DIR);
    std::fs::create_dir_all(&craft_dir).unwrap();
    std::fs::write(
        craft_dir.join(PERMISSIONS_FILE),
        "[bash]\ndeny = [\"rm *\"]\n\n[write]\nallow = true\n",
    )
    .unwrap();

    let config = load_permissions_inner_for_test(tmp.path());
    let bash = ToolKey::native("bash");
    let mgr = PermissionManager::new(config, tmp.path().to_path_buf());
    assert!(matches!(
        mgr.check(&bash, &["rm -rf /tmp/x".to_string()]),
        PermissionCheck::Denied
    ));
    let write = ToolKey::native("write");
    assert!(matches!(
        mgr.check(&write, &["anything.rs".to_string()]),
        PermissionCheck::Allowed
    ));
}

/// Test seam: project file only (global dirs come from the environment).
fn load_permissions_inner_for_test(cwd: &Path) -> PermissionsConfig {
    let project =
        read_permissions_file(&cwd.join(PROJECT_DIR).join(PERMISSIONS_FILE)).unwrap_or_default();
    build_permissions(PermissionsFileConfig::default(), project)
}

#[test]
fn missing_permissions_file_is_empty_config() {
    let tmp = tempfile::tempdir().unwrap();
    let config = load_permissions_inner_for_test(tmp.path());
    assert!(config.rules.is_empty());
    assert_eq!(config.default, DefaultEffect::Prompt);
}

#[test]
fn corrupt_permissions_file_is_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let craft_dir = tmp.path().join(PROJECT_DIR);
    std::fs::create_dir_all(&craft_dir).unwrap();
    std::fs::write(craft_dir.join(PERMISSIONS_FILE), "not [ valid toml").unwrap();
    let config = load_permissions_inner_for_test(tmp.path());
    assert!(config.rules.is_empty());
}

#[test]
fn fork_starts_with_empty_session_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = mgr_with(tmp.path(), vec![allow_rule("write", None)]);
    mgr.add_session_rule(allow_rule("write", Some("a.rs")));
    let forked = mgr.fork();
    assert!(forked.session_rules_snapshot().is_empty());
    // Config rules survive the fork.
    let tool = ToolKey::native("write");
    assert!(matches!(
        forked.check(&tool, &["a.rs".to_string()]),
        PermissionCheck::Allowed
    ));
}

#[test]
fn mcp_rules_match_by_server_and_tool() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = PermissionManager::new(
        PermissionsConfig {
            rules: vec![PermissionRule {
                tool: ToolKey::McpServer {
                    server: "github".into(),
                },
                scope: None,
                effect: Effect::Allow,
            }],
            ..Default::default()
        },
        tmp.path().to_path_buf(),
    );
    let tool = ToolKey::McpTool {
        server: "github".into(),
        tool: "create_issue".into(),
    };
    assert!(matches!(
        mgr.check(&tool, &["{}".to_string()]),
        PermissionCheck::Allowed
    ));
    let other = ToolKey::McpTool {
        server: "gitlab".into(),
        tool: "create_issue".into(),
    };
    assert!(needs_prompt(&mgr.check(&other, &["{}".to_string()])));
}

#[test]
fn permission_error_display_has_prefix_and_guidance() {
    let e = PermissionError::new("bash", "rm -rf /");
    let msg = e.to_string();
    assert!(msg.starts_with(PERMISSION_DENIED_PREFIX), "{msg}");
    assert!(msg.contains(DEFAULT_DENY_GUIDANCE), "{msg}");

    let e = PermissionError::with_guidance("bash", "rm -rf /", "use git clean".into());
    assert!(e.to_string().contains("User guidance: use git clean"));
}
