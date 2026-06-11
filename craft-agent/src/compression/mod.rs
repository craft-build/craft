mod code;
mod diff_comp;
mod json;
mod keywords;
mod log;
mod search;

use once_cell::sync::Lazy;
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

/// Compression configuration. Mirrors craft_config::CompressionConfig but
/// we pass it by value to keep compressors simple.
#[derive(Debug, Clone)]
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
    pub protect_recent_tool_outputs: usize,
    pub semantic_enabled: bool,
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
            semantic_enabled: false,
        }
    }
}

impl From<&craft_config::CompressionConfig> for CompressionConfig {
    fn from(c: &craft_config::CompressionConfig) -> Self {
        Self {
            enabled: c.enabled,
            code_compression_rate: c.code_compression_rate,
            max_log_lines: c.max_log_lines,
            max_search_files: c.max_search_files,
            max_matches_per_file: c.max_matches_per_file,
            max_diff_lines: c.max_diff_lines,
            max_json_items: c.max_json_items,
            json_first_keep: c.json_first_keep,
            json_last_keep: c.json_last_keep,
            protect_recent_tool_outputs: c.protect_recent_tool_outputs,
            semantic_enabled: c.semantic_enabled,
        }
    }
}

static ERROR_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(error|fatal|panic|critical|exception|traceback)").unwrap()
});
static WARNING_LINE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)^(warning|warn)").unwrap()
});
static DIFF_HEADER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(diff --git|---|\+\+\+|@@)").unwrap()
});
static JSON_ARRAY_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*\[").unwrap()
});
static CODE_LINE_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*\d+:\s").unwrap()
});

/// Detect content type from tool output text. Uses simple heuristics.
pub fn detect_content_type(text: &str) -> ContentType {
    if text.is_empty() {
        return ContentType::PlainText;
    }

    if DIFF_HEADER.is_match(text) {
        return ContentType::Diff;
    }

    let code_line_count = text.lines().filter(|l| CODE_LINE_PATTERN.is_match(l)).count();
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

#[cfg(feature = "onnx")]
static MAGIKA_MODEL: std::sync::OnceLock<Result<std::sync::Mutex<magika::Session>, String>> = std::sync::OnceLock::new();

#[cfg(feature = "onnx")]
pub fn detect_content_type_onnx(text: &str) -> ContentType {
    if text.is_empty() {
        return ContentType::PlainText;
    }

    let session = MAGIKA_MODEL.get_or_init(|| {
        magika::Session::new()
            .map(std::sync::Mutex::new)
            .map_err(|e| e.to_string())
    });
    let session = match session {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "magika init failed, falling back to heuristic");
            return detect_content_type(text);
        }
    };
    let mut guard = match session.lock() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "magika lock failed, falling back to heuristic");
            return detect_content_type(text);
        }
    };

    match guard.identify_content_sync(text.as_bytes()) {
        Ok(file_type) => {
            let info = file_type.info();
            let label = info.label;
            let group = info.group;
            if label.contains("json") {
                ContentType::JsonArray
            } else if label.contains("diff") || label.contains("patch") {
                ContentType::Diff
            } else if group.contains("source")
                || group.contains("code")
                || group.contains("script")
            {
                ContentType::Code
            } else if label.contains("log") || label.contains("text") {
                if text.lines().filter(|l| ERROR_LINE.is_match(l) || WARNING_LINE.is_match(l)).count() > 0 {
                    ContentType::Log
                } else {
                    ContentType::PlainText
                }
            } else {
                detect_content_type(text)
            }
        }
        Err(_) => detect_content_type(text),
    }
}

/// Compress content based on type and config. Returns compressed text.
pub fn compress(text: &str, content_type: ContentType, config: &CompressionConfig) -> String {
    if !config.enabled || text.is_empty() {
        return text.to_owned();
    }

    match content_type {
        ContentType::Code => code::compress_code(text, config.code_compression_rate),
        ContentType::Log => log::compress_log(text, config.max_log_lines),
        ContentType::SearchResult => search::compress_search(
            text,
            config.max_search_files,
            config.max_matches_per_file,
        ),
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

#[cfg(feature = "onnx")]
pub fn compress_with_onnx(text: &str, config: &CompressionConfig) -> String {
    let content_type = detect_content_type_onnx(text);
    compress(text, content_type, config)
}