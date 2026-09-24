//! Tool dispatch: our own executor over rig-core portable tools.
//!
//! Takes the tool calls of a turn and executes them sequentially through the
//! registered [`PortableDynamicTool`]s, with two interception points:
//! a [`BeforeExecute`] hook (approval/deny/rewrite) consulted before each
//! execution, and an [`AfterExecute`] hook (output transform) applied to each
//! result. Both default to pass-through; the TUI's approval gate is a
//! `BeforeExecute` implementation.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rig_core::completion::ToolDefinition;
use rig_core::tool::PortableDynamicTool;

use crate::history;

use super::dedup::{self, SharedDedupCache, ToolDedupCache};
use super::guardrails::{GuardrailDecision, SharedGuardrails};
use crate::compression::store::SharedCompressionStore;
use crate::snapshot::SnapshotManager;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// What to do with a tool call before it executes.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Execute the tool as requested.
    Run,
    /// Do not execute; `reason` is reported to the model as the result.
    Skip(String),
    /// Abort the run (cancellation) with the given reason.
    Stop(String),
}

/// Interception point before a tool executes (approval gate).
pub trait BeforeExecute: Send + Sync {
    fn decide(&self, call: history::ToolCall) -> BoxFuture<Decision>;
}

/// Output-transform slot after a tool executes. Initially identity; the future
/// home of output-trimming and presentation features.
pub trait AfterExecute: Send + Sync {
    fn transform(
        &self,
        call: history::ToolCall,
        result: history::ToolResult,
    ) -> BoxFuture<history::ToolResult>;
}

/// The turn's tool set: name-keyed portable tools plus interception hooks.
#[derive(Clone, Default)]
pub struct ToolDispatch {
    tools: BTreeMap<String, PortableDynamicTool>,
    before: Option<Arc<dyn BeforeExecute>>,
    after: Option<Arc<dyn AfterExecute>>,
    dedup: Option<SharedDedupCache>,
    compression_store: Option<SharedCompressionStore>,
    snapshots: Option<SnapshotManager>,
    guardrails: Option<SharedGuardrails>,
}

/// What executing one call produced.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// The tool ran (successfully or not; see the result's `is_error`).
    Ran(history::ToolResult),
    /// A `BeforeExecute` hook skipped the call; the reason is the result.
    Skipped(history::ToolResult),
    /// A `BeforeExecute` hook stopped the run.
    Stopped(String),
}

impl ToolDispatch {
    pub fn new(tools: impl IntoIterator<Item = PortableDynamicTool>) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|tool| (tool.name().to_owned(), tool))
                .collect(),
            before: None,
            after: None,
            dedup: None,
            compression_store: None,
            snapshots: None,
            guardrails: None,
        }
    }

    /// Registered tool names, in dispatch-table (alphabetical) order.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn with_before(mut self, hook: Arc<dyn BeforeExecute>) -> Self {
        self.before = Some(hook);
        self
    }

    pub fn with_after(mut self, hook: Arc<dyn AfterExecute>) -> Self {
        self.after = Some(hook);
        self
    }

    /// Share the session's tool dedup cache: read-only hits replay from
    /// cache, writes invalidate the paths they touch.
    pub fn with_dedup(mut self, cache: SharedDedupCache) -> Self {
        self.dedup = Some(cache);
        self
    }

    /// Share the session's reversible-compression store so the run loop can
    /// attach retrieval markers to request-view replacements; the retrieve
    /// tool reads the same instance.
    pub fn with_compression_store(mut self, store: SharedCompressionStore) -> Self {
        self.compression_store = Some(store);
        self
    }

    pub fn compression_store(&self) -> Option<&SharedCompressionStore> {
        self.compression_store.as_ref()
    }

    /// Share the session's snapshot manager so the run loop can commit
    /// turn sessions and the surface can drive `/undo`.
    pub fn with_snapshots(mut self, snapshots: SnapshotManager) -> Self {
        self.snapshots = Some(snapshots);
        self
    }

    pub fn snapshots(&self) -> Option<&SnapshotManager> {
        self.snapshots.as_ref()
    }

    /// Share the session's tool guardrails: repeat-failure and no-progress
    /// counters consulted around every execution.
    pub fn with_guardrails(mut self, guardrails: SharedGuardrails) -> Self {
        self.guardrails = Some(guardrails);
        self
    }

    /// Provider-facing definitions for the request.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(PortableDynamicTool::definition)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Execute one tool call through the interception pipeline. An unknown
    /// tool name fails the run, mirroring the previous loop's behavior of
    /// surfacing `UnknownToolCall` and leaving history uncommitted.
    pub async fn execute(
        &self,
        call: history::ToolCall,
    ) -> std::result::Result<DispatchOutcome, String> {
        let Some(tool) = self.tools.get(&call.function.name) else {
            return Err(format!("unknown tool: {}", call.function.name));
        };
        if let Some(before) = &self.before {
            match before.decide(call.clone()).await {
                Decision::Run => {}
                Decision::Skip(reason) => {
                    return Ok(DispatchOutcome::Skipped(history::ToolResult {
                        call: call.id,
                        name: call.function.name,
                        content: vec![history::ToolResultContent::text(reason)],
                        is_error: false,
                    }));
                }
                Decision::Stop(reason) => return Ok(DispatchOutcome::Stopped(reason)),
            }
        }
        let name = call.function.name.clone();
        let read_only = ToolDedupCache::is_read_only(&name);
        // Guardrails are consulted after approval so a human-approved call
        // still cannot loop unproductively.
        let mut pre_warned = false;
        if let Some(guardrails) = &self.guardrails
            && let Ok(guard) = guardrails.lock()
        {
            match guard.check_before_call(&name, &call.function.arguments, read_only) {
                GuardrailDecision::Allow => {}
                GuardrailDecision::Warn => pre_warned = true,
                GuardrailDecision::Block => {
                    return Ok(DispatchOutcome::Skipped(history::ToolResult {
                        call: call.id,
                        name: call.function.name,
                        content: vec![history::ToolResultContent::text(GUARDRAIL_BLOCK_MESSAGE)],
                        is_error: true,
                    }));
                }
            }
        }
        let dedup_key = read_only.then(|| ToolDedupCache::key(&name, &call.function.arguments));
        let cached = if let (Some(cache), Some(key)) = (&self.dedup, dedup_key) {
            cache
                .lock()
                .ok()
                .and_then(|guard| guard.get(key, &name, &call.function.arguments).cloned())
        } else {
            None
        };
        if let Some(cached) = cached {
            // Replays re-run the after hook on the cached raw result: the
            // transform is per-call and must not be baked into the cache.
            let mut replayed = dedup::cached_result(&cached, &call.id);
            if let Some(after) = &self.after {
                replayed = after.transform(call.clone(), replayed).await;
            }
            guardrail_note(
                self,
                &name,
                &call.function.arguments,
                &mut replayed,
                read_only,
                pre_warned,
            );
            return Ok(DispatchOutcome::Ran(replayed));
        }
        let output = tool.execute(call.function.arguments.clone()).await;
        let mut result = match output {
            Ok(output) => history::ToolResult {
                call: call.id.clone(),
                name: call.function.name.clone(),
                content: rig_content_to_own(output.as_content()),
                is_error: false,
            },
            Err(error) => history::ToolResult {
                call: call.id.clone(),
                name: call.function.name.clone(),
                content: output_content_of_error(&error),
                is_error: true,
            },
        };
        // Cache bookkeeping runs on the raw, pre-transform result so a
        // call-specific AfterExecute hook cannot poison the cache.
        if !result.is_error
            && let Some(cache) = &self.dedup
            && let Ok(mut guard) = cache.lock()
        {
            if let Some(key) = dedup_key
                && !result
                    .content
                    .iter()
                    .any(|item| matches!(item, history::ToolResultContent::Image(_)))
            {
                let path = dedup::extract_file_path(&call.function.arguments);
                guard.insert(
                    key,
                    &result,
                    path.as_deref(),
                    &name,
                    &call.function.arguments,
                );
            } else if ToolDedupCache::is_write(&name) {
                let paths = dedup::extract_write_paths(&name, &call.function.arguments);
                if paths.is_empty() {
                    guard.invalidate_pathless();
                } else {
                    for path in paths {
                        guard.invalidate_path(&path);
                    }
                }
            }
        }
        if let Some(after) = &self.after {
            result = after.transform(call.clone(), result).await;
        }
        let mut result = result;
        guardrail_note(
            self,
            &name,
            &call.function.arguments,
            &mut result,
            read_only,
            pre_warned,
        );
        Ok(DispatchOutcome::Ran(result))
    }
}

/// Feed one finished result back into the guardrail counters and surface
/// any warning (from this result, or carried from the pre-call check) as
/// a prefix on the model-visible text.
fn guardrail_note(
    dispatch: &ToolDispatch,
    name: &str,
    arguments: &serde_json::Value,
    result: &mut history::ToolResult,
    read_only: bool,
    pre_warned: bool,
) {
    let Some(guardrails) = &dispatch.guardrails else {
        return;
    };
    let Ok(mut guard) = guardrails.lock() else {
        return;
    };
    let text = result
        .content
        .iter()
        .map(history::ToolResultContent::to_text)
        .collect::<Vec<_>>()
        .join("\n");
    let warning = guard
        .record_result(name, arguments, &text, result.is_error, read_only)
        .map(|warning| warning.reason);
    let reason = match (warning, pre_warned) {
        (Some(reason), _) => reason,
        (None, true) => {
            format!("{name} is repeating unproductively; consider a different approach or tool")
        }
        (None, false) => return,
    };
    result.content.insert(
        0,
        history::ToolResultContent::text(format!("{GUARDRAIL_WARN_PREFIX}{reason}")),
    );
}

/// Model-visible content for a failed call: the error's canonical output when
/// it has one, else its message.
fn output_content_of_error(
    error: &rig_core::tool::ToolExecutionError,
) -> Vec<history::ToolResultContent> {
    let content = rig_content_to_own(error.model_output().as_content());
    if content.is_empty() {
        vec![history::ToolResultContent::text(error.to_string())]
    } else {
        content
    }
}

fn rig_content_to_own(
    items: &[rig_core::completion::message::ToolResultContent],
) -> Vec<history::ToolResultContent> {
    items
        .iter()
        .map(|item| match item {
            rig_core::completion::message::ToolResultContent::Text(text) => {
                history::ToolResultContent::text(text.text.clone())
            }
            rig_core::completion::message::ToolResultContent::Json { value, .. } => {
                history::ToolResultContent::Json {
                    value: value.clone(),
                }
            }
            rig_core::completion::message::ToolResultContent::Image(image) => {
                let media_type = match image.media_type {
                    Some(rig_core::completion::message::ImageMediaType::PNG) => {
                        history::ImageMedia::Png
                    }
                    Some(rig_core::completion::message::ImageMediaType::JPEG) => {
                        history::ImageMedia::Jpeg
                    }
                    Some(rig_core::completion::message::ImageMediaType::GIF) => {
                        history::ImageMedia::Gif
                    }
                    Some(rig_core::completion::message::ImageMediaType::WEBP) => {
                        history::ImageMedia::Webp
                    }
                    _ => history::ImageMedia::Png,
                };
                let data = match &image.data {
                    rig_core::completion::message::DocumentSourceKind::Base64(data) => data.clone(),
                    _ => String::new(),
                };
                history::ToolResultContent::Image(history::ImageBlock {
                    media_type,
                    data,
                    caption: "[image]".into(),
                })
            }
        })
        .collect()
}

use crate::history::Message;

use super::doom;
use super::task_set;
use super::{CancelToken, Event, RunOutcome};

/// Model-visible text for a guardrail-blocked call; `commit_wave` also keys
/// the user-facing `Event::Info` off it, so keep the constant in sync.
pub(crate) const GUARDRAIL_BLOCK_MESSAGE: &str = "blocked by guardrails: this tool call keeps \
     repeating without progress; change your approach or use a different tool";
/// The prefix `guardrail_note` puts on warnings carried by a ran result;
/// `commit_wave` strips it into the `Event::Info` text.
const GUARDRAIL_WARN_PREFIX: &str = "[guardrail] ";

/// Tools that must never share a wave with another call: `batch` nests its
/// own parallel dispatch and `question` (not yet ported) blocks on the user.
pub(crate) fn is_never_parallel(name: &str) -> bool {
    matches!(name, "batch" | "question")
}

/// Execute the turn's tool calls, appending their results to `turn`.
/// Returns `Some(outcome)` when the run must stop (cancel or dispatch
/// failure); `None` means the loop continues.
///
/// Calls run concurrently in waves; a wave is joined and drained before the
/// next starts whenever two calls write the same path or a
/// never-parallel tool joins the batch. Results are committed in call
/// order; a panicking tool future becomes an error result instead of
/// unwinding the run.
pub(crate) async fn dispatch_tool_calls(
    tools: &ToolDispatch,
    turn: &mut Vec<Message>,
    calls: Vec<history::ToolCall>,
    recent: &mut doom::RecentCalls,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> (Option<RunOutcome>, doom::ToolBatchOutcome) {
    // Spawned tasks need owned state; the dispatch table is cheap to clone.
    let tools = Arc::new(tools.clone());
    let mut set = task_set::TaskSet::new();
    let mut wave: Vec<history::ToolCall> = Vec::new();
    let mut all_write_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut batch = doom::ToolBatchOutcome::default();

    for call in calls {
        if cancel.cancelled() {
            let outcome =
                commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await;
            return (outcome.or(Some(RunOutcome::Cancelled)), batch);
        }
        let name = call.function.name.clone();
        let arguments = call.function.arguments.clone();
        if recent.is_doom_loop(&name, &arguments) {
            // The call is blocked, not executed: emit the reference's error
            // result directly and clear the window so the warning does not
            // re-fire identically on the next retry.
            batch.doom_loops += 1;
            let result = history::ToolResult {
                call: call.id.clone(),
                name: name.clone(),
                content: vec![history::ToolResultContent::text(doom::DOOM_LOOP_MESSAGE)],
                is_error: true,
            };
            turn.push(Message::User {
                content: vec![history::UserContent::ToolResult(result.clone())],
            });
            emit(Event::ToolDone {
                id: call.id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                result,
            });
            recent.clear();
        } else {
            // A never-parallel tool or a repeat write path must not share a
            // wave with earlier calls, so the pending wave is flushed
            // *before* this call is spawned into a fresh one.
            let write_paths = dedup::extract_write_paths(&name, &call.function.arguments);
            let conflicts = is_never_parallel(&name)
                || write_paths
                    .iter()
                    .any(|path| all_write_paths.contains(path));
            if conflicts && !wave.is_empty() {
                if let Some(outcome) =
                    commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await
                {
                    return (Some(outcome), batch);
                }
                set = task_set::TaskSet::new();
                all_write_paths.clear();
            }
            all_write_paths.extend(write_paths);
            let executor = Arc::clone(&tools);
            wave.push(call.clone());
            set.spawn(async move { executor.execute(call).await });
        }
        recent.record(name, &arguments);
    }
    let stopped = commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await;
    (stopped, batch)
}

/// One finished wave entry: the call paired with its dispatch outcome, or
/// the panic/cancellation string when the tool task itself failed.
type WaveEntry = (
    history::ToolCall,
    Result<Result<DispatchOutcome, String>, String>,
);

/// Join a finished wave, pairing each spawn-order result with its call.
async fn join_wave(
    set: task_set::TaskSet<Result<DispatchOutcome, String>>,
    wave: std::vec::Drain<'_, history::ToolCall>,
) -> Vec<WaveEntry> {
    let ids: Vec<history::ToolCall> = wave.collect();
    set.join_all()
        .await
        .into_iter()
        .zip(ids)
        .map(|(outcome, call)| (call, outcome))
        .collect()
}

/// Commit one wave's results to `turn` in call order, emitting `ToolDone`
/// per call. `Some(outcome)` when the run must stop.
async fn commit_wave(
    results: Vec<WaveEntry>,
    turn: &mut Vec<Message>,
    batch: &mut doom::ToolBatchOutcome,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Option<RunOutcome> {
    let mut wave_results: Vec<history::ToolResult> = Vec::new();
    for (call, outcome) in results {
        // `guardrail_blocked` identifies the one Skipped outcome that is an
        // error (the guardrail block), so its skip surfaces as a warning
        // rather than silent tool output.
        let (guardrail_blocked, result) = match outcome {
            Ok(Ok(DispatchOutcome::Ran(result))) => (false, result),
            Ok(Ok(DispatchOutcome::Skipped(result))) => (
                result.is_error
                    && result
                        .content
                        .first()
                        .map(history::ToolResultContent::to_text)
                        .as_deref()
                        == Some(GUARDRAIL_BLOCK_MESSAGE),
                result,
            ),
            Ok(Ok(DispatchOutcome::Stopped(_))) => return Some(RunOutcome::Cancelled),
            Ok(Err(unknown)) => return Some(RunOutcome::Failed(unknown)),
            // The task itself failed: a panic or cancellation inside the
            // tool future, reported as a per-call error result.
            Err(panic) => (
                false,
                history::ToolResult {
                    call: call.id.clone(),
                    name: call.function.name.clone(),
                    content: vec![history::ToolResultContent::text(format!(
                        "internal error: tool panicked: {panic}"
                    ))],
                    is_error: true,
                },
            ),
        };
        // Guardrail warn/block statuses ride `Event::Info` so a surface can
        // show them as warnings instead of silent tool text.
        if guardrail_blocked {
            let reason = result
                .content
                .first()
                .map(history::ToolResultContent::to_text)
                .unwrap_or_default()
                .trim_start_matches("blocked by guardrails: ")
                .to_owned();
            emit(Event::Info(format!(
                "guardrail blocked {}: {reason}",
                call.function.name
            )));
        } else if let Some(warning) = result
            .content
            .first()
            .map(history::ToolResultContent::to_text)
            .filter(|text| text.starts_with(GUARDRAIL_WARN_PREFIX))
        {
            emit(Event::Info(format!(
                "guardrail warning on {}: {}",
                call.function.name,
                warning.trim_start_matches(GUARDRAIL_WARN_PREFIX)
            )));
        }
        if result.is_error {
            batch.errors += 1;
        } else {
            batch.successes += 1;
        }
        turn.push(Message::User {
            content: vec![history::UserContent::ToolResult(result.clone())],
        });
        wave_results.push(result.clone());
        emit(Event::ToolDone {
            id: call.id.clone(),
            name: call.function.name.clone(),
            arguments: call.function.arguments.clone(),
            result,
        });
    }
    if !wave_results.is_empty() {
        // One submission event per wave, carrying all of its results in
        // call order (reference semantics), independent of the per-call
        // messages committed to history above.
        emit(Event::ToolResultsSubmitted {
            message: Message::User {
                content: wave_results
                    .into_iter()
                    .map(history::UserContent::ToolResult)
                    .collect(),
            },
        });
    }
    None
}
