//! Drives craft-flow's multi-stage pipeline for ACP sessions in Flow mode.
//!
//! `craft-agent` cannot depend on `craft-flow` (the reverse dependency would
//! cycle: `craft-flow` already calls back into `craft-agent::tools::run_subagent`),
//! so unlike Build/Plan mode this does not go through `headless::spawn_interactive`.
//! Instead `server::handle_prompt` routes Flow-mode prompts here directly. Stage
//! subagents still emit through the session's `InteractiveHandle::raw_event_tx`,
//! so their tool calls flow through the same subagent-translation path in
//! `server::start_event_pump` that Build-mode `task` calls use.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use agent_client_protocol_schema::{
    AgentRequest, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, RequestPermissionRequest,
    RequestPermissionResponse, SessionId, ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields,
};
use craft_agent::permissions::{PermissionAnswer, PermissionManager};
use craft_agent::prompt::ResolvedSlots;
use craft_agent::tools::FlowRunnerEnv;
use craft_agent::{AgentConfig, Envelope, EventSender};
use craft_flow::search::{Embedder, FlowSearchBackendImpl, OnnxEmbedder};
use craft_flow::{
    ApprovalPayload, ChunkStatus, FlowOutcome, FlowParams, FlowProgress, Stage, TaskStageRunner,
};
use craft_providers::Timeouts;
use craft_providers::model::Model;
use craft_providers::provider;
use craft_storage::flow::FlowStore;
use flume::Sender;
use serde_json::Value;
use tracing::warn;

use crate::permissions;
use crate::server::{PendingRequests, send_delegated, session_update};

/// Top-level stages `FlowProgress::Stage` reports, in pipeline order. Per-chunk
/// stages (Req/Execute/Review/Qa) are reported via `FlowProgress::Chunk`
/// instead and rendered as their own plan entries.
const TOP_STAGES: &[(Stage, &str)] = &[
    (Stage::Scout, "Scout the codebase"),
    (Stage::Tpm, "Draft the goal"),
    (Stage::Plan, "Plan the chunks"),
    (Stage::Execute, "Execute chunks"),
    (Stage::Integrator, "Integrate results"),
    (Stage::Verifier, "Verify the result"),
];

pub struct FlowDriveParams {
    pub session_id: String,
    pub workstream_id: String,
    pub project_id: String,
    pub request: String,
    pub model: Model,
    pub config: AgentConfig,
    pub permissions: Arc<PermissionManager>,
    pub timeouts: Timeouts,
    pub compression: craft_config::CompressionConfig,
    pub prompt_slots: Arc<ResolvedSlots>,
    pub flow_store: Arc<FlowStore>,
    pub raw_event_tx: Sender<Envelope>,
}

/// Drives one Flow prompt turn to completion (including the goal-approval
/// gate) and returns the ACP stop reason for the originating `session/prompt`.
pub async fn drive(
    params: FlowDriveParams,
    out_tx: Sender<Value>,
    pending: PendingRequests,
    next_request_id: Arc<AtomicI64>,
) -> agent_client_protocol_schema::StopReason {
    let sid = SessionId::from(params.session_id.clone());
    let mut model = params.model;
    let provider = match provider::from_model(&mut model, params.timeouts).await {
        Ok(p) => Arc::from(p),
        Err(e) => {
            session_update(
                &out_tx,
                &sid,
                crate::translate::text_delta(&e.user_message()),
            );
            return agent_client_protocol_schema::StopReason::EndTurn;
        }
    };

    let embedder: Arc<dyn Embedder> =
        Arc::new(OnnxEmbedder::new(craft_agent::EmbeddingService::new()));
    let flow_search = Some(Arc::new(FlowSearchBackendImpl::new(
        Arc::clone(&params.flow_store),
        Arc::clone(&embedder),
        &params.project_id,
        &params.workstream_id,
    )) as Arc<_>);

    let env = Arc::new(FlowRunnerEnv {
        provider,
        model: Arc::new(model),
        config: params.config.clone(),
        permissions: Arc::clone(&params.permissions),
        timeouts: params.timeouts,
        compression: params.compression.clone(),
        prompt_slots: Arc::clone(&params.prompt_slots),
        event_tx: EventSender::new(params.raw_event_tx, 0),
        flow_search,
    });

    let (progress_tx, progress_rx) = flume::unbounded::<FlowProgress>();
    let pump_out = out_tx.clone();
    let pump_sid = sid.clone();
    let pump_ws = params.workstream_id.clone();
    let pump = tokio::spawn(async move {
        let mut live = FlowLiveState::new();
        while let Ok(progress) = progress_rx.recv_async().await {
            for update in live.apply(&pump_ws, progress) {
                session_update(&pump_out, &pump_sid, update);
            }
        }
    });

    let mut approval: Option<ApprovalPayload> = None;
    // Cross-turn resume (re-entering a previously interrupted workstream) is
    // not wired yet: ACP's `PromptRequest` has no `flow_resume` equivalent to
    // plumb it from. Every drive() call starts the workstream fresh.
    let outcome = loop {
        let mut run_params = FlowParams::new(
            params.project_id.clone(),
            params.workstream_id.clone(),
            params.request.clone(),
            params.config.flow.clone(),
            Arc::clone(&params.flow_store),
        );
        run_params.approval = approval.take();
        run_params.runner = Some(Arc::new(TaskStageRunner::new(
            Arc::clone(&env),
            params.workstream_id.clone(),
        )));
        run_params.progress = Some(progress_tx.clone());
        run_params.embedder = Some(Arc::clone(&embedder));

        match craft_flow::run(run_params).await {
            FlowOutcome::AwaitingGoalApproval { goal_doc } => {
                session_update(
                    &out_tx,
                    &sid,
                    crate::translate::text_delta(&format!("## Flow goal\n\n{goal_doc}")),
                );
                let goal_id = format!("flow:{}:goal-approval", params.workstream_id);
                let fields = ToolCallUpdateFields::new().title("Approve Flow goal?".to_string());
                let request =
                    AgentRequest::RequestPermissionRequest(RequestPermissionRequest::new(
                        sid.clone(),
                        ToolCallUpdate::new(ToolCallId::from(goal_id), fields),
                        permissions::permission_options(),
                    ));
                let outcome =
                    match send_delegated(&out_tx, &pending, &next_request_id, request).await {
                        Ok(v) => serde_json::from_value::<RequestPermissionResponse>(v)
                            .map(|r| r.outcome)
                            .ok(),
                        Err(e) => {
                            warn!(error = %e, "flow goal-approval request failed");
                            None
                        }
                    };
                let answer = outcome
                    .map(|o| permissions::outcome_to_answer(&o))
                    .unwrap_or(PermissionAnswer::Deny);
                match answer {
                    PermissionAnswer::AllowOnce | PermissionAnswer::AllowSession => {
                        approval = Some(ApprovalPayload::Approved);
                    }
                    _ => {
                        session_update(
                            &out_tx,
                            &sid,
                            crate::translate::text_delta(
                                "Flow run cancelled at the goal-approval gate.",
                            ),
                        );
                        break FlowOutcome::Cancelled;
                    }
                }
            }
            other => break other,
        }
    };
    drop(progress_tx);
    let _ = pump.await;

    match outcome {
        FlowOutcome::Done {
            verification_report,
        } => {
            session_update(
                &out_tx,
                &sid,
                crate::translate::text_delta(&verification_report),
            );
            agent_client_protocol_schema::StopReason::EndTurn
        }
        FlowOutcome::NeedsReview {
            verification_report,
        } => {
            session_update(
                &out_tx,
                &sid,
                crate::translate::text_delta(&format!(
                    "## Flow verification needs review\n\n{verification_report}"
                )),
            );
            agent_client_protocol_schema::StopReason::EndTurn
        }
        FlowOutcome::Failed { stage, reason } => {
            session_update(
                &out_tx,
                &sid,
                crate::translate::text_delta(&format!("Flow {stage:?} failed: {reason}")),
            );
            agent_client_protocol_schema::StopReason::EndTurn
        }
        FlowOutcome::Cancelled => agent_client_protocol_schema::StopReason::Cancelled,
        FlowOutcome::AwaitingGoalApproval { .. } => unreachable!("handled in the loop above"),
    }
}

/// Stable id for a top-level stage's tool call, matching `flow_stage_id`'s
/// no-chunk branch in `craft-flow::runner` exactly so subagent content
/// streamed by `run_subagent` (keyed by that same id as `parent_tool_use_id`)
/// lands on the tool call this module opens.
fn stage_tool_id(workstream_id: &str, stage: Stage) -> String {
    format!("flow:{workstream_id}:{}", stage.as_str())
}

/// Stable id for a chunk's current sub-stage tool call, matching
/// `flow_stage_id`'s chunk branch.
fn chunk_tool_id(workstream_id: &str, chunk_id: &str, stage: Stage) -> String {
    format!("flow:{workstream_id}:{}:{chunk_id}", stage.as_str())
}

#[derive(Default)]
struct ChunkState {
    title: String,
    status: ChunkStatus,
    stage: Option<Stage>,
    order: usize,
}

/// Tracks Flow pipeline progress for one drive() call and turns it into ACP
/// `SessionUpdate`s: a `Plan` overview plus `ToolCall`/`ToolCallUpdate`
/// entries so each stage/chunk has somewhere for its subagent output to land.
struct FlowLiveState {
    current_stage: Option<Stage>,
    chunks: std::collections::BTreeMap<String, ChunkState>,
    opened_tool_calls: HashMap<String, ()>,
    open_chunk_tool_call: HashMap<String, String>,
}

impl FlowLiveState {
    fn new() -> Self {
        Self {
            current_stage: None,
            chunks: std::collections::BTreeMap::new(),
            opened_tool_calls: HashMap::new(),
            open_chunk_tool_call: HashMap::new(),
        }
    }

    fn apply(
        &mut self,
        workstream_id: &str,
        progress: FlowProgress,
    ) -> Vec<agent_client_protocol_schema::SessionUpdate> {
        let mut updates = Vec::new();
        match progress {
            FlowProgress::Stage(stage) => {
                if let Some(prev) = self.current_stage {
                    updates.push(self.complete_tool_call(&stage_tool_id(workstream_id, prev)));
                }
                self.current_stage = Some(stage);
                let title = TOP_STAGES
                    .iter()
                    .find(|(s, _)| *s == stage)
                    .map(|(_, label)| *label)
                    .unwrap_or(stage.as_str());
                updates.push(self.open_or_update_tool_call(
                    stage_tool_id(workstream_id, stage),
                    title.to_string(),
                ));
            }
            FlowProgress::Chunk {
                id,
                title,
                status,
                stage,
                order,
                ..
            } => {
                let entry = self.chunks.entry(id.clone()).or_default();
                if !title.is_empty() {
                    entry.title = title.clone();
                }
                entry.status = status;
                if stage.is_some() {
                    entry.stage = stage;
                }
                if order != 0 {
                    entry.order = order;
                }
                let display_title = if entry.title.is_empty() {
                    id.clone()
                } else {
                    entry.title.clone()
                };

                if let Some(stage) = stage {
                    let tool_id = chunk_tool_id(workstream_id, &id, stage);
                    self.open_chunk_tool_call
                        .insert(id.clone(), tool_id.clone());
                    updates.push(self.open_or_update_tool_call(
                        tool_id,
                        format!("{display_title} · {}", stage.as_str()),
                    ));
                } else if status == ChunkStatus::Done
                    && let Some(tool_id) = self.open_chunk_tool_call.remove(&id)
                {
                    updates.push(self.complete_tool_call(&tool_id));
                }
            }
            FlowProgress::GoalReady { .. }
            | FlowProgress::Done { .. }
            | FlowProgress::NeedsReview { .. }
            | FlowProgress::Failed { .. }
            | FlowProgress::Cancelled => {
                if let Some(stage) = self.current_stage.take() {
                    updates.push(self.complete_tool_call(&stage_tool_id(workstream_id, stage)));
                }
                for tool_id in self.open_chunk_tool_call.values() {
                    updates.push(agent_client_protocol_schema::SessionUpdate::ToolCallUpdate(
                        ToolCallUpdate::new(
                            ToolCallId::from(tool_id.clone()),
                            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                        ),
                    ));
                }
                self.open_chunk_tool_call.clear();
            }
        }
        updates.push(agent_client_protocol_schema::SessionUpdate::Plan(
            self.to_plan(),
        ));
        updates
    }

    fn open_or_update_tool_call(
        &mut self,
        id: String,
        title: String,
    ) -> agent_client_protocol_schema::SessionUpdate {
        if self.opened_tool_calls.insert(id.clone(), ()).is_none() {
            agent_client_protocol_schema::SessionUpdate::ToolCall(
                ToolCall::new(ToolCallId::from(id), title)
                    .kind(agent_client_protocol_schema::ToolKind::Think)
                    .status(ToolCallStatus::InProgress),
            )
        } else {
            agent_client_protocol_schema::SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::from(id),
                ToolCallUpdateFields::new()
                    .title(title)
                    .status(ToolCallStatus::InProgress),
            ))
        }
    }

    fn complete_tool_call(&self, id: &str) -> agent_client_protocol_schema::SessionUpdate {
        agent_client_protocol_schema::SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::from(id.to_string()),
            ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
        ))
    }

    fn to_plan(&self) -> Plan {
        let current_rank = self
            .current_stage
            .and_then(|s| TOP_STAGES.iter().position(|(st, _)| *st == s));

        let mut entries: Vec<PlanEntry> = TOP_STAGES
            .iter()
            .enumerate()
            .map(|(rank, (_, label))| {
                let status = match current_rank {
                    Some(cur) if rank < cur => PlanEntryStatus::Completed,
                    Some(cur) if rank == cur => PlanEntryStatus::InProgress,
                    _ => PlanEntryStatus::Pending,
                };
                PlanEntry::new(label.to_string(), PlanEntryPriority::Medium, status)
            })
            .collect();

        let mut chunks: Vec<(&String, &ChunkState)> = self.chunks.iter().collect();
        chunks.sort_by_key(|(_, c)| c.order);
        for (id, chunk) in chunks {
            let status = match chunk.status {
                ChunkStatus::Running => PlanEntryStatus::InProgress,
                ChunkStatus::Done => PlanEntryStatus::Completed,
                ChunkStatus::Queued | ChunkStatus::Blocked | ChunkStatus::NeedsReview => {
                    PlanEntryStatus::Pending
                }
            };
            let label = if chunk.title.is_empty() {
                id.clone()
            } else {
                chunk.title.clone()
            };
            let content = match chunk.stage {
                Some(stage) if chunk.status == ChunkStatus::Running => {
                    format!("{label} · {}", stage.as_str())
                }
                _ => label,
            };
            entries.push(PlanEntry::new(content, PlanEntryPriority::Medium, status));
        }

        Plan::new(entries)
    }
}
