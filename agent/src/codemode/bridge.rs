use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use animist::agent::tool::{ToolCall, ToolContext};

use super::{
    adapters,
    catalog::Catalog,
    diagnose::{self, Class, Failure},
    pool, proto,
};
use crate::config::CodemodeConfig;

/// Per-tool ok/err counts across every dispatch wave of one run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub ok: usize,
    pub err: usize,
}

/// One `code` run's model-visible text plus everything telemetry needs.
/// Presentation only: completed child calls already persist in history.
#[derive(Debug)]
pub struct RunOutcome {
    pub text: String,
    pub failure: Option<Failure>,
    pub calls: usize,
    pub tallies: BTreeMap<String, Tally>,
    pub duration: Duration,
}

pub async fn run(
    context: &ToolContext<'_>,
    catalog: &Catalog,
    config: CodemodeConfig,
    code: String,
    timeout_secs: u64,
) -> RunOutcome {
    let started = Instant::now();
    let mut calls = 0usize;
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    if let Err(failure) = diagnose::preflight(&code) {
        return finish(
            failure.render(&code),
            Some(failure),
            calls,
            tallies,
            started,
        );
    }
    let params = proto::WorkerParams {
        code: code.clone(),
        timeout_secs,
        max_output_kib: config.max_output_kib,
        max_tool_calls: config.max_tool_calls,
        names: catalog.external_names(),
    };
    let mut worker = match pool::pool().checkout(params).await {
        Ok(checkout) => checkout,
        Err(failure) => {
            return finish(
                failure.render(&code),
                Some(failure),
                calls,
                tallies,
                started,
            );
        }
    };
    let deadline = tokio::time::sleep(Duration::from_secs(config.wall_clock_secs));
    tokio::pin!(deadline);
    let cancellation = context.run.cancellation.clone();

    enum Loop {
        Ok(String),
        Failed {
            failure: Failure,
            stdout: Option<String>,
        },
    }

    // Handler bodies run only after the select releases the worker borrow,
    // so `next_event` and `send_replies` never overlap.
    enum Step {
        Event(std::io::Result<proto::Message>),
        Cancelled,
        Deadline,
    }

    let outcome = loop {
        let step = tokio::select! {
            biased;
            () = cancellation.cancelled() => Step::Cancelled,
            () = &mut deadline => Step::Deadline,
            event = worker.next_event() => Step::Event(event),
        };
        match step {
            Step::Cancelled => {
                break Loop::Failed {
                    failure: Failure::cancelled(calls),
                    stdout: None,
                };
            }
            Step::Deadline => {
                break Loop::Failed {
                    failure: Failure::plain(
                        Class::Limit,
                        format!(
                            "wall-clock limit {}s exceeded after {calls} tool calls",
                            config.wall_clock_secs,
                        ),
                    ),
                    stdout: None,
                };
            }
            Step::Event(event) => match event {
                Ok(proto::Message::Print(text)) => {
                    context.progress(text).await;
                }
                Ok(proto::Message::Calls(pending)) => {
                    if calls.saturating_add(pending.len()) > config.max_tool_calls {
                        break Loop::Failed {
                            failure: Failure::plain(
                                Class::Limit,
                                format!(
                                    "nested tool-call cap {} exceeded after {calls} tool calls",
                                    config.max_tool_calls,
                                ),
                            ),
                            stdout: None,
                        };
                    }
                    let mut bound = Vec::with_capacity(pending.len());
                    let mut early = Vec::new();
                    for call in pending {
                        match catalog.bind(&call.name, call.args, call.kwargs) {
                            Ok((name, input)) => bound.push((call.id, name, input)),
                            Err(error) => {
                                // The worker prefixes failed replies with
                                // "ToolError from <tool>"; tally under the wire
                                // name so aliased tools share one key.
                                record(&mut tallies, catalog.wire_name(&call.name), false);
                                early.push((call.id, Err(error)));
                            }
                        }
                    }
                    let tool_calls = bound
                        .iter()
                        .enumerate()
                        .map(|(index, (_, name, input))| {
                            ToolCall::builder()
                                .id(format!("code-{}", calls + index))
                                .name(name.clone())
                                .input(input.clone())
                                .build()
                        })
                        .collect::<Vec<_>>();
                    let dispatched = if tool_calls.is_empty() {
                        Ok(Vec::new())
                    } else {
                        context
                            .dispatch_all_in(tool_calls, catalog.selection())
                            .await
                    };
                    match dispatched {
                        Ok(outputs) => {
                            let mut results = early;
                            for ((id, name, _), output) in bound.into_iter().zip(outputs) {
                                let result = if output.is_error {
                                    record(&mut tallies, &name, false);
                                    Err(crate::history::ext::tool_content_text(&output.content))
                                } else {
                                    record(&mut tallies, &name, true);
                                    adapters::adapt(&name, &output)
                                };
                                results.push((id, result));
                            }
                            results.sort_by_key(|(id, _)| *id);
                            calls += results.len();
                            if worker.send_replies(results).await.is_err() {
                                let detail = worker.crash_detail().await;
                                break Loop::Failed {
                                    failure: engine_crash(&detail),
                                    stdout: None,
                                };
                            }
                        }
                        Err(error) => {
                            // A dispatch aborted by cancellation is the
                            // cancellation, not a tool error.
                            let failure = if cancellation.is_cancelled() {
                                Failure::cancelled(calls)
                            } else {
                                Failure::plain(
                                    Class::ToolError,
                                    format!("nested dispatch failed: {error}"),
                                )
                            };
                            break Loop::Failed {
                                failure,
                                stdout: None,
                            };
                        }
                    }
                }
                Ok(proto::Message::Done(finished)) => {
                    worker.complete().await;
                    let stdout = finished.stdout;
                    match finished.result {
                        Ok(value) => {
                            break Loop::Ok(adapters::shape_output(
                                &stdout,
                                value.as_deref(),
                                config.max_output_kib * 1024,
                            ));
                        }
                        Err(failure) => {
                            break Loop::Failed {
                                failure,
                                stdout: Some(stdout),
                            };
                        }
                    }
                }
                Err(_) => {
                    let detail = worker.crash_detail().await;
                    break Loop::Failed {
                        failure: engine_crash(&detail),
                        stdout: None,
                    };
                }
            },
        }
    };

    drop(worker);

    let (text, failure) = match outcome {
        Loop::Ok(text) => (text, None),
        Loop::Failed {
            mut failure,
            stdout,
        } => {
            failure.attach_hints(&catalog.external_names(), &catalog.uncallable_names());
            let mut body = failure.render(&code);
            if calls > 0 {
                body = format!("{}\n{}", progress_summary(calls, &tallies), body);
            }
            let text = match stdout {
                Some(stdout) => {
                    adapters::shape_output(&stdout, Some(&body), config.max_output_kib * 1024)
                }
                None => body,
            };
            (text, Some(failure))
        }
    };
    finish(text, failure, calls, tallies, started)
}

/// The model-facing engine-crash failure: a worker that died mid-run
/// (signal, bad exit, dropped pipe) rather than completing.
fn engine_crash(detail: &str) -> Failure {
    Failure::crash(format!(
        "the Python engine crashed while running your script ({detail}). \
         This is an engine bug, not a script error"
    ))
}

fn finish(
    text: String,
    failure: Option<Failure>,
    calls: usize,
    tallies: BTreeMap<String, Tally>,
    started: Instant,
) -> RunOutcome {
    RunOutcome {
        text,
        failure,
        calls,
        tallies,
        duration: started.elapsed(),
    }
}

fn record(tallies: &mut BTreeMap<String, Tally>, tool: &str, ok: bool) {
    let tally = tallies.entry(tool.to_owned()).or_default();
    if ok {
        tally.ok += 1;
    } else {
        tally.err += 1;
    }
}

/// `Completed 7 of 9 tool calls before failing: edit x3 ok, read x4 ok` —
/// the ok-side counts only; the failure line itself names the failed call.
fn progress_summary(calls: usize, tallies: &BTreeMap<String, Tally>) -> String {
    let ok: usize = tallies.values().map(|tally| tally.ok).sum();
    let completed: Vec<String> = tallies
        .iter()
        .filter(|(_, tally)| tally.ok > 0)
        .map(|(name, tally)| format!("{name} x{} ok", tally.ok))
        .collect();
    if completed.is_empty() {
        format!("Completed {ok} of {calls} tool calls before failing")
    } else {
        format!(
            "Completed {ok} of {calls} tool calls before failing: {completed}",
            completed = completed.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_summary_counts_ok_calls_and_names_their_tools() {
        let mut tallies = BTreeMap::new();
        tallies.insert("read_text".to_owned(), Tally { ok: 4, err: 0 });
        tallies.insert("edit".to_owned(), Tally { ok: 3, err: 0 });
        tallies.insert("bash".to_owned(), Tally { ok: 0, err: 1 });
        assert_eq!(
            progress_summary(8, &tallies),
            "Completed 7 of 8 tool calls before failing: edit x3 ok, read_text x4 ok"
        );
    }

    #[test]
    fn progress_summary_without_completed_calls_has_no_list() {
        let mut tallies = BTreeMap::new();
        tallies.insert("edit".to_owned(), Tally { ok: 0, err: 1 });
        assert_eq!(
            progress_summary(1, &tallies),
            "Completed 0 of 1 tool calls before failing"
        );
    }
}
