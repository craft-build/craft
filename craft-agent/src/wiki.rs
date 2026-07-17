//! Wiki helpers that need a provider. `WikiStore` itself (in `craft-storage`)
//! is pure and sync; this module owns the LLM summarization call used by the
//! `craft wiki ingest` CLI and by the agent tools.

use serde_json::Value;

use craft_providers::{Message, Model, RequestOptions, provider::Provider};
use craft_storage::id::SessionRef;

use crate::AgentError;

const SUMMARIZE_SYSTEM: &str = "\
You are summarizing a document for a local project wiki.\n\
Produce at most two concise sentences capturing what the document is about and its key points. \
Reply with only the summary, no preamble.";

/// Summarize `text` into at most two sentences via a one-shot model call.
/// The text is truncated by the caller to bound cost.
pub async fn summarize(
    provider: &dyn Provider,
    model: &Model,
    text: &str,
    session_id: Option<&SessionRef>,
) -> Result<String, AgentError> {
    let (ptx, _prx) = flume::unbounded();
    let messages = vec![Message::user(text.to_string())];
    let tools = Value::Array(vec![]);
    let response = provider
        .stream_message(
            model,
            &messages,
            SUMMARIZE_SYSTEM,
            &tools,
            &ptx,
            RequestOptions {
                fast: true,
                ..Default::default()
            }
            .clamped(model),
            session_id,
        )
        .await?;
    Ok(response
        .message
        .user_text()
        .unwrap_or_default()
        .trim()
        .to_string())
}
