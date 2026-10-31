use animist::agent::testing::ToolHarness;
use animist::agent::tool::ToolRegistry;

use super::workspace;
use crate::history::ext;

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
    let registry = workspace.registry().unwrap();
    assert!(
        !registry.definitions().iter().any(|d| d.name == "mcp_read"),
        "mcp_read is merged into read, not a separate tool"
    );

    // Qualified form of an unlisted pair: the resource index refuses it.
    let error = read_error(&workspace, "mcp://srv/file:///missing").await;
    assert!(error.contains("unknown MCP resource"), "got: {error}");

    // Verbatim published URI resolves to its server, then hits the same
    // index gate (the stub publishes no readable session).
    let error = read_error(&workspace, "file:///notes.txt").await;
    assert!(error.contains("unknown MCP resource"), "got: {error}");

    // An unlisted verbatim URI fails with the published-resource hint.
    let error = read_error(&workspace, "db://other").await;
    assert!(error.contains("no MCP server publishes"), "got: {error}");
    assert!(error.contains("file:///notes.txt"), "got: {error}");
}

/// Run `read {path}` through the tool harness and return the error text.
async fn read_error(workspace: &crate::tools::Workspace, path: &str) -> String {
    let mut harness = ToolHarness::builder().build().expect("harness");
    let output = harness
        .call(
            crate::tools::Read(workspace.clone()),
            serde_json::json!({"path": path}),
        )
        .await
        .expect("read outcome");
    assert!(output.is_error, "expected an error for {path}");
    ext::tool_content_text(&output.content)
}

/// Run one child call through the registry by dispatching it as a single
/// `batch` entry: the harness routes children through its registry, which
/// is the same routing the agent uses.
async fn dispatch_child(
    registry: &ToolRegistry,
    name: &str,
    input: serde_json::Value,
) -> animist::agent::tool::ToolOutput {
    let mut harness = ToolHarness::builder()
        .tools(registry.clone())
        .build()
        .expect("harness");
    harness
        .call(
            crate::tools::Batch,
            serde_json::json!({"tool_calls": [{"tool": name, "parameters": input}]}),
        )
        .await
        .expect("batch outcome")
}

/// MCP integration (B.11): tools from a stub MCP handle register under their
/// `server__tool` wire name, the executor routes through MCP, and clearing
/// the handle removes them again.
#[tokio::test]
async fn mcp_tools_register_dispatch_and_clear() {
    let (_dir, workspace) = workspace();

    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle(&[(
        "srv.echo",
        "Echo through MCP",
    )])));
    let registry = workspace.registry().unwrap();

    let definitions = registry.definitions();
    let echo = definitions
        .iter()
        .find(|d| d.name == "srv__echo")
        .expect("MCP tool registered under its wire name");
    assert_eq!(echo.description, "Echo through MCP");
    assert_eq!(echo.input_schema["type"], "object");

    let output = dispatch_child(&registry, "srv__echo", serde_json::json!({"message": "hi"})).await;
    let text = ext::tool_content_text(&output.content);
    // The stub session fails every call with `unknown MCP tool`, which
    // proves the call reached MCP routing rather than dying at name lookup.
    assert!(text.contains("[ERROR]"), "{text}");
    assert!(text.contains("unknown MCP tool"), "got: {text}");

    workspace.set_mcp(None);
    let cleared = workspace.registry().unwrap();
    assert!(
        cleared.definitions().iter().all(|d| d.name != "srv__echo"),
        "clearing the handle removes MCP tools"
    );
}

/// MCP output blocks map to `ToolContents` in the server's order: text as
/// text, images as base64 image content, interleaved where they appeared.
#[tokio::test]
async fn mcp_output_maps_text_and_images_in_order() {
    use crate::mcp::session::{McpPart, McpToolImage};

    let (_dir, workspace) = workspace();
    workspace.set_mcp(Some(crate::mcp::test_support::stub_handle_with_output(
        &[("srv.media", "text and images")],
        crate::mcp::McpToolOutput {
            parts: vec![
                McpPart::Text("before".into()),
                McpPart::Image(McpToolImage {
                    data: "aGk=".into(),
                    mime: "image/png".into(),
                }),
                McpPart::Text("after".into()),
                McpPart::Image(McpToolImage {
                    data: "aGk=".into(),
                    mime: "image/x-exotic".into(),
                }),
            ],
        },
    )));
    let executor = animist::agent::tool::ToolExecutor::new(crate::tools::McpTool {
        handle: workspace.mcp().expect("stub handle"),
        qualified: "srv.media".to_owned(),
        definition: animist::agent::tool::ToolDefinition::builder()
            .name("srv__media")
            .description("text and images")
            .input_schema(serde_json::json!({}))
            .build(),
        read_only: false,
    });
    let mut harness = ToolHarness::builder().build().expect("harness");
    let output = harness
        .call_executor(executor, serde_json::json!({}))
        .await
        .expect("mcp outcome");
    let blocks = &output.content.0;
    assert_eq!(blocks.len(), 4, "{blocks:?}");
    assert!(matches!(&blocks[0], crate::history::ToolContent::Text { text } if text == "before"));
    assert!(matches!(
        &blocks[1],
        crate::history::ToolContent::Image {
            source: crate::history::ImageSource::Base64 { media_type, data },
        } if media_type == "image/png" && data == "aGk="
    ));
    assert!(matches!(&blocks[2], crate::history::ToolContent::Text { text } if text == "after"));
    // Unknown mimes degrade to a note instead of a broken image block.
    assert!(matches!(
        &blocks[3],
        crate::history::ToolContent::Text { text } if text.contains("unsupported image mime")
    ));
}
