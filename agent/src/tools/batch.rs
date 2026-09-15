//! Concurrent execution of independent tool calls within one turn.
//!
//! Children are dispatched through the same [`ToolDispatch`] pipeline as
//! top-level calls, so the approval gate, dedup cache, and guardrails apply
//! to each entry. Nesting (batch inside batch) is rejected. Note that
//! workspace tools serialize on the shared workspace lock, so parallelism
//! covers dispatch and non-filesystem work; the write-conflict barrier is
//! a separate concern.

use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use futures::future::join_all;
use rig_core::tool::{IntoToolOutput, PortableTool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

use super::{Result, invalid};
use crate::history::{ToolCall, ToolFunction, ToolResultContent};
use crate::run::{DispatchOutcome, ToolDispatch};

pub const MAX_BATCH_SIZE: usize = 25;

/// The dispatch table the batch tool forwards children to, filled by
/// `Workspace::register` after the table is built (the tool is part of it).
pub type SharedDispatch = Arc<OnceLock<ToolDispatch>>;

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

impl IntoToolOutput for BatchOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        Ok(ToolOutput::text(self.text))
    }
}

#[derive(Clone)]
pub struct Batch(pub SharedDispatch);

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

impl Batch {
    async fn execute(&self, args: BatchArgs) -> Result<BatchOutput> {
        if args.tool_calls.is_empty() {
            return Err(invalid("provide at least one tool call"));
        }
        let mut entries = Vec::with_capacity(args.tool_calls.len());
        for raw in &args.tool_calls {
            let (tool, arguments) = normalize_entry(raw)?;
            if tool == Self::NAME {
                return Err(invalid("cannot nest batch inside batch"));
            }
            entries.push((tool, arguments));
        }

        let dispatch = self.0.get().cloned().ok_or_else(|| {
            invalid("batch tool is not wired to a dispatch table in this session")
        })?;

        let active = &entries[..entries.len().min(MAX_BATCH_SIZE)];
        let discarded = &entries[active.len()..];

        let calls = active.iter().enumerate().map(|(index, (tool, arguments))| {
            let dispatch = dispatch.clone();
            let call = ToolCall {
                id: format!("batch-{index}"),
                function: ToolFunction {
                    name: tool.clone(),
                    arguments: arguments.clone(),
                },
            };
            async move {
                let (text, is_error) = match dispatch.execute(call).await {
                    Ok(DispatchOutcome::Ran(result)) | Ok(DispatchOutcome::Skipped(result)) => (
                        result
                            .content
                            .iter()
                            .map(ToolResultContent::to_text)
                            .collect::<Vec<_>>()
                            .join("\n"),
                        result.is_error,
                    ),
                    Ok(DispatchOutcome::Stopped(reason)) => {
                        (format!("run stopped: {reason}"), true)
                    }
                    Err(error) => (error, true),
                };
                (text, is_error)
            }
        });
        let results = join_all(calls).await;

        let mut failed = discarded.len();
        let mut output = String::new();
        for ((tool, _), (text, is_error)) in active.iter().zip(&results) {
            let _ = writeln!(output, "## {tool}");
            if *is_error {
                failed += 1;
                let _ = write!(output, "[ERROR] {text}");
            } else {
                output.push_str(text);
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

impl PortableTool for Batch {
    const NAME: &'static str = "batch";
    type Args = BatchArgs;
    type Output = BatchOutput;
    type Error = rig_core::tool::ToolExecutionError;

    fn description(&self) -> String {
        "Executes up to 25 independent tool calls concurrently in one turn, reducing \
         round-trips. Children run through the same permission pipeline as normal \
         calls. Use for parallel independent calls (e.g. multiple reads, globs, \
         greps); never for dependent operations. Do not nest batch inside batch. \
         Entries take {\"tool\": name, \"parameters\": {...}} or flat \
         {\"tool\": name, ...params}. Partial failures do not stop other calls."
            .into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(BatchArgs)).expect("JSON Schema is serializable")
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output> {
        // Deliberately not serialized on the workspace lock: children acquire
        // it individually, and holding it here would deadlock every entry.
        self.execute(args).await
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    fn entry(value: Value) -> Map<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    #[tokio::test]
    async fn empty_batch_is_an_error() {
        let batch = Batch(Arc::new(OnceLock::new()));
        let error = batch
            .call(BatchArgs { tool_calls: vec![] })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("at least one"));
    }

    #[tokio::test]
    async fn nested_batch_is_rejected() {
        let batch = Batch(Arc::new(OnceLock::new()));
        let error = batch
            .call(BatchArgs {
                tool_calls: vec![entry(json!({"tool": "batch", "parameters": {}}))],
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("nest"));
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
        fs::write(workspace.root().join("a.txt"), "content").unwrap();
        let dispatch = workspace.register();
        let batch = Batch(Arc::new(OnceLock::new()));
        assert!(batch.0.set(dispatch).is_ok());

        let output = batch
            .call(BatchArgs {
                tool_calls: vec![
                    entry(json!({"tool": "read", "parameters": {"path": "a.txt"}})),
                    entry(json!({"tool": "read", "parameters": {"path": "missing.txt"}})),
                ],
            })
            .await
            .unwrap();
        assert!(output.text.contains("## read"));
        assert!(output.text.contains("content"));
        assert!(output.text.contains("[ERROR]"));
        assert!(output.text.contains("Executed 1/2 successfully. 1 failed."));
    }

    #[tokio::test]
    async fn excess_calls_are_discarded_with_errors() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        let dispatch = workspace.register();
        let batch = Batch(Arc::new(OnceLock::new()));
        assert!(batch.0.set(dispatch).is_ok());

        let calls: Vec<_> = (0..MAX_BATCH_SIZE + 2)
            .map(|_| entry(json!({"tool": "list", "parameters": {"path": "."}})))
            .collect();
        let output = batch.call(BatchArgs { tool_calls: calls }).await.unwrap();
        assert_eq!(
            output.text.matches("maximum of").count(),
            2,
            "two excess entries are discarded"
        );
        assert!(output.text.contains("2 failed"));
    }

    #[tokio::test]
    async fn children_go_through_the_approval_gate() {
        use crate::run::{BeforeExecute, Decision};

        struct DenyAll;
        impl BeforeExecute for DenyAll {
            fn decide(&self, call: crate::history::ToolCall) -> crate::run::BoxFuture<Decision> {
                Box::pin(std::future::ready(Decision::Skip(format!(
                    "denied: {}",
                    call.function.name
                ))))
            }
        }

        let (_dir, workspace) = crate::tools::tests::workspace();
        let dispatch = workspace.register().with_before(Arc::new(DenyAll));
        let batch = Batch(Arc::new(OnceLock::new()));
        assert!(batch.0.set(dispatch).is_ok());

        let output = batch
            .call(BatchArgs {
                tool_calls: vec![entry(json!({"tool": "list", "parameters": {"path": "."}}))],
            })
            .await
            .unwrap();
        assert!(output.text.contains("denied: list"));
    }

    #[tokio::test]
    async fn concurrent_writes_to_one_file_serialize_atomically() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        let dispatch = workspace.register();
        let batch = Batch(Arc::new(OnceLock::new()));
        assert!(batch.0.set(dispatch).is_ok());

        let output = batch
            .call(BatchArgs {
                tool_calls: vec![
                    entry(json!({"tool": "write", "parameters": {"path": "same.txt", "content": "aaaa"}})),
                    entry(json!({"tool": "write", "parameters": {"path": "same.txt", "content": "bbbb"}})),
                ],
            })
            .await
            .unwrap();
        assert!(output.text.contains("All 2 tools executed successfully."));
        let contents = fs::read_to_string(workspace.root().join("same.txt")).unwrap();
        assert!(
            contents == "aaaa" || contents == "bbbb",
            "interleaved write: {contents:?}"
        );
    }
}
