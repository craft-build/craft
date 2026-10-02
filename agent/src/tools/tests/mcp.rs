use super::workspace;

/// MCP resource reads ride the `read` tool (the `mcp_read` merge): the
/// `mcp://<server>/<uri>` form dispatches through MCP routing, verbatim
/// published URIs resolve through the resource index, and unlisted
/// targets fail with the index's UnknownResource error.
#[tokio::test]
async fn read_routes_mcp_resources_and_rejects_unknown_pairs() {
    let (_dir, workspace) = workspace();
    let published = crate::mcp::McpResourceInfo {
        server: "srv".into(),
        uri: "file:///notes.txt".into(),
        name: "notes".into(),
        description: String::new(),
        mime: Some("text/plain".into()),
        size: None,
    };
    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle_with_resources(
        vec![published],
    )));
    let dispatch = workspace.register();
    assert!(
        !dispatch.definitions().iter().any(|d| d.name == "mcp_read"),
        "mcp_read is merged into read, not a separate tool"
    );

    // Qualified form of an unlisted pair: the resource index refuses it.
    let error = read_error(&dispatch, "mcp://srv/file:///missing").await;
    assert!(error.contains("unknown MCP resource"), "got: {error}");

    // Verbatim published URI resolves to its server, then hits the same
    // index gate (the stub publishes no readable session).
    let error = read_error(&dispatch, "file:///notes.txt").await;
    assert!(error.contains("unknown MCP resource"), "got: {error}");

    // An unlisted verbatim URI fails with the published-resource hint.
    let error = read_error(&dispatch, "db://other").await;
    assert!(error.contains("no MCP server publishes"), "got: {error}");
    assert!(error.contains("file:///notes.txt"), "got: {error}");
}

/// Run `read {path}` through a dispatch and return the error text.
async fn read_error(dispatch: &crate::run::ToolDispatch, path: &str) -> String {
    use crate::history::{ToolCall, ToolResultContent};
    let call = ToolCall::new("t1", "read", serde_json::json!({"path": path}));
    match dispatch.execute(call).await.expect("dispatch") {
        crate::run::DispatchOutcome::Ran(result) => {
            assert!(result.is_error, "expected an error for {path}");
            result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.clone().to_string()),
                    _ => None,
                })
                .collect()
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
