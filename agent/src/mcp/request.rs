//! Server → client MCP requests (elicitation) relayed from the rmcp handler
//! to whichever frontend started the manager.
//!
//! The handler cannot answer these itself: elicitation needs a form. So it
//! forwards the request over this channel and awaits the oneshot reply. When
//! no frontend has taken the receiver (headless paths drop it), the send
//! fails and the handler answers the server with a clean denial so it can
//! degrade.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::oneshot;

/// One server-initiated request awaiting a frontend's answer.
///
/// The reply oneshot carries either the MCP result payload or a message to
/// be turned into an MCP error result (`-32603`). A dropped reply (frontend
/// went away mid-request) is also treated as a denial by the handler.
pub enum McpServerRequest {
    /// `elicitation/create`: collect structured user input per the schema.
    Elicitate {
        server: Arc<str>,
        message: String,
        schema: rmcp::model::ElicitationSchema,
        reply: oneshot::Sender<Result<ElicitOutcome, String>>,
    },
}

/// The user's answer to an elicitation form.
#[derive(Debug)]
pub enum ElicitOutcome {
    /// The form was submitted; the object maps field → value.
    Accept(Value),
    /// The form was dismissed without an answer.
    Decline,
}
