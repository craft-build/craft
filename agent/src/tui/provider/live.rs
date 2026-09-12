//! CraftProvider: the TUI's real backend, driving the same provider/config/
//! agent loop the ACP server uses.
//!
//! Turn semantics mirror `acp::run_turn`: a per-turn provider rebuild, the
//! configured compaction stages ahead of the model call, watch-flag
//! cancellation through `agent::CancelHook`, and history committed only on a
//! successful run. Approvals are a documented no-op: the workspace tools apply
//! edits inline, so the UI's approve/reject affordance only appears when a
//! backend explicitly stages a diff (`awaiting_approval`).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use rig::agent::hook::{HookContext, ToolCall as ToolCallEvent, ToolCallAction};
use rig::agent::{Agent, AgentHook, MultiTurnStreamItem, PromptResponse, StreamingError};
use rig::completion::{Message, PromptError};
use rig::model::Model;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::AbortHandle;

use crate::agent::{CancelHook, merge_history};
use crate::compaction::{CompactionEngine, CompactionState};
use crate::config::Config;
use crate::error::{InvalidSnafu, Result, client_error};
use crate::providers::{Provider as ClientProvider, ProviderKind};
use crate::tools::Workspace;

use super::{
    AgentEvent, Command, LineKind, ModelChoice, Provider, Status, Tone, ToolCallData, ToolKind,
    ToolLine, TouchedFile,
};

/// Mutating workspace tools; their results feed the Files panel.
const EDIT_TOOLS: [&str; 5] = ["edit", "edit_lines", "insert_lines", "write", "delete"];

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

/// Session shared between the command loop and the (single) running turn.
#[derive(Default)]
struct SessionState {
    history: Vec<Message>,
    compaction: CompactionState,
    /// Files touched by edit-family tools, keyed by path.
    files: BTreeMap<String, FileStatus>,
    /// Edit-family call awaiting the user's decision, by tool-call id.
    pending_approval: Option<(String, oneshot::Sender<bool>)>,
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
    /// Per-provider discovered models, config key order.
    catalogs: BTreeMap<String, Vec<Model>>,
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
        let workspace = Workspace::new(cwd).map_err(client_error)?;

        let mut catalogs: BTreeMap<String, Vec<Model>> = BTreeMap::new();
        let mut notes = Vec::new();
        for (name, provider_config) in &config.providers {
            if provider_config.kind == ProviderKind::Voyageai {
                notes.push(format!("{name}: no completion models (non-chat provider)"));
                continue;
            }
            match ClientProvider::from_config(provider_config) {
                Ok(provider) => match provider.models(provider_config).await {
                    Ok(models) => {
                        let models: Vec<Model> = models.into_iter().collect();
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
            catalogs,
            notes,
            selection,
            cwd_label: display_path(cwd),
            branch: git_branch(cwd),
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
            let state = Arc::new(Mutex::new(SessionState::default()));
            let (cancel, _guard) = watch::channel(false);
            let mut current_turn: Option<AbortHandle> = None;
            let mut selection = self.selection;
            let workspace = self.workspace;
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
                            label: model.name.clone().unwrap_or_else(|| model.id.clone()),
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
                        let _ = cancel.send(false);
                        let handle = tokio::spawn(run_turn(
                            config.clone(),
                            workspace.clone(),
                            selection.clone(),
                            text,
                            state.clone(),
                            cancel.subscribe(),
                            evt_tx.clone(),
                        ));
                        current_turn = Some(handle.abort_handle());
                    }
                    Command::Approve(id) => decide(&state, id, true).await,
                    Command::Reject(id) => decide(&state, id, false).await,
                    Command::Interrupt => {
                        let _ = cancel.send(true);
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        let _ = evt_tx.send(AgentEvent::AssistantEnd);
                        let _ = evt_tx.send(AgentEvent::StatusChanged(Status::Done));
                    }
                    Command::Clear => {
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        let mut session = state.lock().await;
                        session.history.clear();
                        session.compaction = CompactionState::default();
                    }
                    Command::Reset => {
                        if let Some(h) = current_turn.take() {
                            h.abort();
                        }
                        *state.lock().await = SessionState::default();
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
struct ApprovalHook {
    state: Arc<Mutex<SessionState>>,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel_rx: watch::Receiver<bool>,
}

impl AgentHook for ApprovalHook {
    async fn on_tool_call(&self, _: &HookContext, call: ToolCallEvent<'_>) -> ToolCallAction {
        if !EDIT_TOOLS.contains(&call.tool_name) {
            return ToolCallAction::Run;
        }
        if *self.cancel_rx.borrow() {
            return ToolCallAction::Stop("cancelled by client".into());
        }
        let id = call.internal_call_id.to_string();
        let arguments = serde_json::from_str(call.args).unwrap_or_default();
        let _ = self.tx.send(AgentEvent::AssistantEnd);
        let _ = self.tx.send(AgentEvent::ToolCall(ToolCallData {
            id: id.clone(),
            kind: tool_head(call.tool_name, &arguments),
            lines: Vec::new(),
            awaiting_approval: true,
        }));
        let _ = self
            .tx
            .send(AgentEvent::StatusChanged(Status::WaitingApproval));

        let (decision_tx, decision_rx) = oneshot::channel();
        self.state.lock().await.pending_approval = Some((id, decision_tx));
        let mut cancel_rx = self.cancel_rx.clone();
        let approved = tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                if matches!(changed, Ok(())) && *cancel_rx.borrow_and_update() {
                    self.state.lock().await.pending_approval = None;
                    return ToolCallAction::Stop("cancelled by client".into());
                }
                false
            }
            decision = decision_rx => decision.unwrap_or(false),
        };
        let _ = self.tx.send(AgentEvent::StatusChanged(Status::Running));
        if approved {
            ToolCallAction::Run
        } else {
            ToolCallAction::Skip("the user rejected this change; it was not applied".into())
        }
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

/// One agent turn: mirrors `acp::run_turn`, rendering to the TUI seam.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    config: Arc<Config>,
    workspace: Workspace,
    selection: Selection,
    text: String,
    state: Arc<Mutex<SessionState>>,
    cancel_rx: watch::Receiver<bool>,
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

    let mut history = state.lock().await.history.clone();

    // Run configured compaction stages whose context-fill threshold is crossed
    // before the history is sent to the model; commit effectiveness state only.
    if let Ok(compaction_model) = provider.completion_model(&selection.model) {
        let mut compaction = state.lock().await.compaction.clone();
        CompactionEngine::new(config.compaction.clone())
            .maybe_compact(
                &mut compaction,
                &compaction_model,
                &mut history,
                selection.context_length,
            )
            .await;
        state.lock().await.compaction = compaction;
    }

    let agent = match crate::agent::build(&provider, &selection.model, &config.agent, &workspace) {
        Ok(agent) => agent,
        Err(error) => fail!(report(error)),
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
        match stream_turn(
            &agent,
            &prompt,
            history.clone(),
            &state,
            &cancel_rx,
            &tx,
            selection.context_length,
        )
        .await
        {
            TurnEnd::Cancelled => {
                let _ = tx.send(AgentEvent::AssistantEnd);
                let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
                return;
            }
            TurnEnd::Ended => return,
            TurnEnd::Final {
                response,
                streamed_text,
                streamed_reasoning,
            } => {
                // Commit each successful segment, exactly like the base loop.
                if let Some(messages) = response.messages() {
                    history = merge_history(history, messages.to_vec());
                    state.lock().await.history = history.clone();
                }
                if response.output.is_empty() && continuations > 0 {
                    continuations -= 1;
                    prompt = CONTINUE_AFTER_EMPTY.into();
                    continue;
                }
                if streamed_text || streamed_reasoning {
                    let _ = tx.send(AgentEvent::AssistantEnd);
                } else if !response.output.is_empty() {
                    let _ = tx.send(AgentEvent::AssistantText(response.output.clone()));
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
    let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
}

/// How one runner stream ended.
enum TurnEnd {
    /// The user cancelled; the turn's history is not committed.
    Cancelled,
    /// The stream errored or ended without a final response; the reason has
    /// already been sent to the UI.
    Ended,
    Final {
        response: PromptResponse,
        streamed_text: bool,
        streamed_reasoning: bool,
    },
}

/// Run one runner stream to completion, forwarding every event to the UI.
async fn stream_turn(
    agent: &Agent,
    prompt: &str,
    history: Vec<Message>,
    state: &Arc<Mutex<SessionState>>,
    cancel_rx: &watch::Receiver<bool>,
    tx: &mpsc::UnboundedSender<AgentEvent>,
    context_length: Option<u32>,
) -> TurnEnd {
    let mut cancel_rx = cancel_rx.clone();
    let mut stream = agent
        .runner(prompt)
        .history(history)
        .add_hook(CancelHook(cancel_rx.clone()))
        .add_hook(ApprovalHook {
            state: state.clone(),
            tx: tx.clone(),
            cancel_rx: cancel_rx.clone(),
        })
        .stream()
        .await;

    let mut streamed_text = false;
    let mut streamed_reasoning = false;
    let mut tools_started = false;
    // internal_call_id -> (tool name, arguments), filled at call start.
    let mut pending_tools: Vec<(String, String, serde_json::Value)> = Vec::new();
    enum LoopEnd {
        Cancel,
        Ended,
        Final(PromptResponse),
    }
    let outcome = loop {
        tokio::select! {
            biased;
            changed = cancel_rx.changed() => {
                if matches!(changed, Ok(())) && *cancel_rx.borrow_and_update() {
                    break LoopEnd::Cancel;
                }
            }
            item = stream.next() => match item {
                None => break LoopEnd::Ended,
                Some(Ok(item)) => match item {
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::Text(delta),
                    ) => {
                        streamed_text = true;
                        let _ = tx.send(AgentEvent::AssistantDelta(delta.text));
                    }
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                    ) => {
                        streamed_reasoning = true;
                        let _ = tx.send(AgentEvent::ReasoningDelta(reasoning));
                    }
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ToolCall { tool_call, internal_call_id },
                    ) => {
                        if !tools_started {
                            tools_started = true;
                            let _ = tx.send(AgentEvent::StatusChanged(Status::Running));
                        }
                        pending_tools.push((
                            internal_call_id.clone(),
                            tool_call.function.name.clone(),
                            tool_call.function.arguments.clone(),
                        ));
                        let _ = tx.send(AgentEvent::ToolCall(ToolCallData {
                            id: internal_call_id,
                            kind: tool_head(&tool_call.function.name, &tool_call.function.arguments),
                            lines: Vec::new(),
                            awaiting_approval: false,
                        }));
                    }
                    MultiTurnStreamItem::StreamAssistantItem(_) => {}
                    MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                        tool_result,
                        internal_call_id,
                    }) => {
                        let done = tool_done(internal_call_id, &tool_result, &mut pending_tools);
                        if let Some((path, status)) = done.touched {
                            let mut session = state.lock().await;
                            session.files.insert(path, status);
                            let files = session
                                .files
                                .iter()
                                .map(|(path, s)| TouchedFile {
                                    path: path.clone(),
                                    status: s.label().to_string(),
                                    tone: s.tone(),
                                })
                                .collect();
                            let _ = tx.send(AgentEvent::FilesSet(files));
                        }
                        let _ = tx.send(AgentEvent::ToolCall(done.card));
                    }
                    MultiTurnStreamItem::CompletionCall(call) => {
                        let _ = tx.send(AgentEvent::TokenUsage(usage_label(
                            call.usage.input_tokens + call.usage.output_tokens,
                            call.usage.input_tokens,
                            context_length,
                        )));
                    }
                    MultiTurnStreamItem::ToolExecutionCommitted { .. } => {}
                    MultiTurnStreamItem::ModelTurnRetried { .. } => {}
                    MultiTurnStreamItem::FinalResponse(response) => break LoopEnd::Final(response),
                },
                Some(Err(error)) => {
                    match &error {
                        StreamingError::Prompt(prompt_error) => match prompt_error.as_ref() {
                            PromptError::MaxTurnsError { chat_history, .. } => {
                                // Keep the partial run: the follow-up prompt
                                // continues from where the budget ran out
                                // instead of silently losing the whole turn.
                                state.lock().await.history = chat_history.as_ref().clone();
                                let _ = tx.send(AgentEvent::AssistantText(
                                    "Reached the turn limit. Send another message to continue."
                                        .into(),
                                ));
                                let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
                            }
                            PromptError::PromptCancelled { .. } => {
                                let _ = tx.send(AgentEvent::StatusChanged(Status::Done));
                            }
                            _ => {
                                let _ = tx.send(AgentEvent::AssistantText(error.to_string()));
                                let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
                            }
                        },
                        _ => {
                            let _ = tx.send(AgentEvent::AssistantText(error.to_string()));
                            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
                        }
                    }
                    break LoopEnd::Ended;
                }
            },
        }
    };

    // Dropping the stream aborts the in-flight provider request; a cancelled
    // turn's history is not committed, matching the base loop's semantics.
    drop(stream);
    match outcome {
        LoopEnd::Cancel => TurnEnd::Cancelled,
        LoopEnd::Ended => {
            if streamed_text || streamed_reasoning {
                let _ = tx.send(AgentEvent::AssistantEnd);
            }
            // The stream closed without a final response or an error: surface
            // it instead of ending the turn silently.
            let _ = tx.send(AgentEvent::AssistantText(
                "The agent stream ended without a final response.".into(),
            ));
            let _ = tx.send(AgentEvent::StatusChanged(Status::Failed));
            TurnEnd::Ended
        }
        LoopEnd::Final(response) => TurnEnd::Final {
            response,
            streamed_text,
            streamed_reasoning,
        },
    }
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
    result: &rig::core::completion::message::ToolResult,
    pending: &mut Vec<(String, String, serde_json::Value)>,
) -> ToolDone {
    let text = tool_result_text(&result.content);
    let found = pending
        .iter()
        .position(|(pid, ..)| *pid == id)
        .map(|i| pending.swap_remove(i));
    let (name, arguments) = found
        .map(|(_, name, args)| (Some(name), args))
        .unwrap_or((None, serde_json::Value::Null));
    let detail = first_string_argument(&arguments).unwrap_or_default();

    let (kind, lines, touched) = match name.as_deref() {
        Some("read") => {
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
        Some("grep") => {
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
        Some(tool) if EDIT_TOOLS.contains(&tool) => {
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
                if detail.is_empty() {
                    None
                } else {
                    Some((detail, status))
                },
            )
        }
        Some(other) => (
            ToolKind::Bash {
                cmd: format!("{other} {detail}").trim().to_string(),
            },
            context_lines(&text),
            None,
        ),
        None => (
            ToolKind::Bash { cmd: "tool".into() },
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

/// Built-in tools produce model-facing text; keep JSON readable for anything
/// else without interpreting schemas.
fn tool_result_text(items: &[rig::core::completion::message::ToolResultContent]) -> String {
    items
        .iter()
        .map(|item| match item {
            rig::core::completion::message::ToolResultContent::Text(text) => text.text.clone(),
            rig::core::completion::message::ToolResultContent::Json { value, .. } => {
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            }
            other => format!("{other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
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
fn git_branch(cwd: &Path) -> String {
    std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| "no branch".into())
}
