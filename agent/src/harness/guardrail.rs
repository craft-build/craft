//! Dedup layer (05 scope): replay read-only tool calls from the session
//! cache without executing them, record executed results, and invalidate on
//! writes. Doom and the guardrail counters join this layer in 07.

use std::collections::HashSet;
use std::sync::Mutex;

use async_trait::async_trait;

use animist::agent::tool::{ToolContext, ToolOutput};
use animist::agent::{Agent, LayerImpl, Result, RunContext, ToolDecision};
use animist::history::ContentBlock;

use crate::history::ext;
use crate::run::dedup::{self, SharedDedupCache, ToolDedupCache};

pub(crate) struct GuardrailLayer {
    cache: SharedDedupCache,
    /// Calls this run answered from the cache, so `post_tool_exec` does not
    /// re-record them; one set per run, cleared at `turn_start`.
    responded: Mutex<HashSet<String>>,
}

impl GuardrailLayer {
    pub(crate) fn new(cache: SharedDedupCache) -> Self {
        Self {
            cache,
            responded: Mutex::new(HashSet::new()),
        }
    }

    /// Record an executed result and invalidate what the call may have
    /// written; mirrors the bookkeeping the dispatch table did around every
    /// execution.
    fn record(&self, context: &ToolContext<'_>, output: &ToolOutput) {
        let Ok(mut guard) = self.cache.lock() else {
            return;
        };
        let name = context.call.name.as_str();
        let arguments = &context.call.input;
        let read_only = ToolDedupCache::is_read_only(name);
        let dedup_key = read_only.then(|| ToolDedupCache::key(name, arguments));
        let result = ext::tool_result(
            context.call.id.clone(),
            Some(name),
            output.content.clone(),
            output.is_error,
        );
        // Cache inserts run on the raw, pre-transform result so a
        // call-specific transform cannot poison the cache.
        if !output.is_error
            && let Some(key) = dedup_key
            && !output
                .content
                .0
                .iter()
                .any(|item| matches!(item, animist::history::ToolContent::Image { .. }))
        {
            let path = dedup::extract_file_path(arguments);
            guard.insert(key, &result, path.as_deref(), name, arguments);
        }
        // Invalidation runs even on a failed result: apply_patch and delete
        // apply hunks iteratively, so a call that errors midway may still
        // have written. Clearing a cache entry that did not change only
        // costs one re-read.
        if ToolDedupCache::is_write(name) {
            let mut paths = dedup::extract_write_paths(name, arguments);
            if name == "move_file" {
                // `move_file` also rewrites imports elsewhere; those files
                // appear only in the result text.
                let text = ext::tool_content_text(&output.content);
                paths.extend(crate::tools::move_file::rewritten_files(&text));
            }
            if paths.is_empty() {
                guard.invalidate_pathless();
            } else {
                for path in paths {
                    guard.invalidate_path(&path);
                }
            }
        } else if ToolDedupCache::clears_whole_cache(name) {
            guard.clear();
        }
    }
}

#[async_trait]
impl LayerImpl for GuardrailLayer {
    async fn turn_start(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        message: &animist::history::StoredMessage,
    ) -> Result<animist::history::StoredMessage> {
        self.responded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        Ok(message.clone())
    }

    async fn pre_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
    ) -> Result<ToolDecision> {
        let call = context.call;
        if !ToolDedupCache::is_read_only(&call.name) {
            return Ok(ToolDecision::Run);
        }
        let key = ToolDedupCache::key(&call.name, &call.input);
        let cached = self
            .cache
            .lock()
            .ok()
            .and_then(|guard| guard.get(key, &call.name, &call.input).cloned());
        let Some(cached) = cached else {
            return Ok(ToolDecision::Run);
        };
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &cached
        else {
            return Ok(ToolDecision::Run);
        };
        self.responded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(call.id.clone());
        Ok(ToolDecision::Respond {
            output: ToolOutput {
                content: content.clone(),
                is_error: *is_error,
                loaded_tools: Vec::new(),
            },
        })
    }

    async fn post_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput> {
        if self
            .responded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&context.call.id)
        {
            return Ok(output);
        }
        self.record(context, &output);
        Ok(output)
    }
}
