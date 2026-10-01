// ---------------------------------------------------------------------------
// G.5 ACP permission gate
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

use crate::{
    history,
    permissions::{
        ASK_TIMEOUT, PermissionAnswer, PermissionCheck, PermissionError, PermissionManager,
        ToolKey, append_permission_rule, scope_for_call,
    },
    run,
};

use super::turn::tool_title;

/// Permission option ids on `session/request_permission` (G.5). Ported from
/// the reference `craft-acp/src/permissions.rs`.
const ALLOW_ONCE_ID: &str = "allow_once";
const ALLOW_ALWAYS_ID: &str = "allow_always";
const REJECT_ONCE_ID: &str = "reject_once";
const REJECT_ALWAYS_ID: &str = "reject_always";

/// The four options every `session/request_permission` carries. Ported from
/// the reference `craft-acp/src/permissions.rs`.
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
pub(super) fn outcome_to_answer(outcome: &RequestPermissionOutcome) -> PermissionAnswer {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
            ALLOW_ONCE_ID => PermissionAnswer::AllowOnce,
            ALLOW_ALWAYS_ID => PermissionAnswer::AllowSession,
            REJECT_ONCE_ID => PermissionAnswer::Deny,
            REJECT_ALWAYS_ID => PermissionAnswer::DenyAlwaysLocal,
            _ => PermissionAnswer::Deny,
        },
        RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny,
        _ => PermissionAnswer::Deny,
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

/// Record a client answer, persisting "always" answers to permissions.bml
/// (project-local). Write failures degrade to the session grant — the answer
/// still applies now, it just may be asked again later. Mirrors the TUI
/// gate's `record_answer`.
fn record_answer(
    permissions: &PermissionManager,
    tool: &ToolKey,
    scopes: &[String],
    answer: &PermissionAnswer,
) -> bool {
    let allow = answer.is_allow();
    for (tool, scope, effect, target) in permissions.apply_decision(tool, scopes, answer) {
        if let Err(err) = append_permission_rule(&tool, scope.as_deref(), effect, &target) {
            eprintln!("permissions: could not persist always-rule: {err}");
        }
    }
    allow
}

pub(super) fn denied_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::new(&tool.to_string(), scopes).to_string()
}

/// The ACP approval gate: the permission engine decides, and where it would
/// prompt, the client answers a `session/request_permission` instead of the
/// user answering the TUI overlay. Cancellation is epoch-based, matching the
/// TUI gate.
pub(super) struct AcpPermissionGate {
    pub(super) connection: ConnectionTo<AcpClient>,
    pub(super) session_id: SessionId,
    pub(super) cancel: run::CancelToken,
    pub(super) permissions: Arc<PermissionManager>,
}

impl run::BeforeExecute for AcpPermissionGate {
    fn decide(&self, call: history::ToolCall) -> run::BoxFuture<run::Decision> {
        let gate = AcpPermissionGate {
            connection: self.connection.clone(),
            session_id: self.session_id.clone(),
            cancel: self.cancel.clone(),
            permissions: self.permissions.clone(),
        };
        Box::pin(async move { gate_decide(gate, call).await })
    }
}

async fn gate_decide(gate: AcpPermissionGate, call: history::ToolCall) -> run::Decision {
    let AcpPermissionGate {
        connection,
        session_id,
        cancel,
        permissions,
    } = gate;
    if cancel.cancelled() {
        return run::Decision::Stop("cancelled by client".into());
    }
    let name = call.function.name.as_str();
    let tool = ToolKey::parse(name);
    let (scopes, force_prompt) = scope_for_call(permissions.cwd(), name, &call.function.arguments);
    match permissions.check_multi(&tool, &scopes, force_prompt) {
        PermissionCheck::Allowed => return run::Decision::Run,
        PermissionCheck::Denied => return run::Decision::Skip(denied_message(&tool, &scopes)),
        PermissionCheck::NeedsPrompt { .. } => {}
    }
    let request = permission_request(
        &session_id,
        &call.id,
        name,
        &call.function.arguments,
        &scopes,
    );
    // Dropping the sent request on cancel asks the client to cancel it.
    let mut cancel_rx = cancel.subscribe();
    let answer = {
        let sent = connection.send_request(request);
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                let _ = changed;
                return run::Decision::Stop("cancelled by client".into());
            }
            outcome = tokio::time::timeout(
                ASK_TIMEOUT,
                sent.block_task(),
            ) => match outcome {
                Ok(Ok(response)) => outcome_to_answer(&response.outcome),
                // A timeout, dead connection, or client error denies — but
                // never wedges the turn.
                _ => PermissionAnswer::Deny,
            },
        }
    };
    if record_answer(&permissions, &tool, &scopes, &answer) {
        run::Decision::Run
    } else {
        run::Decision::Skip(denied_message(&tool, &scopes))
    }
}
