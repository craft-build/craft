//! Failure classification, repair hints, and preflight — the single
//! implementation shared by the worker, the bridge, and the corpus runner.
//!
//! Failures are classified at origin (worker and bridge), not by string
//! matching at a final seam: every producer builds a [`Failure`] carrying
//! its class, line, message, detail, and one concrete repair hint.

use monty_types::{ExcType, MontyException};

pub const ALLOWED_MODULES: &[&str] = &[
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

pub const MAX_SCRIPT_LINES: usize = 2_000;
const MAX_SUGGESTIONS: usize = 3;
const MAX_SUGGESTION_DISTANCE: usize = 3;

/// The eight doc classes plus the internal `Crash` variant kept distinct so
/// Phase 4's worker isolation has its signal (a crash is never silently a
/// runtime failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Syntax,
    Type,
    Unsupported,
    UnknownTool,
    ToolError,
    Runtime,
    Limit,
    Cancelled,
    Crash,
}

impl Class {
    /// The telemetry and corpus-runner name (`docs/codemode.md` class table).
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Syntax => "syntax",
            Class::Type => "type",
            Class::Unsupported => "unsupported",
            Class::UnknownTool => "unknown_tool",
            Class::ToolError => "tool_error",
            Class::Runtime => "runtime",
            Class::Limit => "limit",
            Class::Cancelled => "cancelled",
            Class::Crash => "crash",
        }
    }

    /// The token that prefixes the model-facing failure line.
    pub fn label(self) -> &'static str {
        match self {
            Class::Syntax => "Syntax",
            Class::Type => "Type",
            Class::Unsupported => "Unsupported",
            Class::UnknownTool => "UnknownTool",
            Class::ToolError => "ToolError",
            Class::Runtime => "Runtime",
            Class::Limit => "Limit",
            Class::Cancelled => "Cancelled",
            Class::Crash => "Crash",
        }
    }
}

/// One classified script failure: `message` is the concise summary the model
/// sees, `detail` the full underlying error text (Monty display) telemetry
/// records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub class: Class,
    pub line: Option<u32>,
    pub message: String,
    pub detail: String,
    pub hint: Option<String>,
}

impl Failure {
    pub fn plain(class: Class, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            class,
            line: None,
            detail: message.clone(),
            message,
            hint: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn cancelled(calls: usize) -> Self {
        Self::plain(
            Class::Cancelled,
            format!("Cancelled after {calls} tool calls"),
        )
    }

    pub fn crash(message: impl Into<String>) -> Self {
        Self::plain(Class::Crash, message)
    }

    /// Classifies an uncaught Monty exception (ported from the Phase 0
    /// corpus runner so production and tests cannot drift).
    pub fn from_exception(error: &MontyException) -> Self {
        let raw_message = error.message().unwrap_or_default().to_owned();
        let class = classify(error.exc_type(), &raw_message);
        let message = if class == Class::ToolError {
            // The payload already starts with "ToolError from ..."; the
            // RuntimeError exc-type prefix would only repeat itself.
            raw_message
        } else {
            error.summary()
        };
        Self {
            class,
            line: error
                .traceback()
                .iter()
                .next_back()
                .map(|frame| frame.start.line),
            message,
            detail: error.to_string(),
            hint: None,
        }
    }

    /// Attaches the doc's repair hints using the catalog's Python names:
    /// suggestions for unknown tools, the try/except hint for uncaught
    /// child errors, and the `await` hint for the coroutine misuse Monty
    /// reports at runtime while the `ty` preflight is unavailable.
    pub fn attach_hints(&mut self, callable: &[String], uncallable: &[String]) {
        match self.class {
            Class::UnknownTool => {
                let mut parts = Vec::new();
                if let Some(name) = offending_name(&self.message) {
                    let suggestions = suggest(name, callable);
                    if !suggestions.is_empty() {
                        parts.push(format!("did you mean: {}?", suggestions.join(", ")));
                    }
                }
                if !uncallable.is_empty() {
                    parts.push(format!("not callable from code: {}", uncallable.join(", ")));
                }
                if !parts.is_empty() {
                    self.hint = Some(parts.join("; "));
                }
            }
            Class::ToolError => {
                self.hint = Some(
                    "wrap the call in `try/except ToolError`, or use `gather` if partial \
                     progress is acceptable"
                        .to_owned(),
                );
            }
            Class::Limit => {
                self.hint =
                    Some("narrow the loop or raise `timeout_secs` (maximum 120)".to_owned());
            }
            Class::Runtime if is_missing_await(&self.message) => {
                self.hint = Some("add `await` before the call".to_owned());
            }
            _ => {}
        }
    }

    /// The model-facing text: class-prefixed `line N: message`, a three-line
    /// excerpt for syntax errors, and one concrete repair hint. The caller
    /// caps the joined result through [`super::adapters::shape_output`].
    pub fn render(&self, code: &str) -> String {
        let mut out = if self.class == Class::Cancelled {
            // The message already reads "Cancelled after N tool calls".
            self.message.clone()
        } else {
            match self.line {
                Some(line) => format!("{} line {line}: {}", self.class.label(), self.message),
                None => format!("{}: {}", self.class.label(), self.message),
            }
        };
        if self.class == Class::Syntax
            && let Some(line) = self.line
        {
            let excerpt = excerpt(code, line);
            if !excerpt.is_empty() {
                out.push('\n');
                out.push_str(&excerpt);
            }
        }
        if let Some(hint) = &self.hint {
            out.push_str("\nhint: ");
            out.push_str(hint);
        }
        out
    }
}

fn classify(exc_type: ExcType, message: &str) -> Class {
    if message.starts_with("ToolError") {
        Class::ToolError
    } else if matches!(exc_type, ExcType::NameError) {
        Class::UnknownTool
    } else if matches!(exc_type, ExcType::SyntaxError) {
        Class::Syntax
    } else if matches!(exc_type, ExcType::NotImplementedError) {
        Class::Unsupported
    } else if matches!(
        exc_type,
        ExcType::MemoryError | ExcType::RecursionError | ExcType::TimeoutError
    ) || message.contains("limit")
        || message.contains("exceeded")
    {
        Class::Limit
    } else {
        Class::Runtime
    }
}

/// Monty reports a call that was used without `await` as an attribute or
/// length operation on the coroutine object, line-correct and before any
/// dispatch (verified in Phase 0).
fn is_missing_await(message: &str) -> bool {
    message.contains("'coroutine' object") || message.contains("type 'coroutine'")
}

fn offending_name(message: &str) -> Option<&str> {
    message
        .split("name '")
        .nth(1)
        .and_then(|rest| rest.split("' is not defined").next())
        .filter(|name| !name.is_empty() && !name.contains('\''))
}

/// Up to [`MAX_SUGGESTIONS`] callable names within
/// [`MAX_SUGGESTION_DISTANCE`] edits, closest first.
pub fn suggest(name: &str, candidates: &[String]) -> Vec<String> {
    let mut ranked: Vec<(usize, &String)> = candidates
        .iter()
        .map(|candidate| (levenshtein(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= MAX_SUGGESTION_DISTANCE)
        .collect();
    ranked.sort_by(|(left_distance, left), (right_distance, right)| {
        left_distance
            .cmp(right_distance)
            .then_with(|| left.as_str().cmp(right.as_str()))
    });
    ranked
        .into_iter()
        .map(|(_, candidate)| candidate.clone())
        .take(MAX_SUGGESTIONS)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (row, left_char) in left.iter().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right.iter().enumerate() {
            current[column + 1] = (previous[column + 1] + 1)
                .min(current[column] + 1)
                .min(previous[column] + usize::from(left_char != right_char));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// The failing line plus one neighbor on each side, numbered like `read`.
fn excerpt(code: &str, line: u32) -> String {
    let start = line.saturating_sub(1).max(1);
    let end = line.saturating_add(1);
    let mut out = String::new();
    for (index, text) in code.lines().enumerate() {
        let number = index + 1;
        if (number as u32) < start {
            continue;
        }
        if (number as u32) > end {
            break;
        }
        out.push_str(&format!("{number:>4} | {text}\n"));
    }
    out.trim_end().to_owned()
}

/// Lexical preflight over the model's own source: script-size cap and the
/// unsupported-construct scans, raised before any interpretation.
pub fn preflight(code: &str) -> Result<(), Failure> {
    if code.lines().count() > MAX_SCRIPT_LINES {
        return Err(Failure::plain(
            Class::Limit,
            format!("source exceeds {MAX_SCRIPT_LINES} lines"),
        )
        .with_hint("split the work across code calls"));
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
                return Err(
                    Failure::plain(Class::Unsupported, format!("import {module}"))
                        .with_hint(format!("allowed modules: {}", ALLOWED_MODULES.join(", "))),
                );
            }
        }
    }
    if masked
        .lines()
        .any(|line| line.trim_start().starts_with("match "))
    {
        return Err(Failure::plain(Class::Unsupported, "match statements")
            .with_hint("rewrite with if/elif"));
    }
    if contains_call(&masked, "open") {
        return Err(
            Failure::plain(Class::Unsupported, "open()").with_hint("use glob() or read_text()")
        );
    }
    if masked.lines().any(|line| {
        line.trim_start().starts_with("class ")
            && line
                .split_once('(')
                .is_some_and(|(_, bases)| !bases.starts_with(')'))
    }) {
        return Err(Failure::plain(Class::Unsupported, "class inheritance")
            .with_hint("drop the base class"));
    }
    Ok(())
}

/// Replaces string-literal and comment bytes with spaces while preserving byte
/// offsets, so lexical checks never interpret payload text as source.
pub(super) fn mask_strings_and_comments(code: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn callable() -> Vec<String> {
        ["read_text", "read", "edit", "glob"]
            .iter()
            .map(|name| name.to_string())
            .collect()
    }

    #[test]
    fn classify_ports_the_corpus_runner_mapping() {
        let failure = |exc_type, message: &str| classify(exc_type, message);
        assert_eq!(
            failure(ExcType::RuntimeError, "ToolError from edit: boom"),
            Class::ToolError
        );
        assert_eq!(
            failure(ExcType::NameError, "name 'x' is not defined"),
            Class::UnknownTool
        );
        assert_eq!(
            failure(ExcType::SyntaxError, "Expected an expression"),
            Class::Syntax
        );
        assert_eq!(
            failure(ExcType::NotImplementedError, "no"),
            Class::Unsupported
        );
        assert_eq!(
            failure(ExcType::MemoryError, "output cap 42 bytes exceeded"),
            Class::Limit
        );
        assert_eq!(
            failure(ExcType::RecursionError, "maximum recursion depth exceeded"),
            Class::Limit
        );
        assert_eq!(
            failure(ExcType::TimeoutError, "feed time limit exceeded"),
            Class::Limit
        );
        assert_eq!(
            failure(ExcType::RuntimeError, "limit reached"),
            Class::Limit
        );
        assert_eq!(failure(ExcType::ValueError, "boom"), Class::Runtime);
        assert_eq!(
            failure(
                ExcType::AttributeError,
                "'coroutine' object has no attribute"
            ),
            Class::Runtime
        );
    }

    #[test]
    fn exception_failure_carries_the_innermost_line() {
        let mut error = MontyException::new(ExcType::ValueError, Some("boom".to_owned()));
        error.add_traceback(vec![
            monty_types::StackFrame {
                filename: "main.py".to_owned(),
                start: monty_types::CodeLoc { line: 1, column: 1 },
                end: monty_types::CodeLoc { line: 1, column: 5 },
                frame_name: Some("<module>".to_owned()),
                preview_line: None,
                hide_caret: false,
                hide_frame_name: false,
            },
            monty_types::StackFrame {
                filename: "main.py".to_owned(),
                start: monty_types::CodeLoc { line: 4, column: 1 },
                end: monty_types::CodeLoc { line: 4, column: 9 },
                frame_name: Some("boom".to_owned()),
                preview_line: None,
                hide_caret: false,
                hide_frame_name: false,
            },
        ]);
        let failure = Failure::from_exception(&error);
        assert_eq!(failure.class, Class::Runtime);
        assert_eq!(failure.line, Some(4));
        assert_eq!(failure.message, "ValueError: boom");
    }

    #[test]
    fn suggestions_rank_by_distance_then_name() {
        let candidates: Vec<String> = ["read_text", "read", "edit", "grep"]
            .iter()
            .map(|name| name.to_string())
            .collect();
        assert_eq!(suggest("rad", &candidates), ["read", "grep"]);
        assert_eq!(suggest("rea", &candidates), ["read", "grep"]);
        assert_eq!(suggest("edit", &candidates), ["edit"]);
        // Distances above the cap are noise, not suggestions.
        assert!(suggest("missing_tool", &candidates).is_empty());
    }

    #[test]
    fn levenshtein_handles_common_edits() {
        assert_eq!(levenshtein("read", "read"), 0);
        assert_eq!(levenshtein("read", "rea"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
    }

    #[test]
    fn unknown_tool_hint_lists_suggestions_and_uncallables() {
        let mut failure =
            Failure::plain(Class::UnknownTool, "NameError: name 'rad' is not defined");
        failure.attach_hints(&callable(), &["batch".to_owned(), "task".to_owned()]);
        assert_eq!(
            failure.hint.as_deref(),
            Some("did you mean: read?; not callable from code: batch, task")
        );

        let mut unreachable = Failure::plain(
            Class::UnknownTool,
            "NameError: name 'missing_tool' is not defined",
        );
        unreachable.attach_hints(&callable(), &[]);
        assert_eq!(unreachable.hint, None);
    }

    #[test]
    fn missing_await_attaches_the_await_hint() {
        let mut attribute = Failure::plain(
            Class::Runtime,
            "AttributeError: 'coroutine' object has no attribute 'strip'",
        );
        attribute.attach_hints(&callable(), &[]);
        assert_eq!(
            attribute.hint.as_deref(),
            Some("add `await` before the call")
        );

        let mut length = Failure::plain(
            Class::Runtime,
            "TypeError: object of type 'coroutine' has no len()",
        );
        length.attach_hints(&callable(), &[]);
        assert_eq!(length.hint.as_deref(), Some("add `await` before the call"));

        let mut other = Failure::plain(Class::Runtime, "TypeError: unsupported operand");
        other.attach_hints(&callable(), &[]);
        assert_eq!(other.hint, None);
    }

    #[test]
    fn tool_error_and_limit_hints_follow_the_doc() {
        let mut tool_error = Failure::plain(Class::ToolError, "ToolError from edit: boom");
        tool_error.attach_hints(&callable(), &[]);
        assert_eq!(
            tool_error.hint.as_deref(),
            Some(
                "wrap the call in `try/except ToolError`, or use `gather` if partial progress is acceptable"
            )
        );

        let mut limit = Failure::plain(Class::Limit, "wall-clock limit 5s exceeded");
        limit.attach_hints(&callable(), &[]);
        assert_eq!(
            limit.hint.as_deref(),
            Some("narrow the loop or raise `timeout_secs` (maximum 120)")
        );
    }

    #[test]
    fn render_prefixes_class_and_line_for_every_class() {
        let code = "value = 1\nvalue =\nresult = value\n";
        let syntax = Failure {
            class: Class::Syntax,
            line: Some(2),
            message: "SyntaxError: Expected an expression".to_owned(),
            detail: String::new(),
            hint: None,
        };
        let rendered = syntax.render(code);
        assert_eq!(
            rendered,
            "Syntax line 2: SyntaxError: Expected an expression\n   1 | value = 1\n   2 | value =\n   3 | result = value"
        );

        assert_eq!(
            Failure::plain(Class::Type, "line 3: CoroutineType").render(code),
            "Type: line 3: CoroutineType"
        );
        assert_eq!(
            Failure::plain(Class::Unsupported, "import pandas").render(code),
            "Unsupported: import pandas"
        );
        assert_eq!(
            Failure::plain(Class::UnknownTool, "NameError: name 'x' is not defined").render(code),
            "UnknownTool: NameError: name 'x' is not defined"
        );
        assert_eq!(
            Failure::plain(
                Class::ToolError,
                "ToolError from edit: old_string not found"
            )
            .render(code),
            "ToolError: ToolError from edit: old_string not found"
        );
        assert_eq!(
            Failure::plain(Class::Runtime, "ValueError: boom").render(code),
            "Runtime: ValueError: boom"
        );
        assert_eq!(
            Failure::plain(Class::Limit, "wall-clock limit 5s exceeded").render(code),
            "Limit: wall-clock limit 5s exceeded"
        );
        assert_eq!(
            Failure::cancelled(3).render(code),
            "Cancelled after 3 tool calls"
        );
        assert_eq!(
            Failure::crash("codemode worker panicked").render(code),
            "Crash: codemode worker panicked"
        );
    }

    #[test]
    fn render_appends_the_hint_after_the_message() {
        let mut failure = Failure::plain(Class::Limit, "wall-clock limit 5s exceeded");
        failure.attach_hints(&callable(), &[]);
        assert_eq!(
            failure.render(""),
            "Limit: wall-clock limit 5s exceeded\nhint: narrow the loop or raise `timeout_secs` (maximum 120)"
        );
    }

    #[test]
    fn excerpt_clamps_to_the_source() {
        assert_eq!(excerpt("one\ntwo", 1), "   1 | one\n   2 | two");
        assert_eq!(excerpt("one\ntwo", 2), "   1 | one\n   2 | two");
        assert_eq!(excerpt("only", 1), "   1 | only");
        assert_eq!(
            excerpt("one", 9),
            "",
            "a line past the source has no excerpt"
        );
    }

    #[test]
    fn preflight_classifies_each_rejection() {
        assert_eq!(
            preflight("import pandas\npandas.DataFrame()")
                .unwrap_err()
                .class,
            Class::Unsupported
        );
        assert_eq!(
            preflight("match value:\n    case 1:\n        pass")
                .unwrap_err()
                .class,
            Class::Unsupported
        );
        assert_eq!(
            preflight("open('f')").unwrap_err().class,
            Class::Unsupported
        );
        assert_eq!(
            preflight("class Child(Base):\n    pass").unwrap_err().class,
            Class::Unsupported
        );
        assert_eq!(
            preflight(&"line\n".repeat(MAX_SCRIPT_LINES + 1))
                .unwrap_err()
                .class,
            Class::Limit
        );
        assert!(preflight("import json\njson.loads('{}')").is_ok());
    }

    #[test]
    fn preflight_hints_name_the_allowed_modules_and_replacements() {
        let import = preflight("import pandas").unwrap_err();
        assert_eq!(import.message, "import pandas");
        assert_eq!(
            import.hint.as_deref(),
            Some(
                "allowed modules: asyncio, collections, dataclasses, datetime, itertools, json, math, re, typing"
            )
        );
        assert_eq!(
            preflight("open('f')").unwrap_err().hint.as_deref(),
            Some("use glob() or read_text()")
        );
    }

    #[test]
    fn open_preflight_ignores_payload_text_and_prefixed_identifiers() {
        assert!(preflight("print('open(')").is_ok());
        assert!(preflight("# open('file')\nhandle_open(path)").is_ok());
        assert!(preflight("reopen('file')").is_ok());
        assert!(preflight("open  ('file')").is_err());
    }

    #[test]
    fn tool_error_alias_masking_preserves_strings_and_comments() {
        let source =
            "value = 'ToolError'  # ToolError\ntry:\n    pass\nexcept ToolError:\n    pass\n";
        let masked = mask_strings_and_comments(source);
        assert!(!masked.contains("'ToolError'"));
        assert!(masked.trim_start_matches(' ').contains("except ToolError:"));
    }
}
