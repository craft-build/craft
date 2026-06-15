//! Lifecycle hooks: an opt-in extension point that lets an external runtime
//! (the Lua plugin VM) react to agent events. craft-agent defines the trait and
//! the dispatch sites; the implementation that bridges to Lua lives in
//! `craft-lua`, wired in by the consumer. This mirrors the `InterruptSource`
//! trait pattern so craft-agent stays free of a hard craft-lua dependency.
//!
//! Events:
//! - `session_start` — best-effort, once at the start of an agent run.
//! - `pre_tool_use` — blocking, before a tool executes. Can `Deny` (returns the
//!   message to the agent) or `Transform` (replaces the raw args).
//! - `post_tool_use` — best-effort, read-only, after a tool returns.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

/// A boxed, sendable future returned by hook methods. The lifetime borrows the
/// `Hooks` impl, matching the `Provider::stream_message` convention.
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What the agent was about to do (or just did) with a tool.
#[derive(Debug, Clone)]
pub struct ToolUseEvent {
    pub tool: String,
    pub input: Value,
}

/// A `pre_tool_use` hook's verdict. `Deny` surfaces `message` to the agent as a
/// tool error; `Transform` swaps in a new raw input before parsing.
#[derive(Debug, Clone)]
pub enum HookDecision {
    Allow,
    Deny { message: String },
    Transform { input: Value },
}

/// Lifecycle hook bridge. All methods have default no-op implementations so the
/// agent can call unconditionally; dispatch is skipped when no bridge is wired in.
pub trait Hooks: Send + Sync {
    fn session_start(&self) -> HookFuture<'_, ()> {
        Box::pin(async {})
    }

    fn pre_tool_use(&self, event: ToolUseEvent) -> HookFuture<'_, HookDecision> {
        let _ = event;
        Box::pin(async { HookDecision::Allow })
    }

    fn post_tool_use(
        &self,
        event: ToolUseEvent,
        output: String,
        is_error: bool,
    ) -> HookFuture<'_, ()> {
        let _ = (event, output, is_error);
        Box::pin(async {})
    }
}

#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records every event it receives, for assertions in craft-agent tests.
    pub struct RecordingHooks {
        pub events: Mutex<Vec<String>>,
        pub pre_decision: Mutex<HookDecision>,
    }

    impl RecordingHooks {
        pub fn shared() -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
                pre_decision: Mutex::new(HookDecision::Allow),
            })
        }

        pub fn with_decision(decision: HookDecision) -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Vec::new()),
                pre_decision: Mutex::new(decision),
            })
        }

        pub fn record(&self, s: impl Into<String>) {
            self.events.lock().unwrap().push(s.into());
        }

        pub fn snapshot(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    impl Hooks for RecordingHooks {
        fn session_start(&self) -> HookFuture<'_, ()> {
            self.record("session_start");
            Box::pin(async {})
        }
        fn pre_tool_use(&self, event: ToolUseEvent) -> HookFuture<'_, HookDecision> {
            self.record(format!("pre:{}", event.tool));
            let decision = self.pre_decision.lock().unwrap().clone();
            Box::pin(async move { decision })
        }
        fn post_tool_use(
            &self,
            event: ToolUseEvent,
            _output: String,
            is_error: bool,
        ) -> HookFuture<'_, ()> {
            self.record(format!("post:{}:{}", event.tool, is_error));
            Box::pin(async {})
        }
    }
}
