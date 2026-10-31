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

use arc_swap::ArcSwap;

use animist::agent::{Agent, AgentConfig, AgentLayer, Layer};
use animist::history::History;
use snafu::ResultExt;

use crate::config::Config;
use crate::error::{BuildConversationSnafu, Result, ShutdownConversationSnafu};
use crate::provider::ModelHandle;
use crate::run::SharedCompactionState;
use crate::run::ToolDispatch;
use crate::run::dedup::{self, SharedDedupCache};
use crate::run::dispatch::BeforeExecute;
use crate::run::guardrails;
use crate::run::mode::AgentMode;
use crate::run::recency::RecencySource;
use crate::snapshot::SnapshotManager;
use crate::thinking::ThinkingConfig;

use super::events::{CraftEvent, EventSink};
use super::guardrail::GuardrailLayer;
use super::persist::Persister;
use super::persist_layer::PersistLayer;
use super::policy::PolicyLayer;
use super::request::{RequestLayer, RequestLayerInputs};
use super::rig_bridge;
use super::snapshot_layer::SnapshotLayer;

/// How the surface bounds model calls per run.
pub enum MaxTurns {
    /// `config.agent.max_turns`, unbounded when unset (TUI, headless).
    FromConfig,
    /// Always unbounded regardless of config: the ACP client owns stop
    /// decisions through `session/cancel`.
    Unbounded,
}

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
/// same state (dedup cache, snapshot manager) on the same history. Never
/// shared between conversations: run ids are per agent, so a shared layer
/// would collide and corrupt per-run state.
struct LayerSet {
    policy: Layer,
    guardrail: Layer,
    request: Layer,
    snapshot: Layer,
    persist: Option<Layer>,
}

impl LayerSet {
    /// The chain head: policy → guardrail → request → snapshot (→ persist),
    /// so a denial short-circuits everything after it, the snapshot layer
    /// only sees calls that will really run, and persistence observes the
    /// committed results of both.
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
        AgentLayer::builder()
            .layer(self.policy.clone())
            .next(Box::new(
                AgentLayer::builder()
                    .layer(self.guardrail.clone())
                    .next(Box::new(
                        AgentLayer::builder()
                            .layer(self.request.clone())
                            .next(Box::new(tail))
                            .build(),
                    ))
                    .build(),
            ))
            .build()
    }
}

struct Inner {
    history: History,
    model: Arc<ArcSwap<ModelHandle>>,
    settings: Arc<ArcSwap<TurnSettings>>,
    tools: Arc<ArcSwap<ToolDispatch>>,
    agent: ArcSwap<Arc<Agent>>,
    config: AgentConfig,
    layers: LayerSet,
    sink: EventSink,
    dedup: SharedDedupCache,
    compaction: SharedCompactionState,
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
/// former `run_policy` inputs (config read once) into the agent config, the
/// layer wiring, and the initial [`TurnSettings`].
pub struct ConversationBuilder<'a> {
    config: &'a Config,
    cwd: &'a str,
    instructions_text: &'a str,
    model: ModelHandle,
    tools: ToolDispatch,
    sink: EventSink,
    history: Option<History>,
    mode: AgentMode,
    max_turns: MaxTurns,
    thinking: Option<ThinkingConfig>,
    fast: bool,
    approval: Option<Arc<dyn BeforeExecute>>,
    recency: Option<Arc<dyn RecencySource>>,
    snapshots: Option<SnapshotManager>,
    persist: Option<Arc<tokio::sync::Mutex<Persister>>>,
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
        tools: ToolDispatch,
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
            approval: None,
            recency: None,
            snapshots: None,
            persist: None,
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

    /// The surface's approval gate (the TUI's permission prompts); the
    /// interim policy layer consults it before every execution.
    pub fn approval(mut self, approval: Arc<dyn BeforeExecute>) -> Self {
        self.approval = Some(approval);
        self
    }

    /// Volatile per-turn facts for the request view.
    pub fn recency(mut self, recency: Arc<dyn RecencySource>) -> Self {
        self.recency = Some(recency);
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

    /// Assemble the layers, the settings cell, and the first `Agent`.
    #[expect(
        clippy::result_large_err,
        reason = "the crate error type is deliberately unboxed"
    )]
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
            approval,
            recency,
            snapshots,
            persist,
        } = self;
        let write_root = tools.write_root().map(PathBuf::from);
        let snapshots = snapshots.unwrap_or_else(|| SnapshotManager::new(PathBuf::from(cwd)));
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
        let agent_config = AgentConfig::builder()
            // 05 keeps the bound the old loop had: every model call counts,
            // continuations included. 09 extends the budget when step-boundary
            // continuations land.
            .max_steps(match max_turns {
                MaxTurns::FromConfig => config.agent.max_turns.unwrap_or(u32::MAX),
                MaxTurns::Unbounded => u32::MAX,
            })
            // Retry, fallback and reauth are 09's `recover` hooks.
            .max_retries(0_u32)
            .max_concurrent_tools(config.agent.max_concurrent_tools)
            .max_tool_calls(u64::MAX)
            .max_child_calls(u64::MAX)
            .build();
        let dedup = dedup::shared_cache_with_root(
            write_root
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned())
                .as_deref(),
        );
        let compaction = Arc::new(Mutex::new(
            crate::compaction::CompactionState::default()
                .with_dedup(Arc::clone(&dedup))
                .with_guardrails(guardrails::shared_guardrails()),
        ));
        let model_cell = Arc::new(ArcSwap::from_pointee(model));
        let layers = LayerSet {
            policy: Layer::new(PolicyLayer::new(
                approval,
                Arc::clone(&settings),
                write_root.clone(),
            )),
            guardrail: Layer::new(GuardrailLayer::new(Arc::clone(&dedup))),
            request: Layer::new(RequestLayer::new(RequestLayerInputs {
                model: Arc::clone(&model_cell),
                settings: Arc::clone(&settings),
                recency,
                compression_store: tools.compression_store().cloned(),
                write_root: write_root
                    .as_deref()
                    .map(|path| path.to_string_lossy().into_owned()),
                compression: config.compression.clone(),
                compaction: Arc::clone(&compaction),
                preamble_base: format!("{}{instructions_text}", config.agent.preamble),
                cwd: cwd.to_owned(),
            })),
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
            settings,
            tools: Arc::new(ArcSwap::from_pointee(tools)),
            agent: ArcSwap::from_pointee(Arc::new(agent)),
            config: agent_config,
            layers,
            sink,
            dedup,
            compaction,
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
/// bridged tool registry.
#[expect(
    clippy::result_large_err,
    reason = "AgentError is animist's public error type and cannot be boxed here"
)]
fn build_agent(
    config: &AgentConfig,
    history: &History,
    model: &ModelHandle,
    layers: &LayerSet,
    tools: &ToolDispatch,
) -> std::result::Result<Agent, animist::agent::AgentError> {
    let registry = rig_bridge::registry(tools)?;
    Ok(Agent::builder()
        .model(model.model.clone())
        .history(history.clone())
        .config(config.clone())
        .tools(registry)
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

    /// The session's dedup cache; compaction clears it (08).
    pub fn dedup(&self) -> SharedDedupCache {
        self.inner.dedup.clone()
    }

    /// The session's shared compaction state (08 drives it).
    pub fn compaction(&self) -> SharedCompactionState {
        self.inner.compaction.clone()
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

    /// Swap the model and rebuild the agent between runs, keeping the
    /// history, the layer state, and the event subscription.
    #[expect(
        clippy::result_large_err,
        reason = "the crate error type is deliberately unboxed"
    )]
    pub fn set_model(&self, model: ModelHandle) -> Result<()> {
        self.inner.model.store(Arc::new(model));
        self.rebuild().context(BuildConversationSnafu)
    }

    /// Swap the tool set and rebuild the agent between runs.
    #[expect(
        clippy::result_large_err,
        reason = "the crate error type is deliberately unboxed"
    )]
    pub fn set_tools(&self, tools: ToolDispatch) -> Result<()> {
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
    #[expect(
        clippy::result_large_err,
        reason = "the crate error type is deliberately unboxed"
    )]
    pub async fn shutdown(&self) -> Result<()> {
        self.agent()
            .shutdown()
            .await
            .context(ShutdownConversationSnafu)
    }
}
