//! The long-lived conversation: one animist `Agent` per session, rebuilt
//! around the same `History` and the same layer instances whenever the model
//! or the tool set changes.
//!
//! Every surface (TUI, ACP, headless, print, the subagent launcher) owns one
//! [`Conversation`] per session and drives it through
//! [`driver::run_turn`](super::driver::run_turn). The conversation owns the
//! pieces that must outlive a single run — the `History`, the current model
//! and tools, the layer chain with its typed handles, the turn settings
//! cell, and the event sink.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arc_swap::{ArcSwap, ArcSwapOption};

use animist::agent::{Agent, AgentConfig, AgentLayer, Layer};
use animist::history::History;
use snafu::ResultExt;

use crate::config::Config;
use crate::error::{BuildConversationSnafu, Result, ShutdownConversationSnafu};
use crate::provider::ModelHandle;
use animist::agent::tool::ToolRegistry;

use crate::compaction::SharedCompactionState;
use crate::harness::AgentMode;
use crate::harness::guardrails::GuardrailState;
use crate::harness::retry::RetryCtx;
use crate::snapshot::SnapshotManager;
use crate::thinking::ThinkingConfig;

use super::continuation::{ContinuationHandle, ContinuationLayer, DEFAULT_MAX_CONTINUATION_TURNS};
use super::events::{CraftEvent, EventSink};
use super::guardrail::GuardrailLayer;
use super::persist::Persister;
use super::persist_layer::PersistLayer;
use super::policy::{PolicyLayer, PolicyLayerInputs};
use super::recovery::{ReauthHook, RecoveryHandle, RecoveryLayer};
use super::request::{RequestLayer, RequestLayerInputs};
use super::snapshot_layer::SnapshotLayer;

/// How the surface bounds model calls per run.
pub enum MaxTurns {
    /// `config.agent.max_turns`, unbounded when unset (TUI, headless).
    FromConfig,
    /// Always unbounded regardless of config: the ACP client owns stop
    /// decisions through `session/cancel`.
    Unbounded,
}

/// Nesting cap for child dispatches: `batch` children dispatch at
/// depth one and an exclusive child orchestrator may fan serial dispatches
/// deeper; eight is animist's default, owned here so the child budgets the
/// batch relies on are craft's to size.
pub const MAX_TOOL_DEPTH: u32 = 8;

/// Request-level settings that change between runs without rebuilding the
/// agent. Layers read the cell at `turn_start`, so mid-run edits never take
/// effect inside a step.
#[derive(Clone)]
pub struct TurnSettings {
    pub mode: AgentMode,
    pub thinking: ThinkingConfig,
    pub fast: bool,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
}

/// The layer instances, kept as handles so every `Agent` rebuild reuses the
/// same state (dedup cache, snapshot manager, recovery budgets) on the same
/// history. Never shared between conversations: run ids are per agent, so a
/// shared layer would collide and corrupt per-run state.
struct LayerSet {
    test: Option<Layer>,
    policy: Layer,
    guardrail: Layer,
    request: Layer,
    recovery: Layer,
    continuation: Layer,
    snapshot: Layer,
    persist: Option<Layer>,
}

impl LayerSet {
    /// The chain head: policy → guardrail → request →
    /// recovery → continuation → snapshot (→ persist). A denial
    /// short-circuits everything after it; the request layer prepares every
    /// attempt (retries included) so it owns the fallback model; a step's
    /// observations append in a stable order (grace before continuation);
    /// the snapshot layer only sees calls that will really run; persistence
    /// observes the committed results of all of it.
    fn chain(&self) -> AgentLayer {
        let tail = match &self.persist {
            Some(persist) => AgentLayer::builder()
                .layer(self.snapshot.clone())
                .next(Box::new(
                    AgentLayer::builder().layer(persist.clone()).build(),
                ))
                .build(),
            None => AgentLayer::builder().layer(self.snapshot.clone()).build(),
        };
        let chain = AgentLayer::builder()
            .layer(self.policy.clone())
            .next(Box::new(
                AgentLayer::builder()
                    .layer(self.guardrail.clone())
                    .next(Box::new(
                        AgentLayer::builder()
                            .layer(self.request.clone())
                            .next(Box::new(
                                AgentLayer::builder()
                                    .layer(self.recovery.clone())
                                    .next(Box::new(
                                        AgentLayer::builder()
                                            .layer(self.continuation.clone())
                                            .next(Box::new(tail))
                                            .build(),
                                    ))
                                    .build(),
                            ))
                            .build(),
                    ))
                    .build(),
            ))
            .build();
        match &self.test {
            Some(test) => AgentLayer::builder()
                .layer(test.clone())
                .next(Box::new(chain))
                .build(),
            None => chain,
        }
    }
}

struct Inner {
    history: History,
    model: Arc<ArcSwap<ModelHandle>>,
    /// The reauth hook for the current model, shared with the recovery
    /// layer; [`Conversation::set_model`] swaps it with the model.
    reauth: Arc<ArcSwapOption<Arc<dyn ReauthHook>>>,
    settings: Arc<ArcSwap<TurnSettings>>,
    tools: Arc<ArcSwap<ToolRegistry>>,
    agent: ArcSwap<Arc<Agent>>,
    config: AgentConfig,
    layers: LayerSet,
    sink: EventSink,
    /// The guardrail state's typed handle: compaction resets it when a
    /// run rewrites history.
    guardrails: Arc<GuardrailState>,
    /// The recovery state's typed handle: the driver charges the
    /// overflow budget, swaps the serving model, and marks follow-up runs.
    recovery: RecoveryHandle,
    /// The continuation state's typed handle: the driver marks
    /// follow-up runs.
    continuation: ContinuationHandle,
    compaction: SharedCompactionState,
    /// The compaction engine, driven by the driver between runs and by
    /// overflow recovery inside `recover`.
    engine: crate::compaction::CompactionEngine,
    /// The compression settings the durable alteration passes run under.
    compression: crate::compression::CompressionConfig,
    /// The advisor configuration: the driver runs the post-turn
    /// review between runs when it is enabled.
    advisor: crate::config::AdvisorConfig,
    /// The workspace write root, anchoring the read-lifecycle path keying.
    write_root: Option<String>,
    snapshots: SnapshotManager,
    /// Serializes rebuilds; runs never overlap a rebuild because both happen
    /// between runs on the surface's turn loop.
    rebuild: Mutex<()>,
    persist: Option<Arc<tokio::sync::Mutex<Persister>>>,
}

/// One conversation shared by a surface and its driver; cheap to clone.
#[derive(Clone)]
pub struct Conversation {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Conversation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conversation").finish_non_exhaustive()
    }
}

/// The single construction point for a session's conversation; folds the
/// config (read once) into the agent config, the layer wiring, and the
/// initial [`TurnSettings`].
pub struct ConversationBuilder<'a> {
    config: &'a Config,
    cwd: &'a str,
    instructions_text: &'a str,
    model: ModelHandle,
    tools: ToolRegistry,
    sink: EventSink,
    history: Option<History>,
    mode: AgentMode,
    max_turns: MaxTurns,
    thinking: Option<ThinkingConfig>,
    fast: bool,
    policy: Option<PolicyLayerInputs>,
    /// Retry inputs for the recovery layer: the key-rotation hook and the
    /// fallback model chain. Defaults reproduce single-model behavior.
    retry: RetryCtx,
    /// The surface's reauth hook (the TUI and ACP wire one).
    reauth: Option<Arc<dyn ReauthHook>>,
    /// Bound on continuations of truncated replies per turn.
    max_continuation_turns: usize,
    snapshots: Option<SnapshotManager>,
    persist: Option<Arc<tokio::sync::Mutex<Persister>>>,
    /// The session workspace: anchors write-path normalization and owns
    /// the compression store the request layer reads from.
    workspace: Option<crate::tools::Workspace>,
    /// Test seam: one extra layer at the chain's head; `None` outside
    /// tests (the setter is test-only).
    test_layer: Option<Layer>,
    /// Prompt sections appended after `Instructions` (subagent role
    /// sections ride here); the `Plan mode` section still appends per run
    /// at `turn_start`.
    sections: Vec<animist::agent::Section>,
}

impl<'a> ConversationBuilder<'a> {
    /// Start from the pieces every surface already holds; the optional
    /// slots below default to their documented values.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &'a Config,
        cwd: &'a str,
        instructions_text: &'a str,
        model: ModelHandle,
        tools: ToolRegistry,
        sink: EventSink,
    ) -> Self {
        Self {
            config,
            cwd,
            instructions_text,
            model,
            tools,
            sink,
            history: None,
            mode: AgentMode::Build,
            max_turns: MaxTurns::FromConfig,
            thinking: None,
            fast: false,
            policy: None,
            retry: RetryCtx::default(),
            reauth: None,
            max_continuation_turns: DEFAULT_MAX_CONTINUATION_TURNS,
            snapshots: None,
            persist: None,
            workspace: None,
            test_layer: None,
            sections: Vec::new(),
        }
    }

    /// Restore a resumed session; empty conversations start fresh. The
    /// restored history begins a fresh change feed, so the persister takes
    /// its cursor from its first full snapshot.
    pub fn history(mut self, history: History) -> Self {
        self.history = Some(history);
        self
    }

    /// Plan mode threads the plan path into the prompt and the write gate.
    pub fn mode(mut self, mode: AgentMode) -> Self {
        self.mode = mode;
        self
    }

    /// The turn bound; see [`MaxTurns`].
    pub fn max_turns(mut self, max_turns: MaxTurns) -> Self {
        self.max_turns = max_turns;
        self
    }

    /// A thinking preference that wins over config (`always_thinking`, then
    /// `agent.thinking`).
    pub fn thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Price turns at the provider's fast tier.
    pub fn fast(mut self, fast: bool) -> Self {
        self.fast = fast;
        self
    }

    /// The approval policy's wiring: the shared permission rules, the
    /// surface's responder and the auto-review reviewer. Without it the
    /// conversation runs ungated (tests; the workspace trusts its caller).
    pub fn policy(mut self, policy: PolicyLayerInputs) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Retry inputs for the recovery layer (key rotation, fallback chain).
    pub fn retry(mut self, retry: RetryCtx) -> Self {
        self.retry = retry;
        self
    }

    /// The surface's reauth hook; without it a 401 fails the run with the
    /// provider's error.
    pub fn reauth(mut self, reauth: Arc<dyn ReauthHook>) -> Self {
        self.reauth = Some(reauth);
        self
    }

    /// Bound on continuations of truncated replies per turn; 0 disables
    /// them (a truncated reply ends the run `MaxTokens` at once).
    pub fn max_continuation_turns(mut self, max: usize) -> Self {
        self.max_continuation_turns = max;
        self
    }

    /// The workspace's snapshot manager driving `/undo`.
    pub fn snapshots(mut self, snapshots: SnapshotManager) -> Self {
        self.snapshots = Some(snapshots);
        self
    }

    /// The session persister, when the session persists; wires the
    /// persistence layer at the chain's tail and gives the driver its
    /// post-marker flush.
    pub fn persist(mut self, persister: Option<Arc<tokio::sync::Mutex<Persister>>>) -> Self {
        self.persist = persister;
        self
    }

    /// The session workspace. Supplies the write-path anchor for path
    /// normalization; without it the anchor derives from the cwd.
    pub fn workspace(mut self, workspace: crate::tools::Workspace) -> Self {
        self.workspace = Some(workspace);
        self
    }

    /// Append a section to the base prompt after `Instructions` (a
    /// subagent's role section is the one caller); plan mode still
    /// appends its own section last, per run.
    pub fn section(mut self, section: animist::agent::Section) -> Self {
        self.sections.push(section);
        self
    }

    /// Test seam: mount one extra layer at the chain's head so a test
    /// can observe hook calls on a conversation's own instances.
    #[cfg(test)]
    pub(crate) fn test_layer(mut self, layer: Layer) -> Self {
        self.test_layer = Some(layer);
        self
    }

    /// Assemble the layers, the settings cell, and the first `Agent`.
    pub fn build(self) -> Result<Conversation> {
        let Self {
            config,
            cwd,
            instructions_text,
            model,
            tools,
            sink,
            history,
            mode,
            max_turns,
            thinking,
            fast,
            policy,
            retry,
            reauth,
            max_continuation_turns,
            snapshots,
            persist,
            workspace,
            test_layer,
            sections,
        } = self;
        let write_root = workspace
            .as_ref()
            .map(|workspace| PathBuf::from(workspace.root()))
            .or_else(|| Some(PathBuf::from(cwd)));
        let snapshots = snapshots.unwrap_or_else(|| {
            workspace
                .as_ref()
                .map(|workspace| workspace.snapshots().clone())
                .unwrap_or_else(|| SnapshotManager::new(PathBuf::from(cwd)))
        });
        let settings = Arc::new(ArcSwap::from_pointee(TurnSettings {
            mode,
            thinking: thinking
                .or(config.always_thinking)
                .or(config.agent.thinking)
                .unwrap_or_default(),
            fast,
            temperature: config.agent.temperature,
            max_tokens: config.agent.max_tokens,
        }));
        // Step budget: `agent.max_turns` bounds the
        // productive model calls, and each `step_end` continuation (a
        // `MaxTokens` continuation or a nudge) consumes a step of its own,
        // so their budgets ride on top. Unbounded stays unbounded.
        let continuation_headroom = u32::try_from(
            max_continuation_turns + crate::harness::continuation::MAX_NUDGES as usize,
        )
        .unwrap_or(u32::MAX);
        let agent_config = AgentConfig::builder()
            .max_steps(
                match max_turns {
                    MaxTurns::FromConfig => config.agent.max_turns,
                    MaxTurns::Unbounded => Some(u32::MAX),
                }
                .unwrap_or(u32::MAX)
                .saturating_add(continuation_headroom),
            )
            // The recovery layer owns the retry policy (per-kind caps and
            // rearm); animist's run-wide counter just paces it.
            .max_retries(super::recovery::retry_budget())
            .max_concurrent_tools(config.agent.max_concurrent_tools)
            .max_tool_calls(u64::MAX)
            // The budgets are high so a full `batch`
            // (MAX_BATCH_SIZE entries, reserved upfront) always fits.
            .max_child_calls(u64::MAX)
            .max_tool_depth(MAX_TOOL_DEPTH)
            .build();
        let guardrails = Arc::new(GuardrailState::new(
            write_root
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned())
                .as_deref(),
        ));
        let compaction = Arc::new(tokio::sync::Mutex::new(
            crate::compaction::CompactionState::default()
                .with_guardrail_state(Arc::clone(&guardrails)),
        ));
        let engine = crate::compaction::CompactionEngine::new(config.compaction.clone())
            .with_buffer(config.compaction_buffer)
            .with_prompt_format(config.agent.prompt_format);
        let model_cell = Arc::new(ArcSwap::from_pointee(model));
        let reauth_cell = Arc::new(ArcSwapOption::from(reauth.map(Arc::new)));
        let recovery_state = Arc::new(std::sync::Mutex::new(
            super::recovery::RecoveryRunState::default(),
        ));
        let continuation_state = Arc::new(std::sync::Mutex::new(
            super::continuation::ContinuationRunState::default(),
        ));
        // The pieces the request layer rebuilds the system prompt from,
        // per run: `config.agent.preamble` followed by the discovered
        // instructions is the `Instructions` section, verbatim.
        let instructions = format!("{}{instructions_text}", config.agent.preamble);
        let layers = LayerSet {
            test: test_layer,
            policy: Layer::new(PolicyLayer::new(
                policy,
                Arc::clone(&settings),
                write_root.clone(),
                Arc::clone(&guardrails),
                sink.clone(),
            )),
            guardrail: Layer::new(GuardrailLayer::new(Arc::clone(&guardrails), sink.clone())),
            request: Layer::new(RequestLayer::new(RequestLayerInputs {
                model: Arc::clone(&model_cell),
                settings: Arc::clone(&settings),
                recovery: Arc::clone(&recovery_state),
                write_root: write_root
                    .as_deref()
                    .map(|path| path.to_string_lossy().into_owned()),
                compression: config.compression.clone(),
                compaction: Arc::clone(&compaction),
                instructions,
                cwd: cwd.to_owned(),
                sections,
                prompt_format: config.agent.prompt_format,
            })),
            recovery: Layer::new(RecoveryLayer::new(
                retry,
                Arc::clone(&reauth_cell),
                Arc::clone(&recovery_state),
                Arc::clone(&compaction),
                engine.clone(),
                Arc::clone(&guardrails),
                Arc::clone(&model_cell),
                sink.clone(),
            )),
            continuation: Layer::new(ContinuationLayer::new(
                max_continuation_turns,
                Arc::clone(&continuation_state),
                sink.clone(),
            )),
            snapshot: Layer::new(SnapshotLayer::new(snapshots.clone())),
            persist: persist
                .clone()
                .map(|persister| Layer::new(PersistLayer::new(persister, sink.clone()))),
        };
        let history = history.unwrap_or_default();
        let agent = build_agent(&agent_config, &history, &model_cell.load(), &layers, &tools)
            .context(BuildConversationSnafu)?;
        let inner = Inner {
            history,
            model: model_cell,
            reauth: reauth_cell,
            settings,
            tools: Arc::new(ArcSwap::from_pointee(tools)),
            agent: ArcSwap::from_pointee(Arc::new(agent)),
            config: agent_config,
            layers,
            sink,
            guardrails,
            recovery: recovery_state,
            continuation: continuation_state,
            compaction,
            engine,
            compression: config.compression.clone(),
            advisor: config.agent.advisor.clone(),
            write_root: write_root
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned()),
            snapshots,
            rebuild: Mutex::new(()),
            persist,
        };
        Ok(Conversation {
            inner: Arc::new(inner),
        })
    }
}

/// Build one agent around the given config, history, model, layers, and
/// tool registry.
#[expect(
    clippy::result_large_err,
    reason = "AgentError is animist's public error type and cannot be boxed here"
)]
fn build_agent(
    config: &AgentConfig,
    history: &History,
    model: &ModelHandle,
    layers: &LayerSet,
    tools: &ToolRegistry,
) -> std::result::Result<Agent, animist::agent::AgentError> {
    Ok(Agent::builder()
        .model(model.model.clone())
        .history(history.clone())
        .config(config.clone())
        .tools(tools.clone())
        .layers(layers.chain())
        .build())
}

impl Conversation {
    /// Shared history handle; reads are snapshots, writes commit atomically.
    pub fn history(&self) -> History {
        self.inner.history.clone()
    }

    /// The conversation's event publisher.
    pub fn sink(&self) -> EventSink {
        self.inner.sink.clone()
    }

    /// The current model.
    pub fn model(&self) -> ModelHandle {
        Arc::unwrap_or_clone(self.inner.model.load_full())
    }

    /// The `provider/model` key usage accounting bills under.
    pub fn model_spec(&self) -> String {
        let model = self.model();
        format!("{}/{}", model.model.provider.name(), model.model.id)
    }

    /// A copy of the current turn settings.
    pub fn settings(&self) -> TurnSettings {
        Arc::unwrap_or_clone(self.inner.settings.load_full())
    }

    /// Replace the turn settings; takes effect at the next run's start.
    pub fn set_settings(&self, settings: TurnSettings) {
        self.inner.settings.store(Arc::new(settings));
    }

    /// Edit the turn settings in place; takes effect at the next run's start.
    pub fn update_settings(&self, edit: impl FnOnce(&mut TurnSettings)) {
        let mut settings = self.settings();
        edit(&mut settings);
        self.set_settings(settings);
    }

    /// The session's guardrail state (doom tracker, guardrail counters,
    /// dedup cache); compaction resets it when a run rewrites history.
    pub fn guardrails(&self) -> Arc<GuardrailState> {
        Arc::clone(&self.inner.guardrails)
    }

    /// The session's shared compaction state.
    pub fn compaction(&self) -> SharedCompactionState {
        self.inner.compaction.clone()
    }

    /// The recovery state's typed handle: the driver reads the serving
    /// model, charges the overflow budget, and marks follow-up runs on it.
    pub(crate) fn recovery(&self) -> RecoveryHandle {
        Arc::clone(&self.inner.recovery)
    }

    /// The continuation state's typed handle: the driver marks
    /// follow-up runs on it.
    pub(crate) fn continuation(&self) -> ContinuationHandle {
        Arc::clone(&self.inner.continuation)
    }

    /// The session's compaction engine, run by the driver between runs and
    /// by overflow recovery.
    pub fn compaction_engine(&self) -> crate::compaction::CompactionEngine {
        self.inner.engine.clone()
    }

    /// The compression settings the durable alteration passes run under.
    pub(crate) fn compression_config(&self) -> crate::compression::CompressionConfig {
        self.inner.compression.clone()
    }

    /// The advisor configuration; the driver runs the post-turn review
    /// between runs when it is enabled.
    pub(crate) fn advisor_config(&self) -> crate::config::AdvisorConfig {
        self.inner.advisor.clone()
    }

    /// The workspace write root anchoring read-lifecycle path keying.
    pub(crate) fn write_root(&self) -> Option<String> {
        self.inner.write_root.clone()
    }

    /// The snapshot manager driving `/undo`.
    pub fn snapshots(&self) -> SnapshotManager {
        self.inner.snapshots.clone()
    }

    /// The session persister, when the conversation persists; the surface's
    /// meta duties (drafts, model, thinking, mode, cost) go through it.
    pub fn persister(&self) -> Option<Arc<tokio::sync::Mutex<Persister>>> {
        self.inner.persist.clone()
    }

    /// Flush the history change feed after terminal markers; persistence
    /// errors are logged and surfaced, never fatal.
    pub async fn flush(&self) {
        let Some(persister) = &self.inner.persist else {
            return;
        };
        let mut persister = persister.lock().await;
        if let Err(error) = persister.checkpoint().await {
            tracing::warn!("session checkpoint failed: {error}");
            self.sink()
                .craft(CraftEvent::PersistenceFailed(error.to_string()));
        }
    }

    /// The active agent; one load per turn keeps the run on a stable agent.
    pub(crate) fn agent(&self) -> Arc<Agent> {
        let guard = self.inner.agent.load();
        Arc::clone(&guard)
    }

    /// Test seam: the assembled agent config, for pinning the child
    /// budgets a full `batch` depends on.
    #[cfg(test)]
    pub(crate) fn agent_config_for_tests(&self) -> &AgentConfig {
        &self.inner.config
    }

    /// Swap the model, and the reauth hook that rebuilds it, then rebuild
    /// the agent between runs, keeping the history, the layer state, and
    /// the event subscription.
    pub fn set_model(&self, model: ModelHandle, reauth: Option<Arc<dyn ReauthHook>>) -> Result<()> {
        self.inner.model.store(Arc::new(model));
        self.inner.reauth.store(reauth.map(Arc::new));
        self.rebuild().context(BuildConversationSnafu)
    }

    /// Swap the tool registry and rebuild the agent between runs.
    pub fn set_tools(&self, tools: ToolRegistry) -> Result<()> {
        self.inner.tools.store(Arc::new(tools));
        self.rebuild().context(BuildConversationSnafu)
    }

    /// Build a fresh agent around the same history, settings, and layer
    /// instances. Runs only between runs; the rebuild lock keeps concurrent
    /// swaps from interleaving.
    #[expect(
        clippy::result_large_err,
        reason = "AgentError is animist's public error type and cannot be boxed here"
    )]
    fn rebuild(&self) -> std::result::Result<(), animist::agent::AgentError> {
        let _guard = self
            .inner
            .rebuild
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let agent = build_agent(
            &self.inner.config,
            &self.inner.history,
            &self.inner.model.load(),
            &self.inner.layers,
            &self.inner.tools.load(),
        )?;
        self.inner.agent.store(Arc::new(Arc::new(agent)));
        Ok(())
    }

    /// Release the agent's resources. Explicit, idempotent, and required on
    /// every exit path; async cleanup cannot run in `Drop`.
    pub async fn shutdown(&self) -> Result<()> {
        self.agent()
            .shutdown()
            .await
            .context(ShutdownConversationSnafu)
    }
}
