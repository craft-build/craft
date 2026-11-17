//! The persistence layer: checkpoints the history change feed at step and
//! turn boundaries. A persistence error is logged and surfaced as a
//! craft event; it never fails the run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use async_trait::async_trait;

use animist::agent::tool::{ToolContext, ToolOutput};
use animist::agent::{
    Agent, LayerImpl, Result, RunContext, RunStatus, StepAction, StepOutcome, ToolDecision,
};

use super::events::{CraftEvent, EventSink};
use super::persist::Persister;
use crate::storage::sessions::{StoredCodemodeChild, StoredCodemodeRun};

struct RunBuilder {
    started: Instant,
    children: Vec<StoredCodemodeChild>,
}

/// Child outputs are presentation previews, not a second copy of model
/// context; keep one bounded prefix per child.
const CHILD_OUTPUT_BYTES: usize = 4 * 1024;

fn bounded_text(text: &str, max_bytes: usize, marker: &str) -> String {
    let (clipped, truncated) = crate::tools::clip(text, max_bytes);
    if truncated {
        format!("{clipped}\n{marker}")
    } else {
        clipped.to_owned()
    }
}

fn bounded_output(text: &str) -> String {
    bounded_text(text, CHILD_OUTPUT_BYTES, "... [nested output truncated]")
}

/// Keeps large nested inputs (a scripted write's whole payload, say) from
/// turning the presentation projection into a second copy of model context.
fn bounded_input(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(bounded_text(
            text,
            CHILD_OUTPUT_BYTES,
            "... [nested input truncated]",
        )),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(bounded_input).collect())
        }
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, field)| (key.clone(), bounded_input(field)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub(crate) struct PersistLayer {
    persister: Arc<tokio::sync::Mutex<Persister>>,
    sink: EventSink,
    runs: StdMutex<HashMap<String, RunBuilder>>,
}

impl PersistLayer {
    pub(crate) fn new(persister: Arc<tokio::sync::Mutex<Persister>>, sink: EventSink) -> Self {
        Self {
            persister,
            sink,
            runs: StdMutex::new(HashMap::new()),
        }
    }

    async fn checkpoint(&self) {
        let mut persister = self.persister.lock().await;
        if let Err(error) = persister.checkpoint().await {
            tracing::warn!("session checkpoint failed: {error}");
            self.sink
                .craft(CraftEvent::PersistenceFailed(error.to_string()));
        }
    }
}

#[async_trait]
impl LayerImpl for PersistLayer {
    async fn step_end(
        &self,
        _agent: &Agent,
        _run: &RunContext,
        _outcome: &StepOutcome,
    ) -> Result<StepAction> {
        // Runs after the step's results commit (and the agent's squash).
        self.checkpoint().await;
        Ok(StepAction::default())
    }

    async fn turn_end(&self, _agent: &Agent, _run: &RunContext, _status: RunStatus) -> Result<()> {
        self.checkpoint().await;
        Ok(())
    }

    async fn pre_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
    ) -> Result<ToolDecision> {
        if context.call.name == "code" {
            self.runs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(
                    context.call.id.clone(),
                    RunBuilder {
                        started: Instant::now(),
                        children: Vec::new(),
                    },
                );
        }
        Ok(ToolDecision::Run)
    }

    async fn post_tool_exec(
        &self,
        _agent: &Agent,
        context: &ToolContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput> {
        let call = context.call;
        let finished = {
            let mut runs = self
                .runs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(parent) = context.parent_call_id
                && let Some(run) = runs.get_mut(parent)
            {
                run.children.push(StoredCodemodeChild {
                    id: call.id.clone(),
                    tool: call.name.clone(),
                    input: bounded_input(&call.input),
                    output: bounded_output(&crate::history::ext::tool_content_text(
                        &output.content,
                    )),
                    is_error: output.is_error,
                });
            }
            if call.name != "code" {
                None
            } else {
                runs.remove(&call.id).map(|run| {
                    let text = crate::history::ext::tool_content_text(&output.content);
                    let failure = output.is_error.then(|| {
                        crate::codemode::diagnose::Class::from_rendered(&text)
                            .map_or_else(|| "error".to_owned(), |class| class.as_str().to_owned())
                    });
                    StoredCodemodeRun {
                        duration_ms: run.started.elapsed().as_millis().min(u128::from(u64::MAX))
                            as u64,
                        failure,
                        children: run.children,
                    }
                })
            }
        };
        if let Some(run) = finished {
            let mut persister = self.persister.lock().await;
            persister
                .store_mut()
                .record_codemode_run(call.id.clone(), run);
        }
        Ok(output)
    }
}
