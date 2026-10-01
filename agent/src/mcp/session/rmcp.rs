use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;

use super::super::config::{ServerConfig, Transport};
use super::super::error::McpError;
use super::super::request::{ElicitOutcome, McpServerRequest};
use super::events::McpEvents;
use super::{
    BoxFuture, Listings, McpPart, McpSession, McpToolImage, McpToolOutput, PromptArgument,
    PromptInfo, PromptMessage, ResourceInfo, ToolInfo,
};

/// Start one server from its config: build the transport, run the `initialize`
/// handshake, and return the live session.
pub async fn start_session(
    config: &ServerConfig,
    events: McpEvents,
) -> Result<Arc<dyn McpSession>, McpError> {
    match connect(config, events.clone()).await {
        Ok(session) => Ok(session),
        // Transient-failure retry: one more attempt for errors that look like
        // transport blips rather than config or auth problems.
        Err(e) if is_transient(&e) => {
            tracing::warn!(server = %config.name, error = %e, "MCP connect failed; retrying once");
            connect(config, events).await
        }
        Err(e) => Err(e),
    }
}

fn is_transient(e: &McpError) -> bool {
    // A timeout is a hung server, not a blip: retrying doubles the worst-case
    // connect latency (and every gate that waits on it) for no gain.
    !matches!(
        e,
        McpError::Config { .. }
            | McpError::Timeout { .. }
            | McpError::HttpError { status: 401, .. }
            | McpError::RpcError { .. }
    )
}

async fn connect(
    config: &ServerConfig,
    events: McpEvents,
) -> Result<Arc<dyn McpSession>, McpError> {
    let name: Arc<str> = Arc::from(config.name.as_str());
    let handler = McpClientHandler {
        server: Arc::clone(&name),
        events: events.clone(),
    };
    let (service, capabilities, child_pid) = match &config.transport {
        Transport::Stdio {
            program,
            args,
            environment,
        } => {
            let mut command = tokio::process::Command::new(program);
            command.args(args).kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            for (key, value) in environment {
                command.env(key, value);
            }
            let transport = rmcp::transport::TokioChildProcess::new(command).map_err(|e| {
                McpError::StartFailed {
                    server: config.name.clone(),
                    reason: e.to_string(),
                }
            })?;
            // kill_on_drop only reaps the direct child; wrappers like npx
            // fork grandchildren that must die with the session. The child is
            // its own process-group leader, so a group kill reaps them all.
            let child_pid = transport.id();
            let service = run_initialize(config, transport, handler).await?;
            let capabilities = capabilities_of(&service);
            (service, capabilities, child_pid)
        }
        Transport::Http {
            url,
            headers,
            oauth,
        } => {
            let mut cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.clone());
            for (key, value) in headers {
                if let (Ok(name), Ok(val)) = (
                    http::HeaderName::try_from(key.as_str()),
                    http::HeaderValue::try_from(value.as_str()),
                ) {
                    cfg.custom_headers.insert(name, val);
                } else {
                    return Err(McpError::HttpError {
                        server: config.name.clone(),
                        status: 0,
                        reason: format!("invalid header '{key}'"),
                    });
                }
            }
            let _ = oauth; // static OAuth client is honored by the login flow
            let stored = match crate::storage::StateDir::resolve() {
                Ok(state_dir) => {
                    super::super::oauth::stored_manager(&config.name, url, &state_dir).await
                }
                Err(_) => None,
            };
            // The two transports are distinct types, so the generic
            // `run_initialize` is instantiated per arm rather than unified.
            let service = match stored {
                Some(manager) => {
                    let auth_client =
                        rmcp::transport::auth::AuthClient::new(reqwest::Client::new(), manager);
                    run_initialize(
                        config,
                        rmcp::transport::streamable_http_client::                        StreamableHttpClientTransport::with_client(auth_client, cfg),
                        handler,
                    )
                    .await?
                }
                None => {
                    run_initialize(
                        config,
                        rmcp::transport::streamable_http_client::                        StreamableHttpClientTransport::from_config(cfg),
                        handler,
                    )
                    .await?
                }
            };
            let capabilities = capabilities_of(&service);
            (service, capabilities, None)
        }
    };

    // Asymmetric on purpose (reference semantics): sloppy servers omit
    // `capabilities` yet serve tools/list fine, so always ask for tools (fatal
    // only when tools were declared). Prompts only when declared: undeclared
    // endpoints may answer junk, and junk must not take down the tools.
    let tool_infos = match RmcpSession::list_tools_raw(&service).await {
        Ok(tools) => tools,
        Err(e) if !capabilities.tools => {
            tracing::warn!(server = %config.name, error = %e, "tools/list failed; server declared no tools");
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let prompt_infos = if capabilities.prompts {
        RmcpSession::list_prompts_raw(&service).await?
    } else {
        Vec::new()
    };
    // Same gating as prompts: undeclared resource endpoints may answer junk.
    let resource_infos = if capabilities.resources {
        RmcpSession::list_resources_raw(&service).await?
    } else {
        Vec::new()
    };

    tracing::info!(
        server = %config.name,
        tool_count = tool_infos.len(),
        prompt_count = prompt_infos.len(),
        resource_count = resource_infos.len(),
        "MCP server initialized"
    );

    let dead = Arc::new(AtomicBool::new(false));
    let service = Arc::new(service);
    spawn_keepalive(
        Arc::clone(&service),
        Arc::clone(&name),
        Arc::clone(&dead),
        events,
    );

    Ok(Arc::new(RmcpSession {
        name,
        service,
        child_pid,
        dead,
        tool_infos: RwLock::new(tool_infos),
        prompt_infos: RwLock::new(prompt_infos),
        resource_infos: RwLock::new(resource_infos),
        has_resources: capabilities.resources,
        timeout: config.timeout,
    }))
}

type ClientService = rmcp::service::RunningService<rmcp::RoleClient, McpClientHandler>;

/// How often the keepalive task pings the server. 30s is comfortably under
/// typical proxy idle timeouts; the initialize result carries no keepalive
/// hint in any protocol version we speak, so there is nothing to derive from.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Proxy-backed servers silently drop idle connections; a periodic ping
/// detects that within one interval instead of surfacing it later as a
/// mysterious tool-call timeout. On failure the session is marked dead
/// (call_tool refuses fast) and the transport torn down; the manager flips
/// the entry to Failed via `on_dead` so the user is offered Reconnect. Tied
/// to the service's cancellation token, so shutdown stops it.
fn spawn_keepalive(
    service: Arc<ClientService>,
    name: Arc<str>,
    dead: Arc<AtomicBool>,
    events: McpEvents,
) {
    tokio::spawn(async move {
        loop {
            // Session shutdown cancels the service token; `is_closed`
            // observes it, so the task never outlives the session by more
            // than one interval.
            if service.is_closed() {
                return;
            }
            tokio::time::sleep(KEEPALIVE_INTERVAL).await;
            let ping = service.send_request(rmcp::model::ClientRequest::PingRequest(
                rmcp::model::PingRequest {
                    method: Default::default(),
                    extensions: Default::default(),
                },
            ));
            match tokio::time::timeout(KEEPALIVE_INTERVAL, ping).await {
                Ok(Ok(_)) => {}
                _ => {
                    tracing::warn!(server = %name, "MCP keepalive ping failed; marking session dead");
                    dead.store(true, Ordering::SeqCst);
                    events.dead(&name);
                    service.cancellation_token().cancel();
                    return;
                }
            }
        }
    });
}

/// rmcp client handler wiring server notifications to [`McpEvents`]. The
/// handler cannot re-list itself (it does not own the service), so list
/// changes surface as callbacks and the manager refreshes.
struct McpClientHandler {
    server: Arc<str>,
    events: McpEvents,
}

/// A request no frontend can answer: deny with `-32603` so the server can
/// degrade instead of waiting out its own timeout.
fn relay_unavailable(what: &str) -> rmcp::model::ErrorData {
    rmcp::model::ErrorData::internal_error(format!("{what} denied: no frontend is attached"), None)
}

/// `file://` URI for the workspace root. `Url::from_file_path` rejects
/// non-UTF-8 paths, so encode the raw OS bytes by hand in that case — every
/// byte outside the unreserved set is percent-encoded, keeping the URI valid.
fn file_uri(root: &std::path::Path) -> String {
    if let Ok(uri) = url::Url::from_file_path(root) {
        return uri.to_string();
    }
    let mut uri = String::from("file:///");
    for byte in root.as_os_str().as_encoded_bytes() {
        match byte {
            b'/' => uri.push('/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                uri.push(*byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

impl rmcp::ClientHandler for McpClientHandler {
    async fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        tracing::info!(server = %self.server, "tool list changed");
        self.events.list_changed(&self.server);
    }

    async fn on_prompt_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        tracing::info!(server = %self.server, "prompt list changed");
        self.events.list_changed(&self.server);
    }

    async fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        tracing::info!(server = %self.server, "resource list changed");
        self.events.list_changed(&self.server);
    }

    /// Advertise what this handler actually answers: roots (when a root was
    /// threaded in), sampling, and form-mode elicitation. The defaults rmcp
    /// would send declare none of these, so servers never ask.
    fn get_info(&self) -> rmcp::model::ClientConfig {
        let mut capabilities = rmcp::model::ClientCapabilities::default();
        if self.events.root.is_some() {
            let mut roots = rmcp::model::RootsCapabilities::default();
            roots.list_changed = Some(false);
            capabilities.roots = Some(roots);
        }
        capabilities.sampling = Some(rmcp::model::SamplingCapability::default());
        capabilities.elicitation = Some(
            rmcp::model::ElicitationCapability::default()
                .with_form(rmcp::model::FormElicitationCapability::new()),
        );
        rmcp::model::ClientConfig::new(
            capabilities,
            rmcp::model::Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }

    /// `roots/list`: answered directly from the threaded workspace root —
    /// no UI round-trip, the answer is always the same.
    #[allow(deprecated)] // roots are deprecated by SEP-2577; still used by servers
    async fn list_roots(
        &self,
        _context: rmcp::service::RequestContext<rmcp::RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, rmcp::model::ErrorData> {
        Ok(match &self.events.root {
            Some(root) => rmcp::model::ListRootsResult::new(vec![
                rmcp::model::Root::new(file_uri(root)).with_name("workspace"),
            ]),
            None => rmcp::model::ListRootsResult::default(),
        })
    }

    async fn create_message(
        &self,
        params: rmcp::model::CreateMessageRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleClient>,
    ) -> Result<rmcp::model::CreateMessageResult, rmcp::model::ErrorData> {
        let Some(tx) = self.events.server_requests.clone() else {
            return Err(relay_unavailable("sampling"));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx
            .send(McpServerRequest::Sampling {
                server: Arc::clone(&self.server),
                request: params,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(relay_unavailable("sampling"));
        }
        match reply_rx.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(message)) => Err(rmcp::model::ErrorData::internal_error(message, None)),
            Err(_) => Err(relay_unavailable("sampling")),
        }
    }

    /// `elicitation/create`: form requests relay to the frontend; URL-mode
    /// and unknown variants decline — opening external URLs from a server's
    /// say-so is not something this client does.
    async fn create_elicitation(
        &self,
        request: rmcp::model::ElicitRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleClient>,
    ) -> Result<rmcp::model::ElicitResult, rmcp::model::ErrorData> {
        let (message, schema) = match request {
            rmcp::model::ElicitRequestParams::FormElicitationParams {
                message,
                requested_schema,
                ..
            } => (message, requested_schema),
            _ => {
                return Ok(rmcp::model::ElicitResult::new(
                    rmcp::model::ElicitationAction::Decline,
                ));
            }
        };
        let Some(tx) = self.events.server_requests.clone() else {
            return Err(relay_unavailable("elicitation"));
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        if tx
            .send(McpServerRequest::Elicitate {
                server: Arc::clone(&self.server),
                message,
                schema,
                reply: reply_tx,
            })
            .is_err()
        {
            return Err(relay_unavailable("elicitation"));
        }
        match reply_rx.await {
            Ok(Ok(ElicitOutcome::Accept(content))) => Ok(rmcp::model::ElicitResult::new(
                rmcp::model::ElicitationAction::Accept,
            )
            .with_content(content)),
            Ok(Ok(ElicitOutcome::Decline)) => Ok(rmcp::model::ElicitResult::new(
                rmcp::model::ElicitationAction::Decline,
            )),
            Ok(Err(message)) => Err(rmcp::model::ErrorData::internal_error(message, None)),
            Err(_) => Err(relay_unavailable("elicitation")),
        }
    }

    #[allow(deprecated)] // MCP logging notification; still emitted by servers
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
    ) {
        #[allow(deprecated)]
        let (level, message) = (params.level, params.data.to_string());
        let server = self.server.to_string();
        let level_str = level_str(level);
        let warns = matches!(
            level,
            rmcp::model::LoggingLevel::Warning
                | rmcp::model::LoggingLevel::Error
                | rmcp::model::LoggingLevel::Critical
                | rmcp::model::LoggingLevel::Alert
                | rmcp::model::LoggingLevel::Emergency
        );
        let line = format!("mcp {server}: {message}");
        if warns {
            tracing::warn!(server = %server, level = level_str, "MCP log notification: {message}");
            self.events.log(&server, level_str, &line);
        } else {
            tracing::info!(server = %server, level = level_str, "MCP log notification: {message}");
        }
    }
}

#[allow(deprecated)]
fn level_str(level: rmcp::model::LoggingLevel) -> &'static str {
    match level {
        rmcp::model::LoggingLevel::Debug => "debug",
        rmcp::model::LoggingLevel::Info => "info",
        rmcp::model::LoggingLevel::Notice => "notice",
        rmcp::model::LoggingLevel::Warning => "warning",
        rmcp::model::LoggingLevel::Error => "error",
        rmcp::model::LoggingLevel::Critical => "critical",
        rmcp::model::LoggingLevel::Alert => "alert",
        rmcp::model::LoggingLevel::Emergency => "emergency",
    }
}

async fn run_initialize<T, E, A>(
    config: &ServerConfig,
    transport: T,
    handler: McpClientHandler,
) -> Result<ClientService, McpError>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    use rmcp::service::ServiceExt as _;

    let server = config.name.clone();
    let init = tokio::time::timeout(config.timeout, handler.serve(transport)).await;
    let result = match init {
        Ok(r) => r,
        Err(_) => {
            return Err(McpError::Timeout {
                server,
                timeout_ms: config.timeout.as_millis() as u64,
            });
        }
    };
    match result {
        Ok(service) => Ok(service),
        Err(e) if e.is_authorization_required() => {
            let reason = e.auth_challenge().map(str::to_owned).unwrap_or_default();
            Err(McpError::HttpError {
                server,
                status: 401,
                reason,
            })
        }
        Err(e) => Err(McpError::StartFailed {
            server,
            reason: e.to_string(),
        }),
    }
}

#[derive(Default)]
struct Capabilities {
    tools: bool,
    prompts: bool,
    resources: bool,
}

fn capabilities_of(service: &ClientService) -> Capabilities {
    match service.peer_info() {
        Some(info) => Capabilities {
            tools: info.capabilities.tools.is_some(),
            prompts: info.capabilities.prompts.is_some(),
            resources: info.capabilities.resources.is_some(),
        },
        None => Capabilities::default(),
    }
}

/// A live rmcp session. Tool/prompt listings are cached at connect time and
/// re-fetched on `list_changed` notifications; `dead` is set by the keepalive
/// task when the server stops answering pings.
struct RmcpSession {
    name: Arc<str>,
    service: Arc<ClientService>,
    /// stdio child's pid (its process-group id); `None` for HTTP sessions.
    child_pid: Option<u32>,
    /// Set when the keepalive ping failed (or we shut down); further calls
    /// fail fast instead of waiting out the request timeout against a dead
    /// transport.
    dead: Arc<AtomicBool>,
    tool_infos: RwLock<Vec<ToolInfo>>,
    prompt_infos: RwLock<Vec<PromptInfo>>,
    resource_infos: RwLock<Vec<ResourceInfo>>,
    /// Server declared the `resources` capability: refreshes re-list
    /// resources only when it did (undeclared endpoints may answer junk).
    has_resources: bool,
    timeout: Duration,
}

impl RmcpSession {
    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }
}

impl RmcpSession {
    async fn with_timeout<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, rmcp::service::ServiceError>>,
    ) -> Result<T, McpError> {
        let server = self.name.to_string();
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(McpError::RpcError {
                server,
                code: 0,
                message: e.to_string(),
            }),
            Err(_) => Err(McpError::Timeout {
                server,
                timeout_ms: self.timeout.as_millis() as u64,
            }),
        }
    }

    async fn list_tools_raw(service: &ClientService) -> Result<Vec<ToolInfo>, McpError> {
        let tools = service.list_all_tools().await.map_err(rpc_to_mcp)?;
        Ok(tools
            .into_iter()
            .map(|tool| ToolInfo {
                name: tool.name.to_string(),
                description: tool.description.unwrap_or_default().to_string(),
                input_schema: serde_json::Value::Object(
                    serde_json::to_value(&*tool.input_schema)
                        .ok()
                        .and_then(|v| v.as_object().cloned())
                        .unwrap_or_default(),
                ),
                read_only_hint: tool.annotations.as_ref().and_then(|a| a.read_only_hint),
                destructive_hint: tool.annotations.as_ref().and_then(|a| a.destructive_hint),
            })
            .collect())
    }

    async fn list_prompts_raw(service: &ClientService) -> Result<Vec<PromptInfo>, McpError> {
        let prompts = service.list_all_prompts().await.map_err(rpc_to_mcp)?;
        Ok(prompts
            .into_iter()
            .map(|prompt| PromptInfo {
                name: prompt.name,
                description: prompt.description,
                arguments: prompt
                    .arguments
                    .unwrap_or_default()
                    .into_iter()
                    .map(|arg| PromptArgument {
                        name: arg.name,
                        description: arg.description,
                        required: arg.required.unwrap_or(false),
                    })
                    .collect(),
            })
            .collect())
    }

    async fn list_resources_raw(service: &ClientService) -> Result<Vec<ResourceInfo>, McpError> {
        let resources = service.list_all_resources().await.map_err(rpc_to_mcp)?;
        Ok(resources
            .into_iter()
            .map(|resource| ResourceInfo {
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime: resource.mime_type,
                size: resource.size,
            })
            .collect())
    }

    /// `resources/read`: text inline (clipped like embedded resources),
    /// blobs as base64 under a mime header line so the model knows what
    /// the payload is.
    async fn read_resource_raw(
        &self,
        uri: &str,
    ) -> Result<Vec<rmcp::model::ResourceContents>, McpError> {
        let params = rmcp::model::ReadResourceRequestParams::new(uri.to_string());
        let result = self
            .with_timeout(self.service.read_resource(params))
            .await?;
        Ok(result.contents)
    }
}

fn render_resource_contents(contents: &[rmcp::model::ResourceContents]) -> String {
    contents
        .iter()
        .map(|content| match content {
            rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                let (clipped, truncated) = clip(text, MAX_RESOURCE_BYTES);
                if truncated {
                    format!("{clipped}\n... [output truncated]")
                } else {
                    clipped.into()
                }
            }
            rmcp::model::ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => {
                let mime = mime_type.clone().unwrap_or_else(|| "unknown".into());
                // Clip like the text branch: an unbounded base64 blob from a
                // hostile or buggy server would flood the prompt and memory.
                let (clipped, truncated) = clip(blob, MAX_RESOURCE_BYTES);
                if truncated {
                    format!(
                        "[base64 blob resource {uri} ({mime})]\n{clipped}\n... [output truncated]"
                    )
                } else {
                    format!("[base64 blob resource {uri} ({mime})]\n{clipped}")
                }
            }
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn rpc_to_mcp(e: rmcp::service::ServiceError) -> McpError {
    McpError::RpcError {
        server: String::new(),
        code: 0,
        message: e.to_string(),
    }
}

fn arguments_object(args: &Value) -> rmcp::model::JsonObject {
    match args {
        Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    }
}

/// Shared `tools/call` tail for both call paths: join content blocks,
/// append 2025-06-18 `structuredContent` as a fenced JSON block when the
/// blocks don't already carry it (a spec-compliant server echoes the same
/// JSON in a text block, so re-appending duplicates the payload), and
/// surface `isError` results as `RpcError`.
fn call_tool_result_to_output(
    server: &Arc<str>,
    result: rmcp::model::CallToolResult,
) -> Result<McpToolOutput, McpError> {
    let mut output = join_content(&result.content);
    if let Some(structured) = result.structured_content
        && !output.carries_json(&structured)
        && let Ok(json) = serde_json::to_string_pretty(&structured)
    {
        output
            .parts
            .push(McpPart::Text(format!("```json\n{json}\n```")));
    }
    if result.is_error.unwrap_or(false) {
        return Err(McpError::RpcError {
            server: server.to_string(),
            code: -1,
            message: output.joined_text(),
        });
    }
    Ok(output)
}

/// Cap on an embedded resource's inlined text; beyond this the text is cut
/// with the repo-wide truncation marker so one tool result cannot flood the
/// context.
const MAX_RESOURCE_BYTES: usize = 8 * 1024;

/// Byte-safe clip honoring char boundaries (same contract as the tools'
/// `clip`), kept local so the MCP layer does not depend on the tools module.
fn clip(text: &str, max_bytes: usize) -> (&str, bool) {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}

fn inline_resource(resource: &rmcp::model::EmbeddedResource) -> String {
    match &resource.resource {
        rmcp::model::ResourceContents::TextResourceContents { uri, text, .. } => {
            let (clipped, truncated) = clip(text, MAX_RESOURCE_BYTES);
            if truncated {
                format!("{clipped}\n... [output truncated] (resource {uri})")
            } else {
                clipped.into()
            }
        }
        // Binary blobs have no textual form worth a context slot; keep the
        // uri so the model knows where the data lives.
        rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => {
            format!("resource:{uri} [binary blob omitted]")
        }
        _ => String::new(),
    }
}

/// Flatten a tool result's content blocks in order: text (and resource text)
/// as text parts, images kept structured, resource links as `resource:`
/// lines. Nothing is dropped silently.
pub fn join_content(blocks: &[rmcp::model::ContentBlock]) -> McpToolOutput {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            rmcp::model::ContentBlock::Text(text) => parts.push(McpPart::Text(text.text.clone())),
            rmcp::model::ContentBlock::Image(image) => {
                parts.push(McpPart::Image(McpToolImage {
                    data: image.data.clone(),
                    mime: image.mime_type.clone(),
                }));
            }
            rmcp::model::ContentBlock::Resource(resource) => {
                parts.push(McpPart::Text(inline_resource(resource)));
            }
            rmcp::model::ContentBlock::ResourceLink(link) => {
                parts.push(McpPart::Text(format!("resource:{}", link.uri)));
            }
            rmcp::model::ContentBlock::Audio(_) => {
                parts.push(McpPart::Text("[audio content omitted]".into()));
            }
            _ => {}
        }
    }
    McpToolOutput { parts }
}

impl McpSession for RmcpSession {
    fn server_name(&self) -> &str {
        &self.name
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
        Box::pin(async {
            Ok(self
                .tool_infos
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone())
        })
    }

    fn refresh_listings(&self) -> BoxFuture<'_, Result<Listings, McpError>> {
        Box::pin(async {
            if self.is_dead() {
                return Err(McpError::ServerDied {
                    server: self.name.to_string(),
                });
            }
            let tools = Self::list_tools_raw(&self.service).await?;
            let prompts = Self::list_prompts_raw(&self.service).await?;
            let resources = if self.has_resources {
                Self::list_resources_raw(&self.service).await?
            } else {
                Vec::new()
            };
            *self.tool_infos.write().unwrap_or_else(|e| e.into_inner()) = tools.clone();
            *self.prompt_infos.write().unwrap_or_else(|e| e.into_inner()) = prompts.clone();
            *self
                .resource_infos
                .write()
                .unwrap_or_else(|e| e.into_inner()) = resources.clone();
            Ok(Listings {
                tools,
                prompts,
                resources,
            })
        })
    }

    fn call_tool(
        &self,
        name: &str,
        args: &Value,
    ) -> BoxFuture<'_, Result<McpToolOutput, McpError>> {
        if self.is_dead() {
            return Box::pin(async {
                Err(McpError::ServerDied {
                    server: self.name.to_string(),
                })
            });
        }
        let params = rmcp::model::CallToolRequestParams::new(name.to_string())
            .with_arguments(arguments_object(args));
        let fut = self.service.call_tool(params);
        Box::pin(async move {
            let result = self.with_timeout(fut).await?;
            call_tool_result_to_output(&self.name, result)
        })
    }

    /// Phase 6: send `tools/call` as a cancellable request so the request
    /// id stays ours. rmcp sends no `notifications/cancelled` on future
    /// drop (`RequestHandle` has no `Drop`), so on cancel (or timeout) we
    /// notify the server explicitly before giving up on the response.
    fn call_tool_cancellable<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a crate::run::CancelToken,
    ) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
        Box::pin(async move {
            if self.is_dead() {
                return Err(McpError::ServerDied {
                    server: self.name.to_string(),
                });
            }
            let request =
                rmcp::model::ClientRequest::CallToolRequest(rmcp::model::CallToolRequest::new(
                    rmcp::model::CallToolRequestParams::new(name.to_string())
                        .with_arguments(arguments_object(args)),
                ));
            let peer = self.service.peer().clone();
            let handle = peer
                .send_cancellable_request(request, rmcp::service::PeerRequestOptions::no_options())
                .await
                .map_err(|e| McpError::RpcError {
                    server: self.name.to_string(),
                    code: 0,
                    message: e.to_string(),
                })?;
            let id = handle.id.clone();
            let waiter = handle.await_response();
            tokio::pin!(waiter);
            tokio::select! {
                result = &mut waiter => {
                    let result = result.map_err(|e| McpError::RpcError {
                        server: self.name.to_string(),
                        code: 0,
                        message: e.to_string(),
                    })?;
                    match result {
                        rmcp::model::ServerResult::CallToolResult(result) => {
                            call_tool_result_to_output(&self.name, result)
                        }
                        // MRTR `input_required` rounds need the session-level
                        // `call_tool` helper; the cancellable path takes the
                        // single-round shape (craft advertises no input mode).
                        _ => Err(McpError::InvalidResponse {
                            server: self.name.to_string(),
                            reason: "unexpected tools/call response".into(),
                        }),
                    }
                }
                _ = cancel.wait() => {
                    let _ = peer
                        .notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                            Some(id),
                            Some("turn cancelled".into()),
                        ))
                        .await;
                    Err(McpError::Cancelled {
                        server: self.name.to_string(),
                    })
                }
                _ = tokio::time::sleep(self.timeout) => {
                    let _ = peer
                        .notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                            Some(id),
                            Some("request timeout".into()),
                        ))
                        .await;
                    Err(McpError::Timeout {
                        server: self.name.to_string(),
                        timeout_ms: self.timeout.as_millis() as u64,
                    })
                }
            }
        })
    }

    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
        Box::pin(async {
            Ok(self
                .prompt_infos
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone())
        })
    }

    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<ResourceInfo>, McpError>> {
        Box::pin(async {
            Ok(self
                .resource_infos
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone())
        })
    }

    fn read_resource<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<String, McpError>> {
        if self.is_dead() {
            return Box::pin(async {
                Err(McpError::ServerDied {
                    server: self.name.to_string(),
                })
            });
        }
        let uri = uri.to_string();
        Box::pin(async move {
            let contents = self.read_resource_raw(&uri).await?;
            Ok(render_resource_contents(&contents))
        })
    }

    fn get_prompt(
        &self,
        name: &str,
        arguments: &HashMap<String, String>,
    ) -> BoxFuture<'_, Result<Vec<PromptMessage>, McpError>> {
        let params = rmcp::model::GetPromptRequestParams::new(name.to_string()).with_arguments(
            arguments
                .clone()
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        );
        let fut = self.service.get_prompt(params);
        Box::pin(async move {
            let result = self.with_timeout(fut).await?;
            Ok(result
                .messages
                .into_iter()
                .map(|message| PromptMessage {
                    role: match message.role {
                        rmcp::model::Role::Assistant => "assistant".into(),
                        rmcp::model::Role::User => "user".into(),
                    },
                    // Non-text prompt content degrades to text: images are
                    // noted, embedded resources inlined (same rules as tool
                    // results), so no message is dropped for being non-text.
                    text: match &message.content {
                        rmcp::model::ContentBlock::Text(text) => Some(text.text.clone()),
                        rmcp::model::ContentBlock::Image(_) => Some("[image omitted]".into()),
                        rmcp::model::ContentBlock::Resource(resource) => {
                            Some(inline_resource(resource))
                        }
                        rmcp::model::ContentBlock::ResourceLink(link) => {
                            Some(format!("resource:{}", link.uri))
                        }
                        rmcp::model::ContentBlock::Audio(_) => Some("[audio omitted]".into()),
                        _ => None,
                    },
                })
                .collect())
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        // Stop the keepalive pings and fail every later call fast.
        self.dead.store(true, Ordering::SeqCst);
        // This hand-rolled waitpid/killpg applies the same discipline as
        // `crate::child_guard::ChildGuard` (kill the process group only while
        // the child is ours and unreaped, since a reaped pid can be recycled),
        // but it cannot literally reuse `ChildGuard`: rmcp's
        // `TokioChildProcess` owns the `tokio::process::Child` (both the
        // `kill_on_drop(true)` set at spawn and rmcp's own drop-kill depend on
        // it) and exposes no adopt/attach API that would let a guard take
        // ownership — taking it after the transport wraps the pipes would
        // double-manage the child. So the split stays: kill_on_drop reaps the
        // *direct* child, while the block below covers its process group.
        self.service.cancellation_token().cancel();
        #[cfg(unix)]
        if let Some(pid) = self.child_pid {
            let pid = pid as libc::pid_t;
            // Stdio is spawned with `process_group(0)`, so the child leads its
            // own group and killpg reaches the grandchildren wrappers (npx &
            // co.) fork into that group — the ones direct-child reaping alone
            // would miss.
            // REMAINING LEAK: a grandchild that calls setsid()/setpgid(0)
            // escapes this process group, so neither killpg nor kill_on_drop
            // reaches it and it outlives shutdown. Closing that needs
            // containment the session layer does not have (a PID namespace or
            // cgroup), so it is documented here rather than fixed.
            let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if reaped == 0 {
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
        }
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize as _;

    fn text(s: &str) -> rmcp::model::ContentBlock {
        rmcp::model::ContentBlock::text(s)
    }

    fn image(data: &str, mime: &str) -> rmcp::model::ContentBlock {
        rmcp::model::ContentBlock::image(data, mime)
    }

    fn embedded(uri: &str, body: &str) -> rmcp::model::ContentBlock {
        rmcp::model::ContentBlock::embedded_text(uri, body)
    }

    fn resource_link(uri: &str) -> rmcp::model::ContentBlock {
        rmcp::model::Resource::deserialize(serde_json::json!({
            "uri": uri,
            "name": "n",
        }))
        .map(rmcp::model::ContentBlock::resource_link)
        .unwrap()
    }

    #[test]
    fn text_blocks_join_with_newlines() {
        let out = join_content(&[text("a"), text("b")]);
        assert_eq!(out.joined_text(), "a\nb");
        assert!(!out.has_images());
    }

    #[test]
    fn image_blocks_are_retained_structured() {
        let out = join_content(&[text("cap"), image("QQ==", "image/png")]);
        assert_eq!(out.joined_text(), "cap");
        assert_eq!(out.parts.len(), 2);
        assert!(
            matches!(out.parts[1], McpPart::Image(ref i) if i.data == "QQ==" && i.mime == "image/png")
        );
    }

    #[test]
    fn embedded_resource_text_is_inlined() {
        let out = join_content(&[embedded("file:///a.txt", "hello")]);
        assert_eq!(out.joined_text(), "hello");
    }

    #[test]
    fn oversized_resource_text_is_truncated() {
        let big = "x".repeat(MAX_RESOURCE_BYTES + 100);
        let out = join_content(&[embedded("file:///big.txt", &big)]);
        let text = out.joined_text();
        assert!(text.len() < big.len());
        assert!(text.contains("... [output truncated]"), "got: {text}");
        assert!(text.contains("file:///big.txt"));
    }

    #[test]
    fn resource_links_render_as_uri_lines() {
        let out = join_content(&[resource_link("file:///db")]);
        assert_eq!(out.joined_text(), "resource:file:///db");
    }

    #[test]
    fn blob_resources_keep_uri_drop_payload() {
        let block = rmcp::model::ContentBlock::resource(
            rmcp::model::ResourceContents::BlobResourceContents {
                uri: "file:///bin".into(),
                mime_type: Some("application/octet-stream".into()),
                blob: "AA==".into(),
                meta: None,
            },
        );
        let text = join_content(&[block]).joined_text();
        assert!(text.contains("resource:file:///bin"), "got: {text}");
        assert!(!text.contains("AA=="));
    }

    fn blob_resource_contents(uri: &str, blob: &str) -> rmcp::model::ResourceContents {
        rmcp::model::ResourceContents::BlobResourceContents {
            uri: uri.into(),
            mime_type: Some("application/octet-stream".into()),
            blob: blob.into(),
            meta: None,
        }
    }

    #[test]
    fn render_resource_contents_keeps_small_blob_intact() {
        let out = render_resource_contents(&[blob_resource_contents("file:///b.bin", "AA==")]);
        assert_eq!(
            out,
            "[base64 blob resource file:///b.bin (application/octet-stream)]\nAA=="
        );
    }

    #[test]
    fn render_resource_contents_clips_oversized_blob() {
        let big = "QUJD".repeat(MAX_RESOURCE_BYTES); // valid base64, 4x the cap
        let out = render_resource_contents(&[blob_resource_contents("file:///big.bin", &big)]);
        assert!(
            out.len() < big.len(),
            "blob must be clipped, got {} bytes",
            out.len()
        );
        assert!(out.contains("... [output truncated]"), "got tail: {out}");
        assert!(out.contains("[base64 blob resource file:///big.bin"));
        assert!(!out.ends_with(&big), "full payload must not be inlined");
    }

    #[test]
    fn mixed_blocks_keep_order_and_parts() {
        let out = join_content(&[
            text("one"),
            image("QQ==", "image/png"),
            embedded("file:///r", "res"),
            resource_link("file:///link"),
            image("Aw==", "image/jpeg"),
            text("two"),
        ]);
        assert_eq!(out.joined_text(), "one\nres\nresource:file:///link\ntwo");
        // Interleaving is preserved: parts appear exactly in block order.
        assert!(matches!(&out.parts[0], McpPart::Text(t) if t == "one"));
        assert!(matches!(&out.parts[1], McpPart::Image(i) if i.mime == "image/png"));
        assert!(matches!(&out.parts[4], McpPart::Image(i) if i.mime == "image/jpeg"));
        assert!(matches!(&out.parts[5], McpPart::Text(t) if t == "two"));
    }
    #[test]
    fn echoed_structured_content_is_not_appended_twice() {
        // A 2025-06-18 server echoes the JSON in a text block; honouring it
        // verbatim must not also append the fenced copy.
        let mut result = rmcp::model::CallToolResult::success(vec![
            serde_json::to_string(&serde_json::json!({"series": [1, 2, 3]}))
                .map(|s| text(&s))
                .unwrap(),
        ]);
        result.structured_content = Some(serde_json::json!({"series": [1, 2, 3]}));
        let out = call_tool_result_to_output(&Arc::from("srv"), result).unwrap();
        assert!(
            !out.joined_text().contains("```"),
            "got: {}",
            out.joined_text()
        );
        assert_eq!(out.parts.len(), 1);
    }

    #[test]
    fn structured_content_is_appended_when_absent_from_blocks() {
        let mut result = rmcp::model::CallToolResult::success(vec![text("chart ready")]);
        result.structured_content = Some(serde_json::json!({"series": [1, 2, 3]}));
        let out = call_tool_result_to_output(&Arc::from("srv"), result).unwrap();
        assert_eq!(out.parts.len(), 2, "text plus fenced JSON");
        let text = out.joined_text();
        assert!(text.contains("chart ready"));
        assert!(text.contains("```json"), "got: {text}");
        assert!(text.contains("\"series\""));
    }

    #[test]
    fn clip_stops_on_char_boundary() {
        // Multibyte chars: 2 bytes each, so a 3-byte cap keeps 1 full char.
        let s = "ééé";
        let (clipped, truncated) = clip(s, 3);
        assert_eq!(clipped, "é");
        assert!(truncated);
        let (all, truncated) = clip(s, 6);
        assert_eq!(all, s);
        assert!(!truncated);
    }

    /// Phase 4, end-to-end over stdio: the server issues `roots/list` and
    /// `sampling/createMessage` back at the client. Roots answer straight
    /// from the threaded workspace root; sampling (and elicitation, same
    /// seam) deny cleanly when no frontend is attached — here the events
    /// carry no `server_requests` sender, the headless shape.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_to_client_requests_roots_and_sampling() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no python3 on this host; skip rather than fail
        }
        const SCRIPT: &str = r#"
import json, sys
roots, sampling_error = None, None
def send(msg): sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    method, rid = req.get("method"), req.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"asker","version":"1"}}})
        send({"jsonrpc":"2.0","id":"r1","method":"roots/list"})
        send({"jsonrpc":"2.0","id":"s1","method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"hi"}}],"maxTokens":16}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"echo","description":"Echo","inputSchema":{"type":"object"}}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"prompts":[]}})
    elif method == "tools/call":
        send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":json.dumps({"roots": roots, "sampling_error": sampling_error})}],"isError":False}})
    elif method is None and rid == "r1":
        roots = req.get("result", {}).get("roots")
    elif method is None and rid == "s1":
        sampling_error = req.get("error", {}).get("message")
"#;
        let config = ServerConfig {
            name: "asker".into(),
            timeout: Duration::from_secs(5),
            transport: Transport::Stdio {
                program: "python3".into(),
                args: vec!["-u".into(), "-c".into(), SCRIPT.into()],
                environment: HashMap::new(),
            },
        };
        let events = McpEvents {
            root: Some(std::path::PathBuf::from("/craft/phase4/cwd")),
            ..McpEvents::default()
        };
        let session = start_session(&config, events)
            .await
            .expect("connect mock server");

        let reply = session
            .call_tool("echo", &serde_json::json!({}))
            .await
            .unwrap()
            .joined_text();
        let value: serde_json::Value = serde_json::from_str(&reply).expect("tool echoes JSON");
        assert_eq!(
            value["sampling_error"]
                .as_str()
                .expect("sampling denied with an error"),
            "sampling denied: no frontend is attached"
        );
        let roots = value["roots"].as_array().expect("roots answered");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0]["uri"], "file:///craft/phase4/cwd");

        session.shutdown().await;
    }

    /// Phase 6, end-to-end over stdio: a cancelled turn tells the server to
    /// stop the in-flight tool call. The mock server parks the `tools/call`
    /// and records receipt of `notifications/cancelled` to an evidence file
    /// (the only channel a child process can hand back); the test waits for
    /// that record and checks it names the parked request id.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_turn_notifies_the_server_to_stop() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // no python3 on this host; skip rather than fail
        }
        const SCRIPT: &str = r#"
import json, os, sys
def send(msg): sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
evidence = os.environ.get("CANCEL_EVIDENCE")
pending = None
for line in sys.stdin:
    msg = json.loads(line)
    method, rid = msg.get("method"), msg.get("id")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"hanger","version":"1"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"hang","description":"Hangs until cancelled","inputSchema":{"type":"object"}}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"prompts":[]}})
    elif method == "tools/call":
        pending = rid
    elif method == "notifications/cancelled":
        if evidence:
            with open(evidence, "a") as f:
                f.write("cancelled " + json.dumps(msg.get("params", {}).get("requestId")) + "\n")
        # Answer so the connection stays clean for shutdown; the client
        # has already given up on the result.
        if pending is not None:
            send({"jsonrpc":"2.0","id":pending,"result":{"content":[{"type":"text","text":"stopped"}],"isError":False}})
            pending = None
"#;
        let evidence = std::env::temp_dir().join(format!(
            "craft-mcp-cancel-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = ServerConfig {
            name: "hanger".into(),
            timeout: Duration::from_secs(30),
            transport: Transport::Stdio {
                program: "python3".into(),
                args: vec!["-u".into(), "-c".into(), SCRIPT.into()],
                environment: HashMap::from([(
                    "CANCEL_EVIDENCE".into(),
                    evidence.display().to_string(),
                )]),
            },
        };
        let session = start_session(&config, McpEvents::default())
            .await
            .expect("connect mock server");

        let (flag, token) = crate::run::cancel_channel();
        let args = serde_json::json!({});
        let call = session.call_tool_cancellable("hang", &args, &token);
        tokio::pin!(call);
        // Let the request reach the server before cancelling the turn.
        tokio::time::sleep(Duration::from_millis(200)).await;
        flag.set(true);
        let result = tokio::time::timeout(Duration::from_secs(5), &mut call)
            .await
            .expect("cancelled call resolves promptly");
        assert!(
            matches!(result, Err(McpError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        // The server's own record: it received notifications/cancelled
        // naming the in-flight tools/call request id.
        let record = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&evidence)
                    && !text.is_empty()
                {
                    return text;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("server recorded the cancel notification");
        assert!(
            record.starts_with("cancelled "),
            "unexpected evidence record: {record}"
        );
        assert_ne!(
            record.trim(),
            "cancelled null",
            "cancel notification named no request id: {record}"
        );
        let _ = std::fs::remove_file(&evidence);
        session.shutdown().await;
    }
}
