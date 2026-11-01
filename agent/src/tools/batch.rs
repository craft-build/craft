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
        crate::tools::error::text_output(self.text)
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
        // bounded waves. A budget or nesting limit is reserved for the
        // whole batch upfront, so it is every entry's error, not the run's:
        // per-entry text lets the model split the fan-out and retry. Any
        // other dispatch failure (cancellation, a layer stop) is fatal and
        // propagates as the batch's own error.
        let results = match context.dispatch_all(calls).await {
            Ok(results) => results,
            Err(
                error @ (animist::agent::AgentError::ChildCallLimitExceeded { .. }
                | animist::agent::AgentError::ToolDepthExceeded { .. }),
            ) => (0..active.len())
                .map(|_| {
                    ToolOutput::builder()
                        .content(error.to_string())
                        .is_error(true)
                        .build()
                })
                .collect(),
            Err(error) => {
                return Err(invalid(format!("batch dispatch failed: {error}")));
            }
        };

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
            "Executes up to 25 independent tool calls in one fan-out and returns a single \
             per-entry report. Independent tool calls emitted directly in one reply also \
             run in parallel, so prefer plain parallel calls for a handful of operations \
             and reach for batch for larger fan-outs that benefit from one summarized \
             result. Children run through the same permission pipeline as normal calls. \
             Never use for dependent operations. Do not nest batch inside batch. Entries \
             take {\"tool\": name, \"parameters\": {...}} or flat \
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
    use std::sync::Arc;

    use animist::agent::tool::{ToolRegistry, ToolScheduling};
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
            .tools(
                workspace
                    .registry(&animist::history::History::new())
                    .expect("registry"),
            )
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
        assert!(invoke_text(&output).contains("at least one"));
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
        assert!(invoke_text(&output).contains("nest"));
    }

    fn invoke_text(output: &animist::agent::tool::ToolOutput) -> String {
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

    /// The repeat-failure/no-progress guardrail counters must see batch
    /// children: children ride the agent pipeline, so the guardrail layer's
    /// post-hook records their results. The reads alternate between two
    /// files with identical contents — identical *inputs* would trip the
    /// doom window (which also covers children now) before the no-progress
    /// counter could speak.
    #[tokio::test]
    async fn children_feed_guardrail_counters() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        std::fs::write(workspace.root().join("f"), "same\n").unwrap();
        std::fs::write(workspace.root().join("g"), "same\n").unwrap();
        let read = |file: &str| entry(json!({"tool": "read", "parameters": {"path": file}}));
        let guardrails = std::sync::Arc::new(crate::harness::guardrails::GuardrailState::new(None));
        let (sink, _stream) = crate::harness::EventSink::new();
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .tools(
                workspace
                    .registry(&animist::history::History::new())
                    .expect("registry"),
            )
            .layers(
                animist::agent::AgentLayer::builder()
                    .layer(animist::agent::Layer::new(
                        crate::harness::GuardrailLayer::new(guardrails, sink),
                    ))
                    .build(),
            )
            .build()
            .expect("harness");
        // Three reads with alternating inputs but identical results: the
        // third result trips the no-progress warning.
        let output = harness
            .call(
                Batch,
                serde_json::json!({"tool_calls": [read("f"), read("g"), read("f")]}),
            )
            .await
            .expect("batch outcome");
        let text = invoke_text(&output);
        assert!(
            text.contains("guardrail"),
            "guardrail counters must see batch children: {text}"
        );
    }

    /// A child-budget overflow is reserved upfront for the whole call, so
    /// it maps to per-entry error text the model can act on (split the
    /// fan-out and retry), never a run failure.
    #[tokio::test]
    async fn a_child_limit_overflow_reports_per_entry_errors() {
        let (_dir, workspace) = crate::tools::tests::workspace();
        // The batch reserves every entry against the child budget upfront;
        // a budget of 2 cannot admit even the capped set of 25.
        let calls: Vec<_> = (0..MAX_BATCH_SIZE)
            .map(|_| entry(json!({"tool": "list", "parameters": {"path": "."}})))
            .collect();
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .tools(
                workspace
                    .registry(&animist::history::History::new())
                    .expect("registry"),
            )
            .config(
                animist::agent::AgentConfig::builder()
                    .max_child_calls(2_u64)
                    .build(),
            )
            .build()
            .expect("harness");
        let output = harness
            .call(Batch, serde_json::json!({ "tool_calls": calls }))
            .await
            .expect("batch outcome");
        let text = invoke_text(&output);
        assert!(
            !output.is_error,
            "a spent budget is per-entry failure, not the batch's error: {text}"
        );
        assert_eq!(text.matches("## list").count(), MAX_BATCH_SIZE, "{text}");
        assert!(
            text.contains("[ERROR] Child tool call limit exceeded"),
            "{text}"
        );
        assert!(
            text.contains("Executed 0/25 successfully. 25 failed."),
            "{text}"
        );
    }

    /// A test child tool: signals `started`, then parks until the run's
    /// cancellation fires, so a dispatch in flight can be cancelled.
    struct Park {
        started: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[derive(Debug, Default, Deserialize, JsonSchema)]
    struct ParkArgs {
        #[allow(dead_code)]
        _unused: Option<Value>,
    }

    #[async_trait]
    impl ToolImpl for Park {
        type Args = ParkArgs;
        type Output = ToolOutput;
        type Error = Error;

        fn definition(&self) -> ToolDefinition {
            definition::<ParkArgs>("park", "test child that parks until cancelled")
        }

        async fn call(&self, context: &ToolContext<'_>, _args: ParkArgs) -> Result<ToolOutput> {
            let _ = self.started.send(());
            context.run.cancellation.cancelled().await;
            Err(invalid("park was cancelled"))
        }
    }

    /// Cancellation mid-dispatch is fatal for the whole call — Stop and
    /// cancellation propagate — and keeps the batch's own error wording.
    #[tokio::test]
    async fn cancellation_during_dispatch_is_the_batchs_error() {
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = animist::agent::tool::ToolRegistry::default();
        registry.register(Park { started }).expect("park registers");
        registry.register(Batch).expect("batch registers");
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .tools(registry)
            .build()
            .expect("harness");
        let cancel = harness.cancellation();
        let call = tokio::spawn(async move {
            harness
                .call(
                    Batch,
                    serde_json::json!({"tool_calls": [
                        {"tool": "park", "parameters": {}},
                        {"tool": "park", "parameters": {}},
                    ]}),
                )
                .await
                .expect("batch outcome")
        });
        started_rx.recv().await.expect("a child parked");
        cancel.cancel();
        let output = call.await.expect("call joined");
        let text = invoke_text(&output);
        assert!(output.is_error, "the fatal dispatch error is the result");
        assert!(text.starts_with("batch dispatch failed: "), "got: {text}");
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
        let text = invoke_text(&output);
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
        let text = invoke_text(&output);
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
        let text = invoke_text(&output);
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

    // ---- barrier-instrumented scheduling (no wall-clock measurement) ----

    /// A configurable child tool: records its name when it runs, sleeps,
    /// and optionally meets its siblings at a barrier.
    struct Child {
        name: &'static str,
        delay: std::time::Duration,
        log: Option<Arc<std::sync::Mutex<Vec<&'static str>>>>,
        barrier: Option<Arc<tokio::sync::Barrier>>,
        scheduling: ToolScheduling,
    }

    /// The barrier guard: long enough that an overlapping pair always
    /// meets, short enough that a serialized pair fails the test instead
    /// of hanging it.
    const BARRIER_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    #[derive(Debug, Default, Deserialize, JsonSchema)]
    struct ChildArgs {
        #[allow(dead_code)]
        _unused: Option<Value>,
    }

    #[async_trait]
    impl ToolImpl for Child {
        type Args = ChildArgs;
        type Output = ToolOutput;
        type Error = Error;

        fn definition(&self) -> ToolDefinition {
            definition::<ChildArgs>(self.name, "test child")
        }

        fn scheduling(&self, _input: &Value) -> ToolScheduling {
            self.scheduling.clone()
        }

        async fn call(&self, _context: &ToolContext<'_>, _args: ChildArgs) -> Result<ToolOutput> {
            if let Some(log) = &self.log {
                log.lock().unwrap().push(self.name);
            }
            if self.delay > std::time::Duration::ZERO {
                tokio::time::sleep(self.delay).await;
            }
            if let Some(barrier) = &self.barrier
                && tokio::time::timeout(BARRIER_WAIT, barrier.wait())
                    .await
                    .is_err()
            {
                return Err(invalid(format!(
                    "{}: the barrier never filled — the children did not overlap",
                    self.name
                )));
            }
            Ok(crate::tools::error::text_output(self.name))
        }
    }

    fn independent() -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: Vec::new(),
        }
    }

    fn writing(key: &'static str) -> ToolScheduling {
        ToolScheduling::Resources {
            reads: Vec::new(),
            writes: vec![key.to_owned()],
        }
    }

    fn child_registry(children: impl IntoIterator<Item = Child>) -> ToolRegistry {
        let mut registry = ToolRegistry::default();
        for child in children {
            registry.register(child).expect("child registers");
        }
        registry.register(Batch).expect("batch registers");
        registry
    }

    async fn run_batch(
        registry: ToolRegistry,
        max_concurrent: usize,
        entries: &[&'static str],
    ) -> animist::agent::tool::ToolOutput {
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .tools(registry)
            .config(
                animist::agent::AgentConfig::builder()
                    .max_concurrent_tools(max_concurrent)
                    .build(),
            )
            .build()
            .expect("harness");
        let calls: Vec<_> = entries
            .iter()
            .map(|tool| entry(json!({"tool": tool, "parameters": {}})))
            .collect();
        harness
            .call(Batch, serde_json::json!({ "tool_calls": calls }))
            .await
            .expect("batch outcome")
    }

    /// Independent children are in flight at once: the pair barrier only
    /// fills when both run concurrently, so serialization fails the test
    /// through the barrier, not through timing.
    #[tokio::test]
    async fn independent_children_overlap() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let registry = child_registry([
            Child {
                name: "a",
                delay: std::time::Duration::ZERO,
                log: None,
                barrier: Some(Arc::clone(&barrier)),
                scheduling: independent(),
            },
            Child {
                name: "b",
                delay: std::time::Duration::ZERO,
                log: None,
                barrier: Some(barrier),
                scheduling: independent(),
            },
        ]);
        let output = run_batch(registry, 2, &["a", "b"]).await;
        let text = invoke_text(&output);
        assert!(!output.is_error, "{text}");
        assert!(
            !text.contains("[ERROR]"),
            "the barrier must fill, not time out: {text}"
        );
        assert!(
            text.contains("All 2 tools executed successfully."),
            "{text}"
        );
    }

    /// Conflicting children serialize, in call order: the shared log
    /// records execution order and the report keeps entry order.
    #[tokio::test]
    async fn conflicting_children_serialize_in_call_order() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = child_registry([
            Child {
                name: "one",
                delay: std::time::Duration::ZERO,
                log: Some(Arc::clone(&log)),
                barrier: None,
                scheduling: writing("same"),
            },
            Child {
                name: "two",
                delay: std::time::Duration::ZERO,
                log: Some(Arc::clone(&log)),
                barrier: None,
                scheduling: writing("same"),
            },
        ]);
        let output = run_batch(registry, 2, &["one", "two"]).await;
        let text = invoke_text(&output);
        assert!(
            text.contains("All 2 tools executed successfully."),
            "{text}"
        );
        assert_eq!(*log.lock().unwrap(), vec!["one", "two"]);
        let (first, second) = (
            text.find("## one").expect("one reported"),
            text.find("## two").expect("two reported"),
        );
        assert!(first < second, "results stay in call order: {text}");
    }

    /// Results come back in call order however the children finish: the
    /// first entry is the slowest, so a completion-order implementation
    /// would reorder the report.
    #[tokio::test]
    async fn results_return_in_call_order() {
        let registry = child_registry([
            Child {
                name: "slow",
                delay: std::time::Duration::from_millis(120),
                log: None,
                barrier: None,
                scheduling: independent(),
            },
            Child {
                name: "mid",
                delay: std::time::Duration::from_millis(40),
                log: None,
                barrier: None,
                scheduling: independent(),
            },
            Child {
                name: "fast",
                delay: std::time::Duration::ZERO,
                log: None,
                barrier: None,
                scheduling: independent(),
            },
        ]);
        let output = run_batch(registry, 3, &["slow", "mid", "fast"]).await;
        let text = invoke_text(&output);
        let order: Vec<usize> = ["slow", "mid", "fast"]
            .iter()
            .map(|name| text.find(&format!("## {name}")).expect("section present"))
            .collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "sections must appear in call order: {text}"
        );
        assert!(
            text.contains("All 3 tools executed successfully."),
            "{text}"
        );
    }

    /// A child `Stop` decision terminates the batch: in-flight siblings
    /// are dropped and the typed error surfaces as the batch's own result.
    /// The stop layer fires on the first child; the second child parks, so
    /// a batch that kept its siblings running would hang the call.
    #[tokio::test]
    async fn a_child_stop_drops_its_siblings() {
        use animist::agent::LayerImpl;
        use animist::agent::ToolDecision;
        struct StopFirst;
        #[async_trait]
        impl LayerImpl for StopFirst {
            async fn pre_tool_exec(
                &self,
                _agent: &animist::agent::Agent,
                context: &ToolContext<'_>,
            ) -> animist::agent::Result<ToolDecision> {
                if context.call.name == "stopme" {
                    return Ok(ToolDecision::Stop {
                        reason: "test stop".into(),
                    });
                }
                Ok(ToolDecision::Run)
            }
        }
        let (started, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut registry = ToolRegistry::default();
        registry.register(Batch).expect("batch registers");
        registry.register(StopTool).expect("stop tool registers");
        registry.register(Park { started }).expect("park registers");
        let mut harness = animist::agent::testing::ToolHarness::builder()
            .tools(registry)
            .layers(
                animist::agent::AgentLayer::builder()
                    .layer(animist::agent::Layer::new(StopFirst))
                    .build(),
            )
            .build()
            .expect("harness");
        // Exclusive children serialize: the stop child runs first and the
        // parked sibling must never start. The harness models the agent's
        // rule that a child stop terminates even a root that catches its
        // dispatch error, so the call itself fails with the typed stop.
        let error = harness
            .call(
                Batch,
                serde_json::json!({"tool_calls": [
                    {"tool": "stopme", "parameters": {}},
                    {"tool": "park", "parameters": {}},
                ]}),
            )
            .await
            .expect_err("the child stop terminates the root");
        assert!(
            error.to_string().contains("test stop"),
            "the stop reason propagates: {error}"
        );
        assert!(
            started_rx.try_recv().is_err(),
            "the later sibling must be dropped, not started"
        );
    }

    /// The child the stop layer targets: a plain tool, stopped before it
    /// runs.
    struct StopTool;

    #[derive(Debug, Default, Deserialize, JsonSchema)]
    struct StopArgs {
        #[allow(dead_code)]
        _unused: Option<Value>,
    }

    #[async_trait]
    impl ToolImpl for StopTool {
        type Args = StopArgs;
        type Output = ToolOutput;
        type Error = Error;

        fn definition(&self) -> ToolDefinition {
            definition::<StopArgs>("stopme", "test child that a layer stops")
        }

        async fn call(&self, _context: &ToolContext<'_>, _args: StopArgs) -> Result<ToolOutput> {
            Ok(crate::tools::error::text_output("ran anyway"))
        }
    }
}
