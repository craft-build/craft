//! Tool-output pre-compression: heuristic content-type detection plus
//! type-specific compressors, applied to tool-result text at request-build
//! time so the model sees trimmed outputs while history keeps the raw ones.
//!
//! Ported from the reference craft agent's `compression/` module (Magika
//! ONNX detection intentionally not ported; heuristics only).

mod code;
mod diff_comp;
mod json;
mod keywords;
mod log;
mod search;
pub mod store;

use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
    Code,
    Log,
    SearchResult,
    Diff,
    JsonArray,
    PlainText,
}

/// Tunable compression settings; defaults mirror the reference agent.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressionConfig {
    pub enabled: bool,
    pub code_compression_rate: f32,
    pub max_log_lines: usize,
    pub max_search_files: usize,
    pub max_matches_per_file: usize,
    pub max_diff_lines: usize,
    pub max_json_items: usize,
    pub json_first_keep: usize,
    pub json_last_keep: usize,
    /// Carried for config parity; enforcement belongs to compaction (D.9).
    pub protect_recent_tool_outputs: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            code_compression_rate: 0.3,
            max_log_lines: 50,
            max_search_files: 20,
            max_matches_per_file: 5,
            max_diff_lines: 100,
            max_json_items: 15,
            json_first_keep: 5,
            json_last_keep: 3,
            protect_recent_tool_outputs: 2,
        }
    }
}

impl CompressionConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.code_compression_rate.is_finite()
            || self.code_compression_rate <= 0.0
            || self.code_compression_rate > 1.0
        {
            return Err("compression.code_compression_rate must be in (0, 1]".into());
        }
        Ok(())
    }
}

static ERROR_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(error|fatal|panic|critical|exception|traceback)").unwrap());
static WARNING_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(warning|warn)").unwrap());
static DIFF_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(diff --git|---|\+\+\+|@@)").unwrap());
static JSON_ARRAY_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\[").unwrap());
static CODE_LINE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d+:\s").unwrap());

/// Minimum output size before compression is considered worthwhile.
pub const MIN_COMPRESS_LEN: usize = 200;

/// Detect content type from tool output text. Uses simple heuristics.
pub fn detect_content_type(text: &str) -> ContentType {
    if text.is_empty() {
        return ContentType::PlainText;
    }

    if DIFF_HEADER.is_match(text) {
        return ContentType::Diff;
    }

    let code_line_count = text
        .lines()
        .filter(|l| CODE_LINE_PATTERN.is_match(l))
        .count();
    let total_lines = text.lines().count();
    if total_lines > 3 && code_line_count as f32 / total_lines as f32 > 0.7 {
        return ContentType::Code;
    }

    if JSON_ARRAY_START.is_match(text) {
        return ContentType::JsonArray;
    }

    let error_count = text.lines().filter(|l| ERROR_LINE.is_match(l)).count();
    let warning_count = text.lines().filter(|l| WARNING_LINE.is_match(l)).count();
    if error_count + warning_count > 0 && total_lines > 10 {
        return ContentType::Log;
    }

    ContentType::PlainText
}

/// Compress content based on type and config. Returns compressed text.
pub fn compress(text: &str, content_type: ContentType, config: &CompressionConfig) -> String {
    if !config.enabled || text.is_empty() {
        return text.to_owned();
    }

    match content_type {
        ContentType::Code => code::compress_code(text, config.code_compression_rate),
        ContentType::Log => log::compress_log(text, config.max_log_lines),
        ContentType::SearchResult => {
            search::compress_search(text, config.max_search_files, config.max_matches_per_file)
        }
        ContentType::Diff => diff_comp::compress_diff(text, config.max_diff_lines),
        ContentType::JsonArray => json::compress_json_array(
            text,
            config.max_json_items,
            config.json_first_keep,
            config.json_last_keep,
        ),
        ContentType::PlainText => text.to_owned(),
    }
}

/// Tools whose output is exactly what the model asked for and must reach it
/// verbatim. These return caller-selected content that is already bounded by
/// the tool's own budget, and their `N: ` line numbering trips the code
/// detector — so pre-compression would delete the very lines requested.
///
/// - `read`: a line range chosen via `offset`/`limit`.
/// - `grep`: matched lines, already capped by `max_matches`/`MAX_OUTPUT_BYTES`.
/// - `retrieve`: the original text recovered by content hash; re-compressing it
///   would defeat the point of the reversible-compression store.
const VERBATIM_TOOLS: &[&str] = &["read", "grep", "retrieve"];

/// Whether a tool result should pass through request-view pre-compression.
pub fn should_compress_tool(tool: &str) -> bool {
    !VERBATIM_TOOLS.contains(&tool)
}

/// The request-side gate the reference applies via `as_text_for_llm`:
/// short, empty, or disabled outputs pass through; everything else is
/// detected and compressed.
pub fn compress_for_llm(text: &str, config: &CompressionConfig) -> String {
    if !config.enabled || text.len() < MIN_COMPRESS_LEN {
        return text.to_owned();
    }
    let ct = detect_content_type(text);
    compress(text, ct, config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CompressionConfig {
        CompressionConfig::default()
    }

    #[test]
    fn detect_diff_header() {
        let text = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-x\n+y\n";
        assert_eq!(detect_content_type(text), ContentType::Diff);
    }

    #[test]
    fn detect_numbered_code_density() {
        let text = (1..20)
            .map(|i| format!("{i}: code line\n"))
            .collect::<String>();
        assert_eq!(detect_content_type(&text), ContentType::Code);
    }

    #[test]
    fn detect_json_array() {
        let text = "[\n  {\"a\": 1},\n  {\"a\": 2}\n]";
        assert_eq!(detect_content_type(text), ContentType::JsonArray);
    }

    #[test]
    fn detect_log_by_error_density() {
        let mut text = String::new();
        for i in 0..15 {
            text.push_str(&format!("line {i}\n"));
        }
        text.push_str("error: boom\n");
        assert_eq!(detect_content_type(&text), ContentType::Log);
    }

    #[test]
    fn plain_text_falls_through() {
        assert_eq!(
            detect_content_type("just words\nmore words\n"),
            ContentType::PlainText
        );
        assert_eq!(detect_content_type(""), ContentType::PlainText);
    }

    #[test]
    fn verbatim_tools_are_never_compressed() {
        assert!(!should_compress_tool("read"));
        assert!(!should_compress_tool("grep"));
        assert!(!should_compress_tool("retrieve"));
        assert!(should_compress_tool("bash"));
        assert!(should_compress_tool("glob"));
    }

    #[test]
    fn compress_for_llm_passes_short_output_through() {
        assert_eq!(
            compress_for_llm("short content", &config()),
            "short content"
        );
    }

    #[test]
    fn compress_for_llm_disabled_returns_raw() {
        let long = "1: fn foo()\n".repeat(50);
        let cfg = CompressionConfig {
            enabled: false,
            ..config()
        };
        assert_eq!(compress_for_llm(&long, &cfg), long);
    }

    #[test]
    fn compress_for_llm_compresses_long_code_output() {
        let long = (1..=100)
            .map(|i| format!("{i}: let x = {i};\n"))
            .collect::<String>();
        let compressed = compress_for_llm(&long, &config());
        assert!(compressed.len() < long.len());
        assert!(compressed.contains("lines omitted"));
    }
}

