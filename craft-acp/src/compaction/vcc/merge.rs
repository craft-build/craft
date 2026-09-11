use super::format::cap_brief;
use super::util::refine_breadcrumb_key;

const SEPARATOR: &str = "\n\n---\n\n";

const HEADER_NAMES: &[&str] = &[
    "Session Goal",
    "User Preferences",
    "Files And Changes",
    "Commits",
    "Type Catalog",
    "Outstanding Context",
    "Earlier Turns",
];

pub(crate) const HANDOFF_PREAMBLE: &str = "This summary captures work done before the most recent messages in this session. \
Read it to pick up context — this is work already in progress. \
Do not recap what was done, do not ask what to do next. \
Continue directly where you left off. \
Use `vcc_recall` to search for prior work, decisions, and context from before this summary.";

fn section_of(text: &str, header: &str) -> String {
    let tag = format!("[{header}]");
    let start = match text.find(&tag) {
        Some(s) => s,
        None => return String::new(),
    };
    let after = &text[start..];
    let mut candidates: Vec<usize> = Vec::new();
    for h in HEADER_NAMES {
        if *h == header {
            continue;
        }
        let needle = format!("[{h}]");
        if let Some(idx) = after.find(&needle)
            && idx > 0
        {
            candidates.push(idx);
        }
    }
    if let Some(idx) = after.find(SEPARATOR)
        && idx > 0
    {
        candidates.push(idx);
    }
    let end = candidates.into_iter().min();
    match end {
        Some(e) => after[..e].trim().to_string(),
        None => after.trim().to_string(),
    }
}

fn brief_of(text: &str) -> String {
    match text.find(SEPARATOR) {
        Some(idx) => text[idx + SEPARATOR.len()..].trim().to_string(),
        None => String::new(),
    }
}

fn extract_breadcrumb(line: &str) -> String {
    let text = line
        .trim_start_matches(|c: char| c.is_whitespace() || c == '-')
        .trim();
    if text.is_empty() {
        return String::new();
    }
    if let Some(rest) = text.strip_prefix("...recall:") {
        return rest.trim().to_string();
    }
    if text.contains('\u{2192}') {
        let parts: Vec<&str> = text.split('\u{2192}').map(|p| p.trim()).collect();
        let file_re =
            regex::Regex::new(r"(?:edited |read |wrote |created |deleted )?([^\s.]+\.\w{1,12})")
                .unwrap();
        let file = file_re.captures(text).map(|c| c[1].to_string());
        let tool_action_re =
            regex::Regex::new(r"(?i)^(?:read|edited|wrote|created|deleted|ran)\s?").unwrap();
        let more_re = regex::Regex::new(r"\+\d+ more").unwrap();
        let tool_action_idx = parts
            .iter()
            .position(|p| tool_action_re.is_match(p) || more_re.is_match(p));
        let causal_end = tool_action_idx.unwrap_or(parts.len());
        let causal_parts = &parts[1..causal_end];
        let cause_part = causal_parts.first();
        let resolution_part = causal_parts.last();
        if let Some(rp) = resolution_part {
            let key = refine_breadcrumb_key(rp);
            if !key.is_empty() {
                if let Some(f) = &file {
                    return format!("{f}|{key}");
                }
                return key;
            }
        }
        if let Some(cp) = cause_part {
            let key = refine_breadcrumb_key(cp);
            if !key.is_empty() {
                if let Some(f) = &file {
                    return format!("{f}|{key}");
                }
                return key;
            }
        }
        if let Some(f) = file {
            return f;
        }
    }
    let file_re =
        regex::Regex::new(r"(?:edited |read |wrote |created |deleted )?(\S+\.\w{1,12})").unwrap();
    if let Some(c) = file_re.captures(text) {
        return c[1].to_string();
    }
    let before_arrow = text.split('\u{2192}').next().unwrap_or("").trim();
    let words: Vec<&str> = before_arrow
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .take(3)
        .collect();
    if !words.is_empty() {
        return words.join(" ");
    }
    text.split_whitespace()
        .find(|w| w.len() > 2)
        .unwrap_or("")
        .to_string()
}

fn is_clean(l: &str) -> bool {
    l.starts_with("- ") && !l.contains("<skill") && !l.contains("</skill")
}

fn is_recall_breadcrumb(l: &str) -> bool {
    l.starts_with("- ...recall:")
}

fn merge_header_section(header: &str, prev: &str, fresh: &str) -> String {
    if header == "Outstanding Context" || header == "Type Catalog" {
        return fresh.to_string();
    }
    if prev.is_empty() && fresh.is_empty() {
        return String::new();
    }
    if header == "Files And Changes" {
        return merge_file_lines(prev, fresh);
    }

    let prev_lines: Vec<&str> = prev.lines().filter(|l| is_clean(l)).collect();
    let fresh_lines: Vec<&str> = fresh.lines().filter(|l| is_clean(l)).collect();
    let prev_crumbs: Vec<&str> = prev.lines().filter(|l| is_recall_breadcrumb(l)).collect();
    let fresh_crumbs: Vec<&str> = fresh.lines().filter(|l| is_recall_breadcrumb(l)).collect();
    let mut all_crumbs: Vec<&str> = prev_crumbs;
    all_crumbs.extend(fresh_crumbs);
    all_crumbs.sort();
    all_crumbs.dedup();

    let mut content_lines: Vec<&str> = prev_lines
        .iter()
        .filter(|l| !is_recall_breadcrumb(l))
        .copied()
        .collect();
    content_lines.extend(fresh_lines.iter().filter(|l| !is_recall_breadcrumb(l)));
    content_lines.sort();
    content_lines.dedup();

    let cap = match header {
        "Session Goal" | "Commits" => 8,
        _ => 15,
    };
    if content_lines.len() > cap {
        let kept: Vec<&str> = content_lines[content_lines.len() - cap..].to_vec();
        let dropped: Vec<&str> = content_lines[..content_lines.len() - cap].to_vec();
        let crumbs: Vec<String> = dropped
            .iter()
            .map(|l| extract_breadcrumb(l))
            .filter(|s| !s.is_empty())
            .collect();
        let header_line = format!("[{header}]");
        if !crumbs.is_empty() {
            let mut all = all_crumbs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            all.push(format!("- ...recall: {}", crumbs.join(", ")));
            return format!("{header_line}\n{}\n{}", all.join("\n"), kept.join("\n"));
        }
        if !all_crumbs.is_empty() {
            let all: Vec<String> = all_crumbs.iter().map(|s| s.to_string()).collect();
            return format!("{header_line}\n{}\n{}", all.join("\n"), kept.join("\n"));
        }
        return format!("{header_line}\n{}", kept.join("\n"));
    }
    if content_lines.is_empty() && all_crumbs.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for c in &all_crumbs {
        parts.push(c.to_string());
    }
    for l in &content_lines {
        parts.push(l.to_string());
    }
    format!("[{header}]\n{}", parts.join("\n"))
}

fn merge_file_lines(prev: &str, fresh: &str) -> String {
    use std::collections::HashSet;
    let categories = ["Modified", "Created", "Read"];
    let mut merged: [HashSet<String>; 3] = [HashSet::new(), HashSet::new(), HashSet::new()];

    let strip_syms = regex::Regex::new(r"\s*\([^)]*\)").unwrap();
    let strip_more = regex::Regex::new(r"\s*\(\+\d+ more\)\s*$").unwrap();
    let strip_recall = regex::Regex::new(r",\s*\+recall:\s*").unwrap();

    for text in [prev, fresh] {
        if text.is_empty() {
            continue;
        }
        for line in text.lines() {
            for (ci, cat) in categories.iter().enumerate() {
                let prefix = format!("- {cat}: ");
                let Some(rest) = line.strip_prefix(&prefix) else {
                    continue;
                };
                let rest = strip_syms.replace_all(rest, "");
                let rest = strip_more.replace_all(&rest, "");
                let rest = strip_recall.replace_all(&rest, ", ");
                for p in rest.split(',') {
                    let trimmed = p.trim();
                    if trimmed.is_empty() || trimmed.starts_with("+recall:") {
                        continue;
                    }
                    merged[ci].insert(trimmed.to_string());
                }
            }
        }
    }

    let modified: Vec<String> = merged[0].iter().cloned().collect();
    for p in &modified {
        merged[1].remove(p);
    }

    let cap = |set: &HashSet<String>, limit: usize| -> String {
        let mut arr: Vec<&String> = set.iter().collect();
        arr.sort();
        if arr.len() <= limit {
            arr.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            let kept: Vec<&str> = arr[..limit].iter().map(|s| s.as_str()).collect();
            let omitted: Vec<&str> = arr[limit..].iter().map(|s| s.as_str()).collect();
            format!("{}, +recall: {}", kept.join(", "), omitted.join(", "))
        }
    };

    let mut lines = Vec::new();
    if !merged[0].is_empty() {
        lines.push(format!("- Modified: {}", cap(&merged[0], 10)));
    }
    if !merged[1].is_empty() {
        lines.push(format!("- Created: {}", cap(&merged[1], 10)));
    }
    if !merged[2].is_empty() {
        lines.push(format!("- Read: {}", cap(&merged[2], 10)));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("[Files And Changes]\n{}", lines.join("\n"))
}

fn merge_brief_transcript(prev: &str, fresh: &str) -> String {
    if prev.is_empty() {
        return fresh.to_string();
    }
    if fresh.is_empty() {
        return prev.to_string();
    }
    format!("{prev}\n\n{fresh}")
}

pub(crate) fn merge_previous(prev: &str, fresh: &str) -> String {
    let headers: Vec<String> = HEADER_NAMES
        .iter()
        .map(|h| merge_header_section(h, &section_of(prev, h), &section_of(fresh, h)))
        .filter(|s| !s.is_empty())
        .collect();

    let merged_brief = merge_brief_transcript(&brief_of(prev), &brief_of(fresh));

    let mut parts: Vec<String> = Vec::new();
    if !headers.is_empty() {
        parts.push(headers.join("\n\n"));
    }
    if !merged_brief.is_empty() {
        parts.push(cap_brief(&merged_brief));
    }
    parts.join(SEPARATOR)
}

/// Strip the leading `HANDOFF_PREAMBLE` and trailing legacy recall note from a
/// previous summary so the merge only sees clean section data.
pub(crate) fn strip_recall_note(text: &str) -> String {
    let mut result = text;
    if result.starts_with("This summary captures work done before") {
        if let Some(header_start) = result.find('[') {
            if header_start > 0 {
                result = result[header_start..].trim();
            }
        } else if let Some(double_nl) = result.find("\n\n")
            && double_nl > 0
        {
            result = result[double_nl + 2..].trim();
        }
    }
    let legacy = "Use `vcc_recall` to search for prior work, decisions, and context from before this summary.";
    if let Some(idx) = result.rfind(legacy)
        && idx > 0
    {
        let mut end = idx;
        if result[..end].ends_with("\n\n---\n\n") {
            end -= "\n\n---\n\n".len();
        }
        result = result[..end].trim_end();
    }
    result.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_of_extracts_named_section() {
        let text = "[Session Goal]\n- fix bug\n\n[Files And Changes]\n- Modified: a.rs";
        assert_eq!(
            section_of(text, "Session Goal"),
            "[Session Goal]\n- fix bug"
        );
    }

    #[test]
    fn merge_previous_replaces_volatile_sections() {
        let prev = "[Outstanding Context]\n- [ERROR] old\n\n---\n\n[assistant]\nold stuff";
        let fresh = "[Outstanding Context]\n- [ERROR] new\n\n---\n\n[assistant]\nnew stuff";
        let merged = merge_previous(prev, fresh);
        assert!(merged.contains("[ERROR] new"));
        assert!(!merged.contains("[ERROR] old"));
    }

    #[test]
    fn strip_recall_note_removes_preamble() {
        let text = format!("{HANDOFF_PREAMBLE}\n\n[Session Goal]\n- fix bug");
        let stripped = strip_recall_note(&text);
        assert!(stripped.starts_with("[Session Goal]"));
    }
}
