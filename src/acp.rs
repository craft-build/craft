//! Forge's ACP client boundary.
//!
//! Protocol objects stay typed all the way to this boundary. The UI consumes
//! `AcpEvent`s and never parses JSON-RPC method names itself.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ClientCapabilities, CloseSessionRequest, ContentBlock,
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, Implementation, InitializeRequest, NewSessionRequest,
    PermissionOption, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, ResourceLink, SelectedPermissionOutcome, SessionConfigOption,
    SessionConfigOptionValue, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Error, Responder};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::async_runtime;
use crate::config::{AgentConfig, TransportConfig};

const INTERACTION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct AnchoredComment {
    pub target: String,
    pub body: String,
}

#[derive(Clone, Debug, Default)]
pub struct TurnInput {
    pub text: String,
    pub context_files: Vec<String>,
    pub comments: Vec<AnchoredComment>,
}

/// ACP has structured resource links but no stable anchored-review-comment
/// block. Files are therefore links; comments use a clearly delimited text
/// block so attribution and grounding survive without pretending they are
/// ordinary user prose.
pub fn compose_prompt(input: TurnInput) -> Vec<ContentBlock> {
    let mut prompt = vec![ContentBlock::Text(TextContent::new(input.text))];
    prompt.extend(input.context_files.into_iter().map(|path| {
        let uri = if path.starts_with("file://") {
            path.clone()
        } else {
            format!("file://{path}")
        };
        ContentBlock::ResourceLink(ResourceLink::new(path, uri))
    }));
    if !input.comments.is_empty() {
        let mut grounded = String::from("[Forge anchored review context]\n");
        for comment in input.comments {
            grounded.push_str(&format!(
                "- target: {}\n  comment: {}\n",
                comment.target, comment.body
            ));
        }
        grounded.push_str("[/Forge anchored review context]");
        prompt.push(ContentBlock::Text(TextContent::new(grounded)));
    }
    prompt
}

#[derive(Debug)]
pub enum PermissionDecision {
    Select(String),
    Cancel,
}

#[derive(Debug)]
pub enum ElicitationDecision {
    Accept(BTreeMap<String, ElicitationContentValue>),
    Decline,
    Cancel,
}

#[derive(Debug)]
pub struct PendingPermission {
    pub title: String,
    pub options: Vec<PermissionOption>,
    decision: Option<oneshot::Sender<PermissionDecision>>,
}

impl PendingPermission {
    pub fn respond(mut self, decision: PermissionDecision) {
        if let Some(sender) = self.decision.take() {
            let _ = sender.send(decision);
        }
    }
}

#[derive(Debug)]
pub struct PendingElicitation {
    pub request: CreateElicitationRequest,
    decision: Option<oneshot::Sender<ElicitationDecision>>,
}

impl PendingElicitation {
    pub fn respond(mut self, decision: ElicitationDecision) {
        if let Some(sender) = self.decision.take() {
            let _ = sender.send(decision);
        }
    }
}

#[derive(Debug)]
pub enum AcpEvent {
    Connecting,
    Connected { agent_name: Option<String> },
    Reconnecting { attempt: u32, reason: String },
    SessionReady { session_id: String },
    ConfigOptions(Vec<SessionConfigOption>),
    TurnStarted,
    Update(SessionUpdate),
    Permission(PendingPermission),
    Elicitation(PendingElicitation),
    TurnFinished,
    TurnCancelled,
    Error(String),
    Disconnected(String),
}

pub async fn validate_agent(config: AgentConfig) -> Result<(), String> {
    let agent = config.agent()?;
    let handshake =
        Client
            .builder()
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(
                        InitializeRequest::new(ProtocolVersion::V1)
                            .client_capabilities(ClientCapabilities::default())
                            .client_info(Implementation::new("Forge", env!("CARGO_PKG_VERSION"))),
                    )
                    .block_task()
                    .await?;
                Ok::<(), Error>(())
            });

    match tokio::time::timeout(VALIDATION_TIMEOUT, handshake).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("ACP initialization failed: {error}")),
        Err(_) => Err(format!(
            "ACP initialization timed out after {} seconds",
            VALIDATION_TIMEOUT.as_secs()
        )),
    }
}

enum AcpCommand {
    Prompt(Vec<ContentBlock>),
    SetConfig {
        id: String,
        value: SessionConfigOptionValue,
    },
    Cancel,
}

#[derive(Clone)]
pub struct AcpClient {
    commands: mpsc::UnboundedSender<AcpCommand>,
}

impl AcpClient {
    pub fn connect(
        config: AgentConfig,
        local_workspace: PathBuf,
        resume_session: Option<String>,
    ) -> (Self, mpsc::UnboundedReceiver<AcpEvent>) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let command_rx = Arc::new(Mutex::new(command_rx));
        async_runtime::spawn(run_with_reconnect(
            config,
            local_workspace,
            resume_session,
            command_rx,
            events,
        ));
        (Self { commands }, event_rx)
    }

    pub fn prompt(&self, input: TurnInput) -> Result<(), String> {
        self.commands
            .send(AcpCommand::Prompt(compose_prompt(input)))
            .map_err(|_| "ACP connection is not running".into())
    }

    pub fn cancel(&self) -> Result<(), String> {
        self.commands
            .send(AcpCommand::Cancel)
            .map_err(|_| "ACP connection is not running".into())
    }

    pub fn set_config(&self, id: String, value: SessionConfigOptionValue) -> Result<(), String> {
        self.commands
            .send(AcpCommand::SetConfig { id, value })
            .map_err(|_| "ACP connection is not running".into())
    }
}

async fn run_with_reconnect(
    config: AgentConfig,
    local_workspace: PathBuf,
    resume_session: Option<String>,
    commands: Arc<Mutex<mpsc::UnboundedReceiver<AcpCommand>>>,
    events: mpsc::UnboundedSender<AcpEvent>,
) {
    let mut attempt = 0_u32;
    loop {
        let _ = events.send(if attempt == 0 {
            AcpEvent::Connecting
        } else {
            AcpEvent::Reconnecting {
                attempt,
                reason: "agent transport closed".into(),
            }
        });

        let result = run_connection(
            config.clone(),
            local_workspace.clone(),
            resume_session.clone(),
            commands.clone(),
            events.clone(),
        )
        .await;
        let reason = result
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| "agent exited".into());
        let _ = events.send(AcpEvent::Disconnected(reason.clone()));

        if !matches!(config.transport, TransportConfig::Ssh { .. }) {
            return;
        }
        attempt += 1;
        let delay = Duration::from_secs(2_u64.saturating_pow(attempt.min(5)));
        let _ = events.send(AcpEvent::Reconnecting { attempt, reason });
        tokio::time::sleep(delay).await;
    }
}

async fn run_connection(
    config: AgentConfig,
    local_workspace: PathBuf,
    resume_session: Option<String>,
    commands: Arc<Mutex<mpsc::UnboundedReceiver<AcpCommand>>>,
    events: mpsc::UnboundedSender<AcpEvent>,
) -> Result<(), Error> {
    let permission_events = events.clone();
    let permission_prompt_events = events.clone();
    let elicitation_events = events.clone();
    let agent = config
        .agent()
        .map_err(|message| Error::internal_error().data(message))?;

    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                permission_events
                    .send(AcpEvent::Update(notification.update))
                    .map_err(Error::into_internal_error)
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest,
                        responder: Responder<RequestPermissionResponse>,
                        _connection| {
                let (decision_tx, decision_rx) = oneshot::channel();
                let pending = PendingPermission {
                    title: request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "Agent operation".into()),
                    options: request.options,
                    decision: Some(decision_tx),
                };
                if permission_prompt_events
                    .send(AcpEvent::Permission(pending))
                    .is_err()
                {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                let outcome = match tokio::time::timeout(INTERACTION_TIMEOUT, decision_rx).await {
                    Ok(Ok(PermissionDecision::Select(option_id))) => {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option_id,
                        ))
                    }
                    _ => RequestPermissionOutcome::Cancelled,
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateElicitationRequest,
                        responder: Responder<CreateElicitationResponse>,
                        _connection| {
                let (decision_tx, decision_rx) = oneshot::channel();
                let pending = PendingElicitation {
                    request,
                    decision: Some(decision_tx),
                };
                if elicitation_events
                    .send(AcpEvent::Elicitation(pending))
                    .is_err()
                {
                    return responder
                        .respond(CreateElicitationResponse::new(ElicitationAction::Cancel));
                }
                let action = match tokio::time::timeout(INTERACTION_TIMEOUT, decision_rx).await {
                    Ok(Ok(ElicitationDecision::Accept(content))) => {
                        ElicitationAction::Accept(ElicitationAcceptAction::new().content(content))
                    }
                    Ok(Ok(ElicitationDecision::Decline)) => ElicitationAction::Decline,
                    _ => ElicitationAction::Cancel,
                };
                responder.respond(CreateElicitationResponse::new(action))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            let capabilities = ClientCapabilities::default().elicitation(
                ElicitationCapabilities::new().form(ElicitationFormCapabilities::default()),
            );
            let initialized = connection
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1)
                        .client_capabilities(capabilities)
                        .client_info(Implementation::new("Forge", env!("CARGO_PKG_VERSION"))),
                )
                .block_task()
                .await?;
            let _ = events.send(AcpEvent::Connected {
                agent_name: initialized
                    .agent_info
                    .as_ref()
                    .map(|info| info.name.clone()),
            });
            let workspace = config.workspace_for(&local_workspace);
            let (session_id, config_options) = if let Some(session_id) =
                resume_session.filter(|_| initialized.agent_capabilities.load_session)
            {
                let session_id = agent_client_protocol::schema::v1::SessionId::new(session_id);
                let loaded = connection
                    .send_request(agent_client_protocol::schema::v1::LoadSessionRequest::new(
                        session_id.clone(),
                        workspace,
                    ))
                    .block_task()
                    .await?;
                (session_id, loaded.config_options)
            } else {
                let opened = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                (opened.session_id, opened.config_options)
            };
            if let Some(config_options) = config_options {
                let _ = events.send(AcpEvent::ConfigOptions(config_options));
            }
            let _ = events.send(AcpEvent::SessionReady {
                session_id: session_id.to_string(),
            });

            while let Some(command) = commands.lock().await.recv().await {
                match command {
                    AcpCommand::Prompt(prompt) => {
                        let _ = events.send(AcpEvent::TurnStarted);
                        let prompt_events = events.clone();
                        let prompt_connection = connection.clone();
                        let prompt_session = session_id.clone();
                        tokio::spawn(async move {
                            let result = prompt_connection
                                .send_request(PromptRequest::new(prompt_session, prompt))
                                .block_task()
                                .await;
                            let _ = prompt_events.send(match result {
                                Ok(response) if response.stop_reason == StopReason::Cancelled => {
                                    AcpEvent::TurnCancelled
                                }
                                Ok(_) => AcpEvent::TurnFinished,
                                Err(error) => {
                                    AcpEvent::Error(format!("agent turn failed: {error}"))
                                }
                            });
                        });
                    }
                    AcpCommand::Cancel => {
                        connection
                            .send_notification(CancelNotification::new(session_id.clone()))?;
                        let _ = events.send(AcpEvent::TurnCancelled);
                    }
                    AcpCommand::SetConfig { id, value } => {
                        let response = connection
                            .send_request(SetSessionConfigOptionRequest::new(
                                session_id.clone(),
                                id,
                                value,
                            ))
                            .block_task()
                            .await;
                        match response {
                            Ok(response) => {
                                let _ =
                                    events.send(AcpEvent::ConfigOptions(response.config_options));
                            }
                            Err(error) => {
                                let _ = events.send(AcpEvent::Error(format!(
                                    "could not change agent configuration: {error}"
                                )));
                            }
                        }
                    }
                }
            }
            connection
                .send_request(CloseSessionRequest::new(session_id))
                .block_task()
                .await?;
            Ok(())
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_remain_distinct_from_user_message() {
        let prompt = compose_prompt(TurnInput {
            text: "Please revise it".into(),
            context_files: vec!["/tmp/main.rs".into()],
            comments: vec![AnchoredComment {
                target: "src/main.rs line 8".into(),
                body: "Keep this branch".into(),
            }],
        });
        assert_eq!(prompt.len(), 3);
        assert!(matches!(&prompt[1], ContentBlock::ResourceLink(_)));
        let ContentBlock::Text(comment) = &prompt[2] else {
            panic!("expected grounded comment text")
        };
        assert!(comment.text.contains("target: src/main.rs line 8"));
    }

    #[test]
    fn prompt_without_context_contains_only_user_text() {
        let prompt = compose_prompt(TurnInput {
            text: "Hello".into(),
            ..TurnInput::default()
        });

        assert_eq!(prompt.len(), 1);
        let ContentBlock::Text(text) = &prompt[0] else {
            panic!("expected user text")
        };
        assert_eq!(text.text, "Hello");
    }

    #[test]
    fn context_file_uris_are_normalized_without_double_prefixing() {
        let prompt = compose_prompt(TurnInput {
            context_files: vec![
                "/tmp/main.rs".into(),
                "file:///tmp/already-prefixed.rs".into(),
            ],
            ..TurnInput::default()
        });

        let encoded = serde_json::to_string(&prompt).unwrap();
        assert!(encoded.contains("file:///tmp/main.rs"));
        assert!(encoded.contains("file:///tmp/already-prefixed.rs"));
        assert!(!encoded.contains("file://file://"));
    }

    #[test]
    fn multiple_comments_are_kept_in_one_delimited_context_block() {
        let prompt = compose_prompt(TurnInput {
            comments: vec![
                AnchoredComment {
                    target: "src/a.rs:1".into(),
                    body: "First".into(),
                },
                AnchoredComment {
                    target: "src/b.rs:2".into(),
                    body: "Second".into(),
                },
            ],
            ..TurnInput::default()
        });

        assert_eq!(prompt.len(), 2);
        let ContentBlock::Text(context) = &prompt[1] else {
            panic!("expected anchored context")
        };
        assert!(
            context
                .text
                .starts_with("[Forge anchored review context]\n")
        );
        assert!(
            context
                .text
                .contains("- target: src/a.rs:1\n  comment: First\n")
        );
        assert!(
            context
                .text
                .contains("- target: src/b.rs:2\n  comment: Second\n")
        );
        assert!(context.text.ends_with("[/Forge anchored review context]"));
    }

    #[test]
    fn validation_rejects_a_command_that_cannot_start() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(validate_agent(AgentConfig {
            agent_command: "forge-test-command-that-does-not-exist".into(),
            transport: TransportConfig::Local,
        }));

        assert!(result.is_err());
    }
}
