//! Flow mode: a multi-stage pipeline (Scout, TPM, Plan, Req, Execute, Review,
//! QA, Integrator, Verifier) that persists per-workstream documents and pauses
//! for goal approval before execution.
//!
//! Stage agents are launched through the standard `craft_agent` subagent path
//! (the `task` tool with a `model_role`), so all tool-selection, output-schema,
//! isolation, and cancellation wiring is reused. This crate owns the state
//! machine, the document schemas/templates, and the persisted `flow` namespace.

pub mod error;
pub mod runner;
pub mod schema;
pub mod search;
pub mod templates;

pub use error::FlowRunError;
pub use runner::TaskStageRunner;

/// Sent through the user-answer channel when the user approves the goal doc.
/// Shared by the TUI's `FlowGoalPrompt` (sender) and the agent loop's flow
/// branch (receiver) so the two sides never disagree on the magic string.
pub const FLOW_APPROVE_ANSWER: &str = "approved";
/// Sent through the user-answer channel when the user cancels at the gate.
pub const FLOW_CANCEL_ANSWER: &str = "__flow_cancel__";

/// Project id: lowercase basename of `cwd` plus the fnv1a-64 hash of the full
/// path, mirroring the Lua `memory_helpers.project_id` so the Flow and memory
/// namespaces share a per-project key. The CLI (`craft flow`) and the TUI both
/// use this so a workstream's documents land in the same FlowStore directory.
pub fn project_id(cwd: &std::path::Path) -> String {
    let basename = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "root".to_string());
    let path_str = cwd.to_string_lossy();
    format!("{basename}-{}", fnv1a_64(path_str.as_bytes()))
}

/// FNV-1a 64-bit as a 16-hex-char string, matching `memory_helpers.fnv1a_64`'s
/// `%08x%08x` output exactly.
fn fnv1a_64(data: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use craft_config::FlowConfig;
use craft_storage::flow::FlowStore;

/// Default subagent type for a research (read-only) stage.
const SUBAGENT_RESEARCH: &str = "research";
/// Default subagent type for a general (write) stage.
const SUBAGENT_GENERAL: &str = "general";

/// Boxed future returned by [`StageRunner`], matching craft-agent's tool trait
/// style (`Pin<Box<dyn Future + Send>>`) so no `async_trait` dependency is
/// needed and the trait is object-safe.
pub type StageFuture<'a> = Pin<Box<dyn Future<Output = Result<String, FlowRunError>> + Send + 'a>>;

/// One named stage of the pipeline. Order is significant; the orchestrator
/// advances through them in sequence (per chunk for the middle stages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Scout,
    Tpm,
    Plan,
    Req,
    Execute,
    Review,
    Qa,
    Integrator,
    Verifier,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Scout => "scout",
            Stage::Tpm => "tpm",
            Stage::Plan => "plan",
            Stage::Req => "req",
            Stage::Execute => "execute",
            Stage::Review => "review",
            Stage::Qa => "qa",
            Stage::Integrator => "integrator",
            Stage::Verifier => "verifier",
        }
    }

    /// Parse a stage from its serde (`rename_all = "lowercase"`) form. Returns
    /// `None` on unknown strings rather than erroring, since persisted flow
    /// state may come from newer versions with extra stage variants.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "scout" => Some(Stage::Scout),
            "tpm" => Some(Stage::Tpm),
            "plan" => Some(Stage::Plan),
            "req" => Some(Stage::Req),
            "execute" => Some(Stage::Execute),
            "review" => Some(Stage::Review),
            "qa" => Some(Stage::Qa),
            "integrator" => Some(Stage::Integrator),
            "verifier" => Some(Stage::Verifier),
            _ => None,
        }
    }

    /// Per-stage launch intent, mirroring the proposal's §2 table: which model
    /// role, subagent type, and (for Execute) isolation mode the stage runs
    /// under. Phase 6's task-backed runner consumes this to build the `task`
    /// call; the test runner ignores it.
    pub fn spec(self) -> StageSpec {
        match self {
            Stage::Scout => StageSpec::new(self, "scout", SUBAGENT_RESEARCH),
            Stage::Tpm => StageSpec::new(self, "tpm", SUBAGENT_RESEARCH),
            Stage::Plan => StageSpec::new(self, "plan", SUBAGENT_RESEARCH),
            Stage::Req => StageSpec::new(self, "req", SUBAGENT_RESEARCH),
            Stage::Execute => StageSpec {
                isolation: Some(WorktreeIso::Worktree),
                ..StageSpec::new(self, "execute", SUBAGENT_GENERAL)
            },
            Stage::Review => StageSpec::new(self, "review", SUBAGENT_RESEARCH),
            Stage::Qa => StageSpec::new(self, "flow_qa", SUBAGENT_GENERAL),
            Stage::Integrator => StageSpec::new(self, "flow_integrator", SUBAGENT_GENERAL),
            Stage::Verifier => StageSpec::new(self, "flow_verifier", SUBAGENT_RESEARCH),
        }
    }
}

/// Isolation mode for a stage's subagent. `Worktree` runs Execute in a linked
/// git worktree when `parallel_chunks > 1`, so concurrent chunks cannot clobber
/// each other. With `parallel_chunks == 1` the orchestrator downgrades Worktree
/// to None (no point isolating a single chunk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeIso {
    None,
    Worktree,
}

/// Launch intent for one stage. Phase 6's runner turns this into the matching
/// `task` call; the bounded-loop guards live in the orchestrator, not here.
#[derive(Debug, Clone, Copy)]
pub struct StageSpec {
    pub stage: Stage,
    pub role: &'static str,
    pub subagent_type: &'static str,
    pub isolation: Option<WorktreeIso>,
}

impl StageSpec {
    fn new(stage: Stage, role: &'static str, subagent_type: &'static str) -> Self {
        Self {
            stage,
            role,
            subagent_type,
            isolation: None,
        }
    }
}

/// The JSON Schema describing a stage's persisted document, used as the
/// subagent's `output_schema`. Returns `None` for prose-only stages (Review).
pub fn stage_schema(stage: Stage) -> Option<Value> {
    match stage {
        Stage::Tpm => Some(schema::goal_doc()),
        Stage::Plan => Some(schema::plan_doc()),
        Stage::Req => Some(schema::requirement_doc()),
        Stage::Review => Some(schema::review_report()),
        Stage::Qa => Some(schema::qa_report()),
        Stage::Integrator => Some(schema::integration_checkpoint()),
        Stage::Verifier => Some(schema::verification_report()),
        Stage::Scout | Stage::Execute => None,
    }
}

/// Per-chunk lifecycle status. Glyphs are reused by the TUI panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStatus {
    #[default]
    Queued,
    Running,
    NeedsReview,
    Blocked,
    Done,
}

impl ChunkStatus {
    pub fn glyph(self) -> &'static str {
        match self {
            ChunkStatus::Queued => "·",
            ChunkStatus::Running => "▸",
            ChunkStatus::NeedsReview => "?",
            ChunkStatus::Blocked => "✗",
            ChunkStatus::Done => "✓",
        }
    }

    /// Parse from the serde (`rename_all = "snake_case"`) form; unknown
    /// strings yield `None`. Used by the TUI when hydrating persisted state.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "queued" => Some(ChunkStatus::Queued),
            "running" => Some(ChunkStatus::Running),
            "needs_review" => Some(ChunkStatus::NeedsReview),
            "blocked" => Some(ChunkStatus::Blocked),
            "done" => Some(ChunkStatus::Done),
            _ => None,
        }
    }
}

/// A single chunk tracked across stages.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub title: String,
    pub status: ChunkStatus,
    /// Ids of chunks that must reach `Done` before this chunk can start.
    /// Declared by the Plan stage; drives dependency-aware scheduling.
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub review_iterations: u32,
    pub qa_iterations: u32,
}

/// Mutable workstream state, persisted in `SessionMeta::flow_state`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workstream {
    pub project_id: String,
    pub workstream_id: String,
    pub stage: Option<Stage>,
    pub approved: bool,
    pub chunks: BTreeMap<String, Chunk>,
}

impl Workstream {
    pub fn new(project_id: impl Into<String>, workstream_id: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            workstream_id: workstream_id.into(),
            stage: Some(Stage::Scout),
            approved: false,
            chunks: BTreeMap::new(),
        }
    }

    pub fn chunk_status(&self, chunk_id: &str) -> Option<ChunkStatus> {
        self.chunks.get(chunk_id).map(|c| c.status)
    }

    pub fn set_chunk_status(&mut self, chunk_id: &str, status: ChunkStatus) {
        self.chunks
            .entry(chunk_id.to_string())
            .or_insert_with(|| Chunk {
                id: chunk_id.to_string(),
                title: String::new(),
                status,
                depends_on: Vec::new(),
                review_iterations: 0,
                qa_iterations: 0,
            })
            .status = status;
    }

    pub fn all_done(&self) -> bool {
        !self.chunks.is_empty() && self.chunks.values().all(|c| c.status == ChunkStatus::Done)
    }
}

/// Inputs to a Flow run. Mirrors the proposal's `FlowParams`.
pub struct FlowParams {
    pub project_id: String,
    pub request: String,
    pub workstream_id: String,
    pub config: FlowConfig,
    pub store: Arc<FlowStore>,
    pub approval: Option<ApprovalPayload>,
    /// Optional stage runner override. `None` uses [`DefaultRunner`], which is
    /// deterministic and provider-free so the state machine is unit-testable.
    /// Phase 6's CLI wiring injects a `task`-backed runner here.
    pub runner: Option<Arc<dyn StageRunner + Send + Sync>>,
    /// Optional progress channel. When `Some`, the pipeline emits stage
    /// transitions, chunk status changes, and the goal-approval gate pause so
    /// a driver (the TUI FlowPanel) can render live progress. Best-effort: a
    /// dropped receiver never fails the run.
    pub progress: Option<flume::Sender<FlowProgress>>,
    /// Optional embedder for the semantic index. When `Some`, the pipeline
    /// reindexes the workstream's documents before the Verifier stage so
    /// `flow_search` has fresh vectors.
    pub embedder: Option<Arc<dyn search::Embedder>>,
    /// Resume a previously-failed run. When true, the pipeline rehydrates the
    /// persisted `Workstream` state (stage, chunk statuses, iteration counts)
    /// and skips stages whose documents are already on disk, re-entering at the
    /// last persisted stage. Off for fresh runs.
    pub resume: bool,
}

impl FlowParams {
    /// Build params with the default (provider-free) runner.
    pub fn new(
        project_id: impl Into<String>,
        workstream_id: impl Into<String>,
        request: impl Into<String>,
        config: FlowConfig,
        store: Arc<FlowStore>,
    ) -> Self {
        Self {
            project_id: project_id.into(),
            request: request.into(),
            workstream_id: workstream_id.into(),
            config,
            store,
            approval: None,
            runner: None,
            progress: None,
            embedder: None,
            resume: false,
        }
    }
}

/// Resume payload: either an approval or a goal revision.
#[derive(Debug, Clone)]
pub enum ApprovalPayload {
    Approved,
    Revised(String),
}

/// Terminal outcome of a Flow run.
#[derive(Debug)]
pub enum FlowOutcome {
    /// Paused at the goal-approval gate, awaiting user input.
    AwaitingGoalApproval { goal_doc: String },
    /// Pipeline finished with a verification report.
    Done { verification_report: String },
    /// A stage failed irrecoverably.
    Failed { stage: Stage, reason: String },
    /// The pipeline was cancelled before reaching a terminal state.
    Cancelled,
}

/// Live progress events emitted by [`run`] for an attached driver (the TUI
/// FlowPanel). Best-effort: sent through [`FlowParams::progress`] and dropped
/// silently if the receiver is gone. The state machine's correctness does not
/// depend on these being observed.
#[derive(Debug, Clone)]
pub enum FlowProgress {
    /// The pipeline entered a top-level stage (Scout, TPM, Plan, Integrator,
    /// Verifier). Per-chunk stages (Req, Execute, Review, QA) are reported via
    /// [`FlowProgress::Chunk`] instead, since chunks run concurrently.
    Stage(Stage),
    /// A chunk's status changed (queued, running, done, blocked). The title is
    /// carried so a driver can render chunk titles alongside ids; it is empty
    /// when the chunk was created before a Plan stage produced a title.
    /// `stage` is the per-chunk pipeline stage (Req/Execute/Review/QA) the
    /// chunk is currently in when `status` is Running, or None for terminal
    /// transitions (Done/Blocked/Queued). `depends_on` carries the chunk's
    /// dependency edges (ids of chunks that must finish first); `order` is the
    /// chunk's position in the plan array so a driver can render plan order.
    Chunk {
        id: String,
        title: String,
        status: ChunkStatus,
        stage: Option<Stage>,
        depends_on: Vec<String>,
        order: usize,
    },
    /// The pipeline paused at the goal-approval gate; the driver should surface
    /// `goal_doc` to the user and resume with an [`ApprovalPayload`].
    GoalReady { goal_doc: String },
    /// The pipeline finished with a verification report (`verdict` is the raw
    /// verifier output, typically a JSON object with a `verdict` field).
    Done { verdict: String },
    /// A stage failed irrecoverably.
    Failed { stage: Stage, reason: String },
    /// The pipeline was cancelled (e.g. the user interrupted the run or the
    /// driving task was dropped). Emitted so attached drivers can finalize
    /// any in-flight chunk panel state instead of freezing mid-run.
    Cancelled,
}

/// Inputs to a single stage launch. Built by the orchestrator from the stage's
/// [`StageSpec`] plus the substituted prompt and per-chunk context.
#[derive(Debug, Clone)]
pub struct StageLaunch<'a> {
    pub spec: StageSpec,
    pub schema: Option<Value>,
    pub prompt: String,
    /// Chunk id when this is a per-chunk stage, else `None`.
    pub chunk_id: Option<&'a str>,
    /// True when Execute should run in a worktree (parallel chunks > 1).
    pub isolate: bool,
}

/// Launch a stage's subagent and return its textual output (a JSON document for
/// structured stages, prose otherwise).
///
/// The default impl ([`DefaultRunner`]) returns the persisted prior document
/// for the stage (or the launch prompt if none), so the state machine is fully
/// testable without a live provider. Production wiring (Phase 6) supplies a
/// runner that constructs and invokes a real `task` subagent via `Agent::run`.
pub trait StageRunner {
    fn run<'a>(&'a self, launch: StageLaunch<'a>) -> StageFuture<'a>;
}

/// Deterministic, provider-free runner used by tests and as the fallback.
/// Returns the stage's persisted document from the `FlowStore`, or the launch
/// prompt when no document exists yet. This makes the state machine's branching
/// and bounded loops fully exercisable in `cargo nextest` without a mock LLM.
///
/// Owns its handles (rather than borrowing) so a `DefaultRunner` is `'static`
/// and can be erased into `Arc<dyn StageRunner + Send + Sync>`.
pub struct DefaultRunner {
    pub store: Arc<FlowStore>,
    pub project_id: String,
    pub workstream_id: String,
}

impl StageRunner for DefaultRunner {
    fn run<'a>(&'a self, launch: StageLaunch<'a>) -> StageFuture<'a> {
        Box::pin(async move {
            let rel = match launch.chunk_id {
                Some(cid) => format!("{}_{}.md", launch.spec.stage.as_str(), cid),
                None => format!("{}.md", launch.spec.stage.as_str()),
            };
            match self.store.read(&self.project_id, &self.workstream_id, &rel) {
                Ok(doc) => Ok(doc),
                Err(e) => {
                    warn!(
                        stage = launch.spec.stage.as_str(),
                        error = %e,
                        "flow: no prior doc, using prompt"
                    );
                    Ok(launch.prompt.clone())
                }
            }
        })
    }
}

/// Run the pipeline. This is the orchestration entry point: it advances the
/// workstream stage by stage, persisting each stage's document via `FlowStore`,
/// and returns at the approval gate or on completion/failure.
///
/// Per chunk, the loop is Req -> Execute -> Review (P0/P1 -> re-Execute,
/// bounded by `max_review_iterations`) -> QA (fail -> re-Execute, bounded by
/// `max_qa_iterations`). With `parallel_chunks == 1` the Integrator is a
/// pass-through; with `> 1` it merges worktrees via the `conflicts` gate.
pub async fn run(params: FlowParams) -> FlowOutcome {
    let embedder = params.embedder.clone();
    let resume = params.resume;
    let FlowParams {
        project_id,
        request,
        workstream_id,
        config,
        store,
        approval,
        runner,
        progress,
        ..
    } = params;
    let runner: Arc<dyn StageRunner + Send + Sync> = runner.unwrap_or_else(|| {
        Arc::new(DefaultRunner {
            store: Arc::clone(&store),
            project_id: project_id.clone(),
            workstream_id: workstream_id.clone(),
        })
    });
    let ctx = Ctx::new(&project_id, &workstream_id, Arc::clone(&store), progress);

    // Rehydrate persisted workstream state on resume; otherwise start fresh.
    // On resume we skip any top-level stage whose document is already on disk
    // and re-enter at the last persisted stage, so a failed run can be retried
    // without re-running Scout/TPM/Plan.
    let mut ws = if resume {
        load_workstream(&store, &project_id, &workstream_id)
            .unwrap_or_else(|| Workstream::new(project_id.clone(), workstream_id.clone()))
    } else {
        Workstream::new(project_id.clone(), workstream_id.clone())
    };
    // Capture the rehydrated stage before the Scout/TPM section below overwrites
    // ws.stage; the resume-skip decision for the chunk pipeline needs the
    // persisted stage, not the transient Scout/TPM marker.
    let loaded_stage = ws.stage;

    // Goal-approval resume optimization: when an approval payload is supplied
    // and a goal doc is already persisted, re-enter at the gate (skip
    // Scout/TPM re-runs). This is what makes `craft flow -s <id> -p approved`
    // cheap on the second call, and also covers the resume path.
    let persisted_goal = store
        .read(&project_id, &workstream_id, "tpm.md")
        .ok()
        .filter(|_| approval.is_some() || resume);

    // Scout
    ws.stage = Some(Stage::Scout);
    let scout_out = match persisted_goal.as_ref() {
        Some(_) => String::new(),
        None => {
            ctx.emit(FlowProgress::Stage(Stage::Scout));
            match launch(&runner, &ctx, Stage::Scout, &request, None, false).await {
                Ok(s) => s,
                Err(e) => return fail(&ctx, Stage::Scout, e.into_reason()),
            }
        }
    };
    if persisted_goal.is_none() {
        persist(&ctx, Stage::Scout, None, &scout_out);
    }

    // TPM produces the goal doc, then we pause for approval.
    ws.stage = Some(Stage::Tpm);
    let goal_doc = match persisted_goal.as_ref() {
        Some(g) => g.clone(),
        None => {
            ctx.emit(FlowProgress::Stage(Stage::Tpm));
            match launch(&runner, &ctx, Stage::Tpm, &scout_out, None, false).await {
                Ok(s) => s,
                Err(e) => return fail(&ctx, Stage::Tpm, e.into_reason()),
            }
        }
    };
    if persisted_goal.is_none() {
        persist(&ctx, Stage::Tpm, None, &goal_doc);
    }

    let approved_goal = match approval {
        Some(ApprovalPayload::Approved) => goal_doc,
        Some(ApprovalPayload::Revised(rev)) => {
            persist(&ctx, Stage::Tpm, None, &rev);
            rev
        }
        None if ws.approved => goal_doc,
        None => {
            save_workstream(&ctx, &ws);
            ctx.emit(FlowProgress::GoalReady {
                goal_doc: goal_doc.clone(),
            });
            return FlowOutcome::AwaitingGoalApproval { goal_doc };
        }
    };
    ws.approved = true;

    // Plan: skip on resume when the persisted stage is already past Plan and a
    // plan doc exists, so we re-enter directly at the chunk pipeline.
    let resumed_plan = resume
        && loaded_stage.is_some_and(|s| s >= Stage::Execute)
        && store.read(&project_id, &workstream_id, "plan.md").is_ok();
    let plan_doc = if resumed_plan {
        store
            .read(&project_id, &workstream_id, "plan.md")
            .unwrap_or_default()
    } else {
        ws.stage = Some(Stage::Plan);
        ctx.emit(FlowProgress::Stage(Stage::Plan));
        let doc = match launch(&runner, &ctx, Stage::Plan, &approved_goal, None, false).await {
            Ok(s) => s,
            Err(e) => return fail(&ctx, Stage::Plan, e.into_reason()),
        };
        persist(&ctx, Stage::Plan, None, &doc);
        doc
    };

    let chunks = parse_chunks(&plan_doc);
    for (i, c) in chunks.iter().enumerate() {
        ws.chunks.entry(c.id.clone()).or_insert_with(|| Chunk {
            id: c.id.clone(),
            title: c.title.clone(),
            status: ChunkStatus::Queued,
            depends_on: c.depends_on.clone(),
            review_iterations: 0,
            qa_iterations: 0,
        });
        if let Some(entry) = ws.chunks.get_mut(&c.id) {
            entry.title = c.title.clone();
            entry.depends_on = c.depends_on.clone();
        }
        ctx.emit(FlowProgress::Chunk {
            id: c.id.clone(),
            title: c.title.clone(),
            status: ChunkStatus::Queued,
            stage: None,
            depends_on: c.depends_on.clone(),
            order: i,
        });
    }
    save_workstream(&ctx, &ws);
    if chunks.is_empty() {
        return FlowOutcome::Failed {
            stage: Stage::Plan,
            reason: "plan produced no chunks".to_string(),
        };
    }

    let isolate = config.parallel_chunks > 1;
    let review_budget = config.max_review_iterations.max(1) as usize;
    let qa_budget = config.max_qa_iterations.max(1) as usize;
    let concurrency = config.parallel_chunks.max(1) as usize;

    ws.stage = Some(Stage::Execute);
    save_workstream(&ctx, &ws);
    ctx.emit(FlowProgress::Stage(Stage::Execute));

    let results = run_chunk_dag(
        &runner,
        &ctx,
        &mut ws,
        &chunks,
        &plan_doc,
        isolate,
        concurrency,
        review_budget,
        qa_budget,
    )
    .await;
    // `run_chunk_dag` returns on the first chunk failure (with that chunk's
    // id + error) or after all chunks succeed.
    for (chunk_id, res) in results {
        let title = ws
            .chunks
            .get(&chunk_id)
            .map(|c| c.title.clone())
            .unwrap_or_default();
        match res {
            Ok(()) => {
                ws.set_chunk_status(&chunk_id, ChunkStatus::Done);
                ctx.emit(FlowProgress::Chunk {
                    id: chunk_id.clone(),
                    title,
                    status: ChunkStatus::Done,
                    stage: None,
                    depends_on: Vec::new(),
                    order: 0,
                });
            }
            Err(e) => {
                let stage = e.stage();
                ws.set_chunk_status(&chunk_id, ChunkStatus::Blocked);
                save_workstream(&ctx, &ws);
                ctx.emit(FlowProgress::Chunk {
                    id: chunk_id.clone(),
                    title,
                    status: ChunkStatus::Blocked,
                    stage: None,
                    depends_on: Vec::new(),
                    order: 0,
                });
                return FlowOutcome::Failed {
                    stage,
                    reason: e.into_reason(),
                };
            }
        }
    }
    save_workstream(&ctx, &ws);

    // Integrator -> Verifier
    // Refresh the semantic index so the Verifier (and any stage agent calling
    // `flow_search`) sees embeddings for all docs written so far. Best-effort:
    // a failure to embed logs and continues rather than failing the run.
    if let Some(embedder) = embedder.as_deref()
        && search::index_enabled(&config)
    {
        match search::reindex(&store, embedder, &project_id, &workstream_id).await {
            Ok(n) => info!(newly_indexed = n, "flow: reindexed workstream documents"),
            Err(e) => warn!(error = %e, "flow: semantic reindex failed, continuing"),
        }
    }
    ws.stage = Some(Stage::Integrator);
    save_workstream(&ctx, &ws);
    ctx.emit(FlowProgress::Stage(Stage::Integrator));
    let integration = match launch(&runner, &ctx, Stage::Integrator, &plan_doc, None, false).await {
        Ok(s) => s,
        Err(e) => return fail(&ctx, Stage::Integrator, e.into_reason()),
    };
    if isolate && !merge_ok(&integration) {
        save_workstream(&ctx, &ws);
        return FlowOutcome::Failed {
            stage: Stage::Integrator,
            reason: "unresolved merge conflicts across parallel chunks".to_string(),
        };
    }
    persist(&ctx, Stage::Integrator, None, &integration);

    ws.stage = Some(Stage::Verifier);
    save_workstream(&ctx, &ws);
    ctx.emit(FlowProgress::Stage(Stage::Verifier));
    let verification =
        match launch(&runner, &ctx, Stage::Verifier, &approved_goal, None, false).await {
            Ok(s) => s,
            Err(e) => return fail(&ctx, Stage::Verifier, e.into_reason()),
        };
    persist(&ctx, Stage::Verifier, None, &verification);

    save_workstream(&ctx, &ws);
    ctx.emit(FlowProgress::Done {
        verdict: verification.clone(),
    });
    FlowOutcome::Done {
        verification_report: verification,
    }
}

/// Owned orchestrator context: project/workstream ids and the shared store. Held
/// by value (cloned per concurrent chunk future) so per-chunk pipelines can run
/// in parallel without borrowing the orchestrator's stack.
#[derive(Clone)]
struct Ctx {
    project_id: Arc<str>,
    workstream_id: Arc<str>,
    store: Arc<FlowStore>,
    progress: Option<flume::Sender<FlowProgress>>,
}

impl Ctx {
    fn new(
        project_id: &str,
        workstream_id: &str,
        store: Arc<FlowStore>,
        progress: Option<flume::Sender<FlowProgress>>,
    ) -> Self {
        Self {
            project_id: Arc::from(project_id),
            workstream_id: Arc::from(workstream_id),
            store,
            progress,
        }
    }

    fn emit(&self, event: FlowProgress) {
        if let Some(tx) = &self.progress {
            let _ = tx.send(event);
        }
    }

    /// Emit a per-chunk stage transition (Req/Execute/Review/QA) without
    /// changing the chunk's terminal status. Used by `run_chunk` so the TUI
    /// can show which pipeline stage each running chunk is currently in.
    fn emit_chunk(&self, chunk_id: &str, stage: Stage) {
        self.emit(FlowProgress::Chunk {
            id: chunk_id.to_string(),
            title: String::new(),
            status: ChunkStatus::Running,
            stage: Some(stage),
            depends_on: Vec::new(),
            order: 0,
        });
    }
}

fn persist(ctx: &Ctx, stage: Stage, chunk_id: Option<&str>, content: &str) {
    let rel = match chunk_id {
        Some(cid) => format!("{}_{}.md", stage.as_str(), cid),
        None => format!("{}.md", stage.as_str()),
    };
    if let Err(e) = ctx
        .store
        .write(&ctx.project_id, &ctx.workstream_id, &rel, content)
    {
        warn!(stage = stage.as_str(), error = %e, "flow: failed to persist stage document");
    } else {
        info!(stage = stage.as_str(), rel = %rel, "flow: persisted stage document");
    }
}

fn fail(ctx: &Ctx, stage: Stage, reason: String) -> FlowOutcome {
    warn!(stage = stage.as_str(), reason = %reason, "flow: stage failed");
    ctx.emit(FlowProgress::Failed {
        stage,
        reason: reason.clone(),
    });
    FlowOutcome::Failed { stage, reason }
}

/// Serialize and persist the mutable workstream state so a crash or retry can
/// re-enter at the right stage. Best-effort: a write failure logs and continues
/// rather than failing the run (the on-disk stage docs are the source of truth
/// for resume; this state just picks the re-entry point and chunk statuses).
fn save_workstream(ctx: &Ctx, ws: &Workstream) {
    match serde_json::to_vec(ws) {
        Ok(bytes) => {
            if let Err(e) =
                ctx.store
                    .write_workstream_state(&ctx.project_id, &ctx.workstream_id, &bytes)
            {
                warn!(error = %e, "flow: failed to persist workstream state");
            }
        }
        Err(e) => warn!(error = %e, "flow: failed to serialize workstream state"),
    }
}

/// Rehydrate the persisted mutable workstream state. Returns `None` when no
/// state has been written yet (first run) or when the persisted bytes are
/// unreadable (corruption / version skew) so the caller falls back to fresh.
fn load_workstream(store: &FlowStore, project_id: &str, workstream_id: &str) -> Option<Workstream> {
    let bytes = store
        .read_workstream_state(project_id, workstream_id)
        .ok()
        .flatten()?;
    match serde_json::from_slice::<Workstream>(&bytes) {
        Ok(ws) => {
            info!(stage = ?ws.stage, "flow: rehydrated workstream state");
            Some(ws)
        }
        Err(e) => {
            warn!(error = %e, "flow: workstream state unreadable, starting fresh");
            None
        }
    }
}

/// Build the prompt for a stage by substituting the relevant template.
fn stage_prompt(stage: Stage, input: &str, chunk_id: Option<&str>, workstream_id: &str) -> String {
    match stage {
        Stage::Scout => templates::substitute(
            templates::SCOUT,
            &[("workstream_id", workstream_id), ("request", input)],
        ),
        Stage::Tpm => templates::substitute(
            templates::TPM,
            &[
                ("workstream_id", workstream_id),
                ("findings", input),
                ("request", input),
            ],
        ),
        Stage::Plan => templates::substitute(
            templates::PLAN,
            &[("workstream_id", workstream_id), ("findings", input)],
        ),
        Stage::Req => templates::substitute(
            templates::REQ,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", chunk_id.unwrap_or("")),
                ("findings", input),
            ],
        ),
        Stage::Execute => templates::substitute(
            templates::EXECUTE,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", chunk_id.unwrap_or("")),
                ("findings", input),
            ],
        ),
        Stage::Review => templates::substitute(
            templates::REVIEW,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", chunk_id.unwrap_or("")),
            ],
        ),
        Stage::Qa => templates::substitute(
            templates::QA,
            &[
                ("workstream_id", workstream_id),
                ("chunk_id", chunk_id.unwrap_or("")),
                ("findings", input),
            ],
        ),
        Stage::Integrator => {
            templates::substitute(templates::INTEGRATOR, &[("workstream_id", workstream_id)])
        }
        Stage::Verifier => templates::substitute(
            templates::VERIFIER,
            &[("workstream_id", workstream_id), ("findings", input)],
        ),
    }
}

/// Launch a stage with the given input, persisting nothing (the caller
/// persists). `chunk_id` is set for per-chunk stages. `isolate` is forwarded
/// into [`StageLaunch`] so Execute (and only Execute, when `parallel_chunks >
/// 1`) runs in a worktree.
async fn launch(
    runner: &Arc<dyn StageRunner + Send + Sync>,
    ctx: &Ctx,
    stage: Stage,
    input: &str,
    chunk_id: Option<&str>,
    isolate: bool,
) -> Result<String, FlowRunError> {
    let spec = stage.spec();
    let prompt = stage_prompt(stage, input, chunk_id, &ctx.workstream_id);
    let launch = StageLaunch {
        spec,
        schema: stage_schema(stage),
        prompt,
        chunk_id,
        isolate: isolate && stage == Stage::Execute,
    };
    runner.run(launch).await
}

/// Dependency-aware chunk scheduler. Runs chunks in dependency order: a chunk
/// becomes eligible only when every chunk in its `depends_on` list has reached
/// `Done`. Up to `concurrency` chunks execute at once. Returns the per-chunk
/// results in completion order; the caller applies them to the workstream.
///
/// On the first error the scheduler stops launching new chunks, waits for
/// in-flight chunks to finish, and returns with the failed chunk's result
/// included (the caller surfaces the failure).
#[allow(clippy::too_many_arguments)]
async fn run_chunk_dag(
    runner: &Arc<dyn StageRunner + Send + Sync>,
    ctx: &Ctx,
    ws: &mut Workstream,
    chunks: &[PlanChunk],
    plan_doc: &str,
    isolate: bool,
    concurrency: usize,
    review_budget: usize,
    qa_budget: usize,
) -> Vec<(String, Result<(), FlowRunError>)> {
    use tokio::task::JoinSet;

    let mut results: Vec<(String, Result<(), FlowRunError>)> = Vec::new();
    let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut launched: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut failed = false;
    let mut join_set: JoinSet<(String, Result<(), FlowRunError>)> = JoinSet::new();

    loop {
        // Launch eligible chunks up to the concurrency limit.
        while !failed && join_set.len() < concurrency {
            let next = chunks.iter().find(|c| {
                !launched.contains(&c.id) && c.depends_on.iter().all(|dep| done.contains(dep))
            });
            let Some(chunk) = next else { break };
            launched.insert(chunk.id.clone());
            ws.set_chunk_status(&chunk.id, ChunkStatus::Running);
            ctx.emit(FlowProgress::Chunk {
                id: chunk.id.clone(),
                title: chunk.title.clone(),
                status: ChunkStatus::Running,
                stage: Some(Stage::Req),
                depends_on: Vec::new(),
                order: 0,
            });
            let runner = Arc::clone(runner);
            let ctx = ctx.clone();
            let chunk_id = chunk.id.clone();
            let plan_doc = plan_doc.to_string();
            join_set.spawn(async move {
                let res = run_chunk(
                    &runner,
                    &ctx,
                    &chunk_id,
                    &plan_doc,
                    isolate,
                    review_budget,
                    qa_budget,
                )
                .await;
                (chunk_id, res)
            });
        }

        // If nothing is in flight and nothing left to launch, we're done.
        if join_set.is_empty() {
            break;
        }

        // Wait for the next chunk to complete.
        let Some(joined) = join_set.join_next().await else {
            break;
        };
        let (chunk_id, res) = joined.unwrap_or_else(|e| {
            (
                String::new(),
                Err(FlowRunError::Other(format!("chunk task panicked: {e}"))),
            )
        });
        let is_err = res.is_err();
        if is_err {
            failed = true;
        } else {
            done.insert(chunk_id.clone());
        }
        results.push((chunk_id, res));
    }

    results
}

/// Run the per-chunk sub-pipeline: Req -> Execute -> Review (bounded retry)
/// -> QA (bounded retry). Each Execute re-run carries the latest findings.
async fn run_chunk(
    runner: &Arc<dyn StageRunner + Send + Sync>,
    ctx: &Ctx,
    chunk_id: &str,
    plan_doc: &str,
    isolate: bool,
    review_budget: usize,
    qa_budget: usize,
) -> Result<(), FlowRunError> {
    // Req
    ctx.emit_chunk(chunk_id, Stage::Req);
    let req_out = launch(runner, ctx, Stage::Req, plan_doc, Some(chunk_id), false)
        .await
        .map_err(|e| e.with_stage(Stage::Req))?;
    persist(ctx, Stage::Req, Some(chunk_id), &req_out);

    // Execute (first pass). `isolate` only takes effect for Execute.
    ctx.emit_chunk(chunk_id, Stage::Execute);
    let mut exec_out = launch(
        runner,
        ctx,
        Stage::Execute,
        &req_out,
        Some(chunk_id),
        isolate,
    )
    .await
    .map_err(|e| e.with_stage(Stage::Execute))?;
    persist(ctx, Stage::Execute, Some(chunk_id), &exec_out);

    // Review -> Execute retry loop, bounded by max_review_iterations. The loop
    // runs the review subagent, and when it reports `status: fail` (or a lexical
    // [P0]/[P1] marker) re-runs Execute with the findings appended. If the
    // budget is exhausted while the most recent review still blocks, the chunk
    // FAILS rather than silently progressing to QA — this is the gate that
    // prevents known-serious findings from being skipped.
    let mut last_review_blocked = false;
    for _ in 0..review_budget {
        ctx.emit_chunk(chunk_id, Stage::Review);
        let review_out = launch(runner, ctx, Stage::Review, &exec_out, Some(chunk_id), false)
            .await
            .map_err(|e| e.with_stage(Stage::Review))?;
        persist(ctx, Stage::Review, Some(chunk_id), &review_out);
        last_review_blocked = review_failed(&review_out);
        if !last_review_blocked {
            break;
        }
        let re_exec_input = format!("{req_out}\n\nReview findings:\n{review_out}");
        ctx.emit_chunk(chunk_id, Stage::Execute);
        exec_out = launch(
            runner,
            ctx,
            Stage::Execute,
            &re_exec_input,
            Some(chunk_id),
            isolate,
        )
        .await
        .map_err(|e| e.with_stage(Stage::Execute))?;
        persist(ctx, Stage::Execute, Some(chunk_id), &exec_out);
    }
    if last_review_blocked {
        return Err(FlowRunError::Stage {
            stage: Some(Stage::Review),
            reason: format!(
                "chunk {chunk_id} still has blocking review findings after \
                 {review_budget} review iteration(s)"
            ),
        });
    }

    // QA -> Execute retry loop, bounded by max_qa_iterations. Symmetric to the
    // review loop: if QA still reports `fail` when the budget is exhausted, the
    // chunk fails instead of being passed to the Integrator.
    let mut last_qa_failed = false;
    for _ in 0..qa_budget {
        ctx.emit_chunk(chunk_id, Stage::Qa);
        let qa_out = launch(runner, ctx, Stage::Qa, &exec_out, Some(chunk_id), false)
            .await
            .map_err(|e| e.with_stage(Stage::Qa))?;
        persist(ctx, Stage::Qa, Some(chunk_id), &qa_out);
        last_qa_failed = qa_failed(&qa_out);
        if !last_qa_failed {
            break;
        }
        let re_exec_input = format!("{req_out}\n\nQA findings:\n{qa_out}");
        ctx.emit_chunk(chunk_id, Stage::Execute);
        exec_out = launch(
            runner,
            ctx,
            Stage::Execute,
            &re_exec_input,
            Some(chunk_id),
            isolate,
        )
        .await
        .map_err(|e| e.with_stage(Stage::Execute))?;
        persist(ctx, Stage::Execute, Some(chunk_id), &exec_out);
    }
    if last_qa_failed {
        return Err(FlowRunError::Stage {
            stage: Some(Stage::Qa),
            reason: format!("chunk {chunk_id} still fails QA after {qa_budget} qa iteration(s)"),
        });
    }

    // Announce the chunk's own completion so the signal is local to the
    // per-chunk pipeline rather than implicit in the caller. The caller still
    // applies the Done status (a backstop); this emit carries the chunk id and
    // the QA stage so a driver can attribute the transition precisely.
    ctx.emit(FlowProgress::Chunk {
        id: chunk_id.to_string(),
        title: String::new(),
        status: ChunkStatus::Done,
        stage: Some(Stage::Qa),
        depends_on: Vec::new(),
        order: 0,
    });

    Ok(())
}

/// A QA report fails when its `status` field is `fail`. Tolerates prose around
/// the JSON (mirrors `extract_json`).
fn qa_failed(qa_doc: &str) -> bool {
    let Some(value) = extract_json(qa_doc) else {
        return false;
    };
    value
        .get("status")
        .and_then(|s| s.as_str())
        .map(|s| s.eq_ignore_ascii_case("fail"))
        .unwrap_or(false)
}

/// Integrator merge gate: returns true when the integration checkpoint
/// reports no conflicts (`status: integrated` or `conflicts_found: 0`).
/// Called only when `parallel_chunks > 1`, i.e. exactly when a real conflict
/// check matters, so an unparseable Integrator response fails CLOSED (returns
/// false) rather than silently passing a possibly-conflicted merge. Real
/// `conflicts` tool wiring is layered on top of this structured-parse gate.
fn merge_ok(integration_doc: &str) -> bool {
    let Some(value) = extract_json(integration_doc) else {
        warn!("flow: integrator produced no parseable checkpoint, failing merge gate");
        return false;
    };
    let status_ok = value
        .get("status")
        .and_then(|s| s.as_str())
        .map(|s| s == "integrated")
        .unwrap_or(false);
    let conflicts_zero = value
        .get("conflicts_found")
        .and_then(|c| c.as_u64())
        .map(|n| n == 0)
        .unwrap_or(true);
    status_ok && conflicts_zero
}

#[derive(Debug, Clone)]
struct PlanChunk {
    id: String,
    title: String,
    depends_on: Vec<String>,
}

/// Extract chunk id+title+depends_on from a Plan stage JSON document, in array
/// order. Tolerates prose around the JSON (mirrors `extract_json`). Chunks with
/// no `depends_on` get an empty vector.
fn parse_chunks(plan_doc: &str) -> Vec<PlanChunk> {
    let Some(value) = extract_json(plan_doc) else {
        return Vec::new();
    };
    let Some(arr) = value.get("chunks").and_then(|c| c.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|c| {
            let id = c.get("id").and_then(|i| i.as_str())?.to_string();
            let title = c
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let depends_on = c
                .get("depends_on")
                .and_then(|d| d.as_array())
                .map(|deps| {
                    deps.iter()
                        .filter_map(|d| d.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            Some(PlanChunk {
                id,
                title,
                depends_on,
            })
        })
        .collect()
}

/// A review report blocks progression when its structured `status` field is
/// `fail`. Tolerates prose around the JSON (mirrors `extract_json`). When no
/// JSON can be parsed, falls back to a lexical scan for `[P0]`/`[P1]` markers
/// (the form emitted by `report_finding`). If neither yields a signal the
/// review is treated as PASSING — the review subagent is now prompted to emit a
/// structured `review_report` with an explicit `status`, so absence of both the
/// JSON and the markers means "no blocker found", not "unknown".
fn review_failed(review: &str) -> bool {
    if let Some(value) = extract_json(review) {
        return value
            .get("status")
            .and_then(|s| s.as_str())
            .map(|s| s.eq_ignore_ascii_case("fail"))
            .unwrap_or(false);
    }
    review.lines().any(|line| {
        line.contains("[P0]")
            || line.contains("[p0]")
            || line.contains("[P1]")
            || line.contains("[p1]")
    })
}

fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(v @ (Value::Object(_) | Value::Array(_))) = serde_json::from_str::<Value>(trimmed) {
        return Some(v);
    }
    let bytes = trimmed.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth > 0 => {
                depth -= 1;
                if depth == 0 && b == close {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end?;
    serde_json::from_str(&trimmed[start..=end]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use test_case::test_case;

    #[test]
    fn fnv1a_64_is_16_hex_chars() {
        let hash = fnv1a_64(b"craft");
        assert_eq!(hash.len(), 16);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn fnv1a_64_empty_is_offset_basis() {
        assert_eq!(fnv1a_64(b""), "cbf29ce484222325");
    }

    #[test]
    fn project_id_is_basename_dash_hash() {
        let id = project_id(std::path::Path::new("/tmp/craft"));
        assert!(id.starts_with("craft-"), "got {id}");
        let suffix = id.split('-').nth(1).unwrap();
        assert_eq!(suffix.len(), 16);
    }

    #[test_case(Stage::Scout, "scout")]
    #[test_case(Stage::Req, "req")]
    #[test_case(Stage::Qa, "qa")]
    #[test_case(Stage::Verifier, "verifier")]
    fn stage_as_str_roundtrips(stage: Stage, s: &str) {
        assert_eq!(stage.as_str(), s);
    }

    #[test]
    fn stage_spec_matches_proposal_table() {
        assert_eq!(Stage::Scout.spec().subagent_type, SUBAGENT_RESEARCH);
        assert_eq!(Stage::Execute.spec().subagent_type, SUBAGENT_GENERAL);
        assert_eq!(Stage::Execute.spec().isolation, Some(WorktreeIso::Worktree));
        assert_eq!(Stage::Req.spec().isolation, None);
        assert_eq!(Stage::Qa.spec().subagent_type, SUBAGENT_GENERAL);
        assert_eq!(Stage::Verifier.spec().role, "flow_verifier");
    }

    #[test]
    fn structured_stages_have_schemas_prose_stages_do_not() {
        for structured in [
            Stage::Tpm,
            Stage::Plan,
            Stage::Req,
            Stage::Review,
            Stage::Qa,
            Stage::Integrator,
            Stage::Verifier,
        ] {
            assert!(
                stage_schema(structured).is_some(),
                "{structured:?} should have a schema"
            );
        }
        for prose in [Stage::Scout, Stage::Execute] {
            assert!(
                stage_schema(prose).is_none(),
                "{prose:?} should not have a schema"
            );
        }
    }

    #[test]
    fn chunk_status_glyph_is_nonempty() {
        for status in [
            ChunkStatus::Queued,
            ChunkStatus::Running,
            ChunkStatus::NeedsReview,
            ChunkStatus::Blocked,
            ChunkStatus::Done,
        ] {
            assert!(!status.glyph().is_empty());
        }
    }

    #[test]
    fn workstream_tracks_chunk_status() {
        let mut ws = Workstream::new("proj", "ws1");
        ws.set_chunk_status("c1", ChunkStatus::Running);
        assert_eq!(ws.chunk_status("c1"), Some(ChunkStatus::Running));
        assert!(!ws.all_done());
        ws.set_chunk_status("c1", ChunkStatus::Done);
        assert!(ws.all_done());
    }

    #[test]
    fn parse_chunks_reads_plan_json() {
        let plan = r#"{"summary":"x","chunks":[{"id":"a","title":"A","description":"d"},{"id":"b","title":"B","description":"d"}]}"#;
        let chunks = parse_chunks(plan);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].id, "a");
        assert_eq!(chunks[0].title, "A");
        assert_eq!(chunks[1].id, "b");
    }

    #[test]
    fn parse_chunks_reads_depends_on() {
        let plan = r#"{"summary":"x","chunks":[
            {"id":"a","title":"A","description":"d"},
            {"id":"b","title":"B","description":"d","depends_on":["a"]},
            {"id":"c","title":"C","description":"d","depends_on":["a","b"]}
        ]}"#;
        let chunks = parse_chunks(plan);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].depends_on.is_empty());
        assert_eq!(chunks[1].depends_on, vec!["a".to_string()]);
        assert_eq!(chunks[2].depends_on, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_chunks_tolerates_surrounding_prose() {
        let plan = "Here is the plan: {\"chunks\":[{\"id\":\"only\",\"title\":\"Only\"}]} done.";
        let chunks = parse_chunks(plan);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].id, "only");
        assert_eq!(chunks[0].title, "Only");
    }

    #[test]
    fn parse_chunks_empty_when_no_json_or_no_chunks() {
        assert!(parse_chunks("just prose, no json").is_empty());
        assert!(parse_chunks(r#"{"summary":"x"}"#).is_empty());
    }

    #[test_case("[P0] leak", true ; "p0_blocks")]
    #[test_case("a [P1] issue", true ; "p1_blocks")]
    #[test_case("[P2] nit", false ; "p2_does_not_block")]
    #[test_case("no P0/P1 findings", false ; "negation_does_not_block")]
    #[test_case("looks fine", false ; "clean_does_not_block")]
    fn review_failed_lexical_fallback(review: &str, blocks: bool) {
        assert_eq!(review_failed(review), blocks);
    }

    #[test_case(r#"{"status":"pass"}"#, false ; "qa_pass")]
    #[test_case(r#"{"status":"fail","findings":["x"]}"#, true ; "qa_fail")]
    #[test_case("no json here", false ; "qa_no_json_is_pass")]
    fn qa_failure_detected(doc: &str, failed: bool) {
        assert_eq!(qa_failed(doc), failed);
    }

    #[test_case(r#"{"status":"integrated","conflicts_found":0}"#, true ; "clean_merge")]
    #[test_case(r#"{"status":"conflicts","conflicts_found":2}"#, false ; "conflicts_block")]
    #[test_case("prose only", false ; "no_json_fails_closed")]
    fn merge_gate_parses_checkpoint(doc: &str, merged: bool) {
        assert_eq!(merge_ok(doc), merged);
    }

    /// Recorded stage invocation: (stage, optional chunk id, isolate flag).
    type Call = (Stage, Option<String>, bool);

    /// A scripted runner: pops the next scripted output per stage invocation
    /// (so tests push outputs in reverse stage order). Optionally records calls.
    struct ScriptedRunner {
        outputs: Mutex<Vec<String>>,
        calls: Arc<Mutex<Vec<Call>>>,
        record: bool,
    }

    impl ScriptedRunner {
        fn new(outputs: Vec<String>) -> Self {
            Self {
                outputs: Mutex::new(outputs),
                calls: Arc::new(Mutex::new(Vec::new())),
                record: false,
            }
        }

        fn recording(outputs: Vec<String>) -> Self {
            Self {
                outputs: Mutex::new(outputs),
                calls: Arc::new(Mutex::new(Vec::new())),
                record: true,
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl StageRunner for ScriptedRunner {
        fn run<'a>(&'a self, launch: StageLaunch<'a>) -> StageFuture<'a> {
            let calls = self.record.then(|| Arc::clone(&self.calls));
            Box::pin(async move {
                if let Some(calls) = calls {
                    calls.lock().unwrap().push((
                        launch.spec.stage,
                        launch.chunk_id.map(str::to_string),
                        launch.isolate,
                    ));
                }
                self.outputs
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| FlowRunError::Other("scripted runner exhausted".into()))
            })
        }
    }

    fn tmp_store() -> (tempfile::TempDir, Arc<FlowStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::from_root(tmp.path().to_path_buf()));
        (tmp, store)
    }

    fn params(store: Arc<FlowStore>, outputs: Vec<String>) -> FlowParams {
        FlowParams {
            approval: Some(ApprovalPayload::Approved),
            runner: Some(Arc::new(ScriptedRunner::new(outputs))),
            ..FlowParams::new("proj", "ws1", "do the thing", FlowConfig::default(), store)
        }
    }

    fn plan_doc_one_chunk() -> &'static str {
        r#"{"summary":"s","chunks":[{"id":"c1","title":"C1","description":"d"}]}"#
    }

    #[tokio::test]
    async fn runs_single_chunk_workstream_end_to_end() {
        // Pushed in reverse so pop() yields Scout first.
        let outputs_rev = vec![
            // Verifier, Integrator, QA, Review, Execute, Req, Plan, TPM, Scout
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(),
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(),
            r#"{"status":"pass"}"#.to_string(),
            "no blocking findings".to_string(),
            "executed chunk".to_string(),
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),
            plan_doc_one_chunk().to_string(),
            "goal doc".to_string(),
            "scout findings".to_string(),
        ];
        let outputs: Vec<String> = outputs_rev;

        let (_tmp, store) = tmp_store();
        let runner = Arc::new(ScriptedRunner::recording(outputs));
        let p = FlowParams {
            runner: Some(runner.clone()),
            ..params(store.clone(), Vec::new())
        };
        let outcome = run(p).await;

        let FlowOutcome::Done {
            verification_report,
        } = outcome
        else {
            panic!("expected Done, got {outcome:?}");
        };
        assert!(verification_report.contains("ship"));

        // Each stage persisted its document.
        assert!(
            store
                .read("proj", "ws1", "scout.md")
                .unwrap()
                .contains("scout")
        );
        assert!(
            store
                .read("proj", "ws1", "tpm.md")
                .unwrap()
                .contains("goal")
        );
        assert!(
            store
                .read("proj", "ws1", "plan.md")
                .unwrap()
                .contains("chunks")
        );
        assert!(
            store
                .read("proj", "ws1", "req_c1.md")
                .unwrap()
                .contains("spec")
        );
        assert!(
            store
                .read("proj", "ws1", "execute_c1.md")
                .unwrap()
                .contains("executed")
        );
        assert!(
            store
                .read("proj", "ws1", "review_c1.md")
                .unwrap()
                .contains("findings")
        );
        assert!(
            store
                .read("proj", "ws1", "qa_c1.md")
                .unwrap()
                .contains("pass")
        );
        assert!(
            store
                .read("proj", "ws1", "integrator.md")
                .unwrap()
                .contains("integrated")
        );
        assert!(
            store
                .read("proj", "ws1", "verifier.md")
                .unwrap()
                .contains("ship")
        );
    }

    #[tokio::test]
    async fn review_loop_reruns_execute_until_clean() {
        // Review returns fail on the first pass (re-Execute), then pass. Needs
        // max_review_iterations >= 2 so the second review iteration can run.
        let outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator
            r#"{"status":"pass"}"#.to_string(),                  // QA
            r#"{"chunk_id":"c1","status":"pass"}"#.to_string(),  // Review (2nd)
            "executed chunk v2".to_string(),                     // Execute (2nd)
            r#"{"chunk_id":"c1","status":"fail","findings":["[P1] bug here"]}"#.to_string(), // Review (1st, blocks)
            "executed chunk v1".to_string(), // Execute (1st)
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(), // Req
            plan_doc_one_chunk().to_string(), // Plan
            "goal doc".to_string(),          // TPM
            "scout findings".to_string(),    // Scout
        ];
        let outputs: Vec<String> = outputs_rev;

        let (_tmp, store) = tmp_store();
        let params = FlowParams {
            config: FlowConfig {
                max_review_iterations: 2,
                ..FlowConfig::default()
            },
            ..params(store.clone(), outputs)
        };
        let outcome = run(params).await;
        assert!(matches!(outcome, FlowOutcome::Done { .. }));
        let exec = store.read("proj", "ws1", "execute_c1.md").unwrap();
        assert!(
            exec.contains("v2"),
            "execute should reflect the re-run: {exec}"
        );
    }

    #[tokio::test]
    async fn review_loop_exhaustion_fails_chunk_instead_of_proceeding_to_qa() {
        // Review always returns fail; after max_review_iterations the chunk
        // MUST fail rather than silently moving on to QA (the bug being fixed).
        let mut outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier (not reached)
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator (not reached)
            r#"{"status":"pass"}"#.to_string(), // QA (must NOT be reached)
        ];
        for _ in 0..3 {
            outputs_rev.push(
                r#"{"chunk_id":"c1","status":"fail","findings":["[P0] still broken"]}"#.to_string(),
            ); // Review
            outputs_rev.push("executed".to_string()); // Execute
        }
        outputs_rev.push(r#"{"chunk_id":"c1","spec":"s"}"#.to_string()); // Req
        outputs_rev.push(plan_doc_one_chunk().to_string()); // Plan
        outputs_rev.push("goal doc".to_string()); // TPM
        outputs_rev.push("scout findings".to_string()); // Scout
        let outputs: Vec<String> = outputs_rev;

        let (_tmp, store) = tmp_store();
        let runner = Arc::new(ScriptedRunner::recording(outputs));
        let params = FlowParams {
            config: FlowConfig {
                max_review_iterations: 2,
                ..FlowConfig::default()
            },
            runner: Some(runner.clone()),
            ..params(store, Vec::new())
        };
        let outcome = run(params).await;
        let FlowOutcome::Failed { stage, reason } = outcome else {
            panic!("expected Failed when review never clears, got {outcome:?}");
        };
        assert_eq!(stage, Stage::Review);
        assert!(
            reason.contains("blocking review findings"),
            "reason should mention blocking findings: {reason}"
        );
        let launched: Vec<Stage> = runner.calls().iter().map(|(s, _, _)| *s).collect();
        assert!(
            !launched.contains(&Stage::Qa),
            "QA must not run when review never clears, got {launched:?}"
        );
    }

    #[tokio::test]
    async fn qa_loop_exhaustion_fails_chunk_instead_of_proceeding_to_integrator() {
        // QA returns fail; after max_qa_iterations the chunk MUST fail rather
        // than silently moving on to the Integrator. Review passes immediately
        // (no re-Execute). Pop order: Review(pass) -> QA(fail) -> Execute(2nd).
        // For QA to pop the fail JSON first, Execute(2nd) must sit ABOVE it in
        // the reversed script (lower index = popped later).
        let outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier (not reached)
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator (not reached)
            "executed again".to_string(), // Execute (2nd, after QA fail)
            r#"{"chunk_id":"c1","status":"fail"}"#.to_string(), // QA (fails -> re-Execute)
            r#"{"chunk_id":"c1","status":"pass"}"#.to_string(), // Review (passes)
            "executed".to_string(),       // Execute (1st)
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(), // Req
            plan_doc_one_chunk().to_string(), // Plan
            "goal doc".to_string(),       // TPM
            "scout findings".to_string(), // Scout
        ];
        let outputs: Vec<String> = outputs_rev;

        let (_tmp, store) = tmp_store();
        let runner = Arc::new(ScriptedRunner::recording(outputs));
        let params = FlowParams {
            config: FlowConfig {
                max_qa_iterations: 1,
                ..FlowConfig::default()
            },
            runner: Some(runner.clone()),
            ..params(store, Vec::new())
        };
        let outcome = run(params).await;
        let FlowOutcome::Failed { stage, reason } = outcome else {
            panic!("expected Failed when QA never passes, got {outcome:?}");
        };
        assert_eq!(stage, Stage::Qa);
        assert!(
            reason.contains("fails QA"),
            "reason should mention QA failure: {reason}"
        );
    }

    #[tokio::test]
    async fn pauses_at_approval_gate_without_payload() {
        let outputs_rev = vec![
            plan_doc_one_chunk().to_string(), // Plan (not reached)
            "goal doc".to_string(),           // TPM
            "scout findings".to_string(),     // Scout
        ];
        let outputs: Vec<String> = outputs_rev;
        let (_tmp, store) = tmp_store();
        let mut p = params(store, outputs);
        p.approval = None;
        let outcome = run(p).await;
        let FlowOutcome::AwaitingGoalApproval { goal_doc } = outcome else {
            panic!("expected AwaitingGoalApproval, got {outcome:?}");
        };
        assert!(goal_doc.contains("goal"));
    }

    #[tokio::test]
    async fn fails_when_plan_has_no_chunks() {
        let outputs_rev = vec![
            r#"{"summary":"s","chunks":[]}"#.to_string(), // Plan (no chunks)
            "goal doc".to_string(),                       // TPM
            "scout findings".to_string(),                 // Scout
        ];
        let outputs: Vec<String> = outputs_rev;
        let (_tmp, store) = tmp_store();
        let outcome = run(params(store, outputs)).await;
        let FlowOutcome::Failed { stage, reason } = outcome else {
            panic!("expected Failed, got {outcome:?}");
        };
        assert_eq!(stage, Stage::Plan);
        assert!(reason.contains("no chunks"));
    }

    #[tokio::test]
    async fn parallel_chunks_propagates_isolate_flag_to_execute() {
        let outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(),
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(),
            r#"{"status":"pass"}"#.to_string(),
            "clean review".to_string(),
            "executed".to_string(),
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),
            plan_doc_one_chunk().to_string(),
            "goal doc".to_string(),
            "scout findings".to_string(),
        ];
        let outputs: Vec<String> = outputs_rev;

        let (_tmp, store) = tmp_store();
        let runner = Arc::new(ScriptedRunner::recording(outputs));
        let params = FlowParams {
            config: FlowConfig {
                parallel_chunks: 2,
                ..FlowConfig::default()
            },
            runner: Some(runner.clone()),
            ..params(store, Vec::new())
        };
        let outcome = run(params).await;
        assert!(matches!(outcome, FlowOutcome::Done { .. }));

        let execute_isolates: Vec<bool> = runner
            .calls()
            .iter()
            .filter(|(s, _, _)| *s == Stage::Execute)
            .map(|(_, _, iso)| *iso)
            .collect();
        assert!(!execute_isolates.is_empty(), "Execute should have launched");
        assert!(
            execute_isolates.iter().all(|&f| f),
            "every Execute launch should have isolate=true when parallel_chunks > 1"
        );
    }

    #[tokio::test]
    async fn resume_at_gate_skips_scout_and_tpm_when_goal_doc_persisted() {
        // First run: pause at the gate, persisting scout.md + tpm.md.
        let (_tmp, store) = tmp_store();
        let first = FlowParams {
            approval: None,
            runner: Some(Arc::new(ScriptedRunner::recording(vec![
                "goal doc".to_string(),       // TPM
                "scout findings".to_string(), // Scout
            ]))),
            ..params(store.clone(), Vec::new())
        };
        let outcome = run(first).await;
        let FlowOutcome::AwaitingGoalApproval { goal_doc } = outcome else {
            panic!("expected AwaitingGoalApproval, got {outcome:?}");
        };
        assert_eq!(goal_doc, "goal doc");
        assert!(
            store
                .read("proj", "ws1", "tpm.md")
                .unwrap()
                .contains("goal")
        );

        // Second run (resume): approval supplied, tpm.md already persisted.
        // Scout and TPM must NOT be launched again; Plan..Verifier run.
        let runner = Arc::new(ScriptedRunner::recording(vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator
            r#"{"status":"pass"}"#.to_string(),                  // QA
            "clean review".to_string(),                          // Review
            "executed".to_string(),                              // Execute
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),       // Req
            plan_doc_one_chunk().to_string(),                    // Plan
        ]));
        let resume = FlowParams {
            approval: Some(ApprovalPayload::Approved),
            runner: Some(runner.clone()),
            ..params(store, Vec::new())
        };
        let outcome = run(resume).await;
        assert!(matches!(outcome, FlowOutcome::Done { .. }));

        let launched: Vec<Stage> = runner.calls().iter().map(|(s, _, _)| *s).collect();
        assert!(
            !launched.contains(&Stage::Scout),
            "resume should skip Scout, got {launched:?}"
        );
        assert!(
            !launched.contains(&Stage::Tpm),
            "resume should skip TPM, got {launched:?}"
        );
        assert!(launched.contains(&Stage::Plan));
    }

    #[tokio::test]
    async fn single_chunk_does_not_isolate_execute() {
        // parallel_chunks == 1 (default): the Worktree downgrade contract says
        // Execute must launch with isolate=false. This is the regression guard
        // for the bug where launch() hardcoded isolate=true for all Execute.
        let outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(),
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(),
            r#"{"status":"pass"}"#.to_string(),
            "clean review".to_string(),
            "executed".to_string(),
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),
            plan_doc_one_chunk().to_string(),
            "goal doc".to_string(),
            "scout findings".to_string(),
        ];
        let (_tmp, store) = tmp_store();
        let runner = Arc::new(ScriptedRunner::recording(outputs_rev));
        let params = FlowParams {
            config: FlowConfig {
                parallel_chunks: 1,
                ..FlowConfig::default()
            },
            runner: Some(runner.clone()),
            ..params(store, Vec::new())
        };
        let outcome = run(params).await;
        assert!(matches!(outcome, FlowOutcome::Done { .. }));

        let execute_isolates: Vec<bool> = runner
            .calls()
            .iter()
            .filter(|(s, _, _)| *s == Stage::Execute)
            .map(|(_, _, iso)| *iso)
            .collect();
        assert!(
            execute_isolates.iter().all(|&f| !f),
            "Execute should have isolate=false when parallel_chunks == 1, got {execute_isolates:?}"
        );
    }

    #[tokio::test]
    async fn parallel_chunks_run_concurrently_and_all_execute_isolated() {
        // Two chunks, parallel_chunks = 2. Both Execute launches must carry
        // isolate=true, and both chunks must complete (concurrent dispatch).
        let two_chunk_plan = r#"{"summary":"s","chunks":[
            {"id":"a","title":"A","description":"d"},
            {"id":"b","title":"B","description":"d"}
        ]}"#;
        let outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator
            // chunk b (QA, Review, Execute, Req) -- order non-deterministic
            r#"{"status":"pass"}"#.to_string(),
            "clean review".to_string(),
            "executed b".to_string(),
            r#"{"chunk_id":"b","spec":"s"}"#.to_string(),
            // chunk a
            r#"{"status":"pass"}"#.to_string(),
            "clean review".to_string(),
            "executed a".to_string(),
            r#"{"chunk_id":"a","spec":"s"}"#.to_string(),
            two_chunk_plan.to_string(),   // Plan
            "goal doc".to_string(),       // TPM
            "scout findings".to_string(), // Scout
        ];
        let (_tmp, store) = tmp_store();
        let runner = Arc::new(ScriptedRunner::recording(outputs_rev));
        let params = FlowParams {
            config: FlowConfig {
                parallel_chunks: 2,
                ..FlowConfig::default()
            },
            runner: Some(runner.clone()),
            ..params(store.clone(), Vec::new())
        };
        let outcome = run(params).await;
        let FlowOutcome::Done { .. } = outcome else {
            panic!("expected Done, got {outcome:?}");
        };

        let execute_calls: Vec<Option<String>> = runner
            .calls()
            .iter()
            .filter(|(s, _, iso)| *s == Stage::Execute && *iso)
            .map(|(_, c, _)| c.clone())
            .collect();
        let chunk_ids: Vec<&str> = execute_calls.iter().flatten().map(String::as_str).collect();
        assert!(
            chunk_ids.contains(&"a"),
            "chunk a Execute missing: {chunk_ids:?}"
        );
        assert!(
            chunk_ids.contains(&"b"),
            "chunk b Execute missing: {chunk_ids:?}"
        );
        assert!(
            store
                .read("proj", "ws1", "execute_a.md")
                .unwrap()
                .contains("executed a")
        );
        assert!(
            store
                .read("proj", "ws1", "execute_b.md")
                .unwrap()
                .contains("executed b")
        );
    }

    #[tokio::test]
    async fn parallel_unparseable_integrator_fails_merge_gate() {
        // parallel_chunks > 1 but the Integrator returns prose (no JSON): the
        // merge gate must fail closed rather than silently passing.
        let outputs_rev = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier (not reached)
            "i am just prose, no checkpoint json".to_string(),   // Integrator
            r#"{"status":"pass"}"#.to_string(),
            "clean review".to_string(),
            "executed".to_string(),
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),
            plan_doc_one_chunk().to_string(),
            "goal doc".to_string(),
            "scout findings".to_string(),
        ];
        let (_tmp, store) = tmp_store();
        let params = FlowParams {
            config: FlowConfig {
                parallel_chunks: 2,
                ..FlowConfig::default()
            },
            ..params(store, outputs_rev)
        };
        let outcome = run(params).await;
        let FlowOutcome::Failed { stage, reason } = outcome else {
            panic!("expected Failed, got {outcome:?}");
        };
        assert_eq!(stage, Stage::Integrator);
        assert!(
            reason.contains("conflict"),
            "reason should mention conflicts: {reason}"
        );
    }

    #[test_case(Stage::Scout, "scout")]
    #[test_case(Stage::Tpm, "tpm")]
    #[test_case(Stage::Execute, "execute")]
    #[test_case(Stage::Integrator, "integrator")]
    #[test_case(Stage::Verifier, "verifier")]
    fn stage_parse_roundtrips(stage: Stage, s: &str) {
        assert_eq!(Stage::parse(s), Some(stage));
        assert_eq!(Stage::parse(&format!(" {s} ")), Some(stage), "trims input");
        assert_eq!(Stage::parse("nope"), None);
    }

    #[test_case(ChunkStatus::Queued, "queued")]
    #[test_case(ChunkStatus::Running, "running")]
    #[test_case(ChunkStatus::NeedsReview, "needs_review")]
    #[test_case(ChunkStatus::Blocked, "blocked")]
    #[test_case(ChunkStatus::Done, "done")]
    fn chunk_status_parse_roundtrips(status: ChunkStatus, s: &str) {
        assert_eq!(ChunkStatus::parse(s), Some(status));
        assert_eq!(ChunkStatus::parse("nope"), None);
    }

    #[test]
    fn stage_parse_tolerates_quotes_in_input_without_panic() {
        // Regression guard: the old serde-round-trip parse misquoted embedded
        // quotes/escapes; the direct match cannot be confused by them.
        assert_eq!(Stage::parse(r#"scout"injected"#), None);
        assert_eq!(ChunkStatus::parse(r#"done\"escape"#), None);
    }

    #[test_case(r#"{"a":1}"#, Some(r#"{"a":1}"#) ; "plain_object")]
    #[test_case(r#"prefix {"a":1} tail"#, Some(r#"{"a":1}"#) ; "object_in_prose")]
    #[test_case(r#"{"a": [1, {"b": 2}]}"#, Some(r#"{"a": [1, {"b": 2}]}"#) ; "nested_arrays")]
    #[test_case(r#"see item [1] then {"x":2}"#, Some(r#"[1]"#) ; "greedy_first_bracket_wins")]
    #[test_case("no json at all", None ; "no_json")]
    fn extract_json_recovers_object(text: &str, expected: Option<&str>) {
        let v = extract_json(text);
        match expected {
            Some(s) => {
                let v = v.expect("expected Some(json)");
                assert_eq!(v, serde_json::from_str::<serde_json::Value>(s).unwrap());
            }
            None => assert!(v.is_none()),
        }
    }

    #[test_case(r#"{"chunk_id":"c1","status":"fail","findings":["[P0] x"]}"#, true ; "structured_fail_blocks")]
    #[test_case(r#"prefix {"chunk_id":"c1","status":"fail"} tail"#, true ; "structured_fail_in_prose_blocks")]
    #[test_case(r#"{"chunk_id":"c1","status":"pass"}"#, false ; "structured_pass_does_not_block")]
    #[test_case("[P1] bug", true ; "lexical_p1_blocks_without_json")]
    #[test_case("ref to [P0] elsewhere", true ; "lexical_p0_blocks_without_json")]
    #[test_case("no P0/P1 findings", false ; "bare_text_does_not_block")]
    #[test_case("[p2] nit", false ; "lexical_p2_does_not_block")]
    fn review_failed_matches_structured_then_lexical(review: &str, blocks: bool) {
        assert_eq!(review_failed(review), blocks);
    }

    #[tokio::test]
    async fn resume_persists_state_and_skips_completed_stages_on_retry() {
        // First run: approve at the gate, then fail the chunk at Review. The
        // workstream state must be persisted so a retry can re-enter.
        let (_tmp, store) = tmp_store();
        let fail_outputs = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier (not reached)
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator (not reached)
            r#"{"status":"pass"}"#.to_string(),                           // QA (not reached)
            r#"{"chunk_id":"c1","status":"fail","findings":["[P0] broken"]}"#.to_string(), // Review (blocks)
            "executed v1".to_string(),                     // Execute
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(), // Req
            plan_doc_one_chunk().to_string(),              // Plan
            "goal doc".to_string(),                        // TPM
            "scout findings".to_string(),                  // Scout
        ];
        let first = FlowParams {
            approval: Some(ApprovalPayload::Approved),
            config: FlowConfig {
                max_review_iterations: 1,
                ..FlowConfig::default()
            },
            runner: Some(Arc::new(ScriptedRunner::recording(fail_outputs))),
            ..params(store.clone(), Vec::new())
        };
        let outcome = run(first).await;
        let FlowOutcome::Failed { stage, .. } = outcome else {
            panic!("expected Failed, got {outcome:?}");
        };
        assert_eq!(stage, Stage::Review);
        // Workstream state persisted: a retry can read it back.
        assert!(
            store
                .read_workstream_state("proj", "ws1")
                .unwrap()
                .is_some()
        );

        // Retry: resume=true, persisted state exists. Scout/TPM/Plan must be
        // skipped (their docs are on disk). The runner only needs to supply the
        // chunk pipeline outputs again; Review now passes so the run succeeds.
        let retry_outputs = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(), // Verifier
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(), // Integrator
            r#"{"status":"pass"}"#.to_string(),                  // QA
            r#"{"chunk_id":"c1","status":"pass"}"#.to_string(),  // Review (passes)
            "executed v2".to_string(),                           // Execute
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),       // Req
        ];
        let runner = Arc::new(ScriptedRunner::recording(retry_outputs));
        let resume = FlowParams {
            resume: true,
            runner: Some(runner.clone()),
            ..params(store, Vec::new())
        };
        let outcome = run(resume).await;
        assert!(matches!(outcome, FlowOutcome::Done { .. }));

        let launched: Vec<Stage> = runner.calls().iter().map(|(s, _, _)| *s).collect();
        assert!(
            !launched.contains(&Stage::Scout),
            "resume should skip Scout, got {launched:?}"
        );
        assert!(
            !launched.contains(&Stage::Tpm),
            "resume should skip TPM, got {launched:?}"
        );
        assert!(
            !launched.contains(&Stage::Plan),
            "resume should skip Plan when past Execute, got {launched:?}"
        );
    }

    #[tokio::test]
    async fn chunk_emits_done_after_qa_passes() {
        // A successful chunk run must emit a Done progress event for its own
        // chunk id (localized transition, not just the caller's backstop).
        let (_tmp, store) = tmp_store();
        let (tx, rx) = flume::unbounded::<FlowProgress>();
        let outputs = vec![
            r#"{"goal_met":true,"verdict":"ship"}"#.to_string(),
            r#"{"status":"integrated","conflicts_found":0}"#.to_string(),
            r#"{"status":"pass"}"#.to_string(), // QA pass
            "clean review".to_string(),
            "executed".to_string(),
            r#"{"chunk_id":"c1","spec":"s"}"#.to_string(),
            plan_doc_one_chunk().to_string(),
            "goal doc".to_string(),
            "scout findings".to_string(),
        ];
        let p = FlowParams {
            approval: Some(ApprovalPayload::Approved),
            runner: Some(Arc::new(ScriptedRunner::new(outputs))),
            progress: Some(tx),
            ..params(store, Vec::new())
        };
        let outcome = run(p).await;
        assert!(matches!(outcome, FlowOutcome::Done { .. }));

        let events: Vec<FlowProgress> = rx.try_iter().collect();
        let chunk_done_for_c1 = events.iter().any(|e| match e {
            FlowProgress::Chunk {
                id,
                status: ChunkStatus::Done,
                ..
            } => id == "c1",
            _ => false,
        });
        assert!(
            chunk_done_for_c1,
            "expected a Done Chunk event for c1 after QA passed"
        );
    }

    #[test]
    fn save_and_load_workstream_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(FlowStore::from_root(tmp.path().to_path_buf()));
        let mut ws = Workstream::new("proj", "ws1");
        ws.stage = Some(Stage::Execute);
        ws.approved = true;
        ws.set_chunk_status("c1", ChunkStatus::Running);
        let ctx = Ctx::new("proj", "ws1", Arc::clone(&store), None);
        save_workstream(&ctx, &ws);

        let loaded = load_workstream(&store, "proj", "ws1").expect("should rehydrate");
        assert_eq!(loaded.stage, Some(Stage::Execute));
        assert!(loaded.approved);
        assert_eq!(loaded.chunk_status("c1"), Some(ChunkStatus::Running));
    }

    #[test]
    fn load_workstream_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FlowStore::from_root(tmp.path().to_path_buf());
        assert!(load_workstream(&store, "proj", "ws1").is_none());
    }

    #[test]
    fn load_workstream_returns_none_on_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let store = FlowStore::from_root(tmp.path().to_path_buf());
        store
            .write_workstream_state("proj", "ws1", b"not valid json")
            .unwrap();
        assert!(load_workstream(&store, "proj", "ws1").is_none());
    }
}
