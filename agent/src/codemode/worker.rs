use std::{
    borrow::Cow,
    collections::BTreeMap,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyDate, MontyDateTime, MontyException,
    MontyObject, NameLookupResult, OsFunctionCall, PrintWriter, PrintWriterCallback,
    ResourceLimits, ResourceTracker,
};
use serde_json::Value;

use crate::config::CodemodeConfig;

const STACK_BYTES: usize = 64 * 1024 * 1024;
const ALLOWED_MODULES: &[&str] = &[
    "asyncio",
    "collections",
    "dataclasses",
    "datetime",
    "itertools",
    "json",
    "math",
    "re",
    "typing",
];

#[derive(Debug)]
pub struct PendingCall {
    pub id: u32,
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: BTreeMap<String, Value>,
}

#[derive(Debug)]
pub struct Finished {
    pub stdout: String,
    pub result: Result<Option<String>, String>,
}

#[derive(Debug)]
pub enum Message {
    Calls(Vec<PendingCall>),
    Print(String),
    Done(Finished),
}

pub type Reply = (u32, Result<Value, String>);

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

pub fn preflight(code: &str) -> Result<(), String> {
    if code.lines().count() > 2_000 {
        return Err("Limit: source exceeds 2,000 lines".to_owned());
    }
    let masked = mask_strings_and_comments(code);
    for line in masked.lines().map(str::trim) {
        let module = line
            .strip_prefix("import ")
            .and_then(|imports| imports.split([',', ' ']).next())
            .or_else(|| {
                line.strip_prefix("from ")
                    .and_then(|import| import.split(' ').next())
            });
        if let Some(module) = module {
            let module = module.split('.').next().unwrap_or_default();
            if !module.is_empty() && !ALLOWED_MODULES.contains(&module) {
                return Err(format!(
                    "Unsupported: import {module}; allowed modules: {}",
                    ALLOWED_MODULES.join(", ")
                ));
            }
        }
    }
    if masked
        .lines()
        .any(|line| line.trim_start().starts_with("match "))
    {
        return Err("Unsupported: match statements".to_owned());
    }
    if contains_call(&masked, "open") {
        return Err("Unsupported: open(); use glob() or read_text()".to_owned());
    }
    if masked.lines().any(|line| {
        line.trim_start().starts_with("class ")
            && line
                .split_once('(')
                .is_some_and(|(_, bases)| !bases.starts_with(')'))
    }) {
        return Err("Unsupported: class inheritance".to_owned());
    }
    Ok(())
}

/// Replaces string-literal and comment bytes with spaces while preserving byte
/// offsets, so lexical checks never interpret payload text as source.
fn mask_strings_and_comments(code: &str) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        String {
            quote: char,
            triple: bool,
            escaped: bool,
        },
        Comment,
    }

    let mut masked = code.as_bytes().to_vec();
    let mut state = State::Normal;
    let mut index = 0;
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    while index < chars.len() {
        let (offset, character) = chars[index];
        state = match state {
            State::Normal if character == '#' => {
                masked[offset] = b' ';
                State::Comment
            }
            State::Normal if is_string_start(&chars, index) => {
                let quote = character;
                let triple = chars.get(index + 1).is_some_and(|(_, next)| *next == quote)
                    && chars.get(index + 2).is_some_and(|(_, next)| *next == quote);
                for (relative, _) in code[offset..]
                    .char_indices()
                    .take(if triple { 3 } else { 1 })
                {
                    masked[offset + relative] = b' ';
                }
                index += if triple { 2 } else { 0 };
                State::String {
                    quote,
                    triple,
                    escaped: false,
                }
            }
            State::Comment if character == '\n' => State::Normal,
            State::Comment => {
                masked[offset] = b' ';
                State::Comment
            }
            State::String {
                quote,
                triple,
                escaped,
            } => {
                masked[offset] = b' ';
                if escaped {
                    State::String {
                        quote,
                        triple,
                        escaped: false,
                    }
                } else if character == '\\' {
                    State::String {
                        quote,
                        triple,
                        escaped: true,
                    }
                } else if !triple && character == quote {
                    State::Normal
                } else if triple
                    && character == quote
                    && chars.get(index + 1).is_some_and(|(_, next)| *next == quote)
                    && chars.get(index + 2).is_some_and(|(_, next)| *next == quote)
                {
                    if let Some((close, _)) = chars.get(index + 1) {
                        masked[*close] = b' ';
                    }
                    if let Some((close, _)) = chars.get(index + 2) {
                        masked[*close] = b' ';
                    }
                    index += 2;
                    State::Normal
                } else {
                    State::String {
                        quote,
                        triple,
                        escaped: false,
                    }
                }
            }
            State::Normal => State::Normal,
        };
        index += 1;
    }
    String::from_utf8(masked).unwrap_or_default()
}

fn is_string_start(chars: &[(usize, char)], index: usize) -> bool {
    let (_, quote) = chars[index];
    if quote != '"' && quote != '\'' {
        return false;
    }
    let mut prefix = String::new();
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        let character = chars[cursor].1;
        if character.is_ascii_alphabetic() {
            prefix.insert(0, character.to_ascii_lowercase());
        } else {
            let boundary = !(character.is_alphanumeric() || character == '_');
            let valid = matches!(
                prefix.as_str(),
                "" | "r" | "u" | "b" | "f" | "rb" | "br" | "rf" | "fr"
            );
            return boundary && valid;
        }
    }
    matches!(
        prefix.as_str(),
        "r" | "u" | "b" | "f" | "rb" | "br" | "rf" | "fr"
    )
}

fn contains_call(masked: &str, identifier: &str) -> bool {
    let chars: Vec<(usize, char)> = masked.char_indices().collect();
    let mut index = 0;
    while index < chars.len() {
        let (_, character) = chars[index];
        if !(character.is_ascii_alphanumeric() || character == '_') {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len()
            && (chars[index].1.is_ascii_alphanumeric() || chars[index].1 == '_')
        {
            index += 1;
        }
        let end = chars
            .get(index.saturating_sub(1))
            .map_or(start, |(offset, _)| offset + chars[index - 1].1.len_utf8());
        if &masked[start..end] == identifier {
            while index < chars.len() && chars[index].1.is_whitespace() {
                index += 1;
            }
            if chars.get(index).is_some_and(|(_, next)| *next == '(') {
                return true;
            }
        }
    }
    false
}

pub type WorkerChannels = (
    flume::Receiver<Message>,
    flume::Sender<Vec<Reply>>,
    thread::JoinHandle<()>,
);

pub fn spawn(
    code: String,
    timeout_secs: u64,
    config: CodemodeConfig,
    names: Vec<String>,
) -> Result<WorkerChannels, String> {
    preflight(&code)?;
    let (message_tx, message_rx) = flume::unbounded();
    let (reply_tx, reply_rx) = flume::unbounded();
    let handle = thread::Builder::new()
        .name("craft-codemode".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(code, timeout_secs, config, names, &message_tx, &reply_rx)
            }));
            match result {
                Ok(finished) => {
                    let _ = message_tx.send(Message::Done(finished));
                }
                Err(_) => {
                    let _ = message_tx.send(Message::Done(Finished {
                        stdout: String::new(),
                        result: Err("Crash: codemode worker panicked".to_owned()),
                    }));
                }
            }
        })
        .map_err(|error| format!("Crash: could not start codemode worker: {error}"))?;
    Ok((message_rx, reply_tx, handle))
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
) -> Result<Option<String>, String> {
    let code = alias_tool_error(&code);
    let runner = MontyRun::new(code, "main.py", names.clone(), CompileOptions::default())
        .map_err(format_exception)?;
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
        .map_err(format_exception)?;
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
                    .map_err(format_exception)?
            }
            RunProgress::ResolveFutures(waiting) => {
                let calls = std::mem::take(&mut pending)
                    .into_values()
                    .collect::<Vec<_>>();
                _sender
                    .send(Message::Calls(calls))
                    .map_err(|_| "Cancelled: codemode bridge closed".to_owned())?;
                let replies = replies
                    .recv()
                    .map_err(|_| "Cancelled: codemode bridge closed".to_owned())?;
                let results = replies
                    .into_iter()
                    .map(|(id, result)| {
                        let result = match result {
                            Ok(value) => {
                                ExtFunctionResult::Return(super::convert::from_json(&value))
                            }
                            Err(message) => ExtFunctionResult::Error(MontyException::new(
                                ExcType::RuntimeError,
                                Some(message),
                            )),
                        };
                        (id, result)
                    })
                    .collect();
                waiting
                    .resume(results, PrintWriter::Callback(output))
                    .map_err(format_exception)?
            }
            RunProgress::NameLookup(lookup) => {
                let result = if names.contains(&lookup.name) {
                    NameLookupResult::Value(MontyObject::function(lookup.name.clone(), None))
                } else {
                    NameLookupResult::Undefined
                };
                lookup
                    .resume(result, PrintWriter::Callback(output))
                    .map_err(format_exception)?
            }
            RunProgress::OsCall(call) => call
                .resume_with(PrintWriter::Callback(output), os_result)
                .map_err(format_exception)?,
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
    let masked = mask_strings_and_comments(code);
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

fn format_exception(error: MontyException) -> String {
    let class = match error.exc_type() {
        ExcType::SyntaxError => "Syntax",
        ExcType::NameError => "Unknown tool/name",
        ExcType::NotImplementedError => "Unsupported",
        ExcType::MemoryError | ExcType::RecursionError | ExcType::TimeoutError => "Limit",
        ExcType::RuntimeError => "ToolError",
        _ => "Runtime",
    };
    format!("{class}: {error}")
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
    fn open_preflight_ignores_payload_text_and_prefixed_identifiers() {
        assert!(preflight("print('open(')").is_ok());
        assert!(preflight("# open('file')\nhandle_open(path)").is_ok());
        assert!(preflight("reopen('file')").is_ok());
        assert!(preflight("open  ('file')").is_err());
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
