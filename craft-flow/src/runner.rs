//! Live `StageRunner` that launches each stage as a real subagent via
//! `craft_agent::tools::run_subagent` (model-role resolution, worktree
//! isolation, output-schema validation all reused). This closes Phase 4:
//! production wiring supplies this runner in `FlowParams::runner`, replacing
//! the deterministic `DefaultRunner` used by unit tests.

use std::sync::Arc;

use crate::{FlowRunError, StageFuture, StageLaunch, StageRunner};

use craft_agent::tools::subagent::{SubagentRequest, run_subagent};
use craft_agent::tools::{FlowRunnerEnv, flow_runner_ctx};

/// Production stage runner. Holds the immutable LLM environment shared by
/// every stage subagent (provider, model, config, permissions, slots). The
/// per-stage `StageLaunch` carries the role, schema, isolation flag, and
/// substituted prompt that vary stage to stage.
pub struct TaskStageRunner {
    env: Arc<FlowRunnerEnv>,
    workstream_id: String,
}

impl TaskStageRunner {
    pub fn new(env: Arc<FlowRunnerEnv>, workstream_id: String) -> Self {
        Self { env, workstream_id }
    }
}

impl StageRunner for TaskStageRunner {
    fn run<'a>(&'a self, launch: StageLaunch<'a>) -> StageFuture<'a> {
        let env = Arc::clone(&self.env);
        let workstream_id = self.workstream_id.clone();
        Box::pin(async move {
            let stage_id = flow_stage_id(&workstream_id, &launch);
            let ctx = flow_runner_ctx(&env, &workstream_id, &stage_id);
            let isolation = if launch.isolate { "worktree" } else { "none" };
            let description = stage_description(&launch);
            let req = SubagentRequest {
                description,
                prompt: launch.prompt,
                subagent_type: launch.spec.subagent_type,
                model_role: Some(launch.spec.role),
                output_schema: launch.schema,
                isolation,
            };
            // Structured stages (TPM/Plan/Req/QA/Integrator/Verifier) request an
            // `output_schema`, so `run_subagent` returns `Json`; prose stages return
            // `Text`. `into_stage_text` pretty-prints JSON so the state machine's
            // `extract_json`/`parse_chunks`/`qa_failed`/`merge_ok` recover the object.
            match run_subagent(req, &ctx).await {
                Ok(result) => Ok(result.into_stage_text()),
                Err(e) => Err(FlowRunError::Other(e)),
            }
        })
    }
}

fn stage_description(launch: &StageLaunch<'_>) -> String {
    let stage = launch.spec.stage.as_str();
    match launch.chunk_id {
        Some(cid) => format!("flow {stage} chunk {cid}"),
        None => format!("flow {stage}"),
    }
}

/// Stable per-invocation id used as the subagent's `tool_use_id` so the host
/// can route permission prompts back to this exact stage subagent. Includes
/// the chunk id when present because chunks run concurrently within a stage
/// and would otherwise collide on the route key.
fn flow_stage_id(workstream_id: &str, launch: &StageLaunch<'_>) -> String {
    let stage = launch.spec.stage.as_str();
    match launch.chunk_id {
        Some(cid) => format!("flow:{workstream_id}:{stage}:{cid}"),
        None => format!("flow:{workstream_id}:{stage}"),
    }
}
