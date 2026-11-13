use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use animist::agent::tool::{ToolCall, ToolContext};

use super::{
    adapters,
    catalog::Catalog,
    diagnose::{Class, Failure},
    worker,
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
    let (events, replies, worker_handle) =
        match worker::spawn(code.clone(), timeout_secs, config, catalog.external_names()) {
            Ok(channels) => channels,
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
    let mut worker_handle = Some(worker_handle);
    let mut replies = Some(replies);
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

    let outcome = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                break Loop::Failed { failure: Failure::cancelled(calls), stdout: None };
            }
            () = &mut deadline => {
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
            event = events.recv_async() => {
                match event {
                    Ok(worker::Message::Print(text)) => {
                        context.progress(text).await;
                    }
                    Ok(worker::Message::Calls(pending)) => {
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
                        let tool_calls = bound.iter().enumerate().map(|(index, (_, name, input))| {
                            ToolCall::builder()
                                .id(format!("code-{}", calls + index))
                                .name(name.clone())
                                .input(input.clone())
                                .build()
                        }).collect::<Vec<_>>();
                        let dispatched = if tool_calls.is_empty() {
                            Ok(Vec::new())
                        } else {
                            context.dispatch_all_in(tool_calls, catalog.selection()).await
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
                                let Some(sender) = replies.as_ref() else {
                                    break Loop::Failed {
                                        failure: Failure::plain(
                                            Class::Cancelled,
                                            "Cancelled: codemode bridge closed",
                                        ),
                                        stdout: None,
                                    };
                                };
                                if sender.send(results).is_err() {
                                    break Loop::Failed {
                                        failure: Failure::plain(
                                            Class::Cancelled,
                                            "Cancelled: codemode worker stopped",
                                        ),
                                        stdout: None,
                                    };
                                }
                            }
                            Err(error) => {
                                break Loop::Failed {
                                    failure: Failure::plain(
                                        Class::ToolError,
                                        format!("nested dispatch failed: {error}"),
                                    ),
                                    stdout: None,
                                };
                            }
                        }
                    }
                    Ok(worker::Message::Done(finished)) => {
                        let stdout = finished.stdout;
                        if let Some(handle) = worker_handle.take()
                            && tokio::task::spawn_blocking(move || handle.join()).await.is_err()
                        {
                            break Loop::Failed {
                                failure: Failure::crash("codemode worker join failed"),
                                stdout: Some(stdout),
                            };
                        }
                        match finished.result {
                            Ok(value) => break Loop::Ok(adapters::shape_output(
                                &stdout,
                                value.as_deref(),
                                config.max_output_kib * 1024,
                            )),
                            Err(failure) => break Loop::Failed { failure, stdout: Some(stdout) },
                        }
                    }
                    Err(_) => {
                        break Loop::Failed {
                            failure: Failure::crash("codemode worker stopped without a result"),
                            stdout: None,
                        };
                    }
                }
            }
        }
    };

    drop(replies.take());
    drop(events);
    drop(worker_handle);

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
