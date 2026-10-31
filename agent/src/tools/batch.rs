//! Concurrent execution of independent tool calls within one turn.
//!
//! Children are dispatched through [`ToolContext::dispatch_all`], so they
//! run under the same authorization pipeline as top-level calls: the
//! registry's scheduling claims bound them into conflict-free waves, and
//! the layer chain (approval, dedup, guardrails) applies to each entry.
//! Nesting (batch inside batch) is rejected by the dispatch rules and here.

use std::fmt::Write as _;

use animist::agent::tool::{ToolCall, ToolContext, ToolDefinition, ToolImpl, ToolOutput};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::error::{Error, Result, definition, finish, invalid};
use crate::history::ext;

pub const MAX_BATCH_SIZE: usize = 25;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchArgs {
    /// Tool calls to run in parallel. Each entry is
    /// `{"tool": "...", "parameters": {...}}` or flat
    /// `{"tool": "...", ...params}`.
    pub tool_calls: Vec<Map<String, Value>>,
}

#[derive(Debug)]
pub struct BatchOutput {
    pub text: String,
}

impl super::error::IntoToolResult for BatchOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::text_output(self.text)
    }
}

/// Normalize one entry: extract `tool`, and produce the child arguments from
/// `parameters` and/or flat fields. Duplicate keys across both are rejected,
/// mirroring the reference's custom deserializer.
fn normalize_entry(entry: &Map<String, Value>) -> Result<(String, Value)> {
    let tool = match entry.get("tool") {
        Some(Value::String(name)) => name.clone(),
        Some(_) => return Err(invalid("'tool' must be a string")),
        None => return Err(invalid("batch entry is missing 'tool'")),
    };
    let parameters = entry.get("parameters").cloned();
    let rest: Map<String, Value> = entry
        .iter()
        .filter(|(key, _)| key.as_str() != "tool" && key.as_str() != "parameters")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let arguments = match parameters {
        Some(Value::Object(mut object)) => {
            for (key, value) in rest {
                if object.contains_key(&key) {
                    return Err(invalid(format!(
                        "duplicate parameter '{key}' in both 'parameters' and flat fields"
                    )));
                }
                object.insert(key, value);
            }
            Value::Object(object)
        }
        Some(_) if !rest.is_empty() => {
            return Err(invalid(
                "'parameters' must be an object when flat fields are also present",
            ));
        }
        Some(other) => other,
        None if !rest.is_empty() => Value::Object(rest),
        None => return Err(invalid("batch entry is missing 'parameters'")),
    };
    Ok((tool, arguments))
}

/// The fan-out tool: dispatches its entries as child calls under the
/// top-level wave rules.
#[derive(Clone, Copy, Default)]
pub struct Batch;

impl Batch {
    async fn execute(&self, context: &ToolContext<'_>, args: BatchArgs) -> Result<BatchOutput> {
        if args.tool_calls.is_empty() {
            return Err(invalid("provide at least one tool call"));
        }
        let mut entries = Vec::with_capacity(args.tool_calls.len());
        for raw in &args.tool_calls {
            let (tool, arguments) = normalize_entry(raw)?;
            if tool == "batch" {
                return Err(invalid("cannot nest batch inside batch"));
            }
            entries.push((tool, arguments));
        }

        let active = &entries[..entries.len().min(MAX_BATCH_SIZE)];
        let discarded = &entries[active.len()..];

        let calls = active
            .iter()
            .enumerate()
            .map(|(index, (tool, arguments))| {
                ToolCall::builder()
                    .id(format!("batch-{index}"))
                    .name(tool.clone())
                    .input(arguments.clone())
                    .build()
            })
            .collect();
        // Children run under the agent's own pipeline: layers, claims, and
        // bounded waves. A fatal dispatch error (cancellation, budget, a
        // layer stop) aborts the batch and reports to the model.
        let results = context
            .dispatch_all(calls)
            .await
            .map_err(|error| invalid(format!("batch dispatch failed: {error}")))?;

        let mut failed = discarded.len();
        let mut output = String::new();
        for ((tool, _), result) in active.iter().zip(&results) {
            let _ = writeln!(output, "## {tool}");
            if result.is_error {
                failed += 1;
                let _ = write!(
                    output,
                    "[ERROR] {}",
                    ext::tool_content_text(&result.content)
                );
            } else {
                output.push_str(&ext::tool_content_text(&result.content));
            }
            output.push_str("\n\n");
        }
        for (tool, _) in discarded {
            let _ = write!(
                output,
                "## {tool}\n[ERROR] maximum of {MAX_BATCH_SIZE} tools per batch\n\n"
            );
        }

        let total = entries.len();
        let succeeded = total - failed;
        if failed > 0 {
            let _ = write!(
                output,
                "Executed {succeeded}/{total} successfully. {failed} failed."
            );
        } else {
            let _ = write!(output, "All {total} tools executed successfully.");
        }

        Ok(BatchOutput { text: output })
    }
}

#[async_trait]
impl ToolImpl for Batch {
    type Args = BatchArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<BatchArgs>(
            "batch",
            "Executes up to 25 independent tool calls concurrently in one turn, reducing \
             round-trips. Children run through the same permission pipeline as normal \
             calls. Use for parallel independent calls (e.g. multiple reads, globs, \
             greps); never for dependent operations. Do not nest batch inside batch. \
             Entries take {\"tool\": name, \"parameters\": {...}} or flat \
             {\"tool\": name, ...params}. Partial failures do not stop other calls.",
        )
    }

    // Exclusive, as dispatch_all requires: the fan-out owns its wave.

    async fn call(&self, context: &ToolContext<'_>, args: Self::Args) -> Result<ToolOutput> {
        finish(self.execute(context, args).await)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(value: Value) -> Map<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    async fn invoke(
        workspace: &crate::tools::Workspace,
        args: BatchArgs,
    ) -> animist::agent::tool::ToolOutput {
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .tools(workspace.registry().expect("registry"))
            .build()
            .expect("harness");
        harness
            .call(Batch, serde_json::json!({ "tool_calls": args.tool_calls }))
            .await
            .expect("batch outcome")
    }

    #[tokio::test]
    async fn empty_batch_is_an_error() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        let output = invoke(&workspace, BatchArgs { tool_calls: vec![] }).await;
        assert!(output.is_error);
        assert!(invoke_text(output).contains("at least one"));
    }

    #[tokio::test]
    async fn nested_batch_is_rejected() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        let output = invoke(
            &workspace,
            BatchArgs {
                tool_calls: vec![entry(json!({"tool": "batch", "parameters": {}}))],
            },
        )
        .await;
        assert!(output.is_error);
        assert!(invoke_text(output).contains("nest"));
    }

    fn invoke_text(output: animist::agent::tool::ToolOutput) -> String {
        crate::history::ext::tool_content_text(&output.content)
    }

    #[test]
    fn flat_entry_normalizes_to_nested() {
        let (tool, arguments) = normalize_entry(&entry(
            json!({"tool": "glob", "path": "/tmp", "pattern": "*.rs"}),
        ))
        .unwrap();
        assert_eq!(tool, "glob");
        assert_eq!(arguments["path"], "/tmp");
        assert_eq!(arguments["pattern"], "*.rs");
    }

    #[test]
    fn nested_entry_passes_through() {
        let (tool, arguments) = normalize_entry(&entry(
            json!({"tool": "glob", "parameters": {"path": "/tmp", "pattern": "*.rs"}}),
        ))
        .unwrap();
        assert_eq!(tool, "glob");
        assert_eq!(arguments["pattern"], "*.rs");
    }

    #[test]
    fn mixed_entries_merge() {
        let (_, arguments) = normalize_entry(&entry(
            json!({"tool": "glob", "parameters": {"path": "/tmp"}, "pattern": "*.rs"}),
        ))
        .unwrap();
        assert_eq!(arguments["path"], "/tmp");
        assert_eq!(arguments["pattern"], "*.rs");
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let error = normalize_entry(&entry(
            json!({"tool": "glob", "parameters": {"pattern": "*.rs"}, "pattern": "*.txt"}),
        ))
        .unwrap_err();
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn missing_tool_or_params_is_rejected() {
        assert!(normalize_entry(&entry(json!({"parameters": {"path": "/tmp"}}))).is_err());
        assert!(normalize_entry(&entry(json!({}))).is_err());
    }

    #[tokio::test]
    async fn mixed_results_are_reported_per_entry() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        std::fs::write(workspace.root().join("a.txt"), "content").unwrap();
        let output = invoke(
            &workspace,
            BatchArgs {
                tool_calls: vec![
                    entry(json!({"tool": "read", "parameters": {"path": "a.txt"}})),
                    entry(json!({"tool": "read", "parameters": {"path": "missing.txt"}})),
                ],
            },
        )
        .await;
        let text = invoke_text(output);
        assert!(text.contains("## read"), "{text}");
        assert!(text.contains("content"), "{text}");
        assert!(text.contains("[ERROR]"), "{text}");
        assert!(
            text.contains("Executed 1/2 successfully. 1 failed."),
            "{text}"
        );
    }

    #[tokio::test]
    async fn excess_calls_are_discarded_with_errors() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        let calls: Vec<_> = (0..MAX_BATCH_SIZE + 2)
            .map(|_| entry(json!({"tool": "list", "parameters": {"path": "."}})))
            .collect();
        let output = invoke(&workspace, BatchArgs { tool_calls: calls }).await;
        let text = invoke_text(output);
        assert_eq!(
            text.matches("maximum of").count(),
            2,
            "two excess entries are discarded"
        );
        assert!(text.contains("2 failed"), "{text}");
    }

    #[tokio::test]
    async fn concurrent_writes_to_one_file_serialize_atomically() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        let output = invoke(
            &workspace,
            BatchArgs {
                tool_calls: vec![
                    entry(
                        json!({"tool": "write", "parameters": {"path": "same.txt", "content": "aaaa"}}),
                    ),
                    entry(
                        json!({"tool": "write", "parameters": {"path": "same.txt", "content": "bbbb"}}),
                    ),
                ],
            },
        )
        .await;
        let text = invoke_text(output);
        assert!(
            text.contains("All 2 tools executed successfully."),
            "{text}"
        );
        let contents = std::fs::read_to_string(workspace.root().join("same.txt")).unwrap();
        assert!(
            contents == "aaaa" || contents == "bbbb",
            "interleaved write: {contents:?}"
        );
    }
}
