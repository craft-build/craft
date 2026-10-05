//! Execution-level tests for the CLI tool-policy flags: `--allowed-tools`
//! preapprovals and `--disallowed-tools` denies, enforced end-to-end by the
//! headless gate (`--print`'s `HeadlessGate`) on native tools, batch
//! children (shared `before` hook), and MCP wire tools.

use super::*;
use crate::history::UserContent;
use crate::permissions::PERMISSION_DENIED_PREFIX;
use crate::run::dispatch::BeforeExecute;
use crate::run::*;

/// Resolve the flags exactly as `craft --print` would: parse, then map.
fn cli_tool_policy(flags: &[&str]) -> Vec<crate::permissions::PermissionRule> {
    let cli: crate::cli::Cli =
        clap::Parser::try_parse_from(std::iter::once("craft").chain(flags.iter().copied()))
            .expect("cli parses");
    cli.tool_policy().expect("tool policy resolves")
}

fn gated_tools(dir: &std::path::Path, flags: &[&str]) -> crate::run::ToolDispatch {
    let permissions = std::sync::Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        dir.to_path_buf(),
    ));
    permissions.add_cli_rules(cli_tool_policy(flags));
    let gate = std::sync::Arc::new(crate::headless::HeadlessGate::new(permissions, None))
        as std::sync::Arc<dyn BeforeExecute>;
    crate::tools::Workspace::new(dir)
        .unwrap()
        .register()
        .with_before(gate)
}

/// First tool result's text, in call order.
fn first_tool_text(history: &[Message]) -> String {
    history
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content.as_slice(),
            _ => &[],
        })
        .find_map(|b| match b {
            UserContent::ToolResult(r) => Some(r.content[0].to_text()),
            _ => None,
        })
        .expect("a tool result")
}

#[tokio::test]
async fn disallowed_tool_is_denied_at_the_gate() {
    let (model, _) = stream_turns(vec![
        vec![
            tool_event("t1", "delete", serde_json::json!({"files": ["victim.txt"]})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("recovered"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim.txt"), "body").unwrap();
    let tools = gated_tools(dir.path(), &["--disallowed-tools", "delete"]);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let text = first_tool_text(&history);
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
    let (model, _) = stream_turns(vec![
        vec![
            tool_event("t1", "delete", serde_json::json!({"files": ["victim.txt"]})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim.txt"), "body").unwrap();
    // Without the flag `delete` would prompt and fail closed headlessly.
    let tools = gated_tools(dir.path(), &["--allowed-tools", "delete"]);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    assert!(
        !dir.path().join("victim.txt").exists(),
        "the preapproved call executed"
    );
    let text = first_tool_text(&history);
    assert!(!text.starts_with(PERMISSION_DENIED_PREFIX), "got: {text}");
}

#[tokio::test]
async fn batch_children_inherit_the_cli_policy() {
    let (model, _) = stream_turns(vec![
        vec![
            tool_event(
                "t1",
                "batch",
                serde_json::json!({"tool_calls": [
                    {"tool": "delete", "parameters": {"files": ["victim.txt"]}},
                    {"tool": "read", "parameters": {"path": "kept.txt"}},
                ]}),
            ),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
    ]);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("victim.txt"), "body").unwrap();
    std::fs::write(dir.path().join("kept.txt"), "kept").unwrap();
    // The outer batch needs the gate's blessing so the test reaches the
    // children, which flow through the same shared `before` hook.
    let tools = gated_tools(
        dir.path(),
        &["--allowed-tools", "batch", "--disallowed-tools", "delete"],
    );
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let text = first_tool_text(&history);
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
    let (model, _) = stream_turns(vec![
        vec![
            tool_event("t1", "srv__echo", serde_json::json!({"message": "hi"})),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
        vec![
            MockStreamEvent::text("recovered"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ],
    ]);
    let dir = tempfile::tempdir().unwrap();
    let permissions = std::sync::Arc::new(crate::permissions::PermissionManager::new(
        crate::permissions::PermissionsConfig::default(),
        dir.path().to_path_buf(),
    ));
    permissions.add_cli_rules(cli_tool_policy(&["--disallowed-tools", "mcp__srv"]));
    let gate = std::sync::Arc::new(crate::headless::HeadlessGate::new(permissions, None))
        as std::sync::Arc<dyn BeforeExecute>;
    let workspace = crate::tools::Workspace::new(dir.path()).unwrap();
    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle(&[(
        "srv.echo",
        "Echo through MCP",
    )])));
    let tools = workspace.register().with_before(gate);
    let (_, cancel) = cancel_channel();
    let mut history = Vec::new();
    let outcome = run(
        &model,
        &RunParams::default(),
        &tools,
        &mut history,
        "go",
        &cancel,
        &|_| {},
    )
    .await;
    assert!(matches!(outcome, RunOutcome::Done { .. }));
    let text = first_tool_text(&history);
    assert!(
        text.starts_with(PERMISSION_DENIED_PREFIX),
        "server-wide deny covers srv__echo: {text}"
    );
}
