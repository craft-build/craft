use super::session::BoxFuture;
use super::*;
use serde_json::json;

struct FailingSession;

impl McpSession for FailingSession {
    fn server_name(&self) -> &str {
        "stub"
    }
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn call_tool(
        &self,
        _name: &str,
        _args: &Value,
    ) -> BoxFuture<'_, Result<McpToolOutput, McpError>> {
        Box::pin(async {
            Err(McpError::UnknownTool {
                name: String::new(),
            })
        })
    }
    fn call_tool_cancellable<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a crate::run::CancelToken,
    ) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
        super::session::race_call_cancel(self, name, args, cancel)
    }
    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn get_prompt(
        &self,
        _name: &str,
        _arguments: &HashMap<String, String>,
    ) -> BoxFuture<'_, Result<Vec<session::PromptMessage>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub(crate) fn stub_handle(tools: &[(&str, &str)]) -> McpHandle {
    let session: Arc<dyn McpSession> = Arc::new(FailingSession);
    let mut index = ToolIndex::default();
    for (qualified, description) in tools {
        let (_server, raw) = qualified
            .split_once(SEPARATOR)
            .unwrap_or((qualified, "tool"));
        index.tools.insert(
            intern(qualified.to_string()),
            ToolRef {
                raw_name: (*raw).to_string(),
                session: Arc::clone(&session),
            },
        );
        index.descriptors.push(McpToolDescriptor {
            wire_name: wire_tool_name(qualified),
            qualified_name: (*qualified).to_string(),
            description: (*description).to_string(),
            parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
            read_only_hint: None,
            destructive_hint: None,
        });
    }
    let index = Arc::new(ArcSwap::from_pointee(index));
    McpHandle {
        cmd_tx: mpsc::unbounded_channel().0,
        snapshot: Arc::new(ArcSwap::from_pointee(McpSnapshot::default())),
        index,
        ready_rx: watch::channel(true).1,
        server_requests: Arc::new(Mutex::new(None)),
    }
}

/// `stub_handle` with published resources, for surfaces that read them.
pub(crate) fn stub_handle_with_resources(resources: Vec<McpResourceInfo>) -> McpHandle {
    // One server row per distinct owning server so the sheet/keys have a
    // row to select and expand.
    let mut infos: Vec<crate::mcp::config::McpServerInfo> = Vec::new();
    for r in &resources {
        if !infos.iter().any(|i| i.name == r.server) {
            infos.push(crate::mcp::config::McpServerInfo {
                name: r.server.clone(),
                transport_kind: "stub",
                tool_count: 0,
                prompt_count: 0,
                resource_count: resources.iter().filter(|x| x.server == r.server).count(),
                status: crate::mcp::config::McpServerStatus::Running,
                config_path: PathBuf::new(),
                url: None,
                oauth: None,
            });
        }
    }
    McpHandle {
        cmd_tx: mpsc::unbounded_channel().0,
        snapshot: Arc::new(ArcSwap::from_pointee(McpSnapshot {
            infos,
            resources,
            ..McpSnapshot::default()
        })),
        index: Arc::new(ArcSwap::from_pointee(ToolIndex::default())),
        ready_rx: watch::channel(true).1,
        server_requests: Arc::new(Mutex::new(None)),
    }
}

/// `stub_handle` with published prompts, for surfaces that list them.
pub(crate) fn stub_handle_with_prompts(prompts: Vec<McpPromptInfo>) -> McpHandle {
    McpHandle {
        cmd_tx: mpsc::unbounded_channel().0,
        snapshot: Arc::new(ArcSwap::from_pointee(McpSnapshot {
            prompts,
            ..McpSnapshot::default()
        })),
        index: Arc::new(ArcSwap::from_pointee(ToolIndex::default())),
        ready_rx: watch::channel(true).1,
        server_requests: Arc::new(Mutex::new(None)),
    }
}
