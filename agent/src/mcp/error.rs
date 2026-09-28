//! MCP client errors (B.11).

use snafu::Snafu;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum McpError {
    #[snafu(display("server {server} failed to start: {reason}"))]
    StartFailed { server: String, reason: String },

    #[snafu(display("server {server} is not running"))]
    ServerDied { server: String },

    #[snafu(display("server {server} timed out after {timeout_ms}ms"))]
    Timeout { server: String, timeout_ms: u64 },

    #[snafu(display("server {server} returned error {code}: {message}"))]
    RpcError {
        server: String,
        code: i64,
        message: String,
    },

    #[snafu(display("invalid response from server {server}: {reason}"))]
    InvalidResponse { server: String, reason: String },

    #[snafu(display("unknown MCP tool: {name}"))]
    UnknownTool { name: String },

    #[snafu(display("unknown MCP prompt: {name}"))]
    UnknownPrompt { name: String },

    #[snafu(display("config error: {message}"))]
    Config { message: String },

    #[snafu(display("HTTP error from server {server}: {status} {reason}"))]
    HttpError {
        server: String,
        status: u16,
        reason: String,
    },

    #[snafu(display("OAuth failed for server {server}: {reason}"))]
    OAuthFailed { server: String, reason: String },
}
