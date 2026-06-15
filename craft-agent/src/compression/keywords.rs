use aho_corasick::AhoCorasick;

const ERROR_KEYWORDS: &[&str] = &[
    "error",
    "fatal",
    "panic",
    "critical",
    "exception",
    "segfault",
    "abort",
    "assertion failed",
    "stack overflow",
    "out of memory",
];

const WARNING_KEYWORDS: &[&str] = &["warning", "warn", "deprecated", "caution", "note:"];

const SECURITY_KEYWORDS: &[&str] = &[
    "vulnerability",
    "cve-",
    "exploit",
    "injection",
    "xss",
    "csrf",
    "overflow",
    "privilege",
];

const CODE_DEFINITION_KEYWORDS: &[&str] = &[
    "fn ",
    "struct ",
    "enum ",
    "impl ",
    "trait ",
    "interface ",
    "class ",
    "def ",
    "function ",
];

const CODE_IMPORT_KEYWORDS: &[&str] = &["use ", "import ", "from ", "#include", "require("];

const CODE_MODULE_KEYWORDS: &[&str] = &["mod ", "package ", "module "];

pub enum LineCategory {
    Error,
    Warning,
    Security,
    CodeDefinition,
    CodeImport,
    CodeModule,
    Comment,
    ClosingBrace,
    Blank,
    Other,
}

pub fn classify_line(line: &str) -> LineCategory {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return LineCategory::Blank;
    }

    let lower = trimmed.to_lowercase();

    static ERROR_AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();
    static WARNING_AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();
    static SECURITY_AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();
    static CODE_DEF_AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();
    static CODE_IMPORT_AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();
    static CODE_MOD_AC: std::sync::OnceLock<AhoCorasick> = std::sync::OnceLock::new();

    let error_ac = ERROR_AC.get_or_init(|| AhoCorasick::new(ERROR_KEYWORDS).unwrap());
    let warning_ac = WARNING_AC.get_or_init(|| AhoCorasick::new(WARNING_KEYWORDS).unwrap());
    let security_ac = SECURITY_AC.get_or_init(|| AhoCorasick::new(SECURITY_KEYWORDS).unwrap());
    let code_def_ac =
        CODE_DEF_AC.get_or_init(|| AhoCorasick::new(CODE_DEFINITION_KEYWORDS).unwrap());
    let code_import_ac =
        CODE_IMPORT_AC.get_or_init(|| AhoCorasick::new(CODE_IMPORT_KEYWORDS).unwrap());
    let code_mod_ac = CODE_MOD_AC.get_or_init(|| AhoCorasick::new(CODE_MODULE_KEYWORDS).unwrap());

    if error_ac.is_match(&lower) {
        return LineCategory::Error;
    }
    if security_ac.is_match(&lower) {
        return LineCategory::Security;
    }
    if warning_ac.is_match(&lower) {
        return LineCategory::Warning;
    }

    if code_def_ac.is_match(trimmed) {
        return LineCategory::CodeDefinition;
    }
    if code_import_ac.is_match(trimmed) {
        return LineCategory::CodeImport;
    }
    if code_mod_ac.is_match(trimmed) {
        return LineCategory::CodeModule;
    }

    if trimmed.starts_with("//")
        || trimmed.starts_with("#")
        || trimmed.starts_with("--")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
    {
        return LineCategory::Comment;
    }
    if trimmed == "}" || trimmed == "end" || trimmed == "end;" {
        return LineCategory::ClosingBrace;
    }

    LineCategory::Other
}

pub fn score_log_line(line: &str) -> i32 {
    match classify_line(line) {
        LineCategory::Blank => -5,
        LineCategory::Error => 10,
        LineCategory::Security => 8,
        LineCategory::Warning => 5,
        LineCategory::Comment => 0,
        LineCategory::Other => {
            let lower = line.trim().to_lowercase();
            if lower.starts_with("note") || lower.starts_with("info") {
                1
            } else {
                -1
            }
        }
        _ => 0,
    }
}

pub fn score_code_line(line: &str) -> i32 {
    match classify_line(line) {
        LineCategory::Blank => -5,
        LineCategory::Error => 10,
        LineCategory::Security => 8,
        LineCategory::Warning => 5,
        LineCategory::CodeDefinition => 10,
        LineCategory::CodeImport => 8,
        LineCategory::CodeModule => 8,
        LineCategory::Comment => -3,
        LineCategory::ClosingBrace => -4,
        LineCategory::Other => {
            let trimmed = line.trim();
            let mut score = 0;
            if trimmed.starts_with("pub ") || trimmed.starts_with("async ") {
                score += 10;
            }
            if trimmed.starts_with("const ")
                || trimmed.starts_with("let ")
                || trimmed.starts_with("static ")
            {
                score += 5;
            }
            if score == 0 { -1 } else { score }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_keywords_score_high() {
        assert_eq!(score_log_line("error: something failed"), 10);
        assert_eq!(score_log_line("FATAL: crash"), 10);
        assert_eq!(score_log_line("panic occurred"), 10);
        assert_eq!(score_log_line("segfault at 0x0"), 10);
    }

    #[test]
    fn warning_keywords_score_medium() {
        assert_eq!(score_log_line("warning: deprecated"), 5);
        assert_eq!(score_log_line("WARN: check this"), 5);
    }

    #[test]
    fn security_keywords_score_high() {
        assert_eq!(score_log_line("CVE-2024-1234 found"), 8);
        assert_eq!(score_log_line("SQL injection detected"), 8);
    }

    #[test]
    fn code_definitions_score_high() {
        assert_eq!(score_code_line("fn main() {"), 10);
        assert_eq!(score_code_line("struct Foo {"), 10);
        assert_eq!(score_code_line("class Bar:"), 10);
        assert_eq!(score_code_line("def baz():"), 10);
    }

    #[test]
    fn code_imports_score_medium() {
        assert_eq!(score_code_line("use std::sync::Arc;"), 8);
        assert_eq!(score_code_line("import React from 'react'"), 8);
        assert_eq!(score_code_line("#include <stdio.h>"), 8);
    }

    #[test]
    fn comments_and_braces_score_low() {
        assert_eq!(score_code_line("// this is a comment"), -3);
        assert_eq!(score_code_line("}"), -4);
    }

    #[test]
    fn blank_lines_score_lowest() {
        assert_eq!(score_log_line(""), -5);
        assert_eq!(score_code_line(""), -5);
        assert_eq!(score_code_line("   "), -5);
    }
}
