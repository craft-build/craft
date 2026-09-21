//! `websearch` tool: Exa-backed web search (task #57, A.4).
//!
//! Ported from the reference `plugins/websearch/init.lua` + `parse_sse.lua`:
//! same MCP endpoint, JSON-RPC `web_search_exa` call with `type: auto` /
//! `livecrawl: fallback`, optional `EXA_API_KEY` via `x-api-key`, 25s
//! timeout, 5MB cap, and SSE-stream response parsing that extracts the first
//! non-empty `result.content[1].text` payload.

use std::time::Duration;

use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use super::bash::wrap_untrusted;
use super::{MAX_OUTPUT_BYTES, Result, invalid};

const EXA_MCP_ENDPOINT: &str = "https://mcp.exa.ai/mcp";
const REQUEST_TIMEOUT_SECS: u64 = 25;
const DEFAULT_NUM_RESULTS: u32 = 8;
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const NO_RESULTS_MSG: &str = "No search results found";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebsearchArgs {
    /// Search query
    pub query: String,
    /// Number of results to return (default 8)
    pub num_results: Option<u32>,
}

#[derive(Debug)]
pub struct WebsearchOutput {
    pub text: String,
}

impl IntoToolOutput for WebsearchOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

/// Search the web via Exa AI's MCP endpoint and return the result text.
#[derive(Clone, Copy, Default)]
pub struct Websearch;

impl PortableTool for Websearch {
    const NAME: &'static str = "websearch";
    type Args = WebsearchArgs;
    type Output = WebsearchOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        let today = jiff::Zoned::now().strftime("%Y-%m-%d");
        format!(
            "Search the web for real-time information using Exa AI.\n\n\
             Today's date is {today}.\n\n\
             - Use for current events, documentation, APIs, or anything not in local files.\n\
             - Prefer specific, targeted queries over broad ones.\n\
             - Results include page titles, URLs, and content snippets."
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(WebsearchArgs))
            .expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        if args.query.trim().is_empty() {
            return Err(invalid("query is required"));
        }
        let num_results = args.num_results.unwrap_or(DEFAULT_NUM_RESULTS);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| invalid(format!("client error: {e}")))?;
        let mut request = client
            .post(EXA_MCP_ENDPOINT)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Ok(api_key) = std::env::var("EXA_API_KEY") {
            request = request.header("x-api-key", api_key);
        }
        let request = request
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "web_search_exa",
                    "arguments": {
                        "query": args.query,
                        "numResults": num_results,
                        "type": "auto",
                        "livecrawl": "fallback",
                    },
                },
            }))
            .build()
            .map_err(|e| invalid(format!("request build error: {e}")))?;

        let response = client
            .execute(request)
            .await
            .map_err(|e| invalid(format!("request failed: {e}")))?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let preview: String = response
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            return Err(invalid(format!("HTTP {status}: {preview}")));
        }

        let body = response
            .bytes()
            .await
            .map_err(|e| invalid(format!("read error: {e}")))?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(invalid(format!(
                "response too large: over {MAX_RESPONSE_BYTES} bytes"
            )));
        }

        let text = parse_sse_response(&String::from_utf8_lossy(&body)).map_err(invalid)?;
        Ok(WebsearchOutput {
            text: wrap_untrusted(&truncate_output(&text)),
        })
    }
}

/// Walk `data: ` SSE lines and return the first JSON-RPC payload whose
/// `result.content[0].text` is a non-empty string. Ported from the
/// reference `parse_sse.lua`.
fn parse_sse_response(body: &str) -> std::result::Result<String, String> {
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let parsed: serde_json::Value =
            serde_json::from_str(data).map_err(|e| format!("SSE JSON parse error: {e}"))?;
        if let Some(text) = extract_text(&parsed) {
            return Ok(text);
        }
    }
    Ok(NO_RESULTS_MSG.to_string())
}

fn extract_text(parsed: &serde_json::Value) -> Option<String> {
    let text = parsed
        .get("result")?
        .get("content")?
        .get(0)?
        .get("text")?
        .as_str()?;
    (!text.is_empty()).then(|| text.to_string())
}

fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n…[truncated at {} bytes]",
        &text[..cut],
        MAX_OUTPUT_BYTES
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(text: &str) -> String {
        format!("event: message\ndata: {text}\n\n")
    }

    #[test]
    fn parse_sse_extracts_first_result_text() {
        let body = format!(
            "{}{}",
            sse(r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":""}]}}"#),
            sse(
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Result 1\nResult 2"}]}}"#
            )
        );
        assert_eq!(parse_sse_response(&body).unwrap(), "Result 1\nResult 2");
    }

    #[test]
    fn parse_sse_skips_non_data_and_malformed_shapes() {
        let body = concat!(
            ": keep-alive\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"message\":\"nope\"}}\n",
        );
        assert_eq!(parse_sse_response(body).unwrap(), NO_RESULTS_MSG);
    }

    #[test]
    fn parse_sse_empty_body_means_no_results() {
        assert_eq!(parse_sse_response("").unwrap(), NO_RESULTS_MSG);
        assert_eq!(
            parse_sse_response("event: message\n").unwrap(),
            NO_RESULTS_MSG
        );
    }

    #[test]
    fn parse_sse_rejects_invalid_json() {
        let err = parse_sse_response("data: {not json}\n").unwrap_err();
        assert!(err.starts_with("SSE JSON parse error"));
    }

    #[test]
    fn truncate_output_respects_cap_and_boundaries() {
        assert_eq!(truncate_output("short"), "short");
        let long = "ä".repeat(MAX_OUTPUT_BYTES); // 2 bytes per char
        let truncated = truncate_output(&long);
        assert!(truncated.len() < long.len());
        assert!(truncated.ends_with(&format!("…[truncated at {MAX_OUTPUT_BYTES} bytes]")));
    }

    #[tokio::test]
    async fn rejects_empty_query_before_any_network() {
        let error = Websearch
            .call(WebsearchArgs {
                query: "   ".into(),
                num_results: None,
            })
            .await
            .expect_err("empty query");
        assert!(error.to_string().contains("query is required"));
    }
}
