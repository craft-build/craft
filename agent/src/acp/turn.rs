use agent_client_protocol::{
    Client as AcpClient, ConnectionTo, Responder,
    schema::v1::{
        Content, ContentBlock, ContentChunk, PromptResponse as AcpPromptResponse, SessionId,
        SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall as AcpToolCall,
        ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        ToolKind, UsageUpdate,
    },
};
use std::collections::HashMap;
use std::sync::{Arc, atomic::Ordering};

use animist::agent::AgentEvent;
use animist::provider::ProviderEvent;

use crate::harness::{Conversation, CraftEvent, Event, EventStream};

use super::{AppState, clear_turn, internal_error, report};

/// One ACP prompt turn on the session's conversation. `stream` is the
/// conversation's single event stream, lent by the session for the turn.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_turn(
    state: Arc<AppState>,
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    text: String,
    conversation: Conversation,
    stream: &mut EventStream,
    provider_name: String,
    model: String,
    turn_cancel: animist::agent::Cancellation,
    turn_id: u64,
    responder: Responder<AcpPromptResponse>,
) {
    macro_rules! fail {
        ($message:expr) => {{
            clear_turn(&state.sessions, &session_id, turn_id).await;
            let _ = responder.respond_with_error(internal_error($message));
            return;
        }};
    }
    let (provider, _models) = match state.provider_catalog(&provider_name).await {
        Ok(catalog) => catalog,
        Err(message) => fail!(message),
    };
    if model.trim().is_empty() {
        fail!("no model is selected; set the model session configuration option");
    }
    let provider_config = &state.config.providers[&provider_name];
    let model = match provider.model(provider_config, None, &model).await {
        Ok(model) => model,
        Err(error) => fail!(report(error)),
    };
    // A model switch since the conversation was built rebuilds it between
    // runs, keeping the history, the layer state, and the event stream.
    if conversation.model().model.id != model.model.id
        && let Err(error) = conversation.set_model(model.clone())
    {
        fail!(report(error));
    }
    let context_length = conversation.model().context_length();

    // Phase 3: MCP tool annotations were registered when the session's
    // tool set was armed; the gate consults the same hints as the TUI.
    let emitted_text = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The terminal Done's per-model ledger, captured by the forwarder so
    // the store can persist it after the run ends.
    let done_by_model: Arc<
        std::sync::Mutex<Option<HashMap<String, crate::usage::StoredTokenUsage>>>,
    > = Arc::new(std::sync::Mutex::new(None));

    // Forward the conversation's events to ACP notifications while the run
    // progresses, then drain through the terminal Done.
    let connection_for_events = connection.clone();
    let session_id_for_events = session_id.clone();
    let forward_event =
        |event: Event,
         emitted_text: &std::sync::atomic::AtomicBool,
         ledger: &std::sync::Mutex<Option<HashMap<String, crate::usage::StoredTokenUsage>>>|
         -> bool {
            let update = match event {
                Event::Agent(AgentEvent::Provider {
                    event: ProviderEvent::TextDelta { text },
                    ..
                }) => {
                    emitted_text.store(true, Ordering::Relaxed);
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(text),
                    )))
                }
                Event::Agent(AgentEvent::Provider {
                    event: ProviderEvent::ThinkingDelta { text },
                    ..
                }) => SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(text),
                ))),
                Event::Agent(AgentEvent::ToolStarted { call, .. }) => {
                    SessionUpdate::ToolCall(tool_call_start(&call.id, &call.name, &call.input))
                }
                Event::Agent(AgentEvent::ToolFinished { call, output }) => {
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(call.id.clone()),
                        ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Completed)
                            .content(vec![tool_result_content(&call, &output)]),
                    ))
                }
                Event::Agent(AgentEvent::StepFinished { usage, .. }) => {
                    let Some(size) = context_length else {
                        return false;
                    };
                    SessionUpdate::UsageUpdate(UsageUpdate::new(
                        u64::from(usage.input) + u64::from(usage.cache_read),
                        u64::from(size),
                    ))
                }
                Event::Craft(CraftEvent::Done { by_model, .. }) => {
                    *ledger.lock().expect("usage ledger unpoisoned") = Some(by_model);
                    return true;
                }
                // The remaining craft events have no ACP translation yet
                // (compaction, retry, subagents) until 07–10.
                _ => return false,
            };
            // A dead connection stops the notifications but not the turn; the
            // responder still answers the request.
            let _ = connection_for_events.send_notification(SessionNotification::new(
                session_id_for_events.clone(),
                update,
            ));
            false
        };

    let run = crate::harness::run_turn(&conversation, &text, &[], turn_cancel);
    tokio::pin!(run);
    let mut done = false;
    let outcome = loop {
        tokio::select! {
            outcome = &mut run => break outcome,
            Some(event) = stream.next(), if !done => {
                done = forward_event(event, &emitted_text, &done_by_model);
            }
        }
    };
    // Drain anything still queued through the terminal Done.
    while !done {
        match stream.next().await {
            Some(event) => done = forward_event(event, &emitted_text, &done_by_model),
            None => break,
        }
    }
    let send = |update: SessionUpdate| -> std::result::Result<(), agent_client_protocol::Error> {
        connection.send_notification(SessionNotification::new(session_id.clone(), update))
    };
    // Session bookkeeping on the turn's ledger; the persister's layers have
    // already written the history records.
    if let Some(persister) = conversation.persister() {
        let by_model = done_by_model
            .lock()
            .expect("usage ledger unpoisoned")
            .take()
            .unwrap_or_default();
        let mut persister = persister.lock().await;
        persister.store_mut().record_cost(&by_model, false);
        persister.store_mut().set_model(conversation.model_spec());
    }
    match outcome {
        crate::harness::TurnOutcome::Done { reply } => {
            if !emitted_text.load(Ordering::Relaxed) && !reply.is_empty() {
                let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(reply)),
                )));
            }
            let _ = responder.respond(AcpPromptResponse::new(StopReason::EndTurn));
        }
        crate::harness::TurnOutcome::MaxTokens { reply } => {
            if !emitted_text.load(Ordering::Relaxed) && !reply.is_empty() {
                let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(reply)),
                )));
            }
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTokens));
        }
        crate::harness::TurnOutcome::MaxTurns => {
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTurnRequests));
        }
        crate::harness::TurnOutcome::Cancelled => {
            let _ = responder.respond(AcpPromptResponse::new(StopReason::Cancelled));
        }
        crate::harness::TurnOutcome::Failed(message) => fail!(message),
    }
}

fn tool_call_start(id: &str, name: &str, arguments: &serde_json::Value) -> AcpToolCall {
    let mut call = AcpToolCall::new(ToolCallId::new(id), tool_title(name, arguments));
    call.kind = tool_kind(name);
    call.status = ToolCallStatus::InProgress;
    call.raw_input = Some(arguments.clone());
    call
}

pub(super) fn tool_title(name: &str, arguments: &serde_json::Value) -> String {
    if let Some(detail) = crate::tui::provider::cards::first_string_argument(arguments) {
        format!("{name} {detail}")
    } else {
        name.to_owned()
    }
}

pub(super) fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "grep" => ToolKind::Search,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        _ => ToolKind::Other,
    }
}

pub(super) fn tool_result_content(
    call: &animist::agent::tool::ToolCall,
    output: &animist::agent::tool::ToolOutput,
) -> ToolCallContent {
    // The call's text is model-facing output; ACP displays it verbatim.
    let _ = call;
    let text = crate::history::ext::tool_content_text(&output.content);
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

/// Built-in tools produce model-facing text, which ACP displays verbatim.
#[cfg(test)]
pub(super) fn tool_result_text(result: &crate::history::ContentBlock) -> String {
    match result {
        crate::history::ContentBlock::ToolResult { content, .. } => {
            crate::history::ext::tool_content_text(content)
        }
        _ => String::new(),
    }
}
