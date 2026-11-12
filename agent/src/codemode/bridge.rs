use std::time::Duration;

use animist::agent::tool::{ToolCall, ToolContext};

use super::{adapters, catalog::Catalog, worker};
use crate::config::CodemodeConfig;

pub async fn run(
    context: &ToolContext<'_>,
    catalog: &Catalog,
    config: CodemodeConfig,
    code: String,
    timeout_secs: u64,
) -> Result<String, String> {
    let (events, replies, worker_handle) =
        worker::spawn(code, timeout_secs, config, catalog.external_names())?;
    let mut worker_handle = Some(worker_handle);
    let mut replies = Some(replies);
    let deadline = tokio::time::sleep(Duration::from_secs(config.wall_clock_secs));
    tokio::pin!(deadline);
    let cancellation = context.run.cancellation.clone();
    let mut calls = 0usize;

    let outcome = loop {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                break Err(format!("Cancelled after {calls} tool calls"));
            }
            () = &mut deadline => {
                break Err(format!("Limit: wall-clock limit {}s exceeded after {calls} tool calls", config.wall_clock_secs));
            }
            event = events.recv_async() => {
                match event {
                    Ok(worker::Message::Print(text)) => {
                        context.progress(text).await;
                    }
                    Ok(worker::Message::Calls(pending)) => {
                        if calls.saturating_add(pending.len()) > config.max_tool_calls {
                            break Err(format!("Limit: nested tool-call cap {} exceeded", config.max_tool_calls));
                        }
                        let mut bound = Vec::with_capacity(pending.len());
                        let mut early = Vec::new();
                        for call in pending {
                            match catalog.bind(&call.name, call.args, call.kwargs) {
                                Ok((name, input)) => bound.push((call.id, name, input)),
                                Err(error) => early.push((call.id, Err(error))),
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
                                        Err(crate::history::ext::tool_content_text(&output.content))
                                    } else {
                                        adapters::adapt(&name, &output)
                                    };
                                    results.push((id, result));
                                }
                                results.sort_by_key(|(id, _)| *id);
                                calls += results.len();
                                let Some(sender) = replies.as_ref() else {
                                    break Err("Cancelled: codemode bridge closed".to_owned());
                                };
                                if sender.send(results).is_err() {
                                    break Err("Cancelled: codemode worker stopped".to_owned());
                                }
                            }
                            Err(error) => {
                                break Err(format!("ToolError: nested dispatch failed: {error}"));
                            }
                        }
                    }
                    Ok(worker::Message::Done(finished)) => {
                        let stdout = finished.stdout;
                        if let Some(handle) = worker_handle.take()
                            && tokio::task::spawn_blocking(move || handle.join()).await.is_err()
                        {
                            break Err("Crash: codemode worker join failed".to_owned());
                        }
                        break finished.result.map(|value| {
                            adapters::shape_output(&stdout, value.as_deref(), config.max_output_kib * 1024)
                        }).map_err(|error| adapters::shape_output(&stdout, Some(&error), config.max_output_kib * 1024));
                    }
                    Err(_) => {
                        break Err("Crash: codemode worker stopped without a result".to_owned());
                    }
                }
            }
        }
    };

    drop(replies.take());
    drop(events);
    drop(worker_handle);
    outcome
}
