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
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::compaction::{CompactionEngine, CompactionState};
use crate::config::Config;
use crate::error::{InvalidSnafu, Result, client_error};
use crate::history;
use crate::providers::{CatalogModel, Provider as ClientProvider, ProviderKind};
use crate::run::{self, BeforeExecute, BoxFuture, CancelToken, Decision, RunOutcome};
use crate::tools::Workspace;

use super::{
    AgentEvent, Command, LineKind, ModelChoice, Provider, Status, Tone, ToolCallData, ToolKind,
    ToolLine, TouchedFile,
};

/// Mutating workspace tools; their results feed the Files panel.
const EDIT_TOOLS: [&str; 5] = ["edit", "edit_lines", "insert_lines", "write", "delete"];

/// Read-only tools that run without an explicit user decision. Approval is
/// default-deny: any tool not listed here — including tools registered after
/// this gate was written — waits for the user's approve/reject decision.
const READ_TOOLS: [&str; 2] = ["read", "grep"];

/// Render an error and its sources as one client-facing message.
fn report(error: crate::error::Error) -> String {
    snafu::Report::from_error(error).to_string()
}

/// What an edit-family tool did to a file, for the sidebar badge.
#[derive(Clone, Copy)]
enum FileStatus {
    Modified,
    Created,
    Deleted,
}

impl FileStatus {
    fn label(self) -> &'static str {
        match self {
            FileStatus::Modified => "modified",
            FileStatus::Created => "created",
            FileStatus::Deleted => "deleted",
        }
    }

    fn tone(self) -> Tone {
        match self {
            FileStatus::Modified => Tone::Warning,
            FileStatus::Created => Tone::Success,
            FileStatus::Deleted => Tone::Danger,
        }
    }
}

/// Files touched by edit-family tools, keyed by path. A plain mutex: it is
/// only locked briefly from the run's synchronous event callback.
type Files = Arc<StdMutex<BTreeMap<String, FileStatus>>>;

/// Session shared between the command loop and the (single) running turn.
#[derive(Default)]
struct SessionState {
    history: Vec<history::Message>,
    compaction: CompactionState,
    /// Session-wide tool dedup cache, shared by the dispatcher and cleared
    /// by the compaction engine.
    dedup: crate::run::SharedDedupCache,
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pending_approval: Option<(String, oneshot::Sender<bool>)>,
}

impl SessionState {
    /// Link the compaction state to this session's dedup cache so a
    /// compaction run clears it.
    fn linked() -> Self {
        let dedup = crate::run::shared_cache();
        Self {
            compaction: CompactionState::default().with_dedup(dedup.clone()),
            dedup,
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
            catalogs,
            notes,
            selection,
            cwd_label: display_path(cwd),
            branch: git_branch(cwd).await,
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
            let files: Files = Arc::new(StdMutex::new(BTreeMap::new()));
            let (cancel_flag, cancel_token) = run::cancel_channel();
            let mut current_turn: Option<AbortHandle> = None;
            let mut selection = self.selection;
            let workspace = self.workspace;
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
                        cancel_flag.set(false);
                        let handle = tokio::spawn(run_turn(
                            config.clone(),
                            workspace.clone(),
                            instructions_text.clone(),
                            selection.clone(),
                            text,
                            state.clone(),
                            files.clone(),
                            cancel_token.clone(),
                            evt_tx.clone(),
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
                        let mut session = state.lock().await;
                        session.pending_approval = None;
                        session.history.clear();
                        session.compaction = CompactionState::default();
                        let _ = evt_tx.send(AgentEvent::AssistantEnd);
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::Reset => {
                        cancel_flag.set(true);
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        *state.lock().await = SessionState::default();
                        files.lock().expect("files lock").clear();
                        let _ = evt_tx.send(AgentEvent::AssistantEnd);
                        let _ = evt_tx.send(AgentEvent::FilesSet(Vec::new()));
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                        let _ = evt_tx.send(AgentEvent::TokenUsage("0 (0%)".into()));
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

/// Gates edit-family tool calls behind the UI's approve/reject seam: no
/// workspace mutation runs without an explicit user decision. Approval runs
/// the tool; rejection skips it and reports the decision back to the model.
struct ApprovalGate {
    state: Arc<Mutex<SessionState>>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: CancelToken,
}

impl BeforeExecute for ApprovalGate {
    fn decide(&self, call: history::ToolCall) -> BoxFuture<Decision> {
        let state = self.state.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        Box::pin(async move {
            if READ_TOOLS.contains(&call.function.name.as_str()) {
                return Decision::Run;
            }
            if cancel.cancelled() {
                return Decision::Stop("cancelled by client".into());
            }
            let id = call.id.clone();
            let _ = tx.send(AgentEvent::AssistantEnd);
            let _ = tx.send(AgentEvent::ToolCall(ToolCallData {
                id: id.clone(),
                kind: tool_head(&call.function.name, &call.function.arguments),
                lines: Vec::new(),
                awaiting_approval: true,
            }));
            let _ = tx.send(AgentEvent::StatusChanged(Status::WaitingApproval));

            let (decision_tx, decision_rx) = oneshot::channel();
            state.lock().await.pending_approval = Some((id, decision_tx));
            let mut cancel_rx = cancel.subscribe();
            let approved = tokio::select! {
                biased;
                changed = cancel_rx.changed() => {
                    if matches!(changed, Ok(())) && *cancel_rx.borrow_and_update() {
                        state.lock().await.pending_approval = None;
                        return Decision::Stop("cancelled by client".into());
                    }
                    // A dropped flag means the session is gone: stop the call.
                    return Decision::Stop("cancelled by client".into());
                }
                decision = decision_rx => decision.unwrap_or(false),
            };
            let _ = tx.send(AgentEvent::StatusChanged(Status::Running));
            if approved {
                Decision::Run
            } else {
                Decision::Skip("the user rejected this change; it was not applied".into())
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

/// One agent turn: drives the shared run loop, rendering to the TUI seam.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    config: Arc<Config>,
    workspace: Workspace,
    instructions_text: String,
    selection: Selection,
    text: String,
    state: Arc<Mutex<SessionState>>,
    files: Files,
    cancel: CancelToken,
    tx: mpsc::UnboundedSender<AgentEvent>,
) {
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
        .with_before(Arc::new(ApprovalGate {
            state: state.clone(),
            tx: tx.clone(),
            cancel: cancel.clone(),
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
        let streamed_text = Arc::new(AtomicBool::new(false));
        let streamed_reasoning = Arc::new(AtomicBool::new(false));
        let tools_started = Arc::new(AtomicBool::new(false));
        let emit = {
            let tx = tx.clone();
            let files = files.clone();
            let context_length = selection.context_length;
            let streamed_text = streamed_text.clone();
            let streamed_reasoning = streamed_reasoning.clone();
            let tools_started = tools_started.clone();
            move |event: run::Event| match event {
                run::Event::TextDelta(delta) => {
                    streamed_text.store(true, Ordering::Relaxed);
                    let _ = tx.send(AgentEvent::AssistantDelta(delta));
                }
                run::Event::ReasoningDelta(delta) => {
                    streamed_reasoning.store(true, Ordering::Relaxed);
                    let _ = tx.send(AgentEvent::ReasoningDelta(delta));
                }
                run::Event::ToolStart {
                    id,
                    name,
                    arguments,
                } => {
                    if !tools_started.swap(true, Ordering::Relaxed) {
                        let _ = tx.send(AgentEvent::StatusChanged(Status::Running));
                    }
                    let _ = tx.send(AgentEvent::ToolCall(ToolCallData {
                        id,
                        kind: tool_head(&name, &arguments),
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
                    let done = tool_done(id, &name, &arguments, &result);
                    if let Some((path, status)) = done.touched {
                        let panel = {
                            let mut guard = files.lock().expect("files lock");
                            guard.insert(path, status);
                            guard
                                .iter()
                                .map(|(path, s)| TouchedFile {
                                    path: path.clone(),
                                    status: s.label().to_string(),
                                    tone: s.tone(),
                                })
                                .collect::<Vec<_>>()
                        };
                        let _ = tx.send(AgentEvent::FilesSet(panel));
                    }
                    let _ = tx.send(AgentEvent::ToolCall(done.card));
                }
                run::Event::Usage(usage) => {
                    let _ = tx.send(AgentEvent::TokenUsage(usage_label(
                        usage.input_tokens + usage.output_tokens,
                        usage.input_tokens,
                        context_length,
                    )));
                }
                // The nudge is visible in the next model call; nothing to show.
                run::Event::Nudge => {}
            }
        };
        let outcome = run::run(
            &model,
            &params,
            &tools,
            &mut history,
            &prompt,
            &cancel,
            &emit,
        )
        .await;
        match outcome {
            RunOutcome::Cancelled => {
                let _ = tx.send(AgentEvent::AssistantEnd);
                let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
                return;
            }
            RunOutcome::Failed(message) => {
                if streamed_text.load(Ordering::Relaxed)
                    || streamed_reasoning.load(Ordering::Relaxed)
                {
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
            RunOutcome::Done { reply } => {
                if reply.is_empty() && continuations > 0 {
                    continuations -= 1;
                    prompt = CONTINUE_AFTER_EMPTY.into();
                    continue;
                }
                if streamed_text.load(Ordering::Relaxed)
                    || streamed_reasoning.load(Ordering::Relaxed)
                {
                    let _ = tx.send(AgentEvent::AssistantEnd);
                } else if !reply.is_empty() {
                    let _ = tx.send(AgentEvent::AssistantText(reply.clone()));
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

/// Card emitted when a tool call starts: kind from the tool name and its
/// first string argument; the body stays empty until the result arrives.
fn tool_head(name: &str, arguments: &serde_json::Value) -> ToolKind {
    let detail = first_string_argument(arguments).unwrap_or_default();
    match name {
        "read" => ToolKind::Read {
            path: detail,
            summary: String::new(),
        },
        "grep" => ToolKind::Grep {
            pattern: detail,
            summary: String::new(),
        },
        name if EDIT_TOOLS.contains(&name) => ToolKind::Edit { path: detail },
        other => ToolKind::Bash {
            cmd: format!("{other} {detail}").trim().to_string(),
        },
    }
}

/// The state produced by a completed tool call.
struct ToolDone {
    card: ToolCallData,
    /// (path, status) when an edit-family tool succeeded.
    touched: Option<(String, FileStatus)>,
}

/// Card emitted when a tool result arrives: same id as the start card, with
/// the body and summary filled from the result text.
fn tool_done(
    id: String,
    name: &str,
    arguments: &serde_json::Value,
    result: &history::ToolResult,
) -> ToolDone {
    let text = result
        .content
        .iter()
        .map(history::ToolResultContent::to_text)
        .collect::<Vec<_>>()
        .join("\n");
    let detail = first_string_argument(arguments).unwrap_or_default();

    let (kind, lines, touched) = match name {
        "read" => {
            let lines = context_lines(&text);
            let summary = format!("{} lines", lines.len());
            (
                ToolKind::Read {
                    path: detail,
                    summary,
                },
                lines,
                None,
            )
        }
        "grep" => {
            let lines = context_lines(&text);
            let summary = format!("{} lines of output", lines.len());
            (
                ToolKind::Grep {
                    pattern: detail,
                    summary,
                },
                lines,
                None,
            )
        }
        tool if EDIT_TOOLS.contains(&tool) => {
            let status = match tool {
                "delete" => FileStatus::Deleted,
                "write" => FileStatus::Created,
                _ => FileStatus::Modified,
            };
            (
                ToolKind::Edit {
                    path: detail.clone(),
                },
                diff_lines(&text),
                if detail.is_empty() || result.is_error {
                    None
                } else {
                    Some((detail, status))
                },
            )
        }
        other => (
            ToolKind::Bash {
                cmd: format!("{other} {detail}").trim().to_string(),
            },
            context_lines(&text),
            None,
        ),
    };
    ToolDone {
        card: ToolCallData {
            id,
            kind,
            lines,
            awaiting_approval: false,
        },
        touched,
    }
}

fn context_lines(text: &str) -> Vec<ToolLine> {
    text.lines()
        .map(|line| ToolLine {
            kind: LineKind::Context,
            text: line.to_string(),
        })
        .collect()
}

/// Split a diff-formatted tool result into Add/Del lines; anything else is
/// context.
fn diff_lines(text: &str) -> Vec<ToolLine> {
    text.lines()
        .map(|line| ToolLine {
            kind: match line.as_bytes().first() {
                Some(b'+') => LineKind::Add,
                Some(b'-') => LineKind::Del,
                _ => LineKind::Context,
            },
            text: line.to_string(),
        })
        .collect()
}

fn first_string_argument(arguments: &serde_json::Value) -> Option<String> {
    let (_, value) = arguments
        .as_object()?
        .iter()
        .find(|(_, value)| value.is_string())?;
    value.as_str().map(str::to_owned)
}

/// "45.9K (4%)"-style label; the percentage is the prompt's share of the
/// context window, omitted when the context length is unknown.
fn usage_label(tokens: u64, prompt_tokens: u64, context_length: Option<u32>) -> String {
    let k = tokens as f64 / 1000.0;
    match context_length {
        Some(size) if size > 0 => {
            let pct = (prompt_tokens as f64 / f64::from(size) * 100.0).round();
            format!("{k:.1}K ({pct:.0}%)")
        }
        _ => format!("{k:.1}K"),
    }
}

/// "~"-shortened display path for the sidebar.
fn display_path(path: &Path) -> String {
    let display = path.display().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.display().to_string();
        if let Some(rest) = display.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    display
}

/// Current git branch for the sidebar, if the workspace is a repository.
/// Runs on the blocking pool so a slow git invocation (e.g. an NFS-mounted
/// repository) cannot stall the async runtime thread.
async fn git_branch(cwd: &Path) -> String {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|branch| !branch.is_empty())
            .unwrap_or_else(|| "no branch".into())
    })
    .await
    .unwrap_or_else(|_| "no branch".into())
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
            },
            flag,
        )
    }

    #[tokio::test]
    async fn read_only_tools_run_without_approval() {
        let state = Arc::new(Mutex::new(SessionState::default()));
        for name in ["read", "grep"] {
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
}
