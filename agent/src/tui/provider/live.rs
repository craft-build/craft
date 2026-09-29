//! CraftProvider: the TUI's real backend, driving the shared run loop the
//! ACP server uses.
//!
//! Turn semantics mirror `acp::run_turn`: a per-turn provider rebuild, the
//! configured compaction stages ahead of the model call, cancellation through
//! the run loop's `CancelToken`, and history committed only on a successful
//! run. Edit-family tools are gated behind the UI's approve/reject seam via
//! the dispatch `BeforeExecute` hook: no workspace mutation runs without an
//! explicit user decision.

mod approval;
mod question;
mod turn;
mod usage_recorder;

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::AbortHandle;

use crate::compaction::CompactionState;
use crate::config::Config;
use crate::error::{InvalidSnafu, Result, client_error};
use crate::permissions::{PermissionAnswer, PermissionManager, PermissionsConfig};
use crate::providers::{CatalogModel, Provider as ClientProvider, ProviderKind};
use crate::run;
use crate::tools::Workspace;

use approval::decide;
use question::answer_question;
use turn::{TurnCtx, run_turn};
use usage_recorder::UsageLedger;

use super::cards::{self, Files};
use super::{
    AgentEvent, Command, LoadedMessage, ModelChoice, Provider, Status, Tone, UsageFetchState,
};

/// Render an error and its sources as one client-facing message.
pub(super) fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

/// Session shared between the command loop and the (single) running turn.
#[derive(Default)]
struct SessionState {
    history: Vec<crate::history::Message>,
    /// Shared with the run loop for in-run overflow recovery.
    compaction: crate::run::SharedCompactionState,
    /// Session-wide tool dedup cache, shared by the dispatcher and cleared
    /// by the compaction engine.
    dedup: crate::run::SharedDedupCache,
    /// Session-wide guardrail counters, shared by the dispatcher and reset
    /// by the compaction engine.
    guardrails: crate::run::SharedGuardrails,
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pending_approval: Option<(
        String,
        tokio::sync::oneshot::Sender<crate::permissions::PermissionAnswer>,
    )>,
    /// Parked `question` tool call, by question-request id (A.5).
    pending_question: Option<(
        String,
        tokio::sync::oneshot::Sender<crate::tools::QuestionAnswer>,
    )>,
    /// Per-model usage totals and the cost ledger they feed.
    usage: UsageLedger,
    /// Persisted session (history + usage), the same store headless uses.
    /// `None` when the state dir is unavailable: the run is not persisted.
    store: Option<crate::headless::SessionStore>,
}

impl SessionState {
    /// Link the compaction state to this session's dedup cache so a
    /// compaction run clears it. Persistence is opt-in via
    /// [`Self::with_store`] so tests never touch the state dir.
    fn linked() -> Self {
        let dedup = crate::run::shared_cache();
        let guardrails = crate::run::shared_guardrails();
        Self {
            compaction: std::sync::Arc::new(std::sync::Mutex::new(
                CompactionState::default()
                    .with_dedup(dedup.clone())
                    .with_guardrails(guardrails.clone()),
            )),
            dedup,
            guardrails,
            usage: UsageLedger::open(),
            ..Self::default()
        }
    }

    /// Bind a freshly minted persisted session; its id also names this
    /// session's `cost.jsonl` records. `dir` is the resolved state dir;
    /// `None` disables persistence.
    fn with_store(
        mut self,
        dir: Option<&crate::storage::StateDir>,
        cwd: &str,
        model_spec: &str,
    ) -> Self {
        let session_ref = crate::id::SessionRef::generate();
        self.usage = self.usage.with_session_id(session_ref.id().to_string());
        self.store = dir.and_then(|dir| {
            crate::headless::SessionStore::open_in(dir.clone(), session_ref, cwd, model_spec).ok()
        });
        self
    }
}

/// `/sessions` load: replace the session with a persisted one and push the
/// rebuilt transcript plus fresh chrome back to the UI. Load failures only
/// warn; the current session stays put.
async fn load_session(
    state: &Arc<Mutex<SessionState>>,
    files: &Files,
    id: &str,
    dir: Option<&crate::storage::StateDir>,
    cwd: &str,
    model_spec: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let Some(dir) = dir else {
        let _ = tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "session storage is unavailable".into(),
        });
        return;
    };
    let craft_id = match id.parse::<crate::id::CraftId>() {
        Ok(id) => id,
        Err(_) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("unknown session id {id:?}"),
            });
            return;
        }
    };
    // Session logs can be large; read off the async worker.
    let read_dir = dir.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        crate::headless::StoredSession::load(craft_id, &read_dir)
    })
    .await;
    let loaded = match loaded {
        Ok(Ok(session)) => session,
        Ok(Err(e)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not load session: {e}"),
            });
            return;
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not load session: {e}"),
            });
            return;
        }
    };
    let title = loaded.title.clone();
    let draft = loaded.meta.input_draft.clone().unwrap_or_default();
    let messages = loaded.messages().to_vec();
    let rendered = transcript(&messages);
    {
        // Swap history + persistence in one critical section. Compaction/
        // dedup/guardrail handles are session-lifetime and stay valid: the
        // loaded history is what the next turn's engine sees. The store
        // binds the loaded id so future turns resume the same session file
        // and its cost records join its ledger id.
        let mut guard = state.lock().await;
        guard.history = messages;
        guard.usage = UsageLedger::open().with_session_id(loaded.id.id().to_string());
        guard.store =
            crate::headless::SessionStore::open_in(dir.clone(), loaded.id.clone(), cwd, model_spec)
                .ok();
    }
    files.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let _ = tx.send(AgentEvent::FilesSet(Vec::new()));
    let _ = tx.send(AgentEvent::AssistantEnd);
    let _ = tx.send(AgentEvent::SessionLoaded {
        messages: rendered,
        draft,
    });
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
    let _ = tx.send(AgentEvent::Notice {
        tone: Tone::Success,
        text: format!("resumed session \"{title}\""),
    });
}

/// F.3 resume-latest-by-cwd: load the newest session recorded for this
/// directory (cwd index first, disk scan fallback), reusing the `/sessions`
/// load path. Nothing found keeps the fresh session.
async fn resume_latest(
    state: &Arc<Mutex<SessionState>>,
    files: &Files,
    dir: Option<&crate::storage::StateDir>,
    cwd: &str,
    model_spec: &str,
    tx: &mpsc::UnboundedSender<AgentEvent>,
) {
    let Some(dir) = dir else {
        let _ = tx.send(AgentEvent::Notice {
            tone: Tone::Neutral,
            text: "session storage is unavailable".into(),
        });
        return;
    };
    let read_dir = dir.clone();
    let read_cwd = cwd.to_owned();
    let found = tokio::task::spawn_blocking(move || {
        crate::headless::StoredSession::latest(&read_cwd, &read_dir)
    })
    .await;
    match found {
        Ok(Ok(Some(session))) => {
            let id = session.id.id().to_string();
            load_session(state, files, &id, Some(dir), cwd, model_spec, tx).await;
        }
        Ok(Ok(None)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Neutral,
                text: "no previous session in this directory".into(),
            });
        }
        Ok(Err(e)) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not find the latest session: {e}"),
            });
        }
        Err(e) => {
            let _ = tx.send(AgentEvent::Notice {
                tone: Tone::Warning,
                text: format!("could not find the latest session: {e}"),
            });
        }
    }
}

/// User/assistant text of a persisted session, for the conversation view's
/// rebuild: tool calls and results carry no displayable text, so messages
/// containing only those are skipped (they leave an empty `text()`).
fn transcript(messages: &[crate::history::Message]) -> Vec<LoadedMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let text = message.text();
            if text.trim().is_empty() {
                return None;
            }
            Some(match message {
                crate::history::Message::User { .. } => LoadedMessage::User(text),
                _ => LoadedMessage::Assistant(text),
            })
        })
        .collect()
}

/// Immutable-by-loop shared state for the command loop's handlers: the
/// session, its connections, and the pieces every arm reaches for. Loop
/// locals that mutate per command (`current_turn`, `selection`) stay in
/// the loop and are passed as `&mut`.
struct LoopCtx {
    state: Arc<Mutex<SessionState>>,
    files: Files,
    cancel_flag: run::CancelFlag,
    /// Per-tool-call subagent cancellation (task 96): shared with every
    /// turn's task-tool launcher; `Command::CancelSubagent` fires here.
    subagent_cancels: Arc<run::cancel::CancelMap<String>>,
    /// Bang-mode bookkeeping (task 96): run ids, in-flight cancel
    /// triggers, and visible-run results waiting for the next turn.
    shell: Arc<std::sync::Mutex<crate::tui::shell::ShellState>>,
    permissions: Arc<PermissionManager>,
    config: Arc<Config>,
    workspace: Workspace,
    instructions_text: String,
    catalogs: BTreeMap<String, Vec<CatalogModel>>,
    snapshots: crate::snapshot::SnapshotManager,
    state_dir: Option<crate::storage::StateDir>,
    cwd: String,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    /// Signals the loop when a running turn settles, so messages queued
    /// behind it drain even with no further user command.
    wake: Arc<Notify>,
}

impl LoopCtx {
    /// The current "provider/model" spec, for pricing and the session
    /// header.
    fn model_spec(selection: &Selection) -> String {
        format!("{}/{}", selection.provider, selection.model)
    }

    /// Flat model menu rows across all usable providers, with the
    /// current selection's index.
    fn catalog_choices(&self, selection: &Selection) -> (Vec<ModelChoice>, usize) {
        catalog_choices(&self.catalogs, selection)
    }
}

/// Flat model menu rows across all usable providers, with the current
/// selection's index.
fn catalog_choices(
    catalogs: &BTreeMap<String, Vec<CatalogModel>>,
    selection: &Selection,
) -> (Vec<ModelChoice>, usize) {
    let mut choices = Vec::new();
    let mut current = 0;
    for (provider, models) in catalogs {
        for model in models {
            if provider == &selection.provider && model.id == selection.model {
                current = choices.len();
            }
            choices.push(ModelChoice {
                provider: provider.clone(),
                model: model.id.clone(),
                label: model.label().to_owned(),
                provider_label: provider.clone(),
            });
        }
    }
    (choices, current)
}

#[derive(Clone)]
struct Selection {
    provider: String,
    model: String,
    context_length: Option<u32>,
}

/// The live TUI backend: configured providers, discovered model catalogs, and
/// a workspace-scoped agent at `cwd`.
pub struct CraftProvider {
    config: Arc<Config>,
    workspace: Workspace,
    /// Instruction files (AGENTS.md and friends) discovered at startup;
    /// appended to the system prompt and shared with tool injection.
    instructions: crate::instructions::Instructions,
    /// Permission rule engine: persistent `permissions.bml` rules, session
    /// grants, and per-tool defaults; consulted by the approval gate.
    permissions: Arc<PermissionManager>,
    /// Per-provider discovered models, config key order.
    catalogs: BTreeMap<String, Vec<CatalogModel>>,
    /// Discovery problems surfaced to the user as notes at startup.
    notes: Vec<String>,
    selection: Selection,
    cwd_label: String,
    branch: String,
    /// F.3 resume-latest-by-cwd at startup (`craft --continue`).
    resume_latest: bool,
    /// Resume this specific session id at startup (`craft -s/--session`,
    /// G.1); takes precedence over `resume_latest`.
    resume_session: Option<String>,
    /// MCP client handle (B.11): installed on the workspace before the
    /// first turn and cloned into the app for the `/mcp` screen.
    mcp: Option<crate::mcp::McpHandle>,
}

impl CraftProvider {
    /// Discover instruction files and the permission rule engine for the
    /// workspace at `cwd`, off the async worker.
    async fn resolve_instructions(
        cwd: &Path,
    ) -> (crate::instructions::Instructions, PermissionManager) {
        let instructions = tokio::task::spawn_blocking({
            let cwd = cwd.display().to_string();
            move || crate::instructions::load_instructions(&cwd)
        })
        .await
        .unwrap_or_default();
        let permissions = tokio::task::spawn_blocking({
            let cwd = cwd.to_path_buf();
            move || PermissionManager::new(crate::permissions::load_permissions(&cwd), cwd)
        })
        .await
        .unwrap_or_else(|_| {
            PermissionManager::new(PermissionsConfig::default(), cwd.to_path_buf())
        });
        (instructions, permissions)
    }

    /// Validate the whole session up front, before the terminal UI starts:
    /// config, workspace, and at least one usable model catalog.
    pub async fn new(config: Config, cwd: impl AsRef<Path>) -> Result<Self> {
        if config.providers.is_empty() {
            return InvalidSnafu {
                reason: crate::setup::setup_hint(),
            }
            .fail();
        }
        let cwd = cwd.as_ref();
        let (instructions, permissions) = Self::resolve_instructions(cwd).await;
        let workspace = Workspace::new(cwd)
            .map_err(client_error)?
            .with_loaded_instructions(instructions.loaded.clone());
        let mut catalogs: BTreeMap<String, Vec<CatalogModel>> = BTreeMap::new();
        let mut notes = Vec::new();
        // B.11: start the MCP client up front but never await `ready` here —
        // the first frame must not block on a slow server initialize. The
        // turn path awaits the gate before registering tools.
        let (mcp, mcp_errors) = crate::mcp::start(cwd).await;
        if !mcp_errors.is_empty() {
            notes.push(format!("mcp: {mcp_errors}"));
        }
        workspace.set_mcp(mcp.clone());
        for (name, provider_config) in &config.providers {
            if provider_config.kind == ProviderKind::Voyageai {
                notes.push(format!("{name}: no completion models (non-chat provider)"));
                continue;
            }
            match ClientProvider::from_config(provider_config) {
                Ok(provider) => match provider.models(provider_config).await {
                    Ok(models) => {
                        if models.is_empty() {
                            notes.push(format!("{name}: no models discovered"));
                        } else {
                            catalogs.insert(name.clone(), models);
                        }
                    }
                    Err(error) => notes.push(format!("{name}: {}", report(error))),
                },
                Err(error) => notes.push(format!("{name}: {}", report(error))),
            }
        }
        if catalogs.is_empty() {
            return InvalidSnafu {
                reason: format!(
                    "no usable provider/model catalog ({}) - {}",
                    notes.join("; "),
                    crate::setup::setup_hint()
                ),
            }
            .fail();
        }

        // H.2 models.dev catalog: warm the 24h disk cache (best-effort, with
        // a fetch budget) and fill context/output metadata the Rig listing
        // lacks. Failures degrade to whatever discovery already provided.
        let _ =
            tokio::time::timeout(crate::models_dev::FETCH_BUDGET, crate::models_dev::warm()).await;
        for (name, provider_config) in &config.providers {
            if let Some(models) = catalogs.get_mut(name) {
                crate::models_dev::enrich_catalog(provider_config.kind.as_str(), models);
            }
        }

        // H.3 model-tier registry: load persisted tier overrides and feed in
        // the discovered catalogs so tier defaults can be resolved.
        if let Ok(state_dir) = crate::storage::StateDir::resolve() {
            crate::model_registry::load_from_storage(&state_dir);
        }
        for (name, provider_config) in &config.providers {
            if let Some(models) = catalogs.get(name) {
                crate::model_registry::set_known_models(
                    name,
                    provider_config.kind.as_str(),
                    models
                        .iter()
                        .map(|m| crate::model_registry::ModelInfo {
                            context_window: m.context_length,
                            ..crate::model_registry::ModelInfo::new(m.id.clone())
                        })
                        .collect(),
                );
            }
        }

        let (provider, models) = catalogs.iter().next().expect("catalogs is non-empty");
        let first = models.first().expect("each catalog is non-empty");
        // Prefer the Medium-tier default when the registry can resolve one;
        // otherwise keep the first catalog entry.
        let selection =
            crate::model_registry::spec_for_tier_any(crate::model_registry::ModelTier::Medium)
                .and_then(|spec| {
                    let (provider, model) = spec.split_once('/')?;
                    let models = catalogs.get(provider)?;
                    let entry = models.iter().find(|m| m.id == model)?;
                    Some(Selection {
                        provider: provider.to_string(),
                        model: entry.id.clone(),
                        context_length: entry.context_length,
                    })
                })
                .unwrap_or(Selection {
                    provider: provider.clone(),
                    model: first.id.clone(),
                    context_length: first.context_length,
                });
        Ok(Self {
            config: Arc::new(config),
            workspace,
            instructions,
            permissions: Arc::new(permissions),
            catalogs,
            notes,
            selection,
            cwd_label: cards::display_path(cwd),
            branch: cards::git_branch(cwd).await,
            resume_latest: false,
            resume_session: None,
            mcp,
        })
    }

    /// Resume this directory's most recent session when the command loop
    /// starts (F.3 resume-latest-by-cwd, `craft --continue`).
    pub fn with_resume_latest(mut self, yes: bool) -> Self {
        self.resume_latest = yes;
        self
    }

    /// Resume a specific session id at startup (`craft -s/--session`, G.1).
    /// Takes precedence over [`Self::with_resume_latest`].
    pub fn with_session(mut self, id: Option<String>) -> Self {
        self.resume_session = id;
        self
    }

    /// Prepend first-run notes (G.6 auto-detected providers) to the startup
    /// notes so they surface before discovery problems.
    pub fn with_startup_notes(mut self, mut notes: Vec<String>) -> Self {
        notes.append(&mut self.notes);
        self.notes = notes;
        self
    }

    /// Override the startup model selection with a `provider/model` spec
    /// (`craft -m`, G.1). Unknown specs fail fast instead of silently
    /// falling back to the tier default.
    pub fn with_model_spec(mut self, spec: &str) -> crate::error::Result<Self> {
        use snafu::ensure;
        let (provider, model) = spec.split_once('/').ok_or_else(|| {
            crate::error::InvalidSnafu {
                reason: format!("--model expects provider/model-id, got {spec:?}"),
            }
            .build()
        })?;
        ensure!(
            self.catalogs.contains_key(provider),
            crate::error::InvalidSnafu {
                reason: format!(
                    "unknown provider {provider:?} in --model {spec:?} \
                     (configured: {})",
                    self.catalogs.keys().cloned().collect::<Vec<_>>().join(", ")
                )
            }
        );
        let entry = self.catalogs[provider]
            .iter()
            .find(|m| m.id == model)
            .ok_or_else(|| {
                crate::error::InvalidSnafu {
                    reason: format!("provider {provider:?} has no model {model:?}"),
                }
                .build()
            })?;
        self.selection = Selection {
            provider: provider.to_string(),
            model: entry.id.clone(),
            context_length: entry.context_length,
        };
        Ok(self)
    }

    /// Apply G.1 permission flags before the session's first turn: `--yolo`
    /// bypasses every check, `-A/--auto-review` starts auto-review on.
    pub fn with_permission_flags(self, yolo: bool, auto_review: bool) -> Self {
        self.permissions.set_yolo(yolo);
        self.permissions.set_auto_review(auto_review);
        self
    }

    /// The session's command loop: receives [`Command`]s, drives turns and
    /// approvals, and streams [`AgentEvent`]s back. Spawned by `start`.
    async fn spawn_command_loop(
        self,
        mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Command>,
        evt_tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    ) {
        let mut selection = self.selection;
        let cwd = self.workspace.root().display().to_string();
        let state_dir = crate::storage::StateDir::resolve().ok();
        let state = Arc::new(Mutex::new(SessionState::linked().with_store(
            state_dir.as_ref(),
            &cwd,
            &LoopCtx::model_spec(&selection),
        )));
        let files: Files = Files::default();
        let (cancel_flag, _) = run::cancel_channel();
        let subagent_cancels = Arc::new(run::cancel::CancelMap::new());
        let mut current_turn: Option<AbortHandle> = None;
        // Bang-mode bookkeeping lives in `ctx.shell` (ShellState).
        // Quota-fetch generation: shared with the spawned fetches so a slow
        // stale answer never overwrites a newer one (rapid Ctrl+R).
        let usage_gen = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let shell: Arc<std::sync::Mutex<crate::tui::shell::ShellState>> = Arc::default();
        // Signals the loop when a running turn finishes (see LoopCtx).
        let wake = Arc::new(Notify::new());
        let permissions = self.permissions;
        let workspace = self.workspace;
        let snapshots = workspace.snapshots().clone();
        let instructions_text = self.instructions.text;
        let config = self.config;
        let catalogs = self.catalogs;
        let ctx = LoopCtx {
            state,
            files,
            cancel_flag,
            subagent_cancels,
            shell,
            permissions,
            config,
            workspace,
            instructions_text,
            catalogs,
            snapshots,
            state_dir: state_dir.clone(),
            cwd,
            evt_tx: evt_tx.clone(),
            wake: wake.clone(),
        };
        // Messages submitted while a turn is still running queue here and
        // are sent, in order, when that turn settles (never aborting it).
        let mut pending_messages: VecDeque<PendingMessage> = VecDeque::new();

        let _ = evt_tx.send(AgentEvent::SessionInfo {
            cwd: self.cwd_label,
            branch: self.branch,
        });
        for note in self.notes {
            let _ = evt_tx.send(AgentEvent::AssistantText(note));
        }
        let (models, current) = ctx.catalog_choices(&selection);
        let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
        let _ = evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));

        if let Some(id) = &self.resume_session {
            load_session(
                &ctx.state,
                &ctx.files,
                id,
                ctx.state_dir.as_ref(),
                &ctx.cwd,
                &LoopCtx::model_spec(&selection),
                &ctx.evt_tx,
            )
            .await;
        } else if self.resume_latest {
            resume_latest(
                &ctx.state,
                &ctx.files,
                ctx.state_dir.as_ref(),
                &ctx.cwd,
                &LoopCtx::model_spec(&selection),
                &ctx.evt_tx,
            )
            .await;
        }

        loop {
            let cmd = tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
                _ = wake.notified() => {
                    // A turn settled: drop a finished handle and send the
                    // next queued message, if any.
                    maybe_send_next(&ctx, &selection, &mut current_turn, &mut pending_messages)
                        .await;
                    continue;
                }
            };
            // A turn that finished on its own leaves its handle behind;
            // drop it so the turn-running guards (undo/compact/load)
            // don't refuse forever after the first completed turn, then
            // send the next queued message if any.
            maybe_send_next(&ctx, &selection, &mut current_turn, &mut pending_messages).await;
            match cmd {
                Command::SendMessage(text, mode, images) => {
                    handle_send_message(
                        &ctx,
                        &selection,
                        &mut current_turn,
                        &mut pending_messages,
                        text,
                        mode,
                        images,
                    )
                    .await
                }
                Command::Shell { command, visible } => handle_shell(&ctx, command, visible),
                Command::Approve { id, always } => {
                    let answer = if always {
                        PermissionAnswer::AllowAlwaysLocal
                    } else {
                        PermissionAnswer::AllowSession
                    };
                    decide(&ctx.state, id, answer).await
                }
                Command::Reject { id, always } => {
                    let answer = if always {
                        PermissionAnswer::DenyAlwaysLocal
                    } else {
                        PermissionAnswer::Deny
                    };
                    decide(&ctx.state, id, answer).await
                }
                Command::AnswerPermission { id, answer } => {
                    decide(&ctx.state, id, answer).await;
                }
                Command::AnswerQuestion { id, answer } => {
                    answer_question(&ctx.state, id, answer).await;
                }
                Command::ToggleAutoReview => {
                    let on = ctx.permissions.toggle_auto_review();
                    let _ = ctx.evt_tx.send(AgentEvent::AssistantText(format!(
                        "auto-review {}.",
                        if on { "on" } else { "off" }
                    )));
                }
                Command::GetUsage => {
                    let rows = ctx.state.lock().await.usage.rows();
                    let _ = ctx.evt_tx.send(AgentEvent::UsageSnapshot(rows));
                }
                Command::FetchUsage => handle_fetch_usage(&ctx, &selection, &usage_gen).await,
                Command::Compact => handle_compact(&ctx, &selection, &current_turn).await,
                Command::LoadSession { id } => {
                    handle_load_session(&ctx, &selection, &current_turn, id).await
                }
                Command::ResumeLatest => {
                    handle_resume_latest(&ctx, &selection, &current_turn).await
                }
                Command::SetDraft(draft) => handle_set_draft(&ctx, draft).await,
                Command::Interrupt => {
                    handle_interrupt(&ctx, &mut current_turn);
                    pending_messages.clear();
                }
                Command::CancelSubagent { tool_use_id } => {
                    // Marks the id so children spawned later under it die
                    // too; the parent turn keeps running.
                    ctx.subagent_cancels.cancel_or_precancel(tool_use_id);
                }
                Command::Clear => {
                    handle_clear(&ctx, &selection, &mut current_turn).await;
                    pending_messages.clear();
                }
                Command::Reset => {
                    handle_reset(&ctx, &selection, &mut current_turn).await;
                    pending_messages.clear();
                }
                Command::Undo => handle_undo(&ctx, &current_turn).await,
                Command::SelectModel { provider, model } => {
                    handle_select_model(&ctx, &mut selection, provider, model)
                }
            }
        }
        // The UI dropped its command half: the session is over. Persist
        // queued bang-mode results (they never got a next turn to ride)
        // and flush a soft checkpointed draft that never hit its write
        // window, so a keystroke from a second ago still reaches disk.
        persist_on_exit(&ctx, &selection).await;
        // B.11: the UI dropped its command half, so no turn can follow; tear
        // the MCP servers down before the loop task ends (bounded by the
        // manager's own shutdown timeout).
        if let Some(handle) = ctx.workspace.mcp() {
            handle.shutdown().await;
        }
    }
}

/// A message submitted while a turn is running, waiting for that turn to
/// settle before it is sent.
type PendingMessage = (
    String,
    crate::run::AgentMode,
    Vec<crate::history::ImageBlock>,
);

/// `Command::SendMessage`: send now, or queue behind a still-running turn
/// (submitting never aborts the in-flight turn). Queued messages are sent
/// in order when the turn settles (`maybe_send_next`).
async fn handle_send_message(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
    text: String,
    mode: crate::run::AgentMode,
    images: Vec<crate::history::ImageBlock>,
) {
    if text.trim().is_empty() {
        return;
    }
    if current_turn.as_ref().is_some_and(|h| !h.is_finished()) {
        pending.push_back((text, mode, images));
        return;
    }
    *current_turn = None;
    // Bang-mode results queue until the next turn: pushing them into the
    // history directly would race the running turn's whole-history commit.
    drain_shell_results(&ctx.shell, &ctx.state).await;
    *current_turn = Some(start_turn(ctx, selection, text, mode, images));
}

/// Drop a settled turn's handle and, if messages queued behind it, send
/// the next one. No-op while a turn is still running or none is.
async fn maybe_send_next(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
    pending: &mut VecDeque<PendingMessage>,
) {
    if !current_turn.as_ref().is_some_and(AbortHandle::is_finished) {
        return;
    }
    *current_turn = None;
    if let Some((text, mode, images)) = pending.pop_front() {
        drain_shell_results(&ctx.shell, &ctx.state).await;
        *current_turn = Some(start_turn(ctx, selection, text, mode, images));
    }
}

/// Spawn a turn and arm the wake signal for when it settles.
fn start_turn(
    ctx: &LoopCtx,
    selection: &Selection,
    text: String,
    mode: crate::run::AgentMode,
    images: Vec<crate::history::ImageBlock>,
) -> AbortHandle {
    let handle = tokio::spawn(run_turn(
        TurnCtx {
            config: ctx.config.clone(),
            workspace: ctx.workspace.clone(),
            instructions_text: ctx.instructions_text.clone(),
            selection: selection.clone(),
            state: ctx.state.clone(),
            files: ctx.files.clone(),
            cancel: ctx.cancel_flag.token(),
            subagent_cancels: ctx.subagent_cancels.clone(),
            tx: ctx.evt_tx.clone(),
            permissions: ctx.permissions.clone(),
            mode,
        },
        text,
        images,
    ));
    let abort = handle.abort_handle();
    let wake = Arc::clone(&ctx.wake);
    tokio::spawn(async move {
        let _ = handle.await;
        wake.notify_one();
    });
    abort
}

/// `Command::Shell`: spawn a bang-mode shell run under a per-run child
/// token (a parent interrupt cancels it); its trigger is registered in
/// the session's `ShellState` and visible-run results queue there for
/// the next turn.
fn handle_shell(ctx: &LoopCtx, command: String, visible: bool) {
    let (id, cancel) = {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        let id = shell.reserve_id();
        // Child of the interrupt flag: Esc stops this run without
        // touching anything else, and the registered trigger lets
        // `cancel_all` sweep every in-flight run at teardown.
        let (trigger, cancel) = ctx.cancel_flag.token().child();
        shell.add_trigger(&id, trigger);
        (id, cancel)
    };
    let tx = ctx.evt_tx.clone();
    let shell = Arc::clone(&ctx.shell);
    tokio::spawn(crate::tui::shell::run_shell(
        id, command, visible, tx, cancel, shell,
    ));
}

/// `Command::FetchUsage`: mark loading, then resolve off-loop so the UI
/// keeps ticking. Each fetch carries a generation; answers from an older
/// generation than the latest request are dropped so a slow stale fetch
/// can't overwrite a newer answer.
async fn handle_fetch_usage(
    ctx: &LoopCtx,
    selection: &Selection,
    usage_gen: &Arc<std::sync::atomic::AtomicU64>,
) {
    let config = match ctx.config.providers.get(&selection.provider) {
        Some(config) => Some(config.clone()),
        None => {
            let _ = ctx
                .evt_tx
                .send(AgentEvent::UsageQuota(UsageFetchState::Error(format!(
                    "provider {:?} not found in config",
                    selection.provider
                ))));
            None
        }
    };
    let fetch_gen = usage_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let tx = ctx.evt_tx.clone();
    let latest = Arc::clone(usage_gen);
    let _ = ctx
        .evt_tx
        .send(AgentEvent::UsageQuota(UsageFetchState::Loading));
    let Some(config) = config else {
        // Error already emitted above; no fetch to run.
        return;
    };
    tokio::spawn(async move {
        let state = match crate::providers::usage_fetch::fetch_usage(&config).await {
            Ok(Some(usage)) => UsageFetchState::Ready(usage),
            Ok(None) => UsageFetchState::Unsupported,
            Err(error) => UsageFetchState::Error(report(error)),
        };
        if latest.load(std::sync::atomic::Ordering::SeqCst) != fetch_gen {
            return; // superseded by a newer fetch
        }
        let _ = tx.send(AgentEvent::UsageQuota(state));
    });
}

/// `Command::Compact`: refuse while a turn is running, then compact now.
async fn handle_compact(ctx: &LoopCtx, selection: &Selection, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before compacting.".into(),
        });
        return;
    }
    turn::compact_now(&ctx.config, selection, &ctx.state, &ctx.evt_tx).await;
}

/// `Command::LoadSession`: refuse while a turn is running, then swap in the
/// persisted session.
async fn handle_load_session(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &Option<AbortHandle>,
    id: String,
) {
    if current_turn.is_some() {
        // Loading mid-run would race the running turn's history copy and
        // its compaction/dedup/guardrails handles, desyncing the provider
        // from the session.
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before loading a session.".into(),
        });
        return;
    }
    load_session(
        &ctx.state,
        &ctx.files,
        &id,
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
        &ctx.evt_tx,
    )
    .await;
}

/// `Command::ResumeLatest`: refuse while a turn is running, then load this
/// directory's newest session.
async fn handle_resume_latest(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &Option<AbortHandle>,
) {
    if current_turn.is_some() {
        let _ = ctx.evt_tx.send(AgentEvent::Notice {
            tone: Tone::Warning,
            text: "A turn is still running; wait for it to finish before resuming.".into(),
        });
        return;
    }
    resume_latest(
        &ctx.state,
        &ctx.files,
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
        &ctx.evt_tx,
    )
    .await;
}

/// `Command::SetDraft`: soft-checkpoint the input draft, with a delayed
/// write if the store wants one.
async fn handle_set_draft(ctx: &LoopCtx, draft: String) {
    let mut guard = ctx.state.lock().await;
    let Some(store) = &mut guard.store else {
        return;
    };
    store.checkpoint_draft(&draft);
    if let Some(wait) = store.soft_save_wait() {
        let state = Arc::clone(&ctx.state);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(store) = &mut state.lock().await.store {
                store.checkpoint_now();
            }
        });
    }
}

/// Signal cancellation and abort any in-flight turn. Callers then differ
/// only in how much session state they rebuild.
fn interrupt(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>) {
    ctx.cancel_flag.set(true);
    if let Some(h) = current_turn.take() {
        h.abort();
    }
}

/// `Command::Interrupt`: cancel and end the assistant bubble.
fn handle_interrupt(ctx: &LoopCtx, current_turn: &mut Option<AbortHandle>) {
    interrupt(ctx, current_turn);
    let _ = ctx.evt_tx.send(AgentEvent::AssistantEnd);
    let _ = ctx.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
}

/// `Command::Clear`: interrupt, drop queued shell results, and start a
/// fresh session (keeping the same caches via `linked`).
async fn handle_clear(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
) {
    interrupt(ctx, current_turn);
    {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.clear_results();
        shell.cancel_all();
    }
    // Reset through `linked` so the fresh session's compaction state keeps
    // working dedup/guardrails handles; a bare default would strand the
    // caches the dispatcher still points at.
    *ctx.state.lock().await = SessionState::linked().with_store(
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
    );
    let _ = ctx.evt_tx.send(AgentEvent::AssistantEnd);
    let _ = ctx.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
}

/// `Command::Reset`: like `Clear`, plus dropped files and reset chrome.
async fn handle_reset(
    ctx: &LoopCtx,
    selection: &Selection,
    current_turn: &mut Option<AbortHandle>,
) {
    interrupt(ctx, current_turn);
    {
        let mut shell = ctx.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.clear_results();
        shell.cancel_all();
    }
    *ctx.state.lock().await = SessionState::linked().with_store(
        ctx.state_dir.as_ref(),
        &ctx.cwd,
        &LoopCtx::model_spec(selection),
    );
    ctx.files.lock().unwrap_or_else(|e| e.into_inner()).clear();
    let _ = ctx.evt_tx.send(AgentEvent::AssistantEnd);
    let _ = ctx.evt_tx.send(AgentEvent::FilesSet(Vec::new()));
    let _ = ctx.evt_tx.send(AgentEvent::StatusChanged(Status::Done));
    let _ = ctx.evt_tx.send(AgentEvent::TokenUsage("0.0K".into()));
}

/// `Command::Undo`: refuse while a turn is running, then roll the
/// workspace snapshots back one step.
async fn handle_undo(ctx: &LoopCtx, current_turn: &Option<AbortHandle>) {
    if current_turn.is_some() {
        // Restoring mid-run would race the turn's writes and drain its
        // live capture session.
        let _ = ctx.evt_tx.send(AgentEvent::AssistantText(
            "A turn is still running; wait for it to finish before undoing.".into(),
        ));
        return;
    }
    let message = ctx
        .snapshots
        .rollback()
        .await
        .unwrap_or_else(|| "Nothing to undo.".into());
    let _ = ctx.evt_tx.send(AgentEvent::AssistantText(message));
}

/// `Command::SelectModel`: switch the selection when the provider/model
/// pair exists in the catalogs, then re-announce the menu.
fn handle_select_model(ctx: &LoopCtx, selection: &mut Selection, provider: String, model: String) {
    let found = ctx
        .catalogs
        .get(&provider)
        .and_then(|models| models.iter().find(|m| m.id == model))
        .map(|m| m.context_length);
    match found {
        Some(context_length) => {
            *selection = Selection {
                provider,
                model,
                context_length,
            };
            let (models, current) = ctx.catalog_choices(selection);
            let _ = ctx.evt_tx.send(AgentEvent::CatalogSet { models, current });
        }
        None => {
            let _ = ctx.evt_tx.send(AgentEvent::AssistantText(format!(
                "Unknown model {model:?} on provider {provider:?}."
            )));
        }
    }
}

/// Move queued bang-mode visible-run results into the session history.
/// Called at the next `SendMessage`: pushing them when they finish would
/// race the running turn's whole-history commit (turn.rs replaces
/// `session.history` only on success). Returns how many landed.
async fn drain_shell_results(
    shell: &Arc<std::sync::Mutex<crate::tui::shell::ShellState>>,
    state: &Arc<Mutex<SessionState>>,
) -> usize {
    let queued = shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain_results();
    let drained = queued.len();
    if drained > 0 {
        state.lock().await.history.extend(queued);
    }
    drained
}

/// Command-loop exit: fold queued bang-mode results into the history and
/// persist them (no next turn exists to carry them), then flush any soft
/// checkpointed draft.
async fn persist_on_exit(ctx: &LoopCtx, selection: &Selection) {
    // No next turn exists: stop any in-flight bang runs, fold their
    // queued results into the history, and persist them.
    ctx.shell
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cancel_all();
    let drained = drain_shell_results(&ctx.shell, &ctx.state).await;
    let mut guard = ctx.state.lock().await;
    let history = guard.history.clone();
    if drained > 0
        && let Some(store) = &mut guard.store
    {
        store.record_turn(&history, LoopCtx::model_spec(selection));
    }
    if let Some(store) = &mut guard.store {
        store.checkpoint_now();
    }
}

impl Provider for CraftProvider {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();

        tokio::spawn(self.spawn_command_loop(cmd_rx, evt_tx));

        (cmd_tx, evt_rx)
    }

    fn mcp(&self) -> Option<crate::mcp::McpHandle> {
        self.mcp.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::Message;

    /// Bang-mode visible-run results queue and are drained into the session
    /// history exactly once, at the next `SendMessage`.
    #[tokio::test]
    async fn shell_results_drain_into_history_once() {
        let state = Arc::new(Mutex::new(SessionState::linked()));
        let pending: Arc<std::sync::Mutex<crate::tui::shell::ShellState>> = Arc::default();
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_result(Message::user("I ran: $ ls\n\nOutput:\nsrc"));

        drain_shell_results(&pending, &state).await;
        assert_eq!(state.lock().await.history.len(), 1);
        assert_eq!(
            state.lock().await.history[0].text(),
            "I ran: $ ls\n\nOutput:\nsrc"
        );
        assert!(pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain_results()
            .is_empty());

        // A second drain with nothing queued is a no-op.
        drain_shell_results(&pending, &state).await;
        assert_eq!(state.lock().await.history.len(), 1);
    }

    /// Quitting with queued bang-mode results persists them into the
    /// session file, instead of losing them until a next `SendMessage`.
    #[tokio::test]
    async fn bang_results_persist_on_loop_exit() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
        let state = Arc::new(Mutex::new(SessionState::linked().with_store(
            Some(&state_dir),
            "/cwd",
            "mock/model",
        )));
        let ctx = test_ctx(state.clone());
        ctx.shell
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_result(Message::user("I ran: $ ls\n\nOutput:\nsrc"));

        persist_on_exit(
            &ctx,
            &Selection {
                provider: "mock".into(),
                model: "model".into(),
                context_length: None,
            },
        )
        .await;

        assert_eq!(state.lock().await.history.len(), 1);
        // The store mints its own session id; discover it via the cwd list.
        let summaries = crate::headless::StoredSession::list(Some("/cwd"), &state_dir).unwrap();
        let found = summaries.first().expect("the session lists for this cwd");
        let reloaded =
            crate::headless::StoredSession::load(found.id.id().clone(), &state_dir).unwrap();
        assert!(
            reloaded
                .messages()
                .iter()
                .any(|m| m.text().starts_with("I ran: $ ls")),
            "the bang-mode result reached the session file"
        );
    }

    /// Submitting while a turn is alive queues the message instead of
    /// aborting the turn; the queue preserves submission order.
    #[tokio::test]
    async fn submit_mid_turn_queues_without_aborting() {
        let state = Arc::new(Mutex::new(SessionState::linked()));
        let ctx = LoopCtx {
            shell: Arc::default(),
            ..test_ctx(state)
        };
        let selection = Selection {
            provider: String::new(),
            model: String::new(),
            context_length: None,
        };
        // A long-running "turn": alive well past the test body.
        let mut current_turn = Some(
            tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            })
            .abort_handle(),
        );
        let mut pending: VecDeque<PendingMessage> = VecDeque::new();
        for text in ["first", "second"] {
            handle_send_message(
                &ctx,
                &selection,
                &mut current_turn,
                &mut pending,
                text.to_string(),
                crate::run::AgentMode::Build,
                Vec::new(),
            )
            .await;
        }
        assert_eq!(pending.len(), 2, "both messages queue behind the turn");
        assert_eq!(pending[0].0, "first");
        assert_eq!(pending[1].0, "second");
        assert!(
            !current_turn.as_ref().unwrap().is_finished(),
            "the in-flight turn was not aborted"
        );

        // A settled turn with nothing queued is a no-op.
        let aborted = current_turn.take().unwrap();
        aborted.abort();
        while !aborted.is_finished() {
            tokio::task::yield_now().await;
        }
        current_turn = Some(aborted);
        maybe_send_next(&ctx, &selection, &mut current_turn, &mut pending).await;
        assert_eq!(pending.len(), 1, "the first queued message was sent");
        assert_eq!(pending[0].0, "second");
        assert!(
            current_turn.is_some(),
            "the queued message spawned the next turn"
        );
    }

    /// A minimal LoopCtx for the queue/persist tests: only `state`,
    /// ``shell`, and `wake` are exercised.
    fn test_ctx(state: Arc<Mutex<SessionState>>) -> LoopCtx {
        LoopCtx {
            state,
            files: Files::default(),
            cancel_flag: run::cancel_channel().0,
            subagent_cancels: Arc::new(run::cancel::CancelMap::new()),
            shell: Arc::default(),
            permissions: Arc::new(PermissionManager::new(
                PermissionsConfig::default(),
                std::path::PathBuf::new(),
            )),
            config: Arc::new(Config::default()),
            workspace: Workspace::new(std::env::temp_dir()).unwrap(),
            instructions_text: String::new(),
            catalogs: BTreeMap::new(),
            snapshots: crate::snapshot::SnapshotManager::new(std::env::temp_dir()),
            state_dir: None,
            cwd: "/cwd".into(),
            evt_tx: mpsc::unbounded_channel().0,
            wake: Arc::new(Notify::new()),
        }
    }

    /// W10: a persisted session is listed by `/sessions`, and loading it
    /// repopulates the history plus the conversation view.
    #[tokio::test]
    async fn loading_a_persisted_session_repopulates_history() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
        // Persist a session the way a committed turn does.
        let session_ref = crate::id::SessionRef::generate();
        let mut store = crate::headless::SessionStore::open_in(
            state_dir.clone(),
            session_ref.clone(),
            "/cwd",
            "mock/model",
        )
        .unwrap();
        let history = vec![
            Message::user("hello there"),
            Message::assistant("hi — how can I help?"),
        ];
        store.checkpoint_draft("unsent draft");
        store.checkpoint_now();
        store.record_turn(&history, "mock/model".into());

        // The record exists in the state dir and lists for this cwd.
        let summaries = crate::headless::StoredSession::list(Some("/cwd"), &state_dir).unwrap();
        assert!(summaries.iter().any(|s| s.id == session_ref));

        // Loading it repopulates the history and emits the rebuilt transcript.
        let state = Arc::new(Mutex::new(SessionState::default()));
        let files = Files::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        load_session(
            &state,
            &files,
            session_ref.as_str(),
            Some(&state_dir),
            "/cwd",
            "mock/model",
            &tx,
        )
        .await;

        assert_eq!(state.lock().await.history, history);
        // The store rebinds to the loaded id so future turns resume it.
        assert!(state.lock().await.store.is_some());
        let mut loaded = None;
        let mut loaded_draft = String::new();
        let mut resumed = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::SessionLoaded { messages, draft } => {
                    loaded = Some(messages);
                    loaded_draft = draft;
                }
                AgentEvent::Notice { tone, text } => {
                    assert_eq!(tone, Tone::Success);
                    resumed = text.starts_with("resumed session");
                }
                _ => {}
            }
        }
        let loaded = loaded.expect("SessionLoaded event");
        assert!(matches!(&loaded[0], LoadedMessage::User(t) if t == "hello there"));
        assert!(matches!(&loaded[1], LoadedMessage::Assistant(t) if t == "hi — how can I help?"));
        assert_eq!(
            loaded_draft, "unsent draft",
            "the preserved draft rides the load"
        );
        assert!(resumed);
    }

    /// F.3 resume-latest-by-cwd: the newest session for this directory is
    /// loaded; nothing persisted keeps the fresh session untouched.
    #[tokio::test]
    async fn resume_latest_picks_the_newest_session_for_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
        let persist = |text: &str| {
            let session_ref = crate::id::SessionRef::generate();
            let mut store = crate::headless::SessionStore::open_in(
                state_dir.clone(),
                session_ref,
                "/cwd",
                "mock/model",
            )
            .unwrap();
            store.record_turn(&[Message::user(text)], "mock/model".into());
        };
        persist("older session");
        std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct updated_at seconds
        persist("newer session");

        let state = Arc::new(Mutex::new(SessionState::default()));
        let files = Files::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        resume_latest(&state, &files, Some(&state_dir), "/cwd", "mock/model", &tx).await;

        assert_eq!(state.lock().await.history.len(), 1);
        assert_eq!(state.lock().await.history[0].text(), "newer session");
        let mut saw_load = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, AgentEvent::SessionLoaded { .. }) {
                saw_load = true;
            }
        }
        assert!(saw_load, "the resumed session announces a SessionLoaded");

        // No prior session in this cwd: a notice, not a crash, and the
        // current session history stays put.
        let state = Arc::new(Mutex::new(SessionState::default()));
        state.lock().await.history = vec![Message::user("keep me")];
        let (tx, mut rx) = mpsc::unbounded_channel();
        resume_latest(
            &state,
            &files,
            Some(&state_dir),
            "/other-cwd",
            "mock/model",
            &tx,
        )
        .await;
        assert_eq!(state.lock().await.history.len(), 1);
        assert!(
            matches!(
                rx.try_recv(),
                Ok(AgentEvent::Notice {
                    tone: Tone::Neutral,
                    ..
                })
            ),
            "no-session resume is a neutral notice"
        );
    }

    /// Tool calls, results, and system blocks don't render as user/agent
    /// text in the rebuilt transcript; a system block reads as agent text.
    #[test]
    fn transcript_skips_tool_blocks() {
        let messages = vec![
            Message::user("do it"),
            Message::User {
                content: vec![crate::history::UserContent::ToolResult(
                    crate::history::ToolResult {
                        call: "t1".into(),
                        name: "read".into(),
                        content: vec![crate::history::ToolResultContent::text("file body")],
                        is_error: false,
                    },
                )],
            },
            Message::system("compacted summary"),
        ];
        let rendered = transcript(&messages);
        assert_eq!(rendered.len(), 2);
        assert!(matches!(&rendered[0], LoadedMessage::User(t) if t == "do it"));
        assert!(matches!(&rendered[1], LoadedMessage::Assistant(t) if t == "compacted summary"));
    }

    /// An unparseable id keeps the current session and warns.
    #[tokio::test]
    async fn load_with_unknown_id_keeps_the_session_and_warns() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = crate::storage::StateDir::from_path(dir.path().to_path_buf());
        let state = Arc::new(Mutex::new(SessionState::default()));
        state.lock().await.history = vec![Message::user("keep me")];
        let files = Files::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        load_session(
            &state,
            &files,
            "bogus",
            Some(&state_dir),
            "/cwd",
            "mock/model",
            &tx,
        )
        .await;
        assert_eq!(state.lock().await.history.len(), 1);
        let event = rx.try_recv().expect("a warning notice");
        assert!(
            matches!(event, AgentEvent::Notice { tone: Tone::Warning, ref text }
                if text.contains("unknown session id")),
            "{event:?}"
        );
    }
}
