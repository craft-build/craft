//! rmcp-backed session adapter: spawn transports, wrap requests with timeouts.
//!
//! The manager speaks only to the [`McpSession`] trait, so tests can substitute
//! fakes without touching rmcp.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::config::{ServerConfig, Transport};
use super::error::McpError;

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Tool metadata in the manager's own vocabulary.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Prompt metadata in the manager's own vocabulary.
#[derive(Debug, Clone)]
pub struct PromptInfo {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone)]
pub struct PromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

/// One message of a rendered prompt.
#[derive(Debug, Clone)]
pub struct PromptMessage {
    pub role: String,
    pub text: Option<String>,
}

/// Seam between the manager and a live MCP server session.
pub trait McpSession: Send + Sync {
    fn server_name(&self) -> &str;
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>>;
    fn call_tool(&self, name: &str, args: &Value) -> BoxFuture<'_, Result<String, McpError>>;
    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>>;
    fn get_prompt(
        &self,
        name: &str,
        arguments: &HashMap<String, String>,
    ) -> BoxFuture<'_, Result<Vec<PromptMessage>, McpError>>;
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}

/// Start one server from its config: build the transport, run the `initialize`
/// handshake, and return the live session.
pub async fn start_session(config: &ServerConfig) -> Result<Arc<dyn McpSession>, McpError> {
    match connect(config).await {
        Ok(session) => Ok(session),
        // Transient-failure retry: one more attempt for errors that look like
        // transport blips rather than config or auth problems.
        Err(e) if is_transient(&e) => {
            tracing::warn!(server = %config.name, error = %e, "MCP connect failed; retrying once");
            connect(config).await
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

async fn connect(config: &ServerConfig) -> Result<Arc<dyn McpSession>, McpError> {
    let name: Arc<str> = Arc::from(config.name.as_str());
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
            let service = run_initialize(config, transport).await?;
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
                Ok(state_dir) => super::oauth::stored_manager(&config.name, url, &state_dir).await,
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
                        rmcp::transport::streamable_http_client::StreamableHttpClientTransport::with_client(auth_client, cfg),
                    )
                    .await?
                }
                None => {
                    run_initialize(
                        config,
                        rmcp::transport::streamable_http_client::StreamableHttpClientTransport::from_config(cfg),
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

    tracing::info!(
        server = %config.name,
        tool_count = tool_infos.len(),
        prompt_count = prompt_infos.len(),
        "MCP server initialized"
    );

    Ok(Arc::new(RmcpSession {
        name,
        service,
        child_pid,
        tool_infos,
        prompt_infos,
        timeout: config.timeout,
    }))
}

type ClientService = rmcp::service::RunningService<rmcp::RoleClient, ()>;

async fn run_initialize<T, E, A>(
    config: &ServerConfig,
    transport: T,
) -> Result<ClientService, McpError>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    use rmcp::service::ServiceExt as _;

    let server = config.name.clone();
    let init = tokio::time::timeout(config.timeout, ().serve(transport)).await;
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
}

fn capabilities_of(service: &ClientService) -> Capabilities {
    match service.peer_info() {
        Some(info) => Capabilities {
            tools: info.capabilities.tools.is_some(),
            prompts: info.capabilities.prompts.is_some(),
        },
        None => Capabilities::default(),
    }
}

/// A live rmcp session. Tool/prompt listings are captured at connect time
/// (they are read-mostly; Reconnect picks up changes).
struct RmcpSession {
    name: Arc<str>,
    service: ClientService,
    /// stdio child's pid (its process-group id); `None` for HTTP sessions.
    child_pid: Option<u32>,
    tool_infos: Vec<ToolInfo>,
    prompt_infos: Vec<PromptInfo>,
    timeout: Duration,
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

fn join_text(content: &[rmcp::model::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| block.as_text().map(|text| text.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

impl McpSession for RmcpSession {
    fn server_name(&self) -> &str {
        &self.name
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
        Box::pin(async { Ok(self.tool_infos.clone()) })
    }

    fn call_tool(&self, name: &str, args: &Value) -> BoxFuture<'_, Result<String, McpError>> {
        let params = rmcp::model::CallToolRequestParams::new(name.to_string())
            .with_arguments(arguments_object(args));
        let fut = self.service.call_tool(params);
        Box::pin(async move {
            let result = self.with_timeout(fut).await?;
            let text = join_text(&result.content);
            if result.is_error.unwrap_or(false) {
                return Err(McpError::RpcError {
                    server: self.name.to_string(),
                    code: -1,
                    message: text,
                });
            }
            Ok(text)
        })
    }

    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
        Box::pin(async { Ok(self.prompt_infos.clone()) })
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
                    text: message.content.as_text().map(|text| text.text.clone()),
                })
                .collect())
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        // Cancelling the token terminates the transport task; kill_on_drop
        // reaps the direct child. For stdio, wrappers (npx & co.) fork
        // grandchildren the drop does not reach, so signal the whole process
        // group — but only while the child is still ours and unreaped, since
        // a reaped pid can be recycled (same guard as `child_guard.rs`).
        self.service.cancellation_token().cancel();
        #[cfg(unix)]
        if let Some(pid) = self.child_pid {
            let pid = pid as libc::pid_t;
            let reaped = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
            if reaped == 0 {
                unsafe { libc::killpg(pid, libc::SIGKILL) };
            }
        }
        Box::pin(async {})
    }
}
