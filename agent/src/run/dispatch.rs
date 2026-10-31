//! Tool dispatch: our own executor over rig-core portable tools.
//!
//! Takes the tool calls of a turn and executes them sequentially through the
//! registered [`DynamicTool`]s, with two interception points:
//! a [`BeforeExecute`] hook (approval/deny/rewrite) consulted before each
//! execution, and an [`AfterExecute`] hook (output transform) applied to each
//! result. Both default to pass-through; the TUI's approval gate is a
//! `BeforeExecute` implementation.
//!
//! Calls and results are animist content blocks: a call is a
//! `ContentBlock::ToolUse`, a result a `ContentBlock::ToolResult` carrying
//! the tool's name, so results are self-describing everywhere they travel.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use animist::history::{ContentBlock, ToolContents};
use rig_core::completion::ToolDefinition;
use rig_core::tool::DynamicTool;

use crate::history::Message;
use crate::history::{self, ext};

use super::dedup::{self, SharedDedupCache, ToolDedupCache};
use super::guardrails::{GuardrailDecision, SharedGuardrails};
use super::mode::{AgentMode, PLAN_WRITE_RESTRICTED};
use crate::compression::store::SharedCompressionStore;
use crate::snapshot::SnapshotManager;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

tokio::task_local! {
    /// The internal call id of the tool currently executing on this task.
    /// Tools that need their own call identity (the `task` tool tags its
    /// subagent events by it) read it via [`current_call_id`]; it is
    /// unavailable outside a dispatch.
    static CURRENT_CALL_ID: String;
}

/// The call id of the tool executing on this task, if this code runs
/// inside a dispatch.
pub fn current_call_id() -> Option<String> {
    CURRENT_CALL_ID.try_with(|id| id.clone()).ok()
}

/// Run `future` with the dispatch call-id task-local set to `id`; the rig
/// bridge scopes every bridged executor call this way.
pub(crate) async fn scope_call_id<T, F>(id: &str, future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CURRENT_CALL_ID.scope(id.to_owned(), future).await
}

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

/// Interception point before a tool executes (approval gate). `call` is the
/// model's `ToolUse` block.
pub trait BeforeExecute: Send + Sync {
    fn decide(&self, call: ContentBlock) -> BoxFuture<Decision>;
}

/// Output-transform slot after a tool executes. Initially identity; the future
/// home of output-trimming and presentation features.
pub trait AfterExecute: Send + Sync {
    fn transform(&self, call: ContentBlock, result: ContentBlock) -> BoxFuture<ContentBlock>;
}

/// The turn's tool set: name-keyed portable tools plus interception hooks.
#[derive(Clone, Default)]
pub struct ToolDispatch {
    tools: BTreeMap<String, DynamicTool>,
    // The hook/cache slots below are shared across clones on purpose. The
    // `batch` tool holds a clone of the table taken during registration, before
    // the caller attaches these with the `with_*` builders; a plain field would
    // leave batch children blind to the approval gate, guardrails, after hook,
    // and dedup cache. RwLock preserves the builders' last-write-wins behavior.
    before: Arc<RwLock<Option<Arc<dyn BeforeExecute>>>>,
    after: Arc<RwLock<Option<Arc<dyn AfterExecute>>>>,
    dedup: Arc<RwLock<Option<SharedDedupCache>>>,
    write_root: Option<std::path::PathBuf>,
    compression_store: Option<SharedCompressionStore>,
    snapshots: Option<SnapshotManager>,
    guardrails: Arc<RwLock<Option<SharedGuardrails>>>,
    /// Frozen per dispatch table (one per turn): write-gating cannot change
    /// while tools execute. Batch children share it because the child table
    /// is built with the mode already baked in.
    mode: AgentMode,
}

/// What executing one call produced.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// The tool ran (successfully or not; see the result's `is_error`).
    Ran(ContentBlock),
    /// A `BeforeExecute` hook skipped the call; the reason is the result.
    Skipped(ContentBlock),
    /// A `BeforeExecute` hook stopped the run.
    Stopped(String),
}

/// The `ToolUse` block's id, name and input; every dispatch path receives
/// ToolUse-shaped calls.
pub(crate) fn call_parts(call: &ContentBlock) -> (&str, &str, &serde_json::Value) {
    let ContentBlock::ToolUse {
        id, name, input, ..
    } = call
    else {
        unreachable!("dispatch receives ToolUse blocks only");
    };
    (id, name, input)
}

impl ToolDispatch {
    pub fn new(tools: impl IntoIterator<Item = DynamicTool>) -> Self {
        Self {
            tools: tools
                .into_iter()
                .map(|tool| (tool.name().to_string(), tool))
                .collect(),
            before: Arc::new(RwLock::new(None)),
            after: Arc::new(RwLock::new(None)),
            dedup: Arc::new(RwLock::new(None)),
            write_root: std::env::current_dir().ok(),
            compression_store: None,
            snapshots: None,
            guardrails: Arc::new(RwLock::new(None)),
            mode: AgentMode::Build,
        }
    }

    /// Registered tool names, in dispatch-table (alphabetical) order.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// The registered rig tools; the bridge wraps each as an animist
    /// executor until 06 makes them native.
    pub(crate) fn dynamic_tools(&self) -> impl Iterator<Item = &DynamicTool> {
        self.tools.values()
    }

    pub fn with_before(self, hook: Arc<dyn BeforeExecute>) -> Self {
        *self.before.write().unwrap_or_else(|e| e.into_inner()) = Some(hook);
        self
    }

    pub fn with_after(self, hook: Arc<dyn AfterExecute>) -> Self {
        *self.after.write().unwrap_or_else(|e| e.into_inner()) = Some(hook);
        self
    }

    /// Share the session's tool dedup cache: read-only hits replay from
    /// cache, writes invalidate the paths they touch. Set through a shared
    /// slot so the `batch` child table (cloned during registration) sees it.
    pub fn with_dedup(self, cache: SharedDedupCache) -> Self {
        *self.dedup.write().unwrap_or_else(|e| e.into_inner()) = Some(cache);
        self
    }

    /// Anchor write-path conflict normalization at the workspace root, so
    /// relative and absolute spellings of the same file conflict-detected
    /// identically even when the process cwd differs from the workspace.
    pub fn with_write_root(mut self, root: std::path::PathBuf) -> Self {
        self.write_root = Some(root);
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

    /// The workspace root anchoring write-path conflict normalization, if
    /// one was set with `with_write_root`.
    pub fn write_root(&self) -> Option<&std::path::Path> {
        self.write_root.as_deref()
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
    pub fn with_guardrails(self, guardrails: SharedGuardrails) -> Self {
        *self.guardrails.write().unwrap_or_else(|e| e.into_inner()) = Some(guardrails);
        self
    }

    /// Set the agent mode (C.17): in Plan mode every write except the
    /// allocated plan file is blocked before execution.
    pub fn with_mode(mut self, mode: AgentMode) -> Self {
        self.mode = mode;
        self
    }

    /// Provider-facing declarations for the request, converted from the
    /// rig tool definitions until 06 makes the tools native.
    pub fn definitions(&self) -> Vec<animist::provider::Tool> {
        let definitions: Vec<ToolDefinition> =
            self.tools.values().map(DynamicTool::definition).collect();
        crate::tools::animist_definitions(&definitions)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Execute one tool call through the interception pipeline. An unknown
    /// tool name surfaces as a per-call error result so a wrong spelling
    /// costs one call instead of the whole run — the model sees the error
    /// and can correct itself.
    pub async fn execute(
        &self,
        call: ContentBlock,
    ) -> std::result::Result<DispatchOutcome, String> {
        let (call_id, name, arguments) = call_parts(&call);
        let call_id = call_id.to_owned();
        let name = name.to_owned();
        let arguments = arguments.clone();
        let Some(tool) = self.tools.get(&name) else {
            return Ok(DispatchOutcome::Skipped(ext::tool_result(
                call_id,
                Some(&name),
                format!(
                    "unknown tool: {}. Call list_tools() to see available tools.",
                    name
                ),
                true,
            )));
        };
        let before = self
            .before
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(before) = before {
            match before.decide(call.clone()).await {
                Decision::Run => {}
                Decision::Skip(reason) => {
                    return Ok(DispatchOutcome::Skipped(ext::tool_result(
                        call_id,
                        Some(&name),
                        reason,
                        false,
                    )));
                }
                Decision::Stop(reason) => return Ok(DispatchOutcome::Stopped(reason)),
            }
        }
        // Plan mode (C.17): a write may only touch the allocated plan file.
        // Batch children reach this same gate because they flow through the
        // shared dispatch table.
        if let Some(reason) = self.plan_block_reason(&name, &arguments) {
            return Ok(DispatchOutcome::Skipped(ext::tool_result(
                call_id,
                Some(&name),
                reason,
                true,
            )));
        }
        let read_only = ToolDedupCache::is_read_only(&name);
        // Guardrails are consulted after approval so a human-approved call
        // still cannot loop unproductively.
        let mut pre_warned = false;
        let guardrails = self
            .guardrails
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(guardrails) = guardrails
            && let Ok(mut guard) = guardrails.lock()
        {
            match guard.check_before_call(&name, &arguments, read_only) {
                GuardrailDecision::Allow => {}
                GuardrailDecision::Warn => pre_warned = true,
                GuardrailDecision::Block => {
                    return Ok(DispatchOutcome::Skipped(ext::tool_result(
                        call_id,
                        Some(&name),
                        GUARDRAIL_BLOCK_MESSAGE,
                        true,
                    )));
                }
            }
        }
        let dedup_key = read_only.then(|| ToolDedupCache::key(&name, &arguments));
        let cache = self.dedup.read().unwrap_or_else(|e| e.into_inner()).clone();
        let cached = if let (Some(cache), Some(key)) = (cache.as_ref(), dedup_key) {
            cache
                .lock()
                .ok()
                .and_then(|guard| guard.get(key, &name, &arguments).cloned())
        } else {
            None
        };
        if let Some(cached) = cached {
            // Replays re-run the after hook on the cached raw result: the
            // transform is per-call and must not be baked into the cache.
            let mut replayed = dedup::cached_result(&cached, &call_id);
            let after = self.after.read().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(after) = after {
                replayed = after.transform(call.clone(), replayed).await;
            }
            guardrail_note(
                self,
                &name,
                &arguments,
                &mut replayed,
                read_only,
                pre_warned,
            );
            return Ok(DispatchOutcome::Ran(replayed));
        }
        let output = CURRENT_CALL_ID
            .scope(call_id.clone(), tool.execute(arguments.clone()))
            .await;
        let mut result = match output {
            Ok(output) => ext::tool_result(
                call_id.clone(),
                Some(&name),
                rig_content_to_contents(output.as_content()),
                false,
            ),
            Err(error) => ext::tool_result(
                call_id.clone(),
                Some(&name),
                output_content_of_error(&error),
                true,
            ),
        };
        // Cache bookkeeping runs on the raw, pre-transform result so a
        // call-specific AfterExecute hook cannot poison the cache.
        if let Some(cache) = cache
            && let Ok(mut guard) = cache.lock()
        {
            let ContentBlock::ToolResult {
                content, is_error, ..
            } = &result
            else {
                unreachable!("result is tool-result shaped");
            };
            if !*is_error
                && let Some(key) = dedup_key
                && !content
                    .0
                    .iter()
                    .any(|item| matches!(item, animist::history::ToolContent::Image { .. }))
            {
                let path = dedup::extract_file_path(&arguments);
                guard.insert(key, &result, path.as_deref(), &name, &arguments);
            }
            // Invalidation runs even on a failed result: apply_patch and
            // delete apply hunks iteratively, so a call that errors midway may
            // still have written. Clearing a cache entry that did not change
            // only costs one re-read.
            if ToolDedupCache::is_write(&name) {
                let mut paths = dedup::extract_write_paths(&name, &arguments);
                if name == "move_file" {
                    // `move_file` also rewrites imports elsewhere; those files
                    // appear only in the result text.
                    let text = ext::tool_content_text(content);
                    paths.extend(crate::tools::move_file::rewritten_files(&text));
                }
                if paths.is_empty() {
                    guard.invalidate_pathless();
                } else {
                    for path in paths {
                        guard.invalidate_path(&path);
                    }
                }
            } else if ToolDedupCache::clears_whole_cache(&name) {
                guard.clear();
            }
        }
        let after = self.after.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(after) = after {
            result = after.transform(call.clone(), result).await;
        }
        let mut result = result;
        guardrail_note(self, &name, &arguments, &mut result, read_only, pre_warned);
        Ok(DispatchOutcome::Ran(result))
    }

    /// Returns the plan-mode block reason for a call, or `None` when the
    /// call may run. In Plan mode only the allocated plan file may be
    /// written; write calls that name no path fail closed.
    fn plan_block_reason(&self, name: &str, arguments: &serde_json::Value) -> Option<String> {
        let plan = self.mode.plan_path()?;
        if !is_gated_write(name) {
            return None;
        }
        let root = self
            .write_root
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned());
        let plan_key =
            dedup::normalize_write_path_with_root(root.as_deref(), &plan.display().to_string());
        let paths = gated_write_paths(name, arguments);
        let blocked = paths.is_empty()
            || paths
                .iter()
                .any(|p| dedup::normalize_write_path_with_root(root.as_deref(), p) != plan_key);
        blocked.then(|| PLAN_WRITE_RESTRICTED.to_owned())
    }
}

/// Tools whose calls mutate files and must honor the plan-mode gate. Mirrors
/// dedup's write classification exactly: fuzzy replacement ships inside
/// `edit`/`multiedit`, both already gated, and no standalone `fuzzy_replace`
/// tool is registered. `batch` is absent on purpose: its children execute
/// through this same dispatch table and pass the gate individually.
fn is_gated_write(name: &str) -> bool {
    ToolDedupCache::is_write(name)
}

/// Every path a gated write call touches.
fn gated_write_paths(name: &str, input: &serde_json::Value) -> Vec<String> {
    if name == "fuzzy_replace" {
        return dedup::extract_file_path(input).into_iter().collect();
    }
    dedup::extract_write_paths(name, input)
}

/// Feed one finished result back into the guardrail counters and surface
/// any warning (from this result, or carried from the pre-call check) as
/// a prefix on the model-visible text.
fn guardrail_note(
    dispatch: &ToolDispatch,
    name: &str,
    arguments: &serde_json::Value,
    result: &mut ContentBlock,
    read_only: bool,
    pre_warned: bool,
) {
    let guardrails = dispatch
        .guardrails
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(guardrails) = guardrails else {
        return;
    };
    let Ok(mut guard) = guardrails.lock() else {
        return;
    };
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        return;
    };
    let text = ext::tool_content_text(content);
    let warning = guard
        .record_result(name, arguments, &text, *is_error, read_only)
        .map(|warning| warning.reason);
    let reason = match (warning, pre_warned) {
        (Some(reason), _) => reason,
        (None, true) => {
            format!("{name} is repeating unproductively; consider a different approach or tool")
        }
        (None, false) => return,
    };
    let mut blocks = vec![animist::history::ToolContent::Text {
        text: format!("{GUARDRAIL_WARN_PREFIX}{reason}"),
    }];
    blocks.append(&mut content.0);
    *content = ToolContents::from(blocks);
}

/// Model-visible content for a failed call: the error's canonical output when
/// it has one, else its message.
pub(crate) fn output_content_of_error(error: &rig_core::tool::ToolExecutionError) -> ToolContents {
    let content = rig_content_to_contents(error.model_output().as_content());
    if content.0.is_empty() {
        ToolContents::from(error.to_string())
    } else {
        content
    }
}

pub(crate) fn rig_content_to_contents(
    items: &[rig_core::completion::message::ToolResultContent],
) -> ToolContents {
    items
        .iter()
        .map(|item| match item {
            rig_core::completion::message::ToolResultContent::Text(text) => {
                animist::history::ToolContent::Text {
                    text: text.text.clone(),
                }
            }
            rig_core::completion::message::ToolResultContent::Json { value, .. } => {
                animist::history::ToolContent::Text {
                    text: serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                }
            }
            rig_core::completion::message::ToolResultContent::Image(image) => {
                let media_type = match image.media_type {
                    Some(rig_core::completion::message::ImageMediaType::PNG) => "image/png",
                    Some(rig_core::completion::message::ImageMediaType::JPEG) => "image/jpeg",
                    Some(rig_core::completion::message::ImageMediaType::GIF) => "image/gif",
                    Some(rig_core::completion::message::ImageMediaType::WEBP) => "image/webp",
                    _ => "image/png",
                };
                let data = match &image.data {
                    rig_core::completion::message::DocumentSourceKind::Base64(data) => data.clone(),
                    _ => String::new(),
                };
                animist::history::ToolContent::Image {
                    source: history::ImageSource::Base64 {
                        media_type: media_type.to_owned(),
                        data,
                    },
                }
            }
        })
        .collect::<Vec<_>>()
        .into()
}

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
/// own parallel dispatch and `question` blocks on the user.
pub(crate) fn is_never_parallel(name: &str) -> bool {
    matches!(name, "batch" | "question")
}

/// The model-visible text of a result block, or the empty string for a
/// non-result block.
fn result_first_text(result: &ContentBlock) -> String {
    match result {
        ContentBlock::ToolResult { content, .. } => content
            .0
            .first()
            .map(|item| match item {
                animist::history::ToolContent::Text { text } => text.clone(),
                animist::history::ToolContent::Image { .. } => "[image]".to_owned(),
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
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
    turn: &mut Vec<history::StoredMessage>,
    calls: Vec<ContentBlock>,
    recent: &mut doom::RecentCalls,
    cancel: &CancelToken,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> (Option<RunOutcome>, doom::ToolBatchOutcome) {
    // Spawned tasks need owned state; the dispatch table is cheap to clone.
    let tools = Arc::new(tools.clone());
    let mut set = task_set::TaskSet::new();
    let mut wave: Vec<ContentBlock> = Vec::new();
    let mut all_write_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut batch = doom::ToolBatchOutcome::default();

    for call in calls {
        if cancel.cancelled() {
            let outcome =
                commit_wave(join_wave(set, wave.drain(..)).await, turn, &mut batch, emit).await;
            return (outcome.or(Some(RunOutcome::Cancelled)), batch);
        }
        let (id, name, arguments) = call_parts(&call);
        let id = id.to_owned();
        let name = name.to_owned();
        let arguments = arguments.clone();
        if recent.is_doom_loop(&name, &arguments) {
            // The call is blocked, not executed: emit the reference's error
            // result directly and clear the window so the warning does not
            // re-fire identically on the next retry.
            batch.doom_loops += 1;
            let result = ext::tool_result(id.clone(), Some(&name), doom::DOOM_LOOP_MESSAGE, true);
            turn.push(ext::stored(ext::observation(vec![result.clone()])));
            emit(Event::ToolDone {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                result,
            });
            recent.clear();
        } else {
            // A never-parallel tool or a repeat write path must not share a
            // wave with earlier calls, so the pending wave is flushed
            // *before* this call is spawned into a fresh one.
            let write_paths: Vec<String> = dedup::extract_write_paths(&name, &arguments)
                .iter()
                .map(|p| {
                    let root = tools
                        .write_root
                        .as_deref()
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    dedup::normalize_write_path_with_root(Some(&root), p)
                })
                .collect();
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
    ContentBlock,
    Result<Result<DispatchOutcome, String>, String>,
);

/// Join a finished wave, pairing each spawn-order result with its call.
async fn join_wave(
    set: task_set::TaskSet<Result<DispatchOutcome, String>>,
    wave: std::vec::Drain<'_, ContentBlock>,
) -> Vec<WaveEntry> {
    let ids: Vec<ContentBlock> = wave.collect();
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
    turn: &mut Vec<history::StoredMessage>,
    batch: &mut doom::ToolBatchOutcome,
    emit: &(dyn Fn(Event) + Send + Sync),
) -> Option<RunOutcome> {
    let mut wave_results: Vec<ContentBlock> = Vec::new();
    for (call, outcome) in results {
        let (id, name, arguments) = call_parts(&call);
        let id = id.to_owned();
        let name = name.to_owned();
        let arguments = arguments.to_owned();
        // `guardrail_blocked` identifies the one Skipped outcome that is an
        // error (the guardrail block), so its skip surfaces as a warning
        // rather than silent tool output.
        let (guardrail_blocked, result) = match outcome {
            Ok(Ok(DispatchOutcome::Ran(result))) => (false, result),
            Ok(Ok(DispatchOutcome::Skipped(result))) => {
                let ContentBlock::ToolResult { is_error, .. } = &result else {
                    unreachable!("skipped outcome carries a result");
                };
                (
                    *is_error && result_first_text(&result) == GUARDRAIL_BLOCK_MESSAGE,
                    result,
                )
            }
            Ok(Ok(DispatchOutcome::Stopped(_))) => return Some(RunOutcome::Cancelled),
            // A dispatch-level failure is one call's error, not the run's:
            // surface it as a result the model can adapt to instead of
            // dropping the whole turn.
            Ok(Err(e)) => (false, ext::tool_result(id.clone(), Some(&name), e, true)),
            // The task itself failed: a panic or cancellation inside the
            // tool future, reported as a per-call error result.
            Err(panic) => (
                false,
                ext::tool_result(
                    id.clone(),
                    Some(&name),
                    format!("internal error: tool panicked: {panic}"),
                    true,
                ),
            ),
        };
        // Guardrail warn/block statuses ride `Event::Info` so a surface can
        // show them as warnings instead of silent tool text.
        if guardrail_blocked {
            let reason = result_first_text(&result)
                .trim_start_matches("blocked by guardrails: ")
                .to_owned();
            emit(Event::Info(format!("guardrail blocked {name}: {reason}")));
        } else if let Some(warning) = result_first_text(&result)
            .strip_prefix(GUARDRAIL_WARN_PREFIX)
            .map(str::to_owned)
        {
            emit(Event::Info(format!(
                "guardrail warning on {name}: {warning}"
            )));
        }
        let ContentBlock::ToolResult { is_error, .. } = &result else {
            unreachable!("results are tool-result shaped");
        };
        if *is_error {
            batch.errors += 1;
        } else {
            batch.successes += 1;
        }
        turn.push(ext::stored(ext::observation(vec![result.clone()])));
        wave_results.push(result.clone());
        emit(Event::ToolDone {
            id: id.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            result,
        });
    }
    if !wave_results.is_empty() {
        // One submission event per wave, carrying all of its results in
        // call order (reference semantics), independent of the per-call
        // messages committed to history above.
        emit(Event::ToolResultsSubmitted {
            message: Message {
                role: history::Role::User,
                content: wave_results,
                display_text: None,
                kind: history::MessageKind::Observation,
            },
        });
    }
    None
}

#[cfg(test)]
mod result_content_tests {
    use super::*;

    /// Rig tools can return structured JSON; the model sees it pretty
    /// printed as text, not a compact dump.
    #[test]
    fn rig_json_output_renders_to_pretty_text() {
        let value = serde_json::json!({"matches": 3, "files": ["a.rs"]});
        let contents =
            rig_content_to_contents(&[rig_core::completion::message::ToolResultContent::json(
                value,
            )]);
        let text = ext::tool_content_text(&contents);
        assert!(text.contains("\"matches\": 3"), "{text}");
        assert!(text.contains('\n'), "pretty, not compact: {text}");
    }

    #[test]
    fn rig_text_output_passes_through() {
        let contents =
            rig_content_to_contents(&[rig_core::completion::message::ToolResultContent::text(
                "plain",
            )]);
        assert_eq!(ext::tool_content_text(&contents), "plain");
    }
}

#[cfg(test)]
mod plan_mode_tests {
    use super::*;
    use crate::tools::Workspace;
    use serde_json::json;

    fn dispatch(root: &std::path::Path, mode: AgentMode) -> ToolDispatch {
        let workspace = Workspace::new(root).expect("workspace");
        workspace.set_plan_path(mode.plan_path().map(|p| p.to_path_buf()));
        workspace.register_with_mode(mode)
    }

    fn write_call(path: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: "t1".into(),
            name: "write".into(),
            input: json!({ "path": path, "content": "hi\n" }),
            thought_signature: None,
        }
    }

    fn result_text(result: &ContentBlock) -> String {
        let ContentBlock::ToolResult { content, .. } = result else {
            panic!("tool result");
        };
        ext::tool_content_text(content)
    }

    async fn run(dispatch: &ToolDispatch, call: ContentBlock) -> ContentBlock {
        match dispatch.execute(call).await.expect("dispatch") {
            DispatchOutcome::Ran(result) | DispatchOutcome::Skipped(result) => result,
            DispatchOutcome::Stopped(reason) => panic!("stopped: {reason}"),
        }
    }

    fn is_error(result: &ContentBlock) -> bool {
        let ContentBlock::ToolResult { is_error, .. } = result else {
            panic!("tool result");
        };
        *is_error
    }

    #[tokio::test]
    async fn plan_mode_blocks_writes_outside_the_plan_file() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("state/plans/alpha.md");
        let dispatch = dispatch(dir.path(), AgentMode::Plan(plan.clone()));
        let result = run(&dispatch, write_call("src/main.rs")).await;
        assert!(is_error(&result), "non-plan write must be blocked");
        assert!(
            result_text(&result).contains(PLAN_WRITE_RESTRICTED),
            "wrong message: {}",
            result_text(&result)
        );
        assert!(
            !dir.path().join("src/main.rs").exists(),
            "blocked write must not touch disk"
        );
    }

    #[tokio::test]
    async fn plan_mode_allows_writing_the_plan_file_itself() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize: macOS tempdirs live under symlinked /var, and the
        // plan walk rejects symlinked components.
        let root = dir.path().canonicalize().unwrap();
        let plan = root.join("state/plans/alpha.md");
        std::fs::create_dir_all(plan.parent().unwrap()).unwrap();
        let dispatch = dispatch(&root, AgentMode::Plan(plan.clone()));
        let result = run(&dispatch, write_call(&plan.display().to_string())).await;
        assert!(
            !is_error(&result),
            "plan write must run: {}",
            result_text(&result)
        );
        assert!(plan.exists());
    }

    #[tokio::test]
    async fn plan_mode_blocks_batch_children_that_write_outside_the_plan() {
        // The plan file is never inside the workspace in practice, but the
        // point here is the child gate: a batch wrapping a non-plan write
        // still fails because children flow through `execute`.
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("state/plans/alpha.md");
        let dispatch = dispatch(dir.path(), AgentMode::Plan(plan));
        let call = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "batch".into(),
            input: json!({ "tool_calls": [ { "tool": "write", "parameters": {
                "path": "src/main.rs", "content": "hi\n"
            } } ] }),
            thought_signature: None,
        };
        // Batch reports partial child failures inside a successful result,
        // so the block message is the evidence, not `is_error`.
        let result = run(&dispatch, call).await;
        assert!(
            result_text(&result).contains(PLAN_WRITE_RESTRICTED),
            "batch child write must be blocked: {}",
            result_text(&result)
        );
        assert!(!dir.path().join("src/main.rs").exists());
    }

    #[tokio::test]
    async fn build_mode_is_unaffected() {
        let dir = tempfile::tempdir().unwrap();
        let dispatch = dispatch(dir.path(), AgentMode::Build);
        let result = run(&dispatch, write_call("src/main.rs")).await;
        assert!(!is_error(&result), "build mode write must run");
        assert!(dir.path().join("src/main.rs").exists());
    }

    #[test]
    fn gated_write_table_covers_registered_writes_only() {
        for name in [
            "write",
            "edit",
            "edit_lines",
            "insert_lines",
            "multiedit",
            "delete",
        ] {
            assert!(is_gated_write(name), "{name} must be gated in plan mode");
        }
        assert!(
            !is_gated_write("fuzzy_replace"),
            "no tool is registered under that name; it must not appear in the gate"
        );
        assert!(!is_gated_write("read"));
        assert!(!is_gated_write("bash"));
    }

    #[tokio::test]
    async fn plan_mode_leaves_reads_alone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let dispatch = dispatch(dir.path(), AgentMode::Plan(dir.path().join("p.md")));
        let call = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "read".into(),
            input: json!({ "path": "f.txt" }),
            thought_signature: None,
        };
        let result = run(&dispatch, call).await;
        assert!(!is_error(&result));
    }
}
