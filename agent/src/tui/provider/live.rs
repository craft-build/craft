//! CraftProvider: the TUI's real backend, driving the shared run loop the
//! ACP server uses.
//!
//! Turn semantics mirror `acp::run_turn`: a per-turn provider rebuild, the
//! configured compaction stages ahead of the model call, cancellation through
//! the run loop's `CancelToken`, and history committed only on a successful
//! run. Edit-family tools are gated behind the UI's approve/reject seam via
//! the dispatch `BeforeExecute` hook: no workspace mutation runs without an
//! explicit user decision.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::compaction::{CompactionEngine, CompactionState};
use crate::config::Config;
use crate::error::{InvalidSnafu, Result, client_error};
use crate::history;
use crate::permissions::{
    ASK_TIMEOUT, FILE_WRITE_TOOLS, PermissionAnswer, PermissionCheck, PermissionManager,
    PermissionsConfig, ToolKey,
};
use crate::providers::{CatalogModel, Provider as ClientProvider, ProviderKind};
use crate::run::{self, BeforeExecute, BoxFuture, CancelToken, Decision, RunOutcome};
use crate::tools::Workspace;

use super::cards::{self, Files};
use super::{AgentEvent, Command, ModelChoice, Provider, Status, ToolCallData};

/// Render an error and its sources as one client-facing message.
fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

/// Session shared between the command loop and the (single) running turn.
#[derive(Default)]
struct SessionState {
    history: Vec<history::Message>,
    compaction: CompactionState,
    /// Session-wide tool dedup cache, shared by the dispatcher and cleared
    /// by the compaction engine.
    dedup: crate::run::SharedDedupCache,
    /// Session-wide guardrail counters, shared by the dispatcher and reset
    /// by the compaction engine.
    guardrails: crate::run::SharedGuardrails,
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pending_approval: Option<(String, oneshot::Sender<bool>)>,
}

impl SessionState {
    /// Link the compaction state to this session's dedup cache so a
    /// compaction run clears it.
    fn linked() -> Self {
        let dedup = crate::run::shared_cache();
        let guardrails = crate::run::shared_guardrails();
        Self {
            compaction: CompactionState::default()
                .with_dedup(dedup.clone())
                .with_guardrails(guardrails.clone()),
            dedup,
            guardrails,
            ..Self::default()
        }
    }
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
    /// Permission rule engine: persistent `permissions.toml` rules, session
    /// grants, and per-tool defaults; consulted by the approval gate.
    permissions: Arc<PermissionManager>,
    /// Per-provider discovered models, config key order.
    catalogs: BTreeMap<String, Vec<CatalogModel>>,
    /// Discovery problems surfaced to the user as notes at startup.
    notes: Vec<String>,
    selection: Selection,
    cwd_label: String,
    branch: String,
}

impl CraftProvider {
    /// Validate the whole session up front, before the terminal UI starts:
    /// config, workspace, and at least one usable model catalog.
    pub async fn new(config: Config, cwd: impl AsRef<Path>) -> Result<Self> {
        if config.providers.is_empty() {
            return InvalidSnafu {
                reason: "no providers are configured in ~/.config/craft/agent.toml",
            }
            .fail();
        }
        let cwd = cwd.as_ref();
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
        let workspace = Workspace::new(cwd)
            .map_err(client_error)?
            .with_loaded_instructions(instructions.loaded.clone());

        let mut catalogs: BTreeMap<String, Vec<CatalogModel>> = BTreeMap::new();
        let mut notes = Vec::new();
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
                reason: format!("no usable provider/model catalog ({})", notes.join("; ")),
            }
            .fail();
        }

        let (provider, models) = catalogs.iter().next().expect("catalogs is non-empty");
        let first = models.first().expect("each catalog is non-empty");
        let selection = Selection {
            provider: provider.clone(),
            model: first.id.clone(),
            context_length: first.context_length,
        };
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
        })
    }
}

impl Provider for CraftProvider {
    fn start(
        self,
    ) -> (
        mpsc::UnboundedSender<Command>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
        let (evt_tx, evt_rx) = mpsc::unbounded_channel::<AgentEvent>();

        tokio::spawn(async move {
            let state = Arc::new(Mutex::new(SessionState::linked()));
            let files: Files = Files::default();
            let (cancel_flag, mut cancel_token) = run::cancel_channel();
            let mut current_turn: Option<AbortHandle> = None;
            let mut selection = self.selection;
            let workspace = self.workspace;
            let permissions = self.permissions;
            let snapshots = workspace.snapshots().clone();
            let instructions_text = self.instructions.text;
            let config = self.config;
            let catalogs = self.catalogs;

            // Flat model menu rows across all usable providers, with the
            // current selection's index.
            let catalog_choices = |selection: &Selection| -> (Vec<ModelChoice>, usize) {
                let mut choices = Vec::new();
                let mut current = 0;
                for (provider, models) in &catalogs {
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
            };

            let _ = evt_tx.send(AgentEvent::SessionInfo {
                cwd: self.cwd_label,
                branch: self.branch,
            });
            for note in self.notes {
                let _ = evt_tx.send(AgentEvent::AssistantText(note));
            }
            let (models, current) = catalog_choices(&selection);
            let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
            let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
            let _ = evt_tx.send(AgentEvent::TokenUsage("0 (0%)".into()));

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    Command::SendMessage(text) => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        cancel_token = cancel_flag.token();
                        let handle = tokio::spawn(run_turn(
                            TurnCtx {
                                config: config.clone(),
                                workspace: workspace.clone(),
                                instructions_text: instructions_text.clone(),
                                selection: selection.clone(),
                                state: state.clone(),
                                files: files.clone(),
                                cancel: cancel_token.clone(),
                                tx: evt_tx.clone(),
                                permissions: permissions.clone(),
                            },
                            text,
                        ));
                        current_turn = Some(handle.abort_handle());
                    }
                    Command::Approve(id) => decide(&state, id, true).await,
                    Command::Reject(id) => decide(&state, id, false).await,
                    Command::Interrupt => {
                        cancel_flag.set(true);
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        let _ = evt_tx.send(AgentEvent::AssistantEnd);
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::Clear => {
                        cancel_flag.set(true);
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        // Reset through `linked` so the fresh session's
                        // compaction state keeps working dedup/guardrails
                        // handles; a bare default would strand the caches the
                        // dispatcher still points at.
                        *state.lock().await = SessionState::linked();
                        let _ = evt_tx.send(AgentEvent::AssistantEnd);
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::Reset => {
                        cancel_flag.set(true);
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        *state.lock().await = SessionState::linked();
                        files.lock().expect("files lock").clear();
                        let _ = evt_tx.send(AgentEvent::AssistantEnd);
                        let _ = evt_tx.send(AgentEvent::FilesSet(Vec::new()));
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                        let _ = evt_tx.send(AgentEvent::TokenUsage("0 (0%)".into()));
                    }
                    Command::Undo => {
                        if current_turn.is_some() {
                            // Restoring mid-run would race the turn's writes
                            // and drain its live capture session.
                            let _ = evt_tx.send(AgentEvent::AssistantText(
                                "A turn is still running; wait for it to finish before undoing."
                                    .into(),
                            ));
                            continue;
                        }
                        let message = snapshots
                            .rollback()
                            .await
                            .unwrap_or_else(|| "Nothing to undo.".into());
                        let _ = evt_tx.send(AgentEvent::AssistantText(message));
                    }
                    Command::SelectModel { provider, model } => {
                        let found = catalogs
                            .get(&provider)
                            .and_then(|models| models.iter().find(|m| m.id == model))
                            .map(|m| m.context_length);
                        match found {
                            Some(context_length) => {
                                selection = Selection {
                                    provider,
                                    model,
                                    context_length,
                                };
                                let (models, current) = catalog_choices(&selection);
                                let _ = evt_tx.send(AgentEvent::CatalogSet { models, current });
                            }
                            None => {
                                let _ = evt_tx.send(AgentEvent::AssistantText(format!(
                                    "Unknown model {model:?} on provider {provider:?}."
                                )));
                            }
                        }
                    }
                }
            }
        });

        (cmd_tx, evt_rx)
    }
}

/// Gates tool calls behind the permission engine and, when it asks, the
/// UI's approve/reject seam: no workspace mutation runs without an explicit
/// user decision. Approval runs the tool and grants the session; rejection
/// skips it and reports the denial back to the model.
struct ApprovalGate {
    state: Arc<Mutex<SessionState>>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancelToken,
    permissions: Arc<PermissionManager>,
}

/// The scope a call is about: the touched path for file tools, per-command
/// scopes for bash (task B.2), `*` otherwise (scope-less rules still match
/// it; everything else falls to the default). The flag is the bash parser's
/// `force_prompt`: the scopes could not be derived confidently, so allow
/// rules must not silence the prompt.
fn scope_for_call(root: &Path, name: &str, args: &serde_json::Value) -> (Vec<String>, bool) {
    if name == "bash"
        && let Some(command) = args.get("command").and_then(|v| v.as_str())
        && let Some(scopes) = crate::permissions::bash::permission_scopes(command)
    {
        return (scopes.scopes, scopes.force_prompt);
    }
    if name == "apply_patch"
        && let Some(patch) = args.get("patch_text").and_then(|v| v.as_str())
    {
        return (
            crate::tools::patch_paths(patch)
                .iter()
                .map(|p| resolve_scope_path(root, p))
                .collect(),
            false,
        );
    }
    if FILE_WRITE_TOOLS.contains(&name) {
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            return (vec![resolve_scope_path(root, path)], false);
        }
        if let Some(files) = args.get("files").and_then(|v| v.as_array()) {
            return (
                files
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(|p| resolve_scope_path(root, p))
                    .collect(),
                false,
            );
        }
    }
    (vec!["*".to_string()], false)
}

fn resolve_scope_path(root: &Path, path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        root.join(p).display().to_string()
    }
}

fn denied_message(tool: &ToolKey, scopes: &[String]) -> String {
    format!(
        "{} `{}` ({}). {}",
        crate::permissions::PERMISSION_DENIED_PREFIX,
        tool,
        scopes.join("; "),
        crate::permissions::DEFAULT_DENY_GUIDANCE
    )
}

impl BeforeExecute for ApprovalGate {
    fn decide(&self, call: history::ToolCall) -> BoxFuture<Decision> {
        let state = self.state.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        let permissions = self.permissions.clone();
        Box::pin(async move {
            if cancel.cancelled() {
                return Decision::Stop("cancelled by client".into());
            }
            let name = call.function.name.as_str();
            let tool = ToolKey::native(name);
            let (scopes, force_prompt) =
                scope_for_call(permissions.cwd(), name, &call.function.arguments);
            match permissions.check_multi(&tool, &scopes, force_prompt) {
                PermissionCheck::Allowed => return Decision::Run,
                PermissionCheck::Denied => {
                    return Decision::Skip(denied_message(&tool, &scopes));
                }
                PermissionCheck::NeedsPrompt { .. } => {}
            }
            let id = call.id.clone();
            let _ = tx.send(AgentEvent::AssistantEnd);
            let _ = tx.send(AgentEvent::ToolCall(ToolCallData {
                id: id.clone(),
                kind: cards::tool_head(&call.function.name, &call.function.arguments),
                lines: Vec::new(),
                awaiting_approval: true,
            }));
            let _ = tx.send(AgentEvent::StatusChanged(Status::WaitingApproval));

            let (decision_tx, mut decision_rx) = oneshot::channel();
            state.lock().await.pending_approval = Some((id, decision_tx));
            let mut cancel_rx = cancel.subscribe();
            // Cancellation is epoch-based: `changed()` fires only on a
            // `set(true)` generation bump or a dropped flag — a re-arm
            // never writes, so a ready change is always a real cancel.
            let approved = tokio::select! {
                biased;
                changed = cancel_rx.changed() => {
                    let _ = changed;
                    state.lock().await.pending_approval = None;
                    return Decision::Stop("cancelled by client".into());
                }
                decision = tokio::time::timeout(ASK_TIMEOUT, &mut decision_rx) => {
                    state.lock().await.pending_approval = None;
                    decision.unwrap_or(Ok(false)).unwrap_or(false)
                }
            };
            let _ = tx.send(AgentEvent::StatusChanged(Status::Running));
            if approved {
                permissions.apply_decision(&tool, &scopes, &PermissionAnswer::AllowSession);
                Decision::Run
            } else {
                Decision::Skip(denied_message(&tool, &scopes))
            }
        })
    }
}

/// Deliver the user's decision to a tool call waiting on it, if the id
/// matches the currently pending one.
async fn decide(state: &Arc<Mutex<SessionState>>, id: String, approved: bool) {
    let mut session = state.lock().await;
    if matches!(&session.pending_approval, Some((pid, _)) if *pid == id)
        && let Some((_, decision)) = session.pending_approval.take()
    {
        let _ = decision.send(approved);
    }
}

/// Everything one agent turn needs, bundled so the turn's helpers avoid a
/// long parameter list.
struct TurnCtx {
    config: Arc<Config>,
    workspace: Workspace,
    instructions_text: String,
    selection: Selection,
    state: Arc<Mutex<SessionState>>,
    files: Files,
    cancel: CancelToken,
    tx: mpsc::UnboundedSender<AgentEvent>,
    permissions: Arc<PermissionManager>,
}

/// Maps run-loop events to TUI events for one model call. Owns the
/// streaming-progress flags the outcome rendering consults afterwards.
struct TurnRenderer {
    tx: mpsc::UnboundedSender<AgentEvent>,
    files: Files,
    context_length: Option<u32>,
    streamed_text: Arc<AtomicBool>,
    streamed_reasoning: Arc<AtomicBool>,
    tools_started: Arc<AtomicBool>,
}

impl TurnRenderer {
    fn new(
        tx: mpsc::UnboundedSender<AgentEvent>,
        files: Files,
        context_length: Option<u32>,
    ) -> Self {
        Self {
            tx,
            files,
            context_length,
            streamed_text: Arc::new(AtomicBool::new(false)),
            streamed_reasoning: Arc::new(AtomicBool::new(false)),
            tools_started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn map(&self, event: run::Event) {
        match event {
            run::Event::TextDelta(delta) => {
                self.streamed_text.store(true, Ordering::Relaxed);
                let _ = self.tx.send(AgentEvent::AssistantDelta(delta));
            }
            run::Event::ReasoningDelta(delta) => {
                self.streamed_reasoning.store(true, Ordering::Relaxed);
                let _ = self.tx.send(AgentEvent::ReasoningDelta(delta));
            }
            run::Event::ToolStart {
                id,
                name,
                arguments,
            } => {
                if !self.tools_started.swap(true, Ordering::Relaxed) {
                    let _ = self.tx.send(AgentEvent::StatusChanged(Status::Running));
                }
                let _ = self.tx.send(AgentEvent::ToolCall(ToolCallData {
                    id,
                    kind: cards::tool_head(&name, &arguments),
                    lines: Vec::new(),
                    awaiting_approval: false,
                }));
            }
            run::Event::ToolDone {
                id,
                name,
                arguments,
                result,
            } => {
                let done = cards::tool_done(id, &name, &arguments, &result);
                if let Some((path, status)) = done.touched {
                    self.files.lock().expect("files lock").insert(path, status);
                    let _ = self
                        .tx
                        .send(AgentEvent::FilesSet(cards::touched_files(&self.files)));
                }
                let _ = self.tx.send(AgentEvent::ToolCall(done.card));
            }
            run::Event::Usage(usage) => {
                let _ = self.tx.send(AgentEvent::TokenUsage(cards::usage_label(
                    usage.input_tokens + usage.output_tokens,
                    usage.input_tokens,
                    self.context_length,
                )));
            }
            // The nudge is visible in the next model call; nothing to show.
            run::Event::Nudge => {}
        }
    }

    /// Whether text or reasoning already streamed this call (the reply was
    /// shown live, so it must not be emitted again).
    fn streamed(&self) -> bool {
        self.streamed_text.load(Ordering::Relaxed)
            || self.streamed_reasoning.load(Ordering::Relaxed)
    }
}

/// Emit the model's reply unless streaming already showed it live.
fn emit_reply(tx: &mpsc::UnboundedSender<AgentEvent>, streamed: bool, reply: &str) {
    if streamed {
        let _ = tx.send(AgentEvent::AssistantEnd);
    } else if !reply.is_empty() {
        let _ = tx.send(AgentEvent::AssistantText(reply.to_owned()));
    }
}

/// One agent turn: drives the shared run loop, rendering to the TUI seam.
async fn run_turn(ctx: TurnCtx, text: String) {
    let TurnCtx {
        config,
        workspace,
        instructions_text,
        selection,
        state,
        files,
        cancel,
        tx,
        permissions,
    } = ctx;
    macro_rules! fail {
        ($message:expr) => {{
            let _ = tx.send(AgentEvent::AssistantText($message));
            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
            return;
        }};
    }

    state.lock().await.pending_approval = None;

    let Some(provider_config) = config.providers.get(&selection.provider) else {
        fail!(format!("unknown provider {:?}", selection.provider));
    };
    let provider = match ClientProvider::from_config(provider_config) {
        Ok(provider) => provider,
        Err(error) => fail!(report(error)),
    };
    let model = match provider.completion_model(&selection.model) {
        Ok(model) => model,
        Err(error) => fail!(report(error)),
    };

    let mut history = state.lock().await.history.clone();
    let dedup = state.lock().await.dedup.clone();
    let guardrails = state.lock().await.guardrails.clone();

    // Run configured compaction stages whose context-fill threshold is crossed
    // before the history is sent to the model; commit effectiveness state only.
    {
        let mut compaction = state.lock().await.compaction.clone();
        CompactionEngine::new(config.compaction.clone())
            .with_buffer(config.compaction_buffer)
            .maybe_compact(
                &mut compaction,
                &model,
                &mut history,
                selection.context_length,
            )
            .await;
        state.lock().await.compaction = compaction;
    }

    let tools = workspace
        .register()
        .with_dedup(dedup)
        .with_guardrails(guardrails)
        .with_before(Arc::new(ApprovalGate {
            state: state.clone(),
            tx: tx.clone(),
            cancel: cancel.clone(),
            permissions: permissions.clone(),
        }));
    let params = run::RunParams {
        preamble: Some(crate::prompt::build_system_prompt(
            &crate::prompt::Vars::new()
                .set("{cwd}", workspace.root().display().to_string())
                .set("{platform}", std::env::consts::OS)
                .set("{date}", crate::prompt::today_utc()),
            &format!("{}{}", config.agent.preamble, instructions_text),
            &crate::prompt::ResolvedSlots::default(),
        )),
        temperature: config.agent.temperature,
        max_tokens: config.agent.max_tokens,
        max_turns: run::RunParams::UNBOUNDED,
        recency: None,
        compression: config.compression.clone(),
        max_continuation_turns: run::RunParams::DEFAULT_MAX_CONTINUATION_TURNS,
    };

    let _ = tx.send(AgentEvent::StatusChanged(Status::Thinking));
    // A turn that ends with only reasoning (no reply, no tool calls) still
    // carries real work; nudge the model to continue instead of stopping.
    const MAX_EMPTY_CONTINUATIONS: usize = 2;
    const CONTINUE_AFTER_EMPTY: &str = "Your last turn produced no visible reply and no tool \
         calls. Continue the task with your reply or the next tool call.";
    let mut prompt = text;
    let mut continuations = MAX_EMPTY_CONTINUATIONS;
    loop {
        let renderer = TurnRenderer::new(tx.clone(), files.clone(), selection.context_length);
        let outcome = run::run(
            &model,
            &params,
            &tools,
            &mut history,
            &prompt,
            &cancel,
            &|event| renderer.map(event),
        )
        .await;
        match outcome {
            RunOutcome::Cancelled => {
                let _ = tx.send(AgentEvent::AssistantEnd);
                let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
                return;
            }
            RunOutcome::Failed(message) => {
                if renderer.streamed() {
                    let _ = tx.send(AgentEvent::AssistantEnd);
                }
                let _ = tx.send(AgentEvent::AssistantText(message));
                let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
                return;
            }
            RunOutcome::MaxTurns => {
                // Keep the partial run: the follow-up prompt continues from
                // where the budget ran out instead of silently losing it.
                state.lock().await.history = history.clone();
                let _ = tx.send(AgentEvent::AssistantText(
                    "Reached the turn limit. Send another message to continue.".into(),
                ));
                break;
            }
            RunOutcome::MaxTokens { reply } => {
                state.lock().await.history = history.clone();
                emit_reply(&tx, renderer.streamed(), &reply);
                let _ = tx.send(AgentEvent::AssistantText(
                    "The reply hit the output-token limit. Send another message to continue."
                        .into(),
                ));
                break;
            }
            RunOutcome::Done { reply } => {
                if reply.is_empty() && continuations > 0 {
                    continuations -= 1;
                    prompt = CONTINUE_AFTER_EMPTY.into();
                    continue;
                }
                if renderer.streamed() || !reply.is_empty() {
                    emit_reply(&tx, renderer.streamed(), &reply);
                } else {
                    let _ = tx.send(AgentEvent::AssistantText(
                        "The model returned an empty response. Send another message to continue."
                            .into(),
                    ));
                }
                break;
            }
        }
    }
    state.lock().await.history = history;
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_call(id: &str, name: &str) -> history::ToolCall {
        history::ToolCall {
            id: id.into(),
            function: history::ToolFunction {
                name: name.into(),
                arguments: serde_json::json!({}),
            },
        }
    }

    fn gate(state: &Arc<Mutex<SessionState>>) -> (ApprovalGate, run::CancelFlag) {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (flag, cancel) = run::cancel_channel();
        (
            ApprovalGate {
                state: state.clone(),
                tx,
                cancel,
                permissions: Arc::new(PermissionManager::new(
                    PermissionsConfig::default(),
                    std::env::temp_dir(),
                )),
            },
            flag,
        )
    }

    #[test]
    fn apply_patch_scope_covers_every_patched_file() {
        let root = std::path::Path::new("/repo");
        let (scopes, force) = scope_for_call(
            root,
            "apply_patch",
            &serde_json::json!({"patch_text": "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** Delete File: sub/b.rs\n*** End Patch"}),
        );
        assert!(!force);
        assert_eq!(
            scopes,
            vec!["/repo/a.rs".to_string(), "/repo/sub/b.rs".to_string()]
        );
    }

    #[tokio::test]
    async fn read_only_tools_run_without_approval() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        for name in crate::permissions::READ_ONLY_TOOLS {
            let (gate, _flag) = gate(&state);
            let decision = gate.decide(tool_call("t1", name)).await;
            assert!(matches!(decision, Decision::Run), "{name}");
        }
        assert!(state.lock().await.pending_approval.is_none());
    }

    #[tokio::test]
    async fn unknown_and_mutating_tools_require_approval() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        for name in ["write", "delete", "some_future_bash_tool"] {
            let (gate, _flag) = gate(&state);
            let call = tool_call("t1", name);
            let pending = tokio::spawn(async move { gate.decide(call).await });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while state.lock().await.pending_approval.is_none() {
                assert!(std::time::Instant::now() < deadline, "{name} never parked");
                tokio::task::yield_now().await;
            }
            decide(&state, "t1".into(), false).await;
            let decision = pending.await.unwrap();
            assert!(matches!(decision, Decision::Skip(_)), "{name}");
        }
    }

    #[tokio::test]
    async fn decide_ignores_stale_ids_and_delivers_current_one() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, mut rx) = oneshot::channel();
        state.lock().await.pending_approval = Some(("current".into(), tx));

        decide(&state, "stale".into(), true).await;
        assert!(matches!(&state.lock().await.pending_approval, Some((id, _)) if id == "current"));
        assert!(
            rx.try_recv().is_err(),
            "stale id must not consume a decision"
        );

        decide(&state, "current".into(), false).await;
        assert!(state.lock().await.pending_approval.is_none());
        assert!(!rx.await.unwrap());
    }

    #[tokio::test]
    async fn approval_grants_the_session_scope() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = run::cancel_channel();
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::env::temp_dir(),
        ));
        let gate = ApprovalGate {
            state: state.clone(),
            tx,
            cancel,
            permissions: permissions.clone(),
        };

        let mut call = tool_call("t1", "write");
        call.function.arguments = serde_json::json!({ "path": "src/lib.rs" });
        let pending = tokio::spawn(async move { gate.decide(call).await });
        while state.lock().await.pending_approval.is_none() {
            tokio::task::yield_now().await;
        }
        decide(&state, "t1".into(), true).await;
        assert!(matches!(pending.await.unwrap(), Decision::Run));

        // The grant generalized to the parent dir and lives in the session:
        // a sibling write now runs without asking again.
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_flag, cancel) = run::cancel_channel();
        let gate = ApprovalGate {
            state: state.clone(),
            tx,
            cancel,
            permissions,
        };
        let mut sibling = tool_call("t2", "write");
        sibling.function.arguments = serde_json::json!({ "path": "src/other.rs" });
        assert!(matches!(gate.decide(sibling).await, Decision::Run));
        assert!(state.lock().await.pending_approval.is_none());
    }

    #[tokio::test]
    async fn flag_rearm_while_pending_does_not_cancel_the_approval() {
        // A re-arm (`set(false)`) must not disturb a pending approval;
        // with epoch-based cancellation it never writes, so a ready
        // `changed()` is always a genuine cancel.
        let state = Arc::new(Mutex::new(SessionState::default()));
        let (tx, _rx) = mpsc::unbounded_channel();
        let (flag, cancel) = run::cancel_channel();
        let permissions = Arc::new(PermissionManager::new(
            PermissionsConfig::default(),
            std::env::temp_dir(),
        ));
        let gate = ApprovalGate {
            state: state.clone(),
            tx,
            cancel,
            permissions,
        };

        let mut call = tool_call("t1", "bash");
        call.function.arguments = serde_json::json!({ "command": "echo hi" });
        let pending = tokio::spawn(async move { gate.decide(call).await });
        while state.lock().await.pending_approval.is_none() {
            tokio::task::yield_now().await;
        }
        // Version bump with the same (false) value, delivered after the
        // gate subscribed but before the decision.
        flag.set(false);
        tokio::task::yield_now().await;
        decide(&state, "t1".into(), true).await;
        assert!(matches!(pending.await.unwrap(), Decision::Run));
    }
}
