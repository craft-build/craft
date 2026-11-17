use std::{
    borrow::Cow,
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::config::CodemodeConfig;
use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyDate, MontyDateTime, MontyException,
    MontyObject, NameLookupResult, OsFunctionCall, PrintWriter, PrintWriterCallback,
    ResourceLimits, ResourceTracker,
};

use super::diagnose::{self, Class, Failure};
use super::proto;

pub use super::proto::{Finished, Message, PendingCall, Reply, WorkerParams};

const STACK_BYTES: usize = 64 * 1024 * 1024;

/// After this many completed runs a worker self-exits so the pool respawns
/// it fresh — cheap insurance against anything stateful accumulating inside
/// one long-lived process.
pub const RECYCLE_AFTER_RUNS: usize = 64;

struct Output {
    sender: flume::Sender<Message>,
    stdout: String,
    pending: String,
    max_bytes: usize,
}

impl Output {
    fn write(&mut self, text: &str) -> Result<(), MontyException> {
        if self.stdout.len().saturating_add(text.len()) > self.max_bytes {
            return Err(MontyException::new(
                ExcType::MemoryError,
                Some(format!("output cap {} bytes exceeded", self.max_bytes)),
            ));
        }
        self.stdout.push_str(text);
        self.pending.push_str(text);
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending.drain(..=newline).collect::<String>();
            let _ = self.sender.send(Message::Print(line));
        }
        if self.pending.len() >= 1024 {
            self.flush();
        }
        Ok(())
    }

    fn flush(&mut self) {
        if !self.pending.is_empty() {
            let _ = self
                .sender
                .send(Message::Print(std::mem::take(&mut self.pending)));
        }
    }
}

impl PrintWriterCallback for Output {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.write(&output)
    }

    fn stdout_push(&mut self, end: char) -> Result<(), MontyException> {
        let mut buffer = [0; 4];
        self.write(end.encode_utf8(&mut buffer))
    }
}

type RunChannels = (
    flume::Receiver<Message>,
    flume::Sender<Vec<Reply>>,
    thread::JoinHandle<()>,
);

/// Spawns one interpreter run on a dedicated big-stack thread, wired to a
/// fresh pair of in-process channels. The thread always reports through
/// [`Message::Done`], including on panic.
fn spawn_run(params: WorkerParams) -> Result<RunChannels, Failure> {
    let (message_tx, message_rx) = flume::unbounded();
    let (reply_tx, reply_rx) = flume::unbounded();
    let handle = thread::Builder::new()
        .name("craft-codemode".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let config = CodemodeConfig {
                max_output_kib: params.max_output_kib,
                max_tool_calls: params.max_tool_calls,
                ..CodemodeConfig::default()
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(
                    params.code,
                    params.timeout_secs,
                    config,
                    params.names,
                    &message_tx,
                    &reply_rx,
                )
            }));
            match result {
                Ok(finished) => {
                    let _ = message_tx.send(Message::Done(finished));
                }
                Err(_) => {
                    let _ = message_tx.send(Message::Done(Finished {
                        stdout: String::new(),
                        result: Err(Failure::crash("codemode worker panicked")),
                    }));
                }
            }
        })
        .map_err(|error| Failure::crash(format!("could not start codemode worker: {error}")))?;
    Ok((message_rx, reply_tx, handle))
}

/// The reply channel of the run currently in flight; `None` between runs.
type ReplySlot = Arc<Mutex<Option<flume::Sender<Vec<Reply>>>>>;

/// What the reader thread hands the serve loop.
enum Incoming {
    Run(WorkerParams),
    Stop,
}

/// The child-side serve loop, generic over the transport so the stdio
/// subprocess and the in-memory test duplex share one implementation:
/// handshake `ready`, then read a `run` frame, stream one run's events,
/// write `done`, and repeat until EOF or the recycle cap.
pub fn serve<R, W>(read: R, mut write: W)
where
    R: io::Read + Send + 'static,
    W: io::Write,
{
    let (incoming_tx, incoming_rx) = flume::unbounded::<Incoming>();
    let slot: ReplySlot = Arc::new(Mutex::new(None));
    let reader_slot = slot.clone();
    let reader = thread::Builder::new()
        .name("craft-codemode-io".to_owned())
        .spawn(move || {
            let mut read = read;
            while let Some(frame) = proto::read_frame(&mut read).ok().flatten() {
                match proto::parse_frame::<proto::ToWorker>(&frame) {
                    Ok(proto::ToWorker::Run(params)) => {
                        if incoming_tx.send(Incoming::Run(params)).is_err() {
                            return;
                        }
                    }
                    Ok(proto::ToWorker::Replies { replies }) => {
                        let replies = replies
                            .into_iter()
                            .map(|(id, outcome)| (id, outcome.into_result()))
                            .collect::<Vec<_>>();
                        if let Some(sender) = reader_slot.lock().unwrap().as_ref() {
                            let _ = sender.send(replies);
                        }
                    }
                    Err(_) => break,
                }
            }
            reader_slot.lock().unwrap().take();
            let _ = incoming_tx.send(Incoming::Stop);
        });
    if reader.is_err() {
        return;
    }
    if write_frame(&mut write, &proto::FromWorker::Ready).is_err() {
        return;
    }
    let mut runs = 0usize;
    while let Ok(incoming) = incoming_rx.recv() {
        match incoming {
            Incoming::Stop => break,
            Incoming::Run(params) => {
                if run_once(params, &mut write, &slot).is_err() {
                    break;
                }
                runs += 1;
                if runs >= RECYCLE_AFTER_RUNS {
                    break;
                }
            }
        }
    }
    // The reader thread is left detached: it blocks on the pipe and exits
    // when the parent closes it (or when its send target is gone).
}

/// Executes one run frame end to end: interpreter thread up, events out,
/// `done` written, thread reaped. Fails only when the transport is dead.
fn run_once<W: io::Write>(params: WorkerParams, write: &mut W, slot: &ReplySlot) -> io::Result<()> {
    let (events, replies, join) = match spawn_run(params) {
        Ok(wired) => wired,
        Err(failure) => {
            return write_frame(
                write,
                &proto::FromWorker::Done {
                    stdout: String::new(),
                    result: Err(failure),
                },
            );
        }
    };
    *slot.lock().unwrap() = Some(replies);
    let mut result = Ok(());
    for message in events.iter() {
        let done = matches!(message, Message::Done(_));
        if let Err(error) = write_frame(write, &proto::FromWorker::from(message)) {
            result = Err(error);
            break;
        }
        if done {
            break;
        }
    }
    slot.lock().unwrap().take();
    let _ = join.join();
    result
}

fn write_frame<W: io::Write>(write: &mut W, frame: &impl serde::Serialize) -> io::Result<()> {
    let mut buffer = Vec::new();
    proto::encode_into(&mut buffer, frame)?;
    write.write_all(&buffer)?;
    write.flush()
}

/// `crafty __codemode-worker` entry: a fast, side-effect-free protocol
/// loop. Nothing but frames may reach its stdout; stderr stays free for
/// panic output the parent logs at debug level.
pub fn worker_main() -> ! {
    serve(std::io::stdin(), std::io::stdout());
    std::process::exit(0);
}

fn run(
    code: String,
    timeout_secs: u64,
    config: CodemodeConfig,
    names: Vec<String>,
    sender: &flume::Sender<Message>,
    replies: &flume::Receiver<Vec<Reply>>,
) -> Finished {
    let mut output = Output {
        sender: sender.clone(),
        stdout: String::new(),
        pending: String::new(),
        max_bytes: config.max_output_kib * 1024,
    };
    let result = run_inner(
        code,
        timeout_secs,
        config,
        names,
        sender,
        replies,
        &mut output,
    );
    output.flush();
    Finished {
        stdout: output.stdout,
        result,
    }
}

fn run_inner(
    code: String,
    timeout_secs: u64,
    config: CodemodeConfig,
    names: Vec<String>,
    _sender: &flume::Sender<Message>,
    replies: &flume::Receiver<Vec<Reply>>,
    output: &mut Output,
) -> Result<Option<String>, Failure> {
    let code = alias_tool_error(&code);
    let runner = MontyRun::new(code, "main.py", names.clone(), CompileOptions::default())
        .map_err(|error| Failure::from_exception(&error))?;
    let timeout = Duration::from_secs(timeout_secs);
    let limits = ResourceLimits::default()
        .max_feed_duration(timeout)
        .max_turn_duration(timeout)
        .max_recursion_depth(200)
        .max_suspensions(config.max_tool_calls.saturating_add(128));
    let inputs: Vec<MontyObject> = names
        .iter()
        .map(|name| MontyObject::function(name.clone(), None))
        .collect();
    let mut progress = runner
        .start(
            inputs,
            ResourceTracker::new(limits),
            PrintWriter::Callback(output),
        )
        .map_err(|error| Failure::from_exception(&error))?;
    let mut pending = BTreeMap::new();

    loop {
        progress = match progress {
            RunProgress::FunctionCall(call) => {
                let args = call
                    .args
                    .args()
                    .map(|value| super::convert::to_json(value.to_owned().as_ref()))
                    .collect();
                let kwargs = call
                    .args
                    .kwargs()
                    .map(|(key, value)| {
                        (
                            key.as_str().map_or_else(|| key.py_repr(), str::to_owned),
                            super::convert::to_json(value.to_owned().as_ref()),
                        )
                    })
                    .collect();
                let id = call.call_id;
                pending.insert(
                    id,
                    PendingCall {
                        id,
                        name: call.function_name.clone(),
                        args,
                        kwargs,
                    },
                );
                call.resume_pending(PrintWriter::Callback(output))
                    .map_err(|error| Failure::from_exception(&error))?
            }
            RunProgress::ResolveFutures(waiting) => {
                let calls = pending.values().cloned().collect::<Vec<_>>();
                _sender.send(Message::Calls(calls)).map_err(|_| {
                    Failure::plain(Class::Cancelled, "Cancelled: codemode bridge closed")
                })?;
                let replies = replies.recv().map_err(|_| {
                    Failure::plain(Class::Cancelled, "Cancelled: codemode bridge closed")
                })?;
                let results = replies
                    .into_iter()
                    .map(|(id, result)| {
                        let name = pending.get(&id).map(|call| call.name.as_str());
                        let result = match (name, result) {
                            // A name outside the catalog resumes as the
                            // interpreter's own NameError, so the failure
                            // classifies as unknown_tool with suggestions.
                            (Some(name), _) if !names.iter().any(|known| known == name) => {
                                ExtFunctionResult::NotFound(name.to_owned())
                            }
                            (Some(_), Ok(value)) => {
                                ExtFunctionResult::Return(super::convert::from_json(&value))
                            }
                            (name, Err(message)) => ExtFunctionResult::Error(MontyException::new(
                                ExcType::RuntimeError,
                                Some(format!(
                                    "ToolError from {}: {message}",
                                    name.unwrap_or("tool")
                                )),
                            )),
                            // The bridge replies only to ids it received.
                            (None, Ok(_)) => unreachable!("reply for unknown call id"),
                        };
                        (id, result)
                    })
                    .collect();
                pending.clear();
                waiting
                    .resume(results, PrintWriter::Callback(output))
                    .map_err(|error| Failure::from_exception(&error))?
            }
            RunProgress::NameLookup(lookup) => {
                let result = if names.contains(&lookup.name) {
                    NameLookupResult::Value(MontyObject::function(lookup.name.clone(), None))
                } else {
                    NameLookupResult::Undefined
                };
                lookup
                    .resume(result, PrintWriter::Callback(output))
                    .map_err(|error| Failure::from_exception(&error))?
            }
            RunProgress::OsCall(call) => call
                .resume_with(PrintWriter::Callback(output), os_result)
                .map_err(|error| Failure::from_exception(&error))?,
            RunProgress::Complete(value) => {
                let repr =
                    (value.as_ref().type_name() != "NoneType").then(|| value.as_ref().py_repr());
                return Ok(repr);
            }
        };
    }
}

fn alias_tool_error(code: &str) -> String {
    // Monty 1.5 rejects a host `MontyObject::type_object` in an `except`
    // clause, so the documented alias is rewritten as source identifiers while
    // strings and comments stay exactly as the model wrote them.
    let masked = diagnose::mask_strings_and_comments(code);
    let mut rewritten = String::with_capacity(code.len() + 16);
    let mut chars = code.char_indices().peekable();
    while let Some((start, character)) = chars.next() {
        if !is_identifier_byte(masked.as_bytes().get(start).copied().unwrap_or_default()) {
            rewritten.push(character);
            continue;
        }
        let mut identifier = String::new();
        identifier.push(character);
        let mut end = start + character.len_utf8();
        while let Some(&(offset, next)) = chars.peek()
            && is_identifier_byte(masked.as_bytes().get(offset).copied().unwrap_or_default())
        {
            identifier.push(next);
            end = offset + next.len_utf8();
            chars.next();
        }
        if &masked[start..end] == "ToolError" {
            rewritten.push_str("RuntimeError");
        } else {
            rewritten.push_str(&identifier);
        }
    }
    rewritten
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn os_result(call: OsFunctionCall) -> ExtFunctionResult {
    let value = match call {
        OsFunctionCall::DateToday => {
            let days = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                / 86_400;
            civil_from_days(days as i64)
                .map(|(year, month, day)| MontyObject::date(MontyDate { year, month, day }))
        }
        OsFunctionCall::DateTimeNow(_) => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            civil_from_days((now.as_secs() / 86_400) as i64).map(|(year, month, day)| {
                let seconds = now.as_secs() % 86_400;
                MontyObject::datetime(MontyDateTime {
                    year,
                    month,
                    day,
                    hour: (seconds / 3600) as u8,
                    minute: ((seconds / 60) % 60) as u8,
                    second: (seconds % 60) as u8,
                    microsecond: now.subsec_micros(),
                    offset_seconds: None,
                    timezone_name: None,
                })
            })
        }
        OsFunctionCall::Time(_) => Some(MontyObject::float(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
        )),
        OsFunctionCall::Urandom(args) => usize::try_from(args.size)
            .ok()
            .filter(|size| *size <= 1024 * 1024)
            .map(|size| MontyObject::bytes(vec![0; size])),
        OsFunctionCall::Sleep(_) | OsFunctionCall::AsyncSleep(_) => Some(MontyObject::none()),
        other => {
            return ExtFunctionResult::Error(MontyException::new(
                ExcType::PermissionError,
                Some(format!(
                    "OS call '{}' is unavailable in codemode; use glob(), list_dir(), or read_text()",
                    other.name()
                )),
            ));
        }
    };
    value.map_or_else(
        || {
            ExtFunctionResult::Error(MontyException::new(
                ExcType::RuntimeError,
                Some("could not service codemode OS call".to_owned()),
            ))
        },
        ExtFunctionResult::Return,
    )
}

fn civil_from_days(days: i64) -> Option<(i32, u8, u8)> {
    let z = days.checked_add(719_468)?;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    Some((year as i32, month as u8, day as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_matches_epoch() {
        assert_eq!(civil_from_days(0), Some((1970, 1, 1)));
        assert_eq!(civil_from_days(20_468), Some((2026, 1, 15)));
    }

    #[test]
    fn tool_error_alias_preserves_strings_and_comments() {
        let source =
            "value = 'ToolError'  # ToolError\ntry:\n    pass\nexcept ToolError:\n    pass\n";
        assert_eq!(
            alias_tool_error(source),
            "value = 'ToolError'  # ToolError\ntry:\n    pass\nexcept RuntimeError:\n    pass\n"
        );
    }
}
