//! rmcp-backed session adapter: spawn transports, wrap requests with timeouts.
//!
//! The manager speaks only to the [`McpSession`] trait, so tests can substitute
//! fakes without touching rmcp.

mod events;
mod rmcp;

use std::collections::HashMap;

use serde_json::Value;

use super::error::McpError;

pub use self::events::McpEvents;
pub use self::rmcp::{join_content, start_session};
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A live re-list result: refreshed tools, prompts, and resources.
pub struct Listings {
    pub tools: Vec<ToolInfo>,
    pub prompts: Vec<PromptInfo>,
    pub resources: Vec<ResourceInfo>,
}
/// Tool metadata in the manager's own vocabulary.
#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Server-declared `readOnlyHint`, if the tool carried annotations.
    pub read_only_hint: Option<bool>,
    /// Server-declared `destructiveHint`, if the tool carried annotations.
    pub destructive_hint: Option<bool>,
}

/// Prompt metadata in the manager's own vocabulary.
#[derive(Debug, Clone)]
pub struct PromptInfo {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone)]
pub struct PromptArgument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

/// Resource metadata in the manager's own vocabulary.
#[derive(Debug, Clone)]
pub struct ResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
}

/// One message of a rendered prompt.
#[derive(Debug, Clone)]
pub struct PromptMessage {
    pub role: String,
    pub text: Option<String>,
}

/// An image returned by an MCP tool, kept structured so it reaches the model
/// as vision input instead of being flattened into text.
#[derive(Debug, Clone)]
pub struct McpToolImage {
    /// Base64-encoded image bytes.
    pub data: String,
    pub mime: String,
}

/// A tool call's full output, in the server's original block order so text
/// and images stay interleaved as sent.
#[derive(Debug, Clone, Default)]
pub struct McpToolOutput {
    pub parts: Vec<McpPart>,
}

/// One ordered slice of a tool result: text (or inlined resource text) or a
/// retained image.
#[derive(Debug, Clone)]
pub enum McpPart {
    Text(String),
    Image(McpToolImage),
}

impl McpToolOutput {
    /// Whether a text part already carries `value` as JSON. A 2025-06-18
    /// server that returns `structuredContent` also echoes the serialized
    /// JSON in a text block for older clients, so re-appending it would
    /// duplicate the payload.
    fn carries_json(&self, value: &serde_json::Value) -> bool {
        self.parts.iter().any(|part| match part {
            McpPart::Text(text) => serde_json::from_str::<serde_json::Value>(text.trim())
                .is_ok_and(|parsed| parsed == *value),
            McpPart::Image(_) => false,
        })
    }

    /// All text parts joined with newlines — what an error message or a
    /// text-only consumer sees.
    pub fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                McpPart::Text(t) => Some(t.as_str()),
                McpPart::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn has_images(&self) -> bool {
        self.parts.iter().any(|p| matches!(p, McpPart::Image(_)))
    }
}
/// Seam between the manager and a live MCP server session.
pub trait McpSession: Send + Sync {
    fn server_name(&self) -> &str;
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>>;
    fn call_tool(&self, name: &str, args: &Value)
    -> BoxFuture<'_, Result<McpToolOutput, McpError>>;

    /// Phase 6: `call_tool` under the turn's cancellation token. When the
    /// token fires the future resolves with [`McpError::Cancelled`] instead
    /// of parking until the server answers (or the request timeout lapses).
    /// rmcp does not cancel on future drop — the `notifications/cancelled`
    /// must be sent while the request id is still known — so live transport
    /// sessions override this to notify the server; the default only stops
    /// waiting (kept provided so external implementors stay source-compatible).
    fn call_tool_cancellable<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        cancel: &'a crate::run::CancelToken,
    ) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
        race_call_cancel(self, name, args, cancel)
    }
    fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>>;
    fn get_prompt(
        &self,
        name: &str,
        arguments: &HashMap<String, String>,
    ) -> BoxFuture<'_, Result<Vec<PromptMessage>, McpError>>;
    fn shutdown(&self) -> BoxFuture<'_, ()>;

    /// Cached `resources/list` results. Default: none (fakes and servers
    /// without the resources capability report an empty set).
    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<ResourceInfo>, McpError>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// `resources/read`: text inline, blobs as base64 with a mime header
    /// line. Default: unknown-resource error so fakes never fake a read.
    fn read_resource<'a>(&'a self, uri: &'a str) -> BoxFuture<'a, Result<String, McpError>> {
        Box::pin(async {
            Err(McpError::UnknownResource {
                uri: uri.to_string(),
            })
        })
    }

    /// Re-fetch tool/prompt/resource listings from the live server and update the
    /// session's cache. Servers notify `list_changed` after connect, so the
    /// connect-time snapshot goes stale; the manager refreshes instead of
    /// paying a full reconnect. Default: report the cached values (fakes and
    /// read-mostly servers are unaffected).
    fn refresh_listings(&self) -> BoxFuture<'_, Result<Listings, McpError>> {
        Box::pin(async {
            let tools = self.list_tools().await?;
            let prompts = self.list_prompts().await?;
            let resources = self.list_resources().await?;
            Ok(Listings {
                tools,
                prompts,
                resources,
            })
        })
    }
}

/// Race a plain [`McpSession::call_tool`] against a cancel token: the body
/// the manager's test fakes share, since only real transports can notify
/// the server.
pub(crate) fn race_call_cancel<'a, S: McpSession + ?Sized>(
    session: &'a S,
    name: &'a str,
    args: &'a Value,
    cancel: &'a crate::run::CancelToken,
) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
    Box::pin(async move {
        let call = session.call_tool(name, args);
        tokio::select! {
            result = call => result,
            _ = cancel.wait() => Err(McpError::Cancelled {
                server: session.server_name().to_string(),
            }),
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Phase 6, unit: cancelling the token resolves a pending
    /// `call_tool_cancellable` promptly — before the server answers and
    /// well before any request timeout — with `McpError::Cancelled`.
    #[tokio::test]
    async fn cancelling_the_token_resolves_a_pending_call_promptly() {
        struct HangingSession;
        impl McpSession for HangingSession {
            fn server_name(&self) -> &str {
                "hang"
            }
            fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolInfo>, McpError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn call_tool(
                &self,
                name: &str,
                _args: &Value,
            ) -> BoxFuture<'_, Result<McpToolOutput, McpError>> {
                let name = name.to_string();
                Box::pin(async move {
                    // Never answers: only cancellation can end this call.
                    std::future::pending::<()>().await;
                    unreachable!("pending never resolves for {name}");
                })
            }
            fn call_tool_cancellable<'a>(
                &'a self,
                name: &'a str,
                args: &'a Value,
                cancel: &'a crate::run::CancelToken,
            ) -> BoxFuture<'a, Result<McpToolOutput, McpError>> {
                race_call_cancel(self, name, args, cancel)
            }
            fn list_prompts(&self) -> BoxFuture<'_, Result<Vec<PromptInfo>, McpError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn get_prompt(
                &self,
                _name: &str,
                _arguments: &HashMap<String, String>,
            ) -> BoxFuture<'_, Result<Vec<PromptMessage>, McpError>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn shutdown(&self) -> BoxFuture<'_, ()> {
                Box::pin(async {})
            }
        }

        let session = HangingSession;
        let (flag, token) = crate::run::cancel_channel();
        let args = serde_json::json!({});
        let call = session.call_tool_cancellable("tool", &args, &token);
        tokio::pin!(call);
        // Give the call a beat to park on the (never-answering) session,
        // then cancel the turn.
        tokio::time::sleep(Duration::from_millis(50)).await;
        flag.set(true);
        let result = tokio::time::timeout(Duration::from_secs(2), &mut call)
            .await
            .expect("cancelled call resolves promptly");
        assert!(
            matches!(result, Err(McpError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
    }
}
