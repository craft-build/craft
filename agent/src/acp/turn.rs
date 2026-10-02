use agent_client_protocol::{
    Client as AcpClient, ConnectionTo, Error, Responder,
    schema::v1::{
        ClientCapabilities, Content, ContentBlock, ContentChunk,
        PromptResponse as AcpPromptResponse, SessionId, SessionNotification, SessionUpdate,
        StopReason, TextContent, ToolCall as AcpToolCall, ToolCallContent, ToolCallId,
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
    },
};
use std::sync::{Arc, atomic::Ordering};

use crate::{
    history,
    permissions::PermissionManager,
    run::{self, RunOutcome},
    tools::{AskQuestions, Workspace},
};

use super::elicitation::ElicitationAsker;
use super::permissions::AcpPermissionGate;
use super::{AppState, clear_turn, commit_turn, internal_error, report};

/// The per-turn tool stack: the question asker, subagent launcher, and
/// approval gate ride one workspace clone into registration, so the
/// always-registered `task` tool has a live launcher and spawned subagents
/// inherit the turn's approvals (A.5/G.5).
pub(super) fn turn_tools(
    workspace: Workspace,
    asker: Arc<dyn AskQuestions>,
    subagents: Arc<dyn crate::tools::SpawnSubagent>,
    gate: Arc<dyn run::BeforeExecute>,
    dedup: run::SharedDedupCache,
) -> run::ToolDispatch {
    workspace
        .with_questions(asker)
        .with_subagents(subagents)
        .register()
        .with_dedup(dedup)
        .with_before(gate)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_turn(
    state: Arc<AppState>,
    connection: ConnectionTo<AcpClient>,
    session_id: SessionId,
    text: String,
    mut history: Vec<history::Message>,
    workspace: Workspace,
    provider_name: String,
    model: String,
    permissions: Arc<PermissionManager>,
    client_caps: ClientCapabilities,
    cancel: run::CancelToken,
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
    let model_label = model.clone();
    let model = match provider.configured_model(&state.config.providers[&provider_name], &model) {
        Ok(model) => model,
        Err(error) => fail!(report(error)),
    };

    // Run configured compaction stages whose context-fill threshold is
    // crossed before the history is sent to the model. Only the
    // effectiveness state is persisted here; the compacted history is
    // committed by the run's success path, matching the loop's "failed runs
    // leave session history untouched" semantics.
    let (shared_compaction, context_length, dedup, thinking) = {
        let sessions = state.sessions.lock().await;
        sessions
            .get(session_id.0.as_ref())
            .map(|session| {
                (
                    session.compaction.clone(),
                    session.context_length,
                    session.dedup.clone(),
                    session.thinking,
                )
            })
            .unwrap_or_else(|| {
                let dedup = run::shared_cache();
                (
                    crate::runtime::new_compaction_state(dedup.clone(), run::shared_guardrails()),
                    None,
                    dedup,
                    state.config.always_thinking.unwrap_or_default(),
                )
            })
    };
    let compaction_ctx =
        crate::runtime::compaction_ctx(shared_compaction.clone(), &state.config, context_length);
    if let Some(mut compaction_state) = shared_compaction.lock().ok().map(|g| g.clone()) {
        let engine = crate::compaction::CompactionEngine::new(state.config.compaction.clone())
            .with_buffer(state.config.compaction_buffer);
        engine
            .maybe_compact(&mut compaction_state, &model, &mut history, context_length)
            .await;
        if let Ok(mut guard) = shared_compaction.lock() {
            *guard = compaction_state;
        }
    }

    // Phase 3: register MCP tool annotations once the tool set is settled so
    // the ACP permission gate consults the same hints as the TUI.
    if let Some(mcp) = workspace.mcp() {
        permissions.sync_mcp_annotations(&mcp);
    }
    let (cwd, instructions_text) = {
        let sessions = state.sessions.lock().await;
        let session = sessions.get(session_id.0.as_ref());
        (
            session
                .map(|session| session.workspace.root().display().to_string())
                .unwrap_or_default(),
            session
                .map(|session| session.instructions.text.clone())
                .unwrap_or_default(),
        )
    };
    let emitted_text = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let emit: Arc<dyn Fn(run::Event) + Send + Sync> = {
        let connection = connection.clone();
        let session_id = session_id.clone();
        let emitted_text = emitted_text.clone();
        Arc::new(move |event: run::Event| {
            let update = match event {
                run::Event::TextDelta(delta) => {
                    emitted_text.store(true, Ordering::Relaxed);
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(delta),
                    )))
                }
                run::Event::ThinkingDelta(delta) => SessionUpdate::AgentThoughtChunk(
                    ContentChunk::new(ContentBlock::Text(TextContent::new(delta))),
                ),
                run::Event::ToolStart {
                    id,
                    name,
                    arguments,
                } => SessionUpdate::ToolCall(tool_call_start(&id, &name, &arguments)),
                run::Event::ToolDone { id, result, .. } => {
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new(id),
                        ToolCallUpdateFields::new()
                            .status(ToolCallStatus::Completed)
                            .content(vec![tool_result_content(&result)]),
                    ))
                }
                run::Event::TurnComplete { usage, .. } => {
                    let Some(size) = context_length else {
                        return;
                    };
                    SessionUpdate::UsageUpdate(UsageUpdate::new(
                        usage.input_tokens,
                        u64::from(size),
                    ))
                }
                // The nudged retry follows immediately; no ACP notification.
                // The remaining taxonomy variants have no ACP translation yet.
                run::Event::Nudge
                | run::Event::ToolPending { .. }
                | run::Event::ToolOutput { .. }
                | run::Event::ToolResultsSubmitted { .. }
                | run::Event::Done { .. }
                | run::Event::Info(_)
                | run::Event::AdvisorNote { .. }
                | run::Event::Error(_)
                | run::Event::Retry { .. }
                | run::Event::AuthRequired { .. }
                | run::Event::AutoCompacting { .. }
                | run::Event::Subagent { .. }
                | run::Event::CompactionDone { .. }
                | run::Event::StagnationDetected { .. }
                | run::Event::AutoReviewStart { .. }
                | run::Event::AutoReviewDecision { .. }
                | run::Event::StreamClosed => return,
            };
            // A dead connection stops the notifications but not the turn; the
            // responder still answers the request.
            let _ =
                connection.send_notification(SessionNotification::new(session_id.clone(), update));
        })
    };
    // The approval gate is shared by the turn's dispatch table and every
    // subagent the `task` tool spawns, so children cannot bypass approvals.
    let permission_gate = Arc::new(AcpPermissionGate {
        connection: connection.clone(),
        session_id: session_id.clone(),
        cancel: cancel.clone(),
        permissions,
    });
    // The subagent seam (A.5): `task` is always registered, so install a
    // launcher per turn like the TUI and headless surfaces do. Filtered
    // child events arrive wrapped in `Event::Subagent`, which has no ACP
    // translation yet (see the emit arm above), but the child's final
    // message still returns as the tool result.
    let mut agent = state.config.agent.clone();
    agent.thinking = Some(thinking);
    let subagents = Arc::new(crate::subagent::SubagentLauncher {
        parent_model: model.clone(),
        parent_spec: format!("{provider_name}/{model_label}"),
        provider: provider_name.clone(),
        providers: state.config.providers.clone(),
        agent,
        compression: state.config.compression.clone(),
        base_prompt: format!("{}{}", state.config.agent.preamble, instructions_text),
        workspace: workspace.clone(),
        history: history.clone(),
        cancel: cancel.clone(),
        cancels: Arc::new(run::cancel::CancelMap::new()),
        emit: emit.clone(),
        before: Some(permission_gate.clone()),
    });
    let tools = turn_tools(
        workspace,
        Arc::new(ElicitationAsker {
            connection: connection.clone(),
            session_id: session_id.clone(),
            cancel: cancel.clone(),
            caps: client_caps,
        }),
        subagents,
        permission_gate,
        dedup,
    );
    // ACP is always Build mode (plan mode is a TUI/headless concern), and
    // its turn bound is unbounded: the client owns stop decisions through
    // `session/cancel`.
    let mode = run::AgentMode::Build;
    let resolved = crate::runtime::ResolvedModel {
        provider: provider_name.clone(),
        model_id: model_label.clone(),
        context_length,
        max_output_tokens: _models
            .iter()
            .find(|entry| entry.id == model_label)
            .and_then(|entry| entry.max_output_tokens),
    };
    let params = crate::runtime::run_policy(crate::runtime::RunPolicyInputs {
        config: &state.config,
        cwd: &cwd,
        instructions_text: &instructions_text,
        mode: &mode,
        model: &resolved,
        compaction: Some(compaction_ctx),
        recency: None,
        retry: run::RetryCtx::default(),
        fast: false,
        thinking: Some(thinking),
        max_turns: crate::runtime::MaxTurns::Unbounded,
    });

    let send = |update: SessionUpdate| -> std::result::Result<(), Error> {
        connection.send_notification(SessionNotification::new(session_id.clone(), update))
    };

    let outcome = run::run(
        &model,
        &params,
        &tools,
        &mut history,
        &text,
        &cancel,
        emit.as_ref(),
    )
    .await;
    match outcome {
        RunOutcome::Done { reply } => {
            if !emitted_text.load(Ordering::Relaxed) && !reply.is_empty() {
                let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(reply)),
                )));
            }
            // Commit this turn only on success: failed and cancelled runs
            // leave the session history untouched. A superseded turn id
            // commits nothing.
            commit_turn(&state.sessions, &session_id, turn_id, history.clone()).await;
            // Phase 4: detached, best-effort memory extraction into the
            // project's local argosy. Never blocks or fails the turn.
            crate::knowledge_memory::spawn_extraction(
                model.clone(),
                history,
                std::path::PathBuf::from(cwd),
                state.config.agent.memory_extraction,
            );
            let _ = responder.respond(AcpPromptResponse::new(StopReason::EndTurn));
        }
        // The driver committed the sanitized partial history; the next prompt
        // continues from where the budget ran out.
        // The turn still hit the output-token limit after every continuation;
        // the committed history includes the truncated tail.
        RunOutcome::MaxTokens { reply } => {
            if !emitted_text.load(Ordering::Relaxed) && !reply.is_empty() {
                let _ = send(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(reply)),
                )));
            }
            commit_turn(&state.sessions, &session_id, turn_id, history).await;
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTokens));
        }
        RunOutcome::MaxTurns => {
            commit_turn(&state.sessions, &session_id, turn_id, history).await;
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTurnRequests));
        }
        // The doom-loop hard stop committed the sanitized partial history;
        // like MaxTurns, the next prompt continues from the cut-off.
        RunOutcome::DoomStop => {
            commit_turn(&state.sessions, &session_id, turn_id, history).await;
            let _ = responder.respond(AcpPromptResponse::new(StopReason::MaxTurnRequests));
        }
        RunOutcome::Cancelled => {
            clear_turn(&state.sessions, &session_id, turn_id).await;
            let _ = responder.respond(AcpPromptResponse::new(StopReason::Cancelled));
        }
        RunOutcome::Failed(message) => fail!(message),
    }
    // The responder has already answered; linger only long enough for the
    // detached memory extraction to land before the session goes away.
    crate::knowledge_memory::wait_for_pending(std::time::Duration::from_secs(15)).await;
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

pub(super) fn tool_result_content(result: &history::ToolResult) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
        tool_result_text(&result.content),
    ))))
}

/// Built-in tools produce model-facing text, which ACP displays verbatim.
/// Keep JSON readable for additional tools without interpreting their schemas.
pub(super) fn tool_result_text(items: &[history::ToolResultContent]) -> String {
    items
        .iter()
        .map(history::ToolResultContent::to_text)
        .collect::<Vec<_>>()
        .join("\n")
}
