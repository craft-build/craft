//! Tool failures and model-visible results.
//!
//! Craft tools report two distinct failure channels. A model-correctable
//! failure (bad path, denied access, missing file, a fuzzy match that found
//! nothing) is a successful call carrying an `is_error` result whose text is
//! the message the model sees; `Error::Tool` carries exactly that text. Real
//! faults of the tool machinery itself (a blocking worker that panicked, a
//! lock wait interrupted by cancellation) return `Err` and surface as
//! `ToolError::Execution` results.

use animist::agent::tool::{ToolContents, ToolDefinition, ToolOutput};
use snafu::Snafu;

pub(crate) type Result<T> = std::result::Result<T, Error>;

/// One tool call's failure. `Tool` and `Io` failures render as `is_error`
/// outputs with their message text; every other variant is a machinery
/// fault the model cannot correct.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
    /// A model-correctable failure; `message` is the model-visible text,
    /// emitted verbatim.
    #[snafu(display("{message}"))]
    Tool { message: String },

    /// A filesystem failure the model may correct (missing file, denied
    /// access); the display stays the io error's own text, `kind` keeps
    /// the classification callers branch on.
    #[snafu(display("{source}"))]
    Io {
        kind: std::io::ErrorKind,
        source: std::io::Error,
    },

    #[snafu(display("blocking worker failed"))]
    Blocking {
        #[snafu(source(from(tokio::task::JoinError, Box::new)))]
        source: Box<tokio::task::JoinError>,
    },

    #[snafu(display("cancelled while waiting for a file lock"))]
    LockCancelled,
}

/// A successful result rendered as text.
pub(crate) fn text_output(text: impl Into<String>) -> ToolOutput {
    ToolOutput::builder().content(text.into()).build()
}

/// A successful result with mixed content (text and images).
pub(crate) fn contents_output(content: impl Into<ToolContents>) -> ToolOutput {
    ToolOutput::builder().content(content).build()
}

/// A model-correctable failure rendered the way the agent renders an
/// error result: the message as text with `is_error` set.
pub(crate) fn error_result(message: impl Into<String>) -> ToolOutput {
    ToolOutput::builder()
        .content(message.into())
        .is_error(true)
        .build()
}

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::Tool {
        message: message.into(),
    }
}

pub(crate) fn denied(message: impl Into<String>) -> Error {
    Error::Tool {
        message: message.into(),
    }
}

pub(crate) fn failure(message: impl Into<String>) -> Error {
    Error::Tool {
        message: message.into(),
    }
}

pub(crate) fn not_found(message: impl Into<String>) -> Error {
    Error::Tool {
        message: message.into(),
    }
}

pub(crate) fn io_error(error: std::io::Error) -> Error {
    Error::Io {
        kind: error.kind(),
        source: error,
    }
}

/// A typed tool result that renders into its model-visible output.
pub(crate) trait IntoToolResult: Send {
    fn into_tool_result(self) -> ToolOutput;
}

impl IntoToolResult for ToolOutput {
    fn into_tool_result(self) -> ToolOutput {
        self
    }
}

/// Fold one tool execution into its `ToolImpl::call` result: typed outputs
/// render, `Tool` failures become `is_error` outputs, and machinery faults
/// propagate as `Err`.
pub(crate) fn finish<T: IntoToolResult>(outcome: Result<T>) -> Result<ToolOutput> {
    Ok(match outcome {
        Ok(output) => output.into_tool_result(),
        Err(error @ (Error::Tool { .. } | Error::Io { .. })) => error_result(error.to_string()),
        Err(error) => return Err(error),
    })
}

/// The frozen declaration for one tool: a schemars schema for its typed
/// arguments, with the bare-object fallback when serialization fails.
pub(crate) fn definition<A: schemars::JsonSchema>(name: &str, description: &str) -> ToolDefinition {
    let input_schema = serde_json::to_value(schemars::schema_for!(A)).unwrap_or_else(|error| {
        tracing::error!(
            tool = name,
            %error,
            "tool schema serialization failed; serving a bare object schema"
        );
        serde_json::json!({"type": "object"})
    });
    ToolDefinition::builder()
        .name(name)
        .description(description)
        .input_schema(input_schema)
        .build()
}
