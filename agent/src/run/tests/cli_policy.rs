//! Execution-level tests for the CLI tool-policy flags: `--allowed-tools`
//! preapprovals and `--disallowed-tools` denies, enforced end-to-end by the
//! headless gate (`--print`'s `HeadlessGate`) on native tools, batch
//! children (the workspace's shared `before` hook), and MCP wire tools.

use super::*;
use crate::permissions::PERMISSION_DENIED_PREFIX;
use crate::run::BeforeExecute;
use std::sync::Arc;

/// Resolve the flags exactly as `craft --print` would: parse, then map.
fn cli_tool_policy(flags: &[&str]) -> Vec<crate::permissions::PermissionRule> {
    let cli: crate::cli::Cli =
        clap::Parser::try_parse_from(std::iter::once("craft").chain(flags.iter().copied()))
            .expect("cli parses");
    cli.tool_policy().expect("tool policy resolves")
}

/// A conversation whose approval gate enforces the CLI rules: the gate
/// rides the policy layer for model calls and the workspace's before hook
/// for batch children.
async fn gated_conversation(
    dir: &std::path::Path,
    flags: &[&str],
    provider: &crate::testing::ScriptedProvider,
) -> (Conversation, EventStream) {
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        dir.to_path_buf(),
    ));
    permissions.add_cli_rules(cli_tool_policy(flags));
    let gate =
        Arc::new(crate::headless::HeadlessGate::new(permissions, None)) as Arc<dyn BeforeExecute>;
    let workspace = Workspace::new(dir).unwrap();
    let (sink, stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.display().to_string(),
        "",
        model(provider),
        workspace.registry().unwrap(),
        sink,
    )
    .approval(gate)
    .build()
    .unwrap();
    (conversation, stream)
}

#[tokio::test]
async fn disallowed_tool_is_denied_at_the_gate() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "delete",
            serde_json::json!({"files": ["victim.txt"]}),
        )]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim.txt"), "body").unwrap();
    let (conversation, _stream) =
        gated_conversation(dir.path(), &["--disallowed-tools", "delete"], &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    let text = result_text(&results[0]);
    assert!(text.starts_with(PERMISSION_DENIED_PREFIX), "got: {text}");
    assert!(
        !text.contains("no interactive approver"),
        "an explicit deny is not a fail-closed prompt: {text}"
    );
    assert!(
        dir.path().join("victim.txt").exists(),
        "the denied call must not execute"
    );
}

#[tokio::test]
async fn allowed_tool_runs_without_a_prompt() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "delete",
            serde_json::json!({"files": ["victim.txt"]}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim.txt"), "body").unwrap();
    // Without the flag `delete` would prompt and fail closed headlessly.
    let (conversation, _stream) =
        gated_conversation(dir.path(), &["--allowed-tools", "delete"], &provider).await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    assert!(
        !dir.path().join("victim.txt").exists(),
        "the preapproved call executed"
    );
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert!(
        !result_text(&results[0]).starts_with(PERMISSION_DENIED_PREFIX),
        "got: {}",
        result_text(&results[0])
    );
}

#[tokio::test]
async fn batch_children_inherit_the_cli_policy() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "batch",
            serde_json::json!({"tool_calls": [ {"tool": "delete", "parameters": {"files": ["victim.txt"]}}, {"tool": "read", "parameters": {"path": "kept.txt"}}, ]}),
        )]),
        crate::testing::text_turn("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim.txt"), "body").unwrap();
    std::fs::write(dir.path().join("kept.txt"), "kept").unwrap();
    // The outer batch needs the gate's blessing so the test reaches the
    // children, which flow through the same shared `before` hook.
    let (conversation, _stream) = gated_conversation(
        dir.path(),
        &["--allowed-tools", "batch", "--disallowed-tools", "delete"],
        &provider,
    )
    .await;

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    let text = result_text(&results[0]);
    assert!(
        text.contains(PERMISSION_DENIED_PREFIX),
        "the denied batch child reports the deny: {text}"
    );
    assert!(text.contains("kept"), "the allowed batch child ran: {text}");
    assert!(
        dir.path().join("victim.txt").exists(),
        "the denied child must not execute"
    );
}

#[tokio::test]
async fn mcp_server_rule_denies_wire_tool_calls() {
    let provider = script(vec![
        crate::testing::tool_turn(vec![tool_event(
            "t1",
            "srv__echo",
            serde_json::json!({"message": "hi"}),
        )]),
        crate::testing::text_turn("recovered"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let permissions = Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        dir.path().to_path_buf(),
    ));
    permissions.add_cli_rules(cli_tool_policy(&["--disallowed-tools", "mcp__srv"]));
    let gate =
        Arc::new(crate::headless::HeadlessGate::new(permissions, None)) as Arc<dyn BeforeExecute>;
    let workspace = Workspace::new(dir.path()).unwrap();
    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle(&[(
        "srv.echo",
        "Echo through MCP",
    )])));
    let (sink, _stream) = EventSink::new();
    let conversation = ConversationBuilder::new(
        &Config::parse("").unwrap(),
        &dir.path().display().to_string(),
        "",
        model(&provider),
        workspace.registry().unwrap(),
        sink,
    )
    .approval(gate)
    .build()
    .unwrap();

    let outcome = run_turn(&conversation, "go").await;

    assert!(matches!(outcome, TurnOutcome::Done { .. }), "{outcome:?}");
    let results = tool_results(&conversation).await;
    assert_eq!(results.len(), 1);
    assert!(
        result_text(&results[0]).starts_with(PERMISSION_DENIED_PREFIX),
        "server-wide deny covers srv__echo: {}",
        result_text(&results[0])
    );
}
