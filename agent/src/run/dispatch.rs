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
use crate::compression::store::SharedCompressionStore;

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
        }
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
        let dedup_key = read_only.then(|| ToolDedupCache::key(&name, &call.function.arguments));
        let cached = if let (Some(cache), Some(key)) = (&self.dedup, dedup_key) {
            cache.lock().ok().and_then(|guard| guard.get(key).cloned())
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
            if let Some(key) = dedup_key {
                let path = dedup::extract_file_path(&call.function.arguments);
                guard.insert(key, &result, path.as_deref());
            } else if ToolDedupCache::is_write(&name) {
                for path in dedup::extract_write_paths(&name, &call.function.arguments) {
                    guard.invalidate_path(&path);
                }
            }
        }
        if let Some(after) = &self.after {
            result = after.transform(call.clone(), result).await;
        }
        Ok(DispatchOutcome::Ran(result))
    }
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
            rig_core::completion::message::ToolResultContent::Image(_) => {
                history::ToolResultContent::text("[image content]")
            }
        })
        .collect()
}
