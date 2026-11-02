// ---------------------------------------------------------------------------
// ACP permission gate
// ---------------------------------------------------------------------------

use agent_client_protocol::{
    Client as AcpClient, ConnectionTo,
    schema::v1::{
        Content, ContentBlock, PermissionOption, PermissionOptionId, PermissionOptionKind,
        RequestPermissionOutcome, RequestPermissionRequest, SessionId, TextContent,
        ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    },
};
use std::sync::Arc;

use crate::run::approval::{Approval, ApprovalAsk, ApprovalResponder};

use super::turn::tool_title;

/// Permission option ids on `session/request_permission`.
const ALLOW_ONCE_ID: &str = "allow_once";
const ALLOW_ALWAYS_ID: &str = "allow_always";
const REJECT_ONCE_ID: &str = "reject_once";
const REJECT_ALWAYS_ID: &str = "reject_always";

/// The four options every `session/request_permission` carries.
pub(super) fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ONCE_ID),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(ALLOW_ALWAYS_ID),
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ONCE_ID),
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::from(REJECT_ALWAYS_ID),
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Map the client's choice onto the engine's answers, matching the
/// reference `craft-acp/src/permissions.rs`.
pub(super) fn outcome_to_answer(
    outcome: &RequestPermissionOutcome,
) -> crate::permissions::PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_ONCE_ID => crate::permissions::PermissionAnswer::AllowOnce,
            ALLOW_ALWAYS_ID => crate::permissions::PermissionAnswer::AllowSession,
            REJECT_ONCE_ID => crate::permissions::PermissionAnswer::Deny,
            REJECT_ALWAYS_ID => crate::permissions::PermissionAnswer::DenyAlwaysLocal,
            _ => crate::permissions::PermissionAnswer::Deny,
        },
        RequestPermissionOutcome::Cancelled => crate::permissions::PermissionAnswer::Deny,
        _ => crate::permissions::PermissionAnswer::Deny,
    }
}

/// The `session/request_permission` payload for one gated call.
pub(super) fn permission_request(
    session_id: &SessionId,
    id: &str,
    name: &str,
    arguments: &serde_json::Value,
    scopes: &[String],
) -> RequestPermissionRequest {
    let mut fields = ToolCallUpdateFields::new()
        .title(tool_title(name, arguments))
        .status(ToolCallStatus::Pending)
        .content(vec![ToolCallContent::Content(Content::new(
            ContentBlock::Text(TextContent::new(scopes.join("\n"))),
        ))]);
    fields.name = Some(name.to_owned());
    fields.raw_input = Some(arguments.clone());
    RequestPermissionRequest::new(
        session_id.clone(),
        ToolCallUpdate::new(ToolCallId::new(id), fields),
        permission_options(),
    )
}

/// The ACP approval responder: where the policy layer would prompt, the
/// client answers a `session/request_permission` instead of the user
/// answering the TUI overlay. Permission rules live in the policy layer
/// this is only the interaction.
///
/// Asks serialize on an async mutex: clients (and their permission UIs)
/// handle one request at a time, exactly like the TUI's single modal.
pub(super) struct AcpPermissionGate {
    pub(super) connection: ConnectionTo<AcpClient>,
    pub(super) session_id: SessionId,
    /// The session's per-turn cancellation cell; each ask races whatever
    /// turn is current.
    pub(super) cancel: std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    /// Serializes asks; a client sees one request at a time.
    ask_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AcpPermissionGate {
    pub(super) fn new(
        connection: ConnectionTo<AcpClient>,
        session_id: SessionId,
        cancel: std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    ) -> Self {
        Self {
            connection,
            session_id,
            cancel,
            ask_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

impl ApprovalResponder for AcpPermissionGate {
    fn ask(&self, ask: ApprovalAsk<'_>) -> crate::run::BoxFuture<Approval> {
        // Owned copies: the boxed future outlives the borrowed ask.
        let id = ask.id.to_owned();
        let name = ask.name.to_owned();
        let input = ask.input.clone();
        let scopes = ask.scopes.to_vec();
        let tool = crate::permissions::ToolKey::parse(&name);
        let connection = self.connection.clone();
        let session_id = self.session_id.clone();
        let cancel = self.cancel.clone();
        let ask_lock = self.ask_lock.clone();
        Box::pin(async move {
            // Held across the whole ask, so a second gated call queues
            // behind this one's answer instead of racing a second request.
            let _serial = ask_lock.lock().await;
            let turn_cancel = (*cancel.load_full()).clone();
            if turn_cancel.is_cancelled() {
                return Approval::Stop {
                    reason: "cancelled by client".into(),
                };
            }
            let request = permission_request(&session_id, &id, &name, &input, &scopes);
            // Dropping the sent request on cancel asks the client to cancel it.
            let sent = connection.send_request(request);
            tokio::select! {
                biased;
                () = turn_cancel.cancelled() => Approval::Stop {
                    reason: "cancelled by client".into(),
                },
                outcome = tokio::time::timeout(
                    crate::permissions::ASK_TIMEOUT,
                    sent.block_task(),
                ) => match outcome {
                    Ok(Ok(response)) => {
                        answer_to_approval(outcome_to_answer(&response.outcome), &tool, &scopes)
                    }
                    // A timeout, dead connection, or client error denies —
                    // but never wedges the turn.
                    _ => Approval::Deny {
                        message: denied_message(&tool, &scopes),
                        remember: None,
                    },
                },
            }
        })
    }
}

pub(super) fn denied_message(tool: &crate::permissions::ToolKey, scopes: &[String]) -> String {
    crate::permissions::PermissionError::new(&tool.to_string(), scopes).to_string()
}

/// The client's answer as the responder contract: allow/deny plus the
/// "remember" scope the policy layer records.
fn answer_to_approval(
    answer: crate::permissions::PermissionAnswer,
    tool: &crate::permissions::ToolKey,
    scopes: &[String],
) -> Approval {
    if answer.is_allow() {
        Approval::Allow {
            remember: Some(answer),
        }
    } else {
        Approval::Deny {
            message: denied_message(tool, scopes),
            remember: Some(answer),
        }
    }
}
