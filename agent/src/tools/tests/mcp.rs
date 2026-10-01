use super::workspace;

/// Phase 5: the internal `mcp_read` tool registers whenever an MCP handle
/// is installed (resources can appear later via `resources/list_changed`),
/// and a read of an unlisted pair fails with the UnknownResource error.
#[tokio::test]
async fn mcp_read_registers_with_any_handle_and_rejects_unknown_pairs() {
    let (_dir, workspace) = workspace();
    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle(&[(
        "srv.echo",
        "Echo through MCP",
    )])));
    assert!(
        workspace
            .register()
            .definitions()
            .iter()
            .any(|d| d.name == "mcp_read"),
        "an MCP handle is installed: mcp_read must register"
    );

    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle_with_resources(
        vec![crate::mcp::McpResourceInfo {
            server: "srv".into(),
            uri: "file:///notes.txt".into(),
            name: "notes".into(),
            description: String::new(),
            mime: Some("text/plain".into()),
            size: None,
        }],
    )));
    let dispatch = workspace.register();
    use crate::history::ToolCall;
    let call = ToolCall::new(
        "t1",
        "mcp_read",
        serde_json::json!({"server": "srv", "uri": "file:///missing"}),
    );
    match dispatch.execute(call).await.expect("dispatch") {
        crate::run::DispatchOutcome::Ran(result) => {
            assert!(result.is_error, "unlisted pair must fail");
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::history::ToolResultContent::Text(t) => Some(t.text.clone().to_string()),
                    _ => None,
                })
                .collect();
            assert!(text.contains("unknown MCP resource"), "got: {text}");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
}

/// MCP integration (B.11): tools from a stub MCP handle register under their
/// `server__tool` wire name, dispatch executes through MCP routing, and
/// clearing the handle removes them again.
#[tokio::test]
async fn mcp_tools_register_dispatch_and_clear() {
    let (_dir, workspace) = workspace();

    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle(&[(
        "srv.echo",
        "Echo through MCP",
    )])));
    let dispatch = workspace.register();

    let definitions = dispatch.definitions();
    let echo = definitions
        .iter()
        .find(|d| d.name == "srv__echo")
        .expect("MCP tool registered under its wire name");
    assert_eq!(echo.description, "Echo through MCP");
    assert_eq!(echo.parameters["type"], "object");

    use crate::history::ToolCall;
    let call = ToolCall::new("t1", "srv__echo", serde_json::json!({"message": "hi"}));
    match dispatch.execute(call).await.expect("dispatch") {
        crate::run::DispatchOutcome::Ran(result) => {
            // The stub session fails every call with `unknown MCP tool`, which
            // proves the call reached MCP routing rather than dying at name
            // lookup.
            assert!(result.is_error, "stub call must fail");
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::history::ToolResultContent::Text(t) => Some(t.text.clone().to_string()),
                    _ => None,
                })
                .collect();
            assert!(text.contains("unknown MCP tool"), "got: {text}");
        }
        other => panic!("unexpected outcome: {other:?}"),
    }

    workspace.set_mcp(None);
    let cleared = workspace.register();
    assert!(
        cleared.definitions().iter().all(|d| d.name != "srv__echo"),
        "clearing the handle removes MCP tools"
    );
}
