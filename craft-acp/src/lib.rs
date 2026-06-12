//! ACP (Agent Client Protocol) server for craft.
//!
//! Implements `initialize`, `authenticate`, `session/new`, `session/load`,
//! `session/prompt`, `session/cancel`, and `session/set_mode` over stdio.

mod event_bridge;
pub mod fs_proxy;
mod permissions;
mod plan;
pub mod prompt;
mod session;
mod terminal_proxy;
mod tool_kinds;

use agent_client_protocol::Agent;
use agent_client_protocol::Stdio;
use agent_client_protocol::on_receive_notification;
use agent_client_protocol::on_receive_request;
use agent_client_protocol::schema::AgentCapabilities;
use agent_client_protocol::schema::AuthMethod;
use agent_client_protocol::schema::AuthMethodAgent;
use agent_client_protocol::schema::AuthenticateRequest;
use agent_client_protocol::schema::AuthenticateResponse;
use agent_client_protocol::schema::CancelNotification;
use agent_client_protocol::schema::ContentBlock as AcpContentBlock;
use agent_client_protocol::schema::ContentChunk;
use agent_client_protocol::schema::FileSystemCapabilities;
use agent_client_protocol::schema::Implementation;
use agent_client_protocol::schema::InitializeRequest;
use agent_client_protocol::schema::InitializeResponse;
use agent_client_protocol::schema::LoadSessionRequest;
use agent_client_protocol::schema::LoadSessionResponse;
use agent_client_protocol::schema::McpCapabilities;
use agent_client_protocol::schema::NewSessionRequest;
use agent_client_protocol::schema::NewSessionResponse;
use agent_client_protocol::schema::PromptCapabilities;
use agent_client_protocol::schema::PromptRequest;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::SessionId;
use agent_client_protocol::schema::SessionNotification;
use agent_client_protocol::schema::SessionUpdate;
use agent_client_protocol::schema::SetSessionModeRequest;
use agent_client_protocol::schema::SetSessionModeResponse;
use agent_client_protocol::util::internal_error;
use color_eyre::Result;
use color_eyre::eyre::Context;
use craft_config::RawConfig;
use craft_config::load_env_files;
use craft_config::load_permissions;
use craft_providers::ContentBlock as ProviderContentBlock;
use craft_providers::Message as ProviderMessage;
use craft_providers::Role as ProviderRole;
use craft_providers::TokenUsage;
use craft_providers::provider::ProviderKind;
use craft_storage::sessions::Session as StoredSession;
use craft_storage::StateDir;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use strum::IntoEnumIterator;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

use crate::session::Runtime;
use crate::session::SessionState;
use crate::session::Sessions;

const AGENT_NAME: &str = "craft";
const AGENT_TITLE: &str = "Craft";
const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const UNKNOWN_AUTH_METHOD_MSG: &str = "unknown authentication method";
const PROVIDER_UNAVAILABLE_MSG: &str = "provider credentials not available; run `craft auth login`";

/// Run the ACP server on stdio. Blocks until the client disconnects.
pub async fn run_stdio() -> Result<()> {
    let runtime = Arc::new(bootstrap_runtime().await?);
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    let auth_methods = discover_auth_methods().await;

    let init_methods = auth_methods.clone();
    let init_fs_caps = runtime.fs_caps.clone();
    let init_terminal_cap = runtime.terminal_cap.clone();
    let prompt_sessions = sessions.clone();
    let prompt_runtime = runtime.clone();
    let new_sessions = sessions.clone();
    let cancel_sessions = sessions.clone();
    let mode_sessions = sessions.clone();
    let mode_runtime = runtime.clone();
    let load_sessions = sessions.clone();
    let load_runtime = runtime.clone();

    Agent
        .builder()
        .name(AGENT_NAME)
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                let caps = req.client_capabilities;
                tracing::info!(
                    fs_read = caps.fs.read_text_file,
                    fs_write = caps.fs.write_text_file,
                    terminal = caps.terminal,
                    "acp initialize"
                );
                *init_fs_caps.write().await = caps.fs;
                *init_terminal_cap.write().await = caps.terminal;
                responder.respond(build_initialize_response(init_methods.clone()))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async |req: AuthenticateRequest, responder, _cx| {
                let id: &str = &req.method_id.0;
                tracing::info!(method_id = id, "acp authenticate");
                match authenticate_method(id).await {
                    Ok(()) => responder.respond(AuthenticateResponse::new()),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, _cx| {
                let id = session::new_session(&new_sessions, req.cwd).await;
                tracing::info!(session_id = %id, "session/new");
                responder.respond(
                    NewSessionResponse::new(SessionId::new(id))
                        .modes(Some(session::available_modes())),
                )
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx| {
                tracing::info!(session_id = %req.session_id.0, "session/load");
                match load_session(&load_sessions, &load_runtime, req, cx).await {
                    Ok(resp) => responder.respond(resp),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx| {
                tracing::info!(session_id = %req.session_id.0, "session/prompt");
                match session::run_prompt(
                    &prompt_sessions,
                    &prompt_runtime,
                    req.session_id,
                    req.prompt,
                    cx,
                )
                .await
                {
                    Ok(response) => responder.respond(response),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |notif: CancelNotification, _cx| {
                tracing::info!(session_id = %notif.session_id.0, "session/cancel");
                session::cancel(&cancel_sessions, &notif.session_id).await;
                Ok(())
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: SetSessionModeRequest, responder, cx| {
                let mode_id: &str = &req.mode_id.0;
                tracing::info!(session_id = %req.session_id.0, mode_id, "session/set_mode");
                match session::set_mode(
                    &mode_sessions,
                    &mode_runtime,
                    &req.session_id,
                    mode_id,
                    &cx,
                )
                .await
                {
                    Ok(()) => responder.respond(SetSessionModeResponse::new()),
                    Err(err) => responder.respond_with_error(err),
                }
            },
            on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
        .map_err(|e| color_eyre::eyre::eyre!("acp connection failed: {e}"))?;
    Ok(())
}

async fn bootstrap_runtime() -> Result<Runtime> {
    let storage = StateDir::resolve().context("resolve data directory")?;
    craft_providers::tier_map::load_from_storage(&storage);

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    load_env_files(&cwd);

    let mut config = RawConfig::default()
        .into_config(false)
        .context("invalid default config")?;
    config.permissions = load_permissions(&cwd);
    config.validate().context("validate config")?;
    let plugins_config = config.plugins.clone();

    let timeouts = craft_providers::Timeouts {
        connect: config.provider.connect_timeout,
        low_speed: config.provider.low_speed_timeout,
        stream: config.provider.stream_timeout,
    };

    let model = resolve_default_model(&config.provider, &storage).await?;

    Ok(Runtime {
        model,
        config: config.agent,
        compression: config.compression,
        permissions_config: config.permissions,
        timeouts,
        state_dir: storage,
        fs_caps: Arc::new(RwLock::new(FileSystemCapabilities::new())),
        terminal_cap: Arc::new(RwLock::new(false)),
        plugins_config,
    })
}

async fn resolve_default_model(
    provider_config: &craft_config::ProviderConfig,
    storage: &StateDir,
) -> Result<craft_providers::model::Model> {
    if let Some(spec) = craft_storage::model::read_model(storage)
        && let Ok(m) = craft_providers::model::Model::from_spec(&spec)
    {
        return Ok(m);
    }
    if let Some(spec) = provider_config.default_model.as_deref() {
        return craft_providers::model::Model::from_spec(spec)
            .context("invalid default_model in config");
    }
    color_eyre::eyre::bail!(
        "no model configured: set ANTHROPIC_API_KEY (or another provider) and run `craft auth login`, or set provider.default_model in config"
    )
}

fn build_initialize_response(auth_methods: Vec<AuthMethod>) -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::LATEST)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .mcp_capabilities(McpCapabilities::new().http(true).sse(true))
                .prompt_capabilities(
                    PromptCapabilities::new()
                        .image(true)
                        .embedded_context(true),
                ),
        )
        .auth_methods(auth_methods)
        .agent_info(Some(
            Implementation::new(AGENT_NAME, AGENT_VERSION).title(Some(AGENT_TITLE.to_string())),
        ))
}

async fn discover_auth_methods() -> Vec<AuthMethod> {
    let mut methods = Vec::new();
    for kind in ProviderKind::iter() {
        if kind.is_available().await {
            methods.push(AuthMethod::Agent(
                AuthMethodAgent::new(kind.to_string().to_lowercase(), kind.display_name())
                    .description(Some(format!(
                        "Authenticate with {} via craft auth login",
                        kind.display_name()
                    ))),
            ));
        }
    }
    methods
}

/// Probe whether the requested provider is currently available. We have no
/// way to drive a real OAuth/API-key flow from the ACP transport, so all we
/// can do is verify that credentials are already on disk / in env.
async fn authenticate_method(method_id: &str) -> Result<(), agent_client_protocol::Error> {
    let kind = ProviderKind::from_str(method_id)
        .map_err(|_| internal_error(format!("{UNKNOWN_AUTH_METHOD_MSG}: {method_id}")))?;
    if kind.is_available().await {
        Ok(())
    } else {
        Err(internal_error(format!(
            "{PROVIDER_UNAVAILABLE_MSG} ({})",
            kind.display_name()
        )))
    }
}

/// Load a persisted craft session, replay its messages as ACP notifications,
/// and register the rebuilt `SessionState` so subsequent `session/prompt`
/// calls continue from where the saved session left off.
async fn load_session(
    sessions: &Sessions,
    _runtime: &Runtime,
    req: LoadSessionRequest,
    cx: agent_client_protocol::ConnectionTo<agent_client_protocol::role::acp::Client>,
) -> Result<LoadSessionResponse, agent_client_protocol::Error> {
    type StoredCraftSession =
        StoredSession<ProviderMessage, TokenUsage, craft_agent::ToolOutput>;

    let id_arc: Arc<str> = req.session_id.0.clone();
    let id_str = id_arc.to_string();
    let storage = StateDir::resolve()
        .map_err(|e| internal_error(format!("resolve state dir: {e}")))?;

    let stored = StoredCraftSession::load(&id_str, &storage)
        .map_err(|e| internal_error(format!("load session {id_str}: {e}")))?;

    for msg in &stored.messages {
        if let Some(update) = message_to_update(msg) {
            cx.send_notification(SessionNotification::new(req.session_id.clone(), update))?;
        }
    }

    let mut state = SessionState::fresh(req.cwd.clone());
    state.history = craft_agent::History::new(stored.messages);
    session::insert_loaded_session(sessions, id_arc, state).await;

    Ok(LoadSessionResponse::new().modes(Some(session::available_modes())))
}

/// Convert a stored craft message into an ACP `SessionUpdate` for replay.
/// Only the visible text portion is replayed; tool calls, thinking blocks,
/// and images are intentionally dropped.
fn message_to_update(msg: &ProviderMessage) -> Option<SessionUpdate> {
    let text: String = msg
        .content
        .iter()
        .filter_map(|b| match b {
            ProviderContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    let chunk = ContentChunk::new(AcpContentBlock::from(text));
    match msg.role {
        ProviderRole::User => Some(SessionUpdate::UserMessageChunk(chunk)),
        ProviderRole::Assistant => Some(SessionUpdate::AgentMessageChunk(chunk)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_response_advertises_v1() {
        let resp = build_initialize_response(vec![]);
        assert_eq!(resp.protocol_version, ProtocolVersion::LATEST);
        assert!(resp.agent_capabilities.prompt_capabilities.image);
        assert!(resp.agent_capabilities.prompt_capabilities.embedded_context);
        assert!(resp.agent_capabilities.load_session);
        assert!(resp.agent_capabilities.mcp_capabilities.http);
        assert!(resp.agent_capabilities.mcp_capabilities.sse);
        assert!(resp.auth_methods.is_empty());
        let info = resp.agent_info.expect("agent_info present");
        assert_eq!(info.name, AGENT_NAME);
        assert_eq!(info.version, AGENT_VERSION);
    }

    #[test]
    fn initialize_response_includes_auth_methods() {
        let methods = vec![AuthMethod::Agent(AuthMethodAgent::new("anthropic", "Anthropic"))];
        let resp = build_initialize_response(methods);
        assert_eq!(resp.auth_methods.len(), 1);
        assert_eq!(&*resp.auth_methods[0].id().0, "anthropic");
    }

    #[tokio::test]
    async fn authenticate_method_rejects_unknown_id() {
        let err = authenticate_method("not-a-real-provider").await.unwrap_err();
        let data = err.data.expect("data attached").to_string();
        assert!(data.contains(UNKNOWN_AUTH_METHOD_MSG), "data was: {data}");
    }

    #[test]
    fn message_to_update_assistant_text_becomes_agent_chunk() {
        let msg = ProviderMessage {
            role: ProviderRole::Assistant,
            content: vec![ProviderContentBlock::Text {
                text: "hi".into(),
            }],
            display_text: None,
        };
        assert!(matches!(
            message_to_update(&msg),
            Some(SessionUpdate::AgentMessageChunk(_))
        ));
    }

    #[test]
    fn message_to_update_skips_messages_without_text() {
        let msg = ProviderMessage {
            role: ProviderRole::User,
            content: vec![],
            display_text: None,
        };
        assert!(message_to_update(&msg).is_none());
    }
}
