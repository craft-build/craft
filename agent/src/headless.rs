//! Headless session API: the substrate non-TUI surfaces (ACP, print mode,
//! SDK-style embedding) share instead of each hand-rolling the run loop.
//!
//! Ported from the reference `craft-agent/src/headless.rs`, re-expressed on
//! this repo's seams: events ride [`crate::run::events`] (tokio mpsc, not
//! flume), turns are driven by [`crate::run::run`] rather than a
//! crate-owned agent loop, and persistence lands on the JSONL
//! [`Session`](crate::storage::sessions::Session) store behind
//! [`SessionStore`]. Params and handles carry only the inputs this repo's
//! run seam actually takes; MCP handles, Flow attachments, and plugin rule
//! stores from the reference have no counterpart here (yet).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::history::{self, Message};
use crate::id::SessionRef;
use crate::permissions::{
    PermissionCheck, PermissionError, PermissionManager, ToolKey, scope_for_call,
};
use crate::providers::DynamicModel;
use crate::run::dispatch::BeforeExecute;
use crate::run::events::{SessionEvents, event_stream};
use crate::run::{Event, RunParams, cancel_channel};
use crate::storage::sessions::{self, SessionError, StoredTokenUsage};
use crate::storage::stats::{self, CostLedger, CostUsage};
use crate::storage::{StateDir, StorageError};
use crate::tools::Workspace;

/// The persisted shape of a live session: our own message/usage/tool-result
/// domain types.
pub type StoredSession = sessions::Session<Message, StoredTokenUsage, history::ToolResult>;

/// The shortest gap between two writes that carry only soft session state
/// (the composer draft). Ported from the reference `SOFT_SAVE_DELAY`.
const SOFT_SAVE_DELAY: Duration = Duration::from_millis(1000);

/// Owns a session's persisted state for the lifetime of one headless run.
/// Save failures are warnings, never fatal: a headless run must finish even
/// when the disk is unhappy.
pub struct SessionStore {
    dir: StateDir,
    /// Cost ledger handle, opened once so per-turn appends don't re-resolve
    /// the path. `None` when the state dir cannot host it.
    ledger: Option<CostLedger>,
    session: StoredSession,
    /// When the last disk write happened, so draft-only changes land at
    /// most once per [`SOFT_SAVE_DELAY`] (each costs a meta record plus an
    /// fsync).
    last_save: Instant,
    /// A soft change is waiting for its write window; flushed by
    /// [`SessionStore::checkpoint_now`].
    pending_soft_save: bool,
}

impl SessionStore {
    /// Open (or create) in the resolved state dir; `None` when the state dir
    /// is unavailable, meaning the session simply will not be persisted.
    pub fn open(session_ref: SessionRef, cwd: &str, model_spec: &str) -> Option<Self> {
        let dir = StateDir::resolve().ok()?;
        Self::open_in(dir, session_ref, cwd, model_spec).ok()
    }

    /// Open (or create) against an explicit dir; a fresh session is saved
    /// immediately so it is loadable before the first turn completes.
    /// Returns `Err` when an existing session cannot be read for any reason
    /// other than absence — the caller must disable persistence for the run
    /// so the unreadable file is never overwritten by a later save.
    pub fn open_in(
        dir: StateDir,
        session_ref: SessionRef,
        cwd: &str,
        model_spec: &str,
    ) -> Result<Self, SessionError> {
        // Opened once and reused for every turn's cost append; a failure here
        // only means no cost records, the session itself still persists.
        let ledger = CostLedger::from_state_dir(&dir).ok();
        match StoredSession::load(session_ref.id(), &dir) {
            Ok(session) => Ok(Self {
                dir,
                ledger,
                session,
                last_save: Instant::now(),
                pending_soft_save: false,
            }),
            Err(SessionError::Storage {
                source: StorageError::NotFound { .. },
            }) => {
                let mut session = StoredSession::new(model_spec, cwd);
                session.id = session_ref;
                let mut store = Self {
                    dir,
                    ledger,
                    session,
                    last_save: Instant::now(),
                    pending_soft_save: false,
                };
                store.save();
                Ok(store)
            }
            Err(e) => Err(e),
        }
    }

    fn save(&mut self) {
        self.last_save = Instant::now();
        self.pending_soft_save = false;
        if let Err(e) = self.session.save(&self.dir) {
            eprintln!("failed to persist session: {e}");
        }
    }

    /// A draft-only change: written at most once per [`SOFT_SAVE_DELAY`],
    /// bounding what a crash takes with it without charging a keystroke an
    /// fsync. Anything a turn produced still writes immediately
    /// ([`Self::record_turn`]). An empty draft clears the field.
    pub fn checkpoint_draft(&mut self, draft: &str) {
        let draft = (!draft.is_empty()).then(|| draft.to_owned());
        if self.session.meta.input_draft.as_deref() == draft.as_deref() {
            return;
        }
        self.session.set_input_draft(draft);
        if self.last_save.elapsed() >= SOFT_SAVE_DELAY {
            self.save();
        } else {
            self.pending_soft_save = true;
        }
    }

    /// Flush a pending soft write: shutdown, or a session swap, where no
    /// later frame will carry the change to disk.
    pub fn checkpoint_now(&mut self) {
        if self.pending_soft_save {
            self.save();
        }
    }

    /// How long until a pending soft write may land, so a caller can arm a
    /// timer instead of waiting for the next keystroke to carry it.
    pub fn soft_save_wait(&self) -> Option<Duration> {
        self.pending_soft_save
            .then(|| SOFT_SAVE_DELAY.saturating_sub(self.last_save.elapsed()))
    }

    /// Persist the post-turn history and the model that served it, deriving
    /// the title from the first user message when it is still the default.
    pub fn record_turn(&mut self, messages: &[Message], model_spec: String) {
        self.session.replace_messages(messages.to_vec());
        self.session.set_model(model_spec);
        self.session.update_title_if_default();
        self.save();
    }

    /// Record the currently selected model without a turn, so the choice is
    /// restored on the next launch even when no message followed it.
    pub fn set_model(&mut self, model_spec: String) {
        self.session.set_model(model_spec);
        self.save();
    }

    pub fn thinking(&self) -> Option<crate::thinking::ThinkingConfig> {
        self.session.meta.thinking
    }

    pub fn set_thinking(&mut self, thinking: crate::thinking::ThinkingConfig) {
        self.session.set_thinking(thinking);
        self.save();
    }

    /// Persist the session mode so a resume restores it (`build`/`plan`).
    /// Mode changes are user-initiated and rare, so they write immediately.
    pub fn set_mode(&mut self, plan: bool) {
        let mode = if plan { "plan" } else { "build" };
        if self.session.meta.mode.as_deref() == Some(mode) {
            return;
        }
        self.session.set_mode(Some(mode.to_string()));
        self.save();
    }

    /// Fold a finished run's per-model usage into the session and append one
    /// cost-ledger record per model to `cost.jsonl`. Ledger failures are
    /// warnings, never fatal. A model whose cost could not be resolved records
    /// `cost_usd: null` — the token counts carry the information and callers
    /// must not show "$0.00" as a bill.
    pub fn record_cost(
        &mut self,
        by_model: &HashMap<String, crate::usage::StoredTokenUsage>,
        fast: bool,
    ) {
        if by_model.is_empty() {
            return;
        }
        for (spec, usage) in by_model {
            let stored = sessions::StoredTokenUsage::from(*usage);
            self.session.add_model_usage(spec, stored);
            if let Some(ledger) = &self.ledger {
                let (provider, model) = spec.split_once('/').unwrap_or(("", spec.as_str()));
                let record = stats::make_record(
                    self.session.id.id().to_string(),
                    model,
                    provider,
                    CostUsage::from_stored(usage),
                    usage.cost,
                    fast,
                );
                if let Err(e) = ledger.append(&record) {
                    eprintln!("warning: failed to append cost record: {e}");
                }
            }
        }
        self.save();
    }
}

/// The headless permission gate (`--print`, `craft term run`,
/// `craft recipe run`): the permission engine decides every tool call, and
/// where an interactive surface would prompt, headless fails closed — the
/// auto-review reviewer answers when the session started with `-A`,
/// otherwise the call is skipped with guidance naming the deliberate
/// opt-outs. Installing it on the dispatch table covers batch children and
/// subagent dispatch through the shared `before` hook; the `question` tool
/// degrades to `DismissAsk` headlessly, so nothing can park on a user.
pub struct HeadlessGate {
    permissions: Arc<PermissionManager>,
    /// One reviewer call per `NeedsPrompt` decision; `None` when the run
    /// did not start with auto-review on.
    reviewer: Option<crate::auto_review::Reviewer>,
}

impl HeadlessGate {
    pub fn new(
        permissions: Arc<PermissionManager>,
        reviewer: Option<crate::auto_review::Reviewer>,
    ) -> Self {
        Self {
            permissions,
            reviewer,
        }
    }
}

impl BeforeExecute for HeadlessGate {
    fn decide(&self, call: history::ToolCall) -> crate::run::BoxFuture<crate::run::Decision> {
        let gate = Self {
            permissions: Arc::clone(&self.permissions),
            reviewer: self.reviewer.clone(),
        };
        Box::pin(async move { gate_decide(gate, call).await })
    }
}

fn denied_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::new(&tool.to_string(), scopes).to_string()
}

/// What an unresolved ask becomes headlessly: a skip whose guidance says
/// why and lists every deliberate opt-out, instead of hanging or allowing.
fn headless_deny_message(tool: &ToolKey, scopes: &[String]) -> String {
    PermissionError::with_guidance(
        &tool.to_string(),
        scopes,
        "this headless run has no interactive approver, so the call was not run; \
         re-run with --yolo to bypass permission checks, with -A/--auto-review \
         to let a reviewer decide, or pre-approve the call in permissions.bml"
            .to_string(),
    )
    .to_string()
}

async fn gate_decide(gate: HeadlessGate, call: history::ToolCall) -> crate::run::Decision {
    let HeadlessGate {
        permissions,
        reviewer,
    } = gate;
    let name = call.function.name.as_str();
    let tool = ToolKey::parse(name);
    let (scopes, force_prompt) = scope_for_call(permissions.cwd(), name, &call.function.arguments);
    match permissions.check_multi(&tool, &scopes, force_prompt) {
        PermissionCheck::Allowed => crate::run::Decision::Run,
        PermissionCheck::Denied => crate::run::Decision::Skip(denied_message(&tool, &scopes)),
        PermissionCheck::NeedsPrompt { .. } => {
            if permissions.is_auto_review()
                && let Some(reviewer) = reviewer.as_ref()
            {
                auto_review_decide(reviewer, &permissions, &tool, &scopes).await
            } else {
                crate::run::Decision::Skip(headless_deny_message(&tool, &scopes))
            }
        }
    }
}

/// Auto-review path for a `NeedsPrompt` decision: one reviewer call answers
/// the prompt, its verdict is recorded as a session rule, and the outcome
/// is reported back to the model. Reviewer failures (timeout, provider
/// error, unparseable output) deny without recording a rule — the reviewer
/// never actually decided. Mirrors the TUI gate minus the overlay events.
async fn auto_review_decide(
    reviewer: &crate::auto_review::Reviewer,
    permissions: &Arc<PermissionManager>,
    tool: &ToolKey,
    scopes: &[String],
) -> crate::run::Decision {
    match reviewer(tool.to_string(), scopes.to_vec()).await {
        Ok(decision) => {
            let allow = decision.verdict == crate::auto_review::Verdict::Allow;
            permissions.apply_auto_review(tool, scopes, allow);
            eprintln!(
                "auto-review {}: {} — {}",
                decision.verdict.as_str(),
                decision.risk.as_str(),
                decision.rationale
            );
            if allow {
                return crate::run::Decision::Run;
            }
            crate::run::Decision::Skip(
                PermissionError::with_guidance(
                    &tool.to_string(),
                    scopes,
                    format!("auto-review: {}", decision.rationale),
                )
                .to_string(),
            )
        }
        Err(err) => {
            eprintln!("auto-review failed closed: {err}");
            crate::run::Decision::Skip(
                PermissionError::with_guidance(
                    &tool.to_string(),
                    scopes,
                    format!("auto-review denied this action: {err}"),
                )
                .to_string(),
            )
        }
    }
}

/// A ready-to-run one-shot turn: model, run parameters, tools workspace,
/// and the prompt to send.
pub struct HeadlessParams {
    pub model: DynamicModel,
    /// `provider/model` for pricing and the session header; `None` disables
    /// pricing and records the model as unknown.
    pub model_spec: Option<Arc<str>>,
    pub run: RunParams,
    pub workspace: Workspace,
    /// Optional interception point for every tool call (approval gate).
    pub before: Option<Arc<dyn BeforeExecute>>,
    pub prompt: String,
    /// Vision attachments staged on the prompt message (`-p --image`).
    pub images: Vec<crate::history::ImageBlock>,
    pub initial_wd: PathBuf,
    /// Where the session is persisted; `None` disables persistence.
    pub state_dir: Option<StateDir>,
    pub session_id: Option<SessionRef>,
}

pub struct HeadlessHandle {
    /// The run's events; `None` (closed) once the task ends.
    pub events: SessionEvents,
    pub tool_names: Vec<String>,
    pub session_id: SessionRef,
    pub cwd: String,
    /// Why the session is not being persisted, if persistence failed to open.
    pub persistence_error: Option<String>,
    pub task: JoinHandle<()>,
}

fn lock_sink<T>(sink: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run one prompt to completion in the background. The event stream closes
/// when the task ends; provider/run failures surface as `Event::Error`
/// followed by a terminal `Done` (the run driver emits both itself).
pub fn spawn(params: HeadlessParams) -> HeadlessHandle {
    let cwd = params.initial_wd.to_string_lossy().into_owned();
    let session_ref = params.session_id.unwrap_or_else(SessionRef::generate);
    let model_spec: Option<String> = params.model_spec.as_ref().map(|s| s.to_string());

    let (guard, events) = event_stream();
    let tool_names = params.workspace.register().names();

    let opened = params
        .state_dir
        .map(|dir| {
            SessionStore::open_in(
                dir,
                session_ref.clone(),
                &cwd,
                model_spec.as_deref().unwrap_or("unknown"),
            )
        })
        .transpose();
    let persistence_error = opened.as_ref().err().map(|e| e.to_string());
    if let Some(e) = &persistence_error {
        eprintln!("session {session_ref} will not be persisted: {e}");
    }
    let store = opened.ok().flatten();

    let task = tokio::spawn(async move {
        let mut history = Vec::new();
        let (_flag, cancel) = cancel_channel();
        let mut run_params = params.run;
        run_params.model_spec = params.model_spec.clone();
        let event_tx = guard.sender(0);
        let subagent_event_tx = event_tx.clone();
        // The subagent seam (A.5): headless sessions carry no provider
        // configs, so tier selection degrades to the parent model.
        let provider = params
            .model_spec
            .as_ref()
            .and_then(|s| s.split('/').next().map(str::to_owned))
            .unwrap_or_default();
        // The gate rides the workspace itself so every registration — the
        // turn's table, batch fan-outs, subagent children — sees it; a
        // post-register attach would miss the batch snapshot.
        let gated_workspace = match &params.before {
            Some(hook) => params.workspace.clone().with_before(Arc::clone(hook)),
            None => params.workspace.clone(),
        };
        let subagents = Arc::new(crate::subagent::SubagentLauncher {
            parent_model: params.model.clone(),
            parent_spec: params.model_spec.as_deref().unwrap_or("").to_owned(),
            provider,
            providers: Default::default(),
            agent: crate::config::AgentConfig {
                temperature: run_params.temperature,
                max_tokens: run_params.max_tokens,
                thinking: Some(run_params.thinking),
                ..Default::default()
            },
            compression: run_params.compression.clone(),
            base_prompt: String::new(),
            workspace: gated_workspace.clone(),
            history: Vec::new(),
            cancel: cancel.clone(),
            cancels: Arc::new(crate::run::cancel::CancelMap::new()),
            emit: Arc::new(move |event| subagent_event_tx.send(event)),
            before: params.before.clone(),
        });
        let tools = gated_workspace
            .with_subagents(subagents)
            .with_cancel(cancel.clone())
            .register();

        let mut store = store;
        if let Some(store) = &mut store {
            store.set_thinking(run_params.thinking);
        }
        // The terminal Done's per-model ledger, captured from the event
        // seam so the store can persist it after the run ends.
        let done_by_model = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&done_by_model);
        crate::run::run_with_images(
            &params.model,
            &run_params,
            &tools,
            &mut history,
            &params.prompt,
            &params.images,
            &cancel,
            &|event| {
                if let Event::Done { by_model, .. } = &event {
                    lock_sink(&sink).replace(by_model.clone());
                }
                event_tx.send(event);
            },
        )
        .await;
        let by_model = lock_sink(&done_by_model).take().unwrap_or_default();
        if let Some(store) = &mut store {
            store.record_cost(&by_model, run_params.fast);
            store.record_turn(&history, model_spec.unwrap_or_else(|| "unknown".into()));
        }
        // `guard` drops here, closing the stream.
    });

    HeadlessHandle {
        events,
        tool_names,
        session_id: session_ref,
        cwd,
        persistence_error,
        task,
    }
}

/// A channel-driven interactive session: each input runs one multi-turn
/// run against the shared history, which is persisted after every turn.
pub struct InteractiveParams {
    pub model: DynamicModel,
    pub model_spec: Option<Arc<str>>,
    pub run: RunParams,
    pub workspace: Workspace,
    /// Optional interception point for every tool call (approval gate).
    pub before: Option<Arc<dyn BeforeExecute>>,
    pub initial_wd: PathBuf,
    /// Resume an existing session; omitted generates a fresh id.
    pub session_id: Option<SessionRef>,
    /// Seeded history (e.g. replayed from a prior session).
    pub initial_history: Vec<Message>,
    /// Where the session is persisted; `None` disables persistence.
    pub state_dir: Option<StateDir>,
    /// Post-turn argosy memory extraction (Phase 4): mirrors
    /// `agent.memory_extraction`; callers with a config pass it through.
    pub memory_extraction: bool,
}

pub struct InteractiveHandle {
    /// The session's events for its whole lifetime; `None` (closed) once
    /// the task ends.
    pub events: SessionEvents,
    pub tool_names: Vec<String>,
    /// Next prompt to run.
    pub input_tx: mpsc::UnboundedSender<String>,
    /// Cancel the in-flight turn (or any pending signal is drained before
    /// the next turn starts).
    pub cancel_tx: mpsc::UnboundedSender<()>,
    /// Swap the model before the next turn; the last value wins.
    pub model_tx: mpsc::UnboundedSender<(DynamicModel, Option<Arc<str>>)>,
    pub session_id: SessionRef,
    /// Why the session is not being persisted, if persistence failed to open.
    pub persistence_error: Option<String>,
    pub task: JoinHandle<()>,
}

pub fn spawn_interactive(params: InteractiveParams) -> InteractiveHandle {
    let cwd = params.initial_wd.to_string_lossy().into_owned();
    let session_ref = params
        .session_id
        .clone()
        .unwrap_or_else(SessionRef::generate);
    let model_spec: Option<String> = params.model_spec.as_ref().map(|s| s.to_string());

    let (guard, events) = event_stream();
    let tools = params.workspace.register();
    let tool_names = tools.names();

    let (input_tx, input_rx) = mpsc::unbounded_channel::<String>();
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<()>();
    let (model_tx, model_rx) = mpsc::unbounded_channel::<(DynamicModel, Option<Arc<str>>)>();
    let cancel_rx = Arc::new(tokio::sync::Mutex::new(cancel_rx));

    let opened = params
        .state_dir
        .map(|dir| {
            SessionStore::open_in(
                dir,
                session_ref.clone(),
                &cwd,
                model_spec.as_deref().unwrap_or("unknown"),
            )
        })
        .transpose();
    let persistence_error = opened.as_ref().err().map(|e| e.to_string());
    if let Some(e) = &persistence_error {
        eprintln!("session {session_ref} will not be persisted: {e}");
    }
    let store = opened.ok().flatten();

    let task = tokio::spawn(async move {
        let mut model = params.model;
        let mut model_spec = model_spec;
        let mut run_params = params.run;
        let mut history = params.initial_history;
        let mut store = store;
        if let Some(store) = &mut store {
            if let Some(thinking) = store.thinking() {
                run_params.thinking = thinking;
            } else {
                store.set_thinking(run_params.thinking);
            }
        }
        let mut input_rx = input_rx;
        let mut model_rx = model_rx;
        let mut run_id: u64 = 0;

        while let Some(prompt) = input_rx.recv().await {
            let event_tx = guard.sender(run_id);

            // Last model swap requested before this turn wins: drain the
            // queue and apply only the most recent spec.
            let mut swap = None;
            while let Ok(next) = model_rx.try_recv() {
                swap = Some(next);
            }
            if let Some((new_model, new_spec)) = swap {
                model = new_model;
                model_spec = new_spec
                    .as_ref()
                    .map(|s| s.to_string())
                    .or_else(|| model.label().map(str::to_owned));
            }
            run_params.model_spec = model_spec.clone().map(Arc::from);

            // A cancel left over from a previous turn must not kill this
            // one, so drain before arming the watcher.
            drain_cancel(&cancel_rx).await;
            let (flag, cancel) = cancel_channel();

            // The subagent seam (A.5), armed with this turn's cancel token
            // and event channel; headless carries no provider configs, so
            // tier selection degrades to the parent model.
            let turn_event_tx = event_tx.clone();
            let provider = model_spec
                .as_ref()
                .and_then(|s| s.split('/').next().map(str::to_owned))
                .unwrap_or_default();
            // The gate rides the workspace itself so every registration —
            // this turn's table, batch fan-outs, subagent children — sees
            // it; a post-register attach would miss the batch snapshot.
            let turn_workspace = match &params.before {
                Some(hook) => params.workspace.clone().with_before(Arc::clone(hook)),
                None => params.workspace.clone(),
            };
            let subagents = Arc::new(crate::subagent::SubagentLauncher {
                parent_model: model.clone(),
                parent_spec: model_spec.as_deref().unwrap_or("").to_owned(),
                provider,
                providers: Default::default(),
                agent: crate::config::AgentConfig {
                    temperature: run_params.temperature,
                    max_tokens: run_params.max_tokens,
                    thinking: Some(run_params.thinking),
                    ..Default::default()
                },
                compression: run_params.compression.clone(),
                base_prompt: String::new(),
                workspace: turn_workspace.clone(),
                history: history.clone(),
                cancel: cancel.clone(),
                cancels: Arc::new(crate::run::cancel::CancelMap::new()),
                emit: Arc::new(move |event| turn_event_tx.send(event)),
                before: params.before.clone(),
            });
            let tools = turn_workspace.with_subagents(subagents).register();

            let watcher = {
                let cancel_rx = Arc::clone(&cancel_rx);
                tokio::spawn(async move {
                    let mut guard = cancel_rx.lock().await;
                    if guard.recv().await.is_some() {
                        flag.set(true);
                    }
                })
            };

            // The terminal Done's per-model ledger for this turn.
            let done_by_model = Arc::new(std::sync::Mutex::new(None));
            let sink = Arc::clone(&done_by_model);
            crate::run::run(
                &model,
                &run_params,
                &tools,
                &mut history,
                &prompt,
                &cancel,
                &|event| {
                    if let Event::Done { by_model, .. } = &event {
                        *sink.lock().expect("usage sink") = Some(by_model.clone());
                    }
                    event_tx.send(event);
                },
            )
            .await;
            watcher.abort();

            // Phase 4: detached, best-effort memory extraction into the
            // project's local argosy. Never blocks or fails the turn.
            crate::knowledge_memory::spawn_extraction(
                model.clone(),
                history.clone(),
                params.workspace.root().to_path_buf(),
                params.memory_extraction,
            );
            if let Some(store) = &mut store {
                let by_model = lock_sink(&done_by_model).take().unwrap_or_default();
                store.record_cost(&by_model, run_params.fast);
                store.record_turn(
                    &history,
                    model_spec.clone().unwrap_or_else(|| "unknown".into()),
                );
            }
            run_id += 1;
        }
        crate::knowledge_memory::wait_for_pending(std::time::Duration::from_secs(15)).await;
        // `guard` drops here, closing the stream.
    });

    InteractiveHandle {
        events,
        tool_names,
        input_tx,
        cancel_tx,
        model_tx,
        session_id: session_ref,
        persistence_error,
        task,
    }
}

async fn drain_cancel(cancel_rx: &Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<()>>>) {
    let mut guard = cancel_rx.lock().await;
    while guard.try_recv().is_ok() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::Event;

    use super::StoredSession;
    use crate::storage::sessions::generate_title;
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    const CWD: &str = "/project";
    const MODEL_SPEC: &str = "anthropic/claude-test";

    fn state_dir(tmp: &tempfile::TempDir) -> StateDir {
        StateDir::from_path(tmp.path().to_path_buf())
    }

    fn session_ref() -> SessionRef {
        SessionRef::from_id("01965087-4c71-7f00-8000-000000000000".parse().unwrap())
    }

    fn store_in(tmp: &tempfile::TempDir) -> SessionStore {
        SessionStore::open_in(state_dir(tmp), session_ref(), CWD, MODEL_SPEC).unwrap()
    }

    fn load(tmp: &tempfile::TempDir) -> StoredSession {
        StoredSession::load(session_ref().id(), &state_dir(tmp)).unwrap()
    }

    fn mock(turns: Vec<Vec<MockStreamEvent>>) -> DynamicModel {
        crate::providers::DynamicModel::wrap(
            Some("mock"),
            MockCompletionModel::from_stream_turns(turns),
        )
    }

    #[test]
    fn new_session_is_loadable_before_first_turn() {
        let tmp = tempfile::tempdir().unwrap();
        store_in(&tmp);
        let loaded = load(&tmp);
        assert_eq!(loaded.id, session_ref());
        assert_eq!(loaded.cwd, CWD);
        assert_eq!(loaded.model, MODEL_SPEC);
        assert!(loaded.messages().is_empty());
    }

    #[test]
    fn record_turn_persists_messages_and_title() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        let messages = vec![Message::user("fix the login bug")];
        store.record_turn(&messages, MODEL_SPEC.into());

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 1);
        assert_eq!(loaded.title, generate_title(&messages));
    }

    /// F.3 draft preservation: a draft-only change waits for its soft-write
    /// window on disk, `checkpoint_now` flushes it, and an emptied draft
    /// clears the stored copy.
    #[test]
    fn checkpoint_draft_soft_delays_then_flushes_and_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        // The store just wrote its fresh header, so the first draft lands
        // inside the soft window: recorded, not yet on disk.
        store.checkpoint_draft("half-finished thought");
        assert!(
            load(&tmp).meta.input_draft.is_none(),
            "a draft inside the soft window must not hit disk yet"
        );
        let wait = store
            .soft_save_wait()
            .expect("a deferred draft owes a write window");
        assert!(wait <= SOFT_SAVE_DELAY);

        // Shutdown flush: the pending draft reaches disk.
        store.checkpoint_now();
        assert_eq!(
            load(&tmp).meta.input_draft.as_deref(),
            Some("half-finished thought")
        );

        // A changed draft within the window again defers; the next turn's
        // record_turn write carries it regardless of the delay.
        store.checkpoint_draft("edited");
        store.record_turn(&[Message::user("sent")], MODEL_SPEC.into());
        let loaded = load(&tmp);
        assert_eq!(loaded.meta.input_draft.as_deref(), Some("edited"));
        assert_eq!(loaded.messages().len(), 1);

        // Submitting empties the draft: the stored copy is cleared.
        store.checkpoint_draft("");
        store.checkpoint_now();
        assert!(load(&tmp).meta.input_draft.is_none());

        // An unchanged draft is a no-op (no revision bump, no write).
        let before = store.session.revision();
        store.checkpoint_draft("");
        assert_eq!(store.session.revision(), before);
        assert!(
            store.soft_save_wait().is_none(),
            "nothing pending, nothing owed"
        );
    }

    #[test]
    fn record_turn_persists_tool_results() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(
            &[
                Message::user("fix the login bug"),
                Message::User {
                    content: vec![crate::history::UserContent::ToolResult(
                        history::ToolResult {
                            call: "t1".into(),
                            name: "read".into(),
                            content: vec![history::ToolResultContent::text("build failed")],
                            is_error: false,
                        },
                    )],
                },
            ],
            MODEL_SPEC.into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
    }

    #[test]
    fn reopening_resumes_existing_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(&tmp);
        store.record_turn(&[Message::user("first prompt")], MODEL_SPEC.into());
        drop(store);

        let mut store = store_in(&tmp);
        assert_eq!(store.session.messages().len(), 1);

        store.record_turn(
            &[
                Message::user("first prompt"),
                Message::user("second prompt"),
            ],
            "other/model".into(),
        );

        let loaded = load(&tmp);
        assert_eq!(loaded.messages().len(), 2);
        assert_eq!(loaded.model, "other/model");
    }

    fn workspace() -> Workspace {
        let root = std::env::temp_dir().join(format!("craft-headless-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        Workspace::new(&root).unwrap()
    }

    fn params(model: DynamicModel, prompt: &str, state: Option<StateDir>) -> HeadlessParams {
        HeadlessParams {
            model,
            model_spec: Some(Arc::from("mock/model")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            prompt: prompt.into(),
            images: Vec::new(),
            initial_wd: std::env::temp_dir(),
            state_dir: state,
            session_id: None,
        }
    }

    async fn drain_until_done(events: &mut SessionEvents) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(envelope) = events.next().await {
            let is_done = matches!(envelope.event, Event::Done { .. });
            out.push(envelope.event);
            if is_done {
                break;
            }
        }
        out
    }

    #[tokio::test]
    async fn spawn_streams_events_and_persists_the_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let model = mock(vec![vec![
            MockStreamEvent::text("all done"),
            MockStreamEvent::final_response_with_total_tokens(3),
        ]]);
        let mut handle = spawn(params(model, "hello", Some(state_dir(&tmp))));

        let events = drain_until_done(&mut handle.events).await;
        assert!(events.iter().any(|e| matches!(e, Event::Done { .. })));
        let _ = handle.task.await;

        let loaded = StoredSession::load(handle.session_id.id(), &state_dir(&tmp)).unwrap();
        // User prompt + assistant reply.
        assert_eq!(loaded.messages().len(), 2);
        assert!(loaded.messages()[1].text().contains("all done"));
    }

    #[tokio::test]
    async fn spawn_stream_closes_after_the_task_ends() {
        let model = mock(vec![vec![
            MockStreamEvent::text("done"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let mut handle = spawn(params(model, "hello", None));
        let events = drain_until_done(&mut handle.events).await;
        assert!(events.iter().any(|e| matches!(e, Event::Done { .. })));
        let _ = handle.task.await;
        assert!(handle.events.next().await.is_none());
    }

    #[tokio::test]
    async fn interactive_runs_and_persists_multiple_turns() {
        let tmp = tempfile::tempdir().unwrap();
        let model = mock(vec![
            vec![
                MockStreamEvent::text("one"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("two"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let handle = spawn_interactive(InteractiveParams {
            model,
            model_spec: Some(Arc::from("mock/model")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: Some(state_dir(&tmp)),
            memory_extraction: false,
        });
        let mut events = handle.events;

        handle.input_tx.send("first".into()).unwrap();
        let one = drain_until_done(&mut events).await;
        assert!(one.iter().any(|e| matches!(e, Event::Done { .. })));

        handle.input_tx.send("second".into()).unwrap();
        let two = drain_until_done(&mut events).await;
        assert!(two.iter().any(|e| matches!(e, Event::Done { .. })));

        drop(handle.input_tx);
        let _ = handle.task.await;

        let loaded = StoredSession::load(handle.session_id.id(), &state_dir(&tmp)).unwrap();
        // two user prompts + two assistant replies
        assert_eq!(loaded.messages().len(), 4);
    }

    #[tokio::test]
    async fn interactive_applies_model_swap_before_next_turn() {
        let first = mock(vec![vec![
            MockStreamEvent::text("from first"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let second = mock(vec![vec![
            MockStreamEvent::text("from second"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let handle = spawn_interactive(InteractiveParams {
            model: first,
            model_spec: Some(Arc::from("mock/first")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
            memory_extraction: false,
        });
        let mut events = handle.events;

        handle.input_tx.send("go".into()).unwrap();
        let one = drain_until_done(&mut events).await;
        assert!(
            one.iter()
                .any(|e| matches!(e, Event::TextDelta(t) if t.contains("from first")))
        );

        handle
            .model_tx
            .send((second, Some(Arc::from("mock/second"))))
            .unwrap();
        handle.input_tx.send("again".into()).unwrap();
        let two = drain_until_done(&mut events).await;
        assert!(
            two.iter()
                .any(|e| matches!(e, Event::TextDelta(t) if t.contains("from second")))
        );

        drop(handle.input_tx);
        let _ = handle.task.await;
    }

    #[tokio::test]
    async fn interactive_last_queued_model_swap_wins() {
        let initial = mock(vec![vec![
            MockStreamEvent::text("from initial"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let stale = mock(vec![vec![
            MockStreamEvent::text("from stale"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let latest = mock(vec![vec![
            MockStreamEvent::text("from latest"),
            MockStreamEvent::final_response_with_total_tokens(1),
        ]]);
        let handle = spawn_interactive(InteractiveParams {
            model: initial,
            model_spec: Some(Arc::from("mock/initial")),
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
            memory_extraction: false,
        });
        let mut events = handle.events;

        // Both swaps land in the queue before the turn starts (swaps are
        // only drained after a prompt arrives); the LAST one must win.
        handle
            .model_tx
            .send((stale, Some(Arc::from("mock/stale"))))
            .unwrap();
        handle
            .model_tx
            .send((latest, Some(Arc::from("mock/latest"))))
            .unwrap();
        handle.input_tx.send("go".into()).unwrap();
        let events = drain_until_done(&mut events).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TextDelta(t) if t.contains("from latest"))),
            "the last queued model swap must win: {events:?}"
        );

        drop(handle.input_tx);
        let _ = handle.task.await;
    }

    #[tokio::test]
    async fn stale_cancel_is_drained_before_the_next_turn() {
        let model = mock(vec![
            vec![
                MockStreamEvent::text("one"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("two"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let handle = spawn_interactive(InteractiveParams {
            model,
            model_spec: None,
            run: RunParams::default(),
            workspace: workspace(),
            before: None,
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
            memory_extraction: false,
        });
        let mut events = handle.events;

        handle.input_tx.send("first".into()).unwrap();
        let one = drain_until_done(&mut events).await;
        assert!(matches!(
            one.last(),
            Some(Event::Done {
                reason: crate::run::DoneReason::Stop,
                ..
            })
        ));

        // A cancel with no turn in flight must not poison the next turn.
        handle.cancel_tx.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        handle.input_tx.send("second".into()).unwrap();
        let two = drain_until_done(&mut events).await;
        assert!(matches!(
            two.last(),
            Some(Event::Done {
                reason: crate::run::DoneReason::Stop,
                ..
            })
        ));

        drop(handle.input_tx);
        let _ = handle.task.await;
    }

    /// Blocks the run's only tool call until the test releases it, so the
    /// cancel lands while the turn is genuinely in flight.
    struct GatedHook(Arc<tokio::sync::Notify>);

    impl BeforeExecute for GatedHook {
        fn decide(
            &self,
            _call: history::ToolCall,
        ) -> crate::run::BoxFuture<crate::run::dispatch::Decision> {
            let release = Arc::clone(&self.0);
            Box::pin(async move {
                release.notified().await;
                crate::run::dispatch::Decision::Run
            })
        }
    }

    #[tokio::test]
    async fn interactive_cancel_mid_run_ends_the_turn_as_cancelled() {
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call("t1", "list", serde_json::json!({"path": "."})),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("never reached"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let gate = Arc::new(tokio::sync::Notify::new());
        let handle = spawn_interactive(InteractiveParams {
            model,
            model_spec: None,
            run: RunParams::default(),
            workspace: workspace(),
            before: Some(Arc::new(GatedHook(Arc::clone(&gate)))),
            initial_wd: std::env::temp_dir(),
            session_id: None,
            initial_history: Vec::new(),
            state_dir: None,
            memory_extraction: false,
        });
        let mut events = handle.events;

        handle.input_tx.send("go".into()).unwrap();
        // Wait until the tool call is actually in flight, then cancel and
        // let the gated call through; the next model call must not run.
        loop {
            let Some(envelope) = events.next().await else {
                panic!("stream closed")
            };
            if matches!(envelope.event, Event::ToolStart { .. }) {
                break;
            }
        }
        handle.cancel_tx.send(()).unwrap();
        gate.notify_one();

        let mut terminal = None;
        while let Some(envelope) = events.next().await {
            if let Event::Done { reason, .. } = envelope.event {
                terminal = Some(reason);
                break;
            }
        }
        assert_eq!(terminal, Some(crate::run::DoneReason::Cancelled));

        drop(handle.input_tx);
        let _ = handle.task.await;
    }

    // --- HeadlessGate --------------------------------------------------------

    use crate::permissions::{Effect, PermissionRule, PermissionsConfig, READ_ONLY_TOOLS};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn manager(root: &std::path::Path) -> Arc<PermissionManager> {
        Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            root.to_path_buf(),
        ))
    }

    fn deny(permissions: &PermissionManager, tool: ToolKey) {
        permissions.add_session_rule(PermissionRule {
            tool,
            scope: None,
            effect: Effect::Deny,
        });
    }

    async fn decide(
        gate: &HeadlessGate,
        name: &str,
        args: serde_json::Value,
    ) -> crate::run::Decision {
        gate.decide(history::ToolCall::new("t1", name, args)).await
    }

    fn allow_reviewer() -> (crate::auto_review::Reviewer, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let reviewer: crate::auto_review::Reviewer = Arc::new(move |_tool, _scopes| {
            seen.fetch_add(1, Ordering::Relaxed);
            let decision = crate::auto_review::Decision {
                verdict: crate::auto_review::Verdict::Allow,
                risk: crate::auto_review::Risk::Low,
                rationale: "local read-only build".into(),
            };
            Box::pin(async move { Ok(decision) })
        });
        (reviewer, calls)
    }

    #[tokio::test]
    async fn read_only_tools_pass_the_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = HeadlessGate::new(manager(tmp.path()), None);
        for name in READ_ONLY_TOOLS {
            assert!(
                matches!(
                    decide(&gate, name, serde_json::json!({})).await,
                    crate::run::Decision::Run
                ),
                "{name} should run without a decision"
            );
        }
    }

    #[tokio::test]
    async fn deny_rules_block_bash_mutations_and_mcp_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("bash"));
        deny(&permissions, ToolKey::native("write"));
        deny(&permissions, ToolKey::parse("github__create_issue"));
        let gate = HeadlessGate::new(permissions, None);
        for (name, args) in [
            ("bash", serde_json::json!({"command": "rm -rf /"})),
            (
                "write",
                serde_json::json!({"path": "out.rs", "content": "x"}),
            ),
            ("github__create_issue", serde_json::json!({"title": "x"})),
        ] {
            match decide(&gate, name, args).await {
                crate::run::Decision::Skip(reason) => assert!(
                    reason.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                    "{name}: {reason}"
                ),
                other => panic!("{name} must be denied, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn unresolved_asks_fail_closed_with_headless_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = HeadlessGate::new(manager(tmp.path()), None);
        // `bash` defaults to an ask; headless has no approver to answer it.
        match decide(&gate, "bash", serde_json::json!({"command": "cargo build"})).await {
            crate::run::Decision::Skip(reason) => {
                assert!(
                    reason.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                    "{reason}"
                );
                for opt in ["--yolo", "--auto-review", "permissions.bml"] {
                    assert!(reason.contains(opt), "guidance must name {opt}: {reason}");
                }
            }
            other => panic!("an unresolved ask must skip, not {other:?}"),
        }
    }

    #[tokio::test]
    async fn destructive_mcp_hints_fail_closed_despite_allow_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.add_session_rule(PermissionRule {
            tool: ToolKey::parse("db__drop_table"),
            scope: None,
            effect: Effect::Allow,
        });
        permissions.register_mcp_annotations("db", "drop_table", Some(false), Some(true));
        let gate = HeadlessGate::new(permissions, None);
        match decide(
            &gate,
            "db__drop_table",
            serde_json::json!({"table": "users"}),
        )
        .await
        {
            crate::run::Decision::Skip(reason) => {
                assert!(
                    reason.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                    "{reason}"
                );
                assert!(
                    reason.contains("--yolo"),
                    "guidance must name the opt-outs: {reason}"
                );
            }
            other => panic!("a destructive hint must force a decision, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn yolo_runs_even_under_explicit_denies() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("bash"));
        permissions.set_yolo(true);
        let gate = HeadlessGate::new(permissions, None);
        assert!(matches!(
            decide(&gate, "bash", serde_json::json!({"command": "rm -rf /"})).await,
            crate::run::Decision::Run
        ));
    }

    #[tokio::test]
    async fn auto_review_allow_runs_and_records_a_session_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let (reviewer, calls) = allow_reviewer();
        let gate = HeadlessGate::new(Arc::clone(&permissions), Some(reviewer));
        let args = serde_json::json!({"command": "cargo build"});
        for _ in 0..2 {
            assert!(matches!(
                decide(&gate, "bash", args.clone()).await,
                crate::run::Decision::Run
            ));
        }
        // The recorded session rule answers the second call; one review.
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn auto_review_deny_skips_with_rationale_and_sticks() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let reviewer: crate::auto_review::Reviewer = Arc::new(move |_tool, _scopes| {
            let decision = crate::auto_review::Decision {
                verdict: crate::auto_review::Verdict::Deny,
                risk: crate::auto_review::Risk::High,
                rationale: "destructive glob".into(),
            };
            Box::pin(async move { Ok(decision) })
        });
        let gate = HeadlessGate::new(Arc::clone(&permissions), Some(reviewer));
        let args = serde_json::json!({"command": "rm -rf /"});
        match decide(&gate, "bash", args.clone()).await {
            crate::run::Decision::Skip(reason) => {
                assert!(reason.contains("auto-review: destructive glob"), "{reason}");
            }
            other => panic!("a deny verdict must skip, not {other:?}"),
        }
        // The verdict became a session rule: the sibling call is denied
        // outright, never reaching a reviewer.
        let reason = match decide(&gate, "bash", args).await {
            crate::run::Decision::Skip(reason) => reason,
            other => panic!("expected the recorded deny, got {other:?}"),
        };
        assert!(!reason.contains("auto-review"), "{reason}");
    }

    #[tokio::test]
    async fn auto_review_failure_fails_closed_without_recording() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let reviewer: crate::auto_review::Reviewer = Arc::new(move |_tool, _scopes| {
            let error = crate::auto_review::ReviewError::Timeout {
                deadline: std::time::Duration::from_secs(30),
            };
            Box::pin(async move { Err(error) })
        });
        let gate = HeadlessGate::new(Arc::clone(&permissions), Some(reviewer));
        match decide(&gate, "bash", serde_json::json!({"command": "cargo build"})).await {
            crate::run::Decision::Skip(reason) => {
                assert!(reason.contains("auto-review denied"), "{reason}");
            }
            other => panic!("a failed review must deny, not {other:?}"),
        }
        // Nothing was decided, so nothing was recorded.
        assert!(!matches!(
            permissions.check(&ToolKey::native("bash"), &["cargo build".to_string()]),
            PermissionCheck::Denied
        ));
        // And a session that somehow lost its reviewer still fails closed.
        let gate = HeadlessGate::new(permissions, None);
        assert!(matches!(
            decide(&gate, "bash", serde_json::json!({"command": "cargo build"})).await,
            crate::run::Decision::Skip(_)
        ));
    }

    // --- Gate-wired execution -------------------------------------------------

    /// The concatenated text of one tool result, for deny-text assertions.
    fn result_text(result: &history::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                history::ToolResultContent::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool_results(events: &[Event]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::ToolDone { name, result, .. } => Some((name.clone(), result_text(result))),
                _ => None,
            })
            .collect()
    }

    /// A spawn-shaped [`HeadlessParams`] with the real gate installed:
    /// permissions rooted at `tmp`, workspace at `tmp`.
    fn gated(
        model: DynamicModel,
        tmp: &tempfile::TempDir,
        permissions: Arc<PermissionManager>,
        reviewer: Option<crate::auto_review::Reviewer>,
    ) -> HeadlessParams {
        let before = Some(
            Arc::new(HeadlessGate::new(Arc::clone(&permissions), reviewer))
                as Arc<dyn BeforeExecute>,
        );
        HeadlessParams {
            model,
            model_spec: Some(Arc::from("mock/model")),
            run: RunParams::default(),
            workspace: Workspace::new(tmp.path()).unwrap(),
            before,
            prompt: "go".into(),
            images: Vec::new(),
            initial_wd: tmp.path().to_path_buf(),
            state_dir: Some(state_dir(tmp)),
            session_id: None,
        }
    }

    #[tokio::test]
    async fn denied_calls_never_execute_and_report_back_to_the_model() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("bash"));
        deny(&permissions, ToolKey::native("write"));
        let marker = tmp.path().join("marker");
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call(
                    "t1",
                    "bash",
                    serde_json::json!({"command": format!("touch {}", marker.display())}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::tool_call(
                    "t2",
                    "write",
                    serde_json::json!({"path": "blocked.rs", "content": "never"}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done anyway"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let mut handle = spawn(gated(model, &tmp, permissions, None));
        let events = drain_until_done(&mut handle.events).await;
        let _ = handle.task.await;

        assert!(
            matches!(
                events.last(),
                Some(Event::Done {
                    reason: crate::run::DoneReason::Stop,
                    ..
                })
            ),
            "a denial is a normal end, not a run failure"
        );
        assert!(!marker.exists(), "the denied bash call must not run");
        assert!(
            !tmp.path().join("blocked.rs").exists(),
            "the denied write must not run"
        );
        let results = tool_results(&events);
        assert_eq!(results.len(), 2);
        for (name, text) in &results {
            assert!(
                text.starts_with(crate::permissions::PERMISSION_DENIED_PREFIX),
                "{name} result must carry the deny: {text}"
            );
        }
    }

    #[tokio::test]
    async fn yolo_actually_runs_what_standard_denies() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        deny(&permissions, ToolKey::native("write"));
        permissions.set_yolo(true);
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call(
                    "t1",
                    "write",
                    serde_json::json!({"path": "free.rs", "content": "made it"}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let mut handle = spawn(gated(model, &tmp, permissions, None));
        drain_until_done(&mut handle.events).await;
        let _ = handle.task.await;
        assert!(
            tmp.path().join("free.rs").exists(),
            "yolo must bypass the deny"
        );
    }

    #[tokio::test]
    async fn auto_review_allows_an_ask_into_execution() {
        // `bash` is the genuine ask (never builtin-allowed); the sandbox
        // wrapper must be off because nested sandbox-exec is denied on
        // hosts that already run the test process sandboxed.
        // SAFETY: single process-wide test knob, same as the bash tool tests.
        unsafe { std::env::set_var("CRAFT_SANDBOX", "off") };
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        permissions.set_auto_review(true);
        let (reviewer, _calls) = allow_reviewer();
        let marker = tmp.path().join("reviewed-marker");
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call(
                    "t1",
                    "bash",
                    serde_json::json!({"command": format!("touch {}", marker.display())}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let mut handle = spawn(gated(model, &tmp, permissions, Some(reviewer)));
        let events = drain_until_done(&mut handle.events).await;
        let _ = handle.task.await;
        assert!(
            marker.exists(),
            "an allowed verdict must let the call execute: {events:?}"
        );
    }

    #[tokio::test]
    async fn batch_children_go_through_the_headless_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let permissions = manager(tmp.path());
        // The parent batch call is allowed so the children are what matter.
        permissions.add_session_rule(PermissionRule {
            tool: ToolKey::native("batch"),
            scope: None,
            effect: Effect::Allow,
        });
        deny(&permissions, ToolKey::native("write"));
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call(
                    "t1",
                    "batch",
                    serde_json::json!({"tool_calls": [
                        {"tool": "write", "parameters": {"path": "child.rs", "content": "no"}}
                    ]}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("done"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let mut handle = spawn(gated(model, &tmp, permissions, None));
        let events = drain_until_done(&mut handle.events).await;
        let _ = handle.task.await;
        assert!(
            !tmp.path().join("child.rs").exists(),
            "the denied batch child must not run"
        );
        let results = tool_results(&events);
        let batch = results
            .iter()
            .find(|(name, _)| name == "batch")
            .expect("the batch call itself completed");
        assert!(
            batch
                .1
                .contains(crate::permissions::PERMISSION_DENIED_PREFIX),
            "the child denial must surface in the batch result: {}",
            batch.1
        );
    }

    #[tokio::test]
    async fn question_calls_complete_as_dismissed_headlessly() {
        let tmp = tempfile::tempdir().unwrap();
        let model = mock(vec![
            vec![
                MockStreamEvent::tool_call(
                    "t1",
                    "question",
                    serde_json::json!({"questions": [
                        {"question": "which?", "options": [{"label": "a"}, {"label": "b"}]}
                    ]}),
                ),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
            vec![
                MockStreamEvent::text("ok"),
                MockStreamEvent::final_response_with_total_tokens(1),
            ],
        ]);
        let mut handle = spawn(gated(model, &tmp, manager(tmp.path()), None));
        let events = drain_until_done(&mut handle.events).await;
        let _ = handle.task.await;
        assert!(
            matches!(
                events.last(),
                Some(Event::Done {
                    reason: crate::run::DoneReason::Stop,
                    ..
                })
            ),
            "a dismissed ask ends the run normally, it never hangs"
        );
        let asked = tool_results(&events);
        assert!(
            asked
                .iter()
                .any(|(name, text)| name == "question" && text.contains("dismissed")),
            "{asked:?}"
        );
    }
}
