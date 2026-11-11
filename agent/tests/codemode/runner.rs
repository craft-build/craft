use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult,
    PrintWriter, ResourceLimits, ResourceTracker,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    convert::{from_json, object, to_json},
    stubs::TOOL_NAMES,
};

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
const MAX_TOOL_CALLS: usize = 4;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/codemode/corpus");

#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Vec<Value>,
    pub kwargs: BTreeMap<String, Value>,
}

impl Call {
    fn arg(&self, index: usize, name: &str) -> Option<&Value> {
        self.args.get(index).or_else(|| self.kwargs.get(name))
    }
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub calls: Vec<Call>,
    pub value: Option<Value>,
    pub failure: Option<String>,
    pub detail: String,
    pub stdout: String,
}

#[derive(Debug, Deserialize)]
struct Expected {
    #[serde(default)]
    calls: Vec<String>,
    #[serde(default)]
    failure: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    adversarial: bool,
    #[serde(default)]
    origin: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub regular_passed: usize,
    pub regular_total: usize,
    pub adversarial_passed: usize,
    pub adversarial_total: usize,
    pub failures: Vec<String>,
    pub origins: BTreeMap<String, usize>,
}

impl Report {
    fn record(&mut self, path: &Path, expected: &Expected, error: Option<String>) {
        *self.origins.entry(expected.origin.clone()).or_default() += 1;
        let (passed, total) = if expected.adversarial {
            (&mut self.adversarial_passed, &mut self.adversarial_total)
        } else {
            (&mut self.regular_passed, &mut self.regular_total)
        };
        *total += 1;
        if let Some(error) = error {
            self.failures.push(format!("{}: {error}", path.display()));
        } else {
            *passed += 1;
        }
    }
}

pub fn preflight(code: &str) -> Option<(String, String)> {
    if code.lines().count() > 2_000 {
        return Some(("limit".to_owned(), "source exceeds 2,000 lines".to_owned()));
    }
    for line in code.lines().map(str::trim) {
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
                return Some((
                    "unsupported".to_owned(),
                    format!("import {module}; use only {}", ALLOWED_MODULES.join(", ")),
                ));
            }
        }
    }
    if code
        .lines()
        .any(|line| line.trim_start().starts_with("match "))
    {
        return Some(("unsupported".to_owned(), "match statements".to_owned()));
    }
    if code.contains("open(") {
        return Some((
            "unsupported".to_owned(),
            "open(); use glob() or read_text()".to_owned(),
        ));
    }
    if code.lines().any(|line| {
        line.trim_start().starts_with("class ")
            && line
                .split_once('(')
                .is_some_and(|(_, bases)| !bases.starts_with(')'))
    }) {
        return Some(("unsupported".to_owned(), "class inheritance".to_owned()));
    }

    None
}

pub fn execute(code: &str) -> Outcome {
    let mut outcome = Outcome::default();
    if let Some((class, detail)) = preflight(code) {
        outcome.failure = Some(class);
        outcome.detail = detail;
        return outcome;
    }

    let runner = match MontyRun::new(
        code.to_owned(),
        "main.py",
        TOOL_NAMES.iter().map(|name| (*name).to_owned()).collect(),
        CompileOptions::default(),
    ) {
        Ok(runner) => runner,
        Err(error) => return exception_outcome(error, outcome),
    };
    let limits = ResourceLimits::default()
        .max_feed_duration(Duration::from_millis(100))
        .max_turn_duration(Duration::from_millis(100))
        .max_recursion_depth(50)
        .max_suspensions(100);
    let inputs = TOOL_NAMES
        .iter()
        .map(|name| MontyObject::function(*name, None))
        .collect();
    let mut progress = match runner.start(
        inputs,
        ResourceTracker::new(limits),
        PrintWriter::CollectString(&mut outcome.stdout, Some(MAX_OUTPUT_BYTES)),
    ) {
        Ok(progress) => progress,
        Err(error) => return exception_outcome(error, outcome),
    };
    let mut pending = BTreeMap::new();

    loop {
        progress = match progress {
            RunProgress::FunctionCall(call) => {
                if pending.len() + outcome.calls.len() >= MAX_TOOL_CALLS {
                    outcome.failure = Some("limit".to_owned());
                    outcome.detail = format!("nested tool-call cap {MAX_TOOL_CALLS} exceeded");
                    return outcome;
                }
                let args = call
                    .args
                    .args()
                    .map(|value| to_json(value.to_owned().as_ref()))
                    .collect();
                let kwargs = call
                    .args
                    .kwargs()
                    .map(|(key, value)| {
                        (
                            key.py_repr().trim_matches('\'').to_owned(),
                            to_json(value.to_owned().as_ref()),
                        )
                    })
                    .collect();
                let pending_call = Call {
                    name: call.function_name.clone(),
                    args,
                    kwargs,
                };
                let id = call.call_id;
                pending.insert(id, pending_call);
                match call.resume_pending(PrintWriter::CollectString(
                    &mut outcome.stdout,
                    Some(MAX_OUTPUT_BYTES),
                )) {
                    Ok(progress) => progress,
                    Err(error) => return exception_outcome(error, outcome),
                }
            }
            RunProgress::ResolveFutures(waiting) => {
                let results = std::mem::take(&mut pending)
                    .into_iter()
                    .map(|(id, call)| {
                        if !TOOL_NAMES.contains(&call.name.as_str()) {
                            return (id, ExtFunctionResult::NotFound(call.name.clone()));
                        }
                        let result = match dispatch(&call) {
                            Ok(value) => ExtFunctionResult::Return(from_json(&value)),
                            Err(message) => ExtFunctionResult::Error(MontyException::new(
                                ExcType::RuntimeError,
                                Some(message),
                            )),
                        };
                        outcome.calls.push(call);
                        (id, result)
                    })
                    .collect();
                match waiting.resume(
                    results,
                    PrintWriter::CollectString(&mut outcome.stdout, Some(MAX_OUTPUT_BYTES)),
                ) {
                    Ok(progress) => progress,
                    Err(error) => return exception_outcome(error, outcome),
                }
            }
            RunProgress::NameLookup(lookup) => {
                match lookup.resume(
                    NameLookupResult::Undefined,
                    PrintWriter::CollectString(&mut outcome.stdout, Some(MAX_OUTPUT_BYTES)),
                ) {
                    Ok(progress) => progress,
                    Err(error) => return exception_outcome(error, outcome),
                }
            }
            RunProgress::OsCall(call) => {
                outcome.failure = Some("unsupported".to_owned());
                outcome.detail = format!(
                    "OS call denied: {:?}; use glob() or read_text()",
                    call.function_call
                );
                return outcome;
            }
            RunProgress::Complete(value) => {
                let json = to_json(value.as_ref());
                if json.to_string().len() + outcome.stdout.len() > MAX_OUTPUT_BYTES {
                    outcome.failure = Some("limit".to_owned());
                    outcome.detail = format!("output cap {MAX_OUTPUT_BYTES} bytes exceeded");
                } else {
                    outcome.value = Some(json);
                }
                return outcome;
            }
        };
    }
}

fn exception_outcome(error: MontyException, mut outcome: Outcome) -> Outcome {
    let class = if error
        .message()
        .is_some_and(|message| message.starts_with("ToolError"))
    {
        "tool_error"
    } else if matches!(error.exc_type(), ExcType::NameError) {
        "unknown_tool"
    } else if matches!(error.exc_type(), ExcType::SyntaxError) {
        "syntax"
    } else if matches!(error.exc_type(), ExcType::NotImplementedError) {
        "unsupported"
    } else if matches!(
        error.exc_type(),
        ExcType::MemoryError | ExcType::RecursionError | ExcType::TimeoutError
    ) || error
        .message()
        .is_some_and(|message| message.contains("limit") || message.contains("exceeded"))
    {
        "limit"
    } else {
        "runtime"
    };
    outcome.failure = Some(class.to_owned());
    outcome.detail = error.to_string();
    outcome
}

fn dispatch(call: &Call) -> Result<Value, String> {
    let string_arg = |index: usize, name: &str| -> Result<String, String> {
        call.arg(index, name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                format!(
                    "ToolError from {}: missing string argument {name}",
                    call.name
                )
            })
    };
    match call.name.as_str() {
        "read_text" => {
            let path = string_arg(0, "path")?;
            source_file(&path)
                .map(Value::String)
                .ok_or_else(|| format!("ToolError from read_text: {path} not found"))
        }
        "read" => {
            let path = string_arg(0, "path")?;
            let source = source_file(&path)
                .ok_or_else(|| format!("ToolError from read: {path} not found"))?;
            let offset = call.arg(1, "offset").and_then(Value::as_u64).unwrap_or(1) as usize;
            let limit = call.arg(2, "limit").and_then(Value::as_u64).unwrap_or(200) as usize;
            let text = source
                .lines()
                .enumerate()
                .skip(offset.saturating_sub(1))
                .take(limit)
                .map(|(index, line)| format!("{}: {line}\n", index + 1))
                .collect();
            Ok(Value::String(text))
        }
        "glob" => Ok(json!(["src/alpha.rs", "src/beta.rs", "docs/guide.md"])),
        "grep" => Ok(json!([
            {"path": "src/alpha.rs", "line": 1, "text": "pub fn alpha() {"},
            {"path": "src/alpha.rs", "line": 4, "text": "// needle alpha"},
            {"path": "src/beta.rs", "line": 1, "text": "def beta():"}
        ])),
        "list_dir" => Ok(json!(["Cargo.toml", "README.md", "src/"])),
        "bash" => Ok(object([
            ("exit_code", json!(0)),
            ("stdout", json!("alpha\nbeta\n")),
            ("stderr", json!("")),
        ])),
        "edit" => {
            let path = string_arg(0, "path")?;
            let old = string_arg(1, "old_string")?;
            if old == "missing" {
                Err("ToolError from edit: old_string not found".to_owned())
            } else {
                Ok(Value::String(format!("updated {path}")))
            }
        }
        name => Err(format!("ToolError: {name} is not callable here")),
    }
}

fn source_file(path: &str) -> Option<String> {
    match path {
        "src/alpha.rs" => {
            Some("pub fn alpha() {\n    println!(\"alpha\");\n}\n// needle alpha\n".to_owned())
        }
        "src/beta.rs" => Some("def beta():\n    return \"beta\"\n\n# needle beta\n".to_owned()),
        "docs/guide.md" => Some("# Guide\n\nalpha beta alpha\n".to_owned()),
        "data.json" => Some("{\"items\":[1,2,3],\"name\":\"demo\"}".to_owned()),
        _ => None,
    }
}

pub fn run_corpus() -> Report {
    let mut report = Report::default();
    let mut paths: Vec<PathBuf> = fs::read_dir(CORPUS_DIR)
        .expect("corpus directory")
        .map(|entry| entry.expect("corpus entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "py"))
        .collect();
    paths.sort();
    assert_eq!(paths.len(), 150, "phase-0 corpus must contain 150 scripts");

    for path in paths {
        let expected_path = path.with_extension("expect.toml");
        let expected: Expected =
            toml::from_str(&fs::read_to_string(&expected_path).expect("expectation file"))
                .unwrap_or_else(|error| panic!("parsing {}: {error}", expected_path.display()));
        let source = fs::read_to_string(&path).expect("corpus source");
        let outcome = execute(&source);
        let actual_calls: Vec<&str> = outcome
            .calls
            .iter()
            .map(|call| call.name.as_str())
            .collect();
        let expected_calls: Vec<&str> = expected.calls.iter().map(String::as_str).collect();
        let mut mismatches = Vec::new();
        if actual_calls != expected_calls {
            mismatches.push(format!(
                "calls expected {expected_calls:?}, got {actual_calls:?}"
            ));
        }
        if outcome.failure != expected.failure {
            mismatches.push(format!(
                "failure expected {:?}, got {:?}",
                expected.failure, outcome.failure
            ));
        }
        if let Some(value) = &expected.value
            && outcome.value.as_ref() != Some(value)
        {
            mismatches.push(format!("value expected {value}, got {:?}", outcome.value));
        }
        if let Some(stdout) = &expected.stdout
            && outcome.stdout != *stdout
        {
            mismatches.push(format!(
                "stdout expected {stdout:?}, got {:?}",
                outcome.stdout
            ));
        }
        if let Some(failure) = &expected.failure
            && outcome.detail.is_empty()
        {
            mismatches.push(format!("{failure} had no diagnostic"));
        }
        if mismatches.is_empty() {
            report.record(&path, &expected, None);
        } else {
            report.record(
                &path,
                &expected,
                Some(format!("{}; {}", mismatches.join("; "), outcome.detail)),
            );
        }
    }
    report
}
