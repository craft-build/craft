use std::collections::HashSet;

use regex::Regex;

use super::super::normalize::NormalizedBlock;
use super::super::util::{clip, clip_sentence, first_line, non_empty_lines};

use std::sync::LazyLock;

static BLOCKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(fail(ed|s|ure|ing)?|broken|cannot|can't|won't work|does not work|doesn't work|still (broken|failing|wrong)|blocked|blocker|not (fixed|resolved|working)|crash(es|ed|ing)?)\b").unwrap()
});
static TSC_ERROR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"error TS\d+:.+").unwrap());
static TEST_FAIL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:FAIL|✗|✘|×)\s|(\d+)\s+(?:failed|failure|failing)").unwrap()
});
static EMPTY_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:No matches? found\.?|No files? matched\.?|0 results?|No results?\.?)$")
        .unwrap()
});
static TSC_FILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[tsc\]\s+(\S+)\(\d+,\d+\)").unwrap());

const BASH_OUTPUT_SCAN_LIMIT: usize = 8_000;
const OUTSTANDING_CAP: usize = 8;
const SEARCH_TOOLS: &[&str] = &["grep", "Grep", "Glob", "glob"];
const FILE_EDIT_TOOLS: &[&str] = &["Edit", "Write", "edit", "write", "MultiEdit"];
const PRIORITY_ERROR: &str = "[ERROR]";
const PRIORITY_WARN: &str = "[WARN]";
const PRIORITY_INFO: &str = "[INFO]";

fn priority_tag(item: &str) -> String {
    if item.starts_with("[tsc]") {
        return format!("{PRIORITY_ERROR} {item}");
    }
    if let Some(rest) = item.strip_prefix("[bash:exit ")
        && let Some(n_end) = rest.find(']')
        && rest[..n_end].parse::<u32>().is_ok_and(|n| n >= 1)
    {
        return format!("{PRIORITY_ERROR} {item}");
    }
    if item.starts_with("[tests]") {
        return format!("{PRIORITY_WARN} {item}");
    }
    if item.starts_with("[no matches]") {
        return format!("{PRIORITY_INFO} {item}");
    }
    if item.starts_with("[user]") {
        return format!("{PRIORITY_WARN} {item}");
    }
    if item.starts_with('[') && item.find(']').is_some() {
        return format!("{PRIORITY_ERROR} {item}");
    }
    format!("{PRIORITY_WARN} {item}")
}

fn extract_tsc_file(item: &str) -> Option<String> {
    TSC_FILE_RE.captures(item).map(|c| c[1].to_string())
}

/// Extract outstanding context: bash exit codes, tsc/test failures, empty
/// search results, tool errors, and blocker text from user/assistant messages.
pub(crate) fn extract_outstanding_context(blocks: &[NormalizedBlock]) -> Vec<String> {
    let tail_start = blocks.len().saturating_sub(25);
    let tail = &blocks[tail_start..];
    let mut items: Vec<String> = Vec::new();
    let mut item_tail_indices: Vec<isize> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut push =
        |items: &mut Vec<String>, idxs: &mut Vec<isize>, item: String, tail_idx: Option<usize>| {
            if seen.insert(item.clone()) {
                items.push(item);
                idxs.push(tail_idx.map(|t| t as isize).unwrap_or(-1));
            }
        };

    for (bi, b) in tail.iter().enumerate() {
        match b {
            NormalizedBlock::Bash {
                command,
                output,
                exit_code,
                ..
            } => {
                if let Some(code) = exit_code
                    && *code != 0
                {
                    let cmd = command
                        .lines()
                        .map(|l| l.trim())
                        .find(|l| !l.is_empty())
                        .unwrap_or(command);
                    let cmd_display = if cmd.len() > 80 {
                        let cut = cmd.floor_char_boundary(77);
                        format!("{}...", &cmd[..cut])
                    } else {
                        cmd.to_string()
                    };
                    let out_line = first_line(output, 120);
                    let tag = format!("exit {code}");
                    let item = if !out_line.is_empty() && out_line != cmd_display {
                        format!("[bash:{tag}] {cmd_display} \u{2192} {out_line}")
                    } else {
                        format!("[bash:{tag}] {cmd_display}")
                    };
                    push(&mut items, &mut item_tail_indices, item, None);
                    continue;
                }
                if !output.is_empty() {
                    let head = &output[..output.floor_char_boundary(BASH_OUTPUT_SCAN_LIMIT)];
                    if TSC_ERROR_RE.is_match(head) {
                        for line in head
                            .lines()
                            .filter(|l| TSC_ERROR_RE.is_match(l.trim()))
                            .take(3)
                        {
                            push(
                                &mut items,
                                &mut item_tail_indices,
                                format!("[tsc] {}", clip(line.trim(), 150)),
                                Some(bi),
                            );
                        }
                        continue;
                    }
                    if TEST_FAIL_RE.is_match(head) {
                        push(
                            &mut items,
                            &mut item_tail_indices,
                            format!("[tests] {}", first_line(output, 150)),
                            None,
                        );
                        continue;
                    }
                }
            }
            NormalizedBlock::ToolResult {
                name,
                text,
                is_error,
                ..
            } => {
                if SEARCH_TOOLS.contains(&name.as_str()) {
                    let trimmed = text.trim();
                    if EMPTY_RESULT_RE.is_match(trimmed) || trimmed.is_empty() {
                        push(
                            &mut items,
                            &mut item_tail_indices,
                            format!("[no matches] {name}"),
                            None,
                        );
                        continue;
                    }
                }
                if *is_error {
                    if TSC_ERROR_RE.is_match(text) {
                        for line in text
                            .lines()
                            .filter(|l| TSC_ERROR_RE.is_match(l.trim()))
                            .take(3)
                        {
                            push(
                                &mut items,
                                &mut item_tail_indices,
                                format!("[tsc] {}", clip(line.trim(), 150)),
                                Some(bi),
                            );
                        }
                        continue;
                    }
                    if TEST_FAIL_RE.is_match(text) {
                        push(
                            &mut items,
                            &mut item_tail_indices,
                            format!("[tests] {}", first_line(text, 150)),
                            None,
                        );
                        continue;
                    }
                    push(
                        &mut items,
                        &mut item_tail_indices,
                        format!("[{name}] {}", first_line(text, 150)),
                        None,
                    );
                    continue;
                }
            }
            NormalizedBlock::Assistant { text, .. } | NormalizedBlock::User { text, .. } => {
                let is_user = matches!(b, NormalizedBlock::User { .. });
                for line in non_empty_lines(text) {
                    if !BLOCKER_RE.is_match(&line) || line.len() < 15 {
                        continue;
                    }
                    if line.trim_start().starts_with(['-', '*', '+', '>', '(']) {
                        continue;
                    }
                    let first = line.chars().next();
                    let ok = matches!(first, Some(c) if c.is_uppercase() || c == '"' || c == '\'' || c == '`' || c == '*' || c == '_');
                    if !ok {
                        continue;
                    }
                    let clipped = if is_user {
                        format!("[user] {}", clip_sentence(&line, 150))
                    } else {
                        clip_sentence(&line, 150)
                    };
                    push(&mut items, &mut item_tail_indices, clipped, None);
                    break;
                }
            }
            _ => {}
        }
    }

    let mut edit_positions: Vec<(usize, HashSet<String>)> = Vec::new();
    for (i, b) in tail.iter().enumerate() {
        if let NormalizedBlock::ToolCall { name, args, .. } = b
            && FILE_EDIT_TOOLS.contains(&name.as_str())
            && let Some(path) = super::super::util::extract_path(args)
        {
            if let Some(pos) = edit_positions.iter_mut().find(|(p, _)| *p == i) {
                pos.1.insert(path);
            } else {
                let mut set = HashSet::new();
                set.insert(path);
                edit_positions.push((i, set));
            }
        }
    }

    items
        .into_iter()
        .zip(item_tail_indices)
        .take(OUTSTANDING_CAP)
        .map(|(item, tail_idx)| {
            let tagged = priority_tag(&item);
            if tail_idx >= 0
                && let Some(file) = extract_tsc_file(&item)
            {
                let resolved = edit_positions
                    .iter()
                    .any(|(pos, files)| *pos > tail_idx as usize && files.contains(&file));
                if resolved {
                    return tagged.replacen("[ERROR]", "[RESOLVED]", 1).replacen(
                        "[WARN]",
                        "[RESOLVED]",
                        1,
                    );
                }
            }
            tagged
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_bash_nonzero_exit() {
        let blocks = vec![NormalizedBlock::Bash {
            command: "cargo build".into(),
            output: "error: could not compile".into(),
            exit_code: Some(1),
            source_index: 0,
        }];
        let out = extract_outstanding_context(&blocks);
        assert!(
            out.iter()
                .any(|s| s.contains("[bash:exit 1]") && s.contains("[ERROR]"))
        );
    }

    #[test]
    fn captures_tool_error() {
        let blocks = vec![NormalizedBlock::ToolResult {
            name: "edit".into(),
            text: "old_string not found".into(),
            is_error: true,
            source_index: 0,
        }];
        let out = extract_outstanding_context(&blocks);
        assert!(out.iter().any(|s| s.contains("[edit]")));
    }
}
