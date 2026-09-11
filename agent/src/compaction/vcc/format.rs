const TUI_SAFE_LINE_CHARS: usize = 120;
const BRIEF_MAX_LINES: usize = 120;

pub(crate) struct SectionData {
    pub session_goal: Vec<String>,
    pub user_preferences: Vec<String>,
    pub files_and_changes: Vec<String>,
    pub commits: Vec<String>,
    pub type_catalog: Vec<String>,
    pub outstanding_context: Vec<String>,
    pub turn_summaries: Vec<String>,
    pub brief_transcript: String,
}

fn section(title: &str, items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let body = items
        .iter()
        .map(|i| format!("- {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[{title}]\n{body}")
}

fn wrap_line(line: &str, max_chars: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= max_chars {
        return vec![line.to_string()];
    }
    let indent_len = chars.iter().take_while(|c| c.is_whitespace()).count();
    let continuation_indent: String = " ".repeat(indent_len.min(8));
    let mut wrapped = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        let prefix_len = if first { 0 } else { continuation_indent.len() };
        let avail = max_chars.saturating_sub(prefix_len).max(20);
        if start + avail >= chars.len() {
            let tail: String = chars[start..].iter().collect();
            let pfx = if first {
                ""
            } else {
                continuation_indent.as_str()
            };
            wrapped.push(format!("{pfx}{}", tail.trim_end()));
            break;
        }
        let region_end = (start + avail).min(chars.len());
        let mut split = None;
        for i in (start..region_end).rev() {
            if chars[i] == ' ' {
                split = Some(i);
                break;
            }
        }
        let split = match split {
            Some(s) if s >= start + avail / 2 => s,
            _ => region_end,
        };
        let head: String = chars[start..split].iter().collect();
        let pfx = if first {
            ""
        } else {
            continuation_indent.as_str()
        };
        wrapped.push(format!("{pfx}{}", head.trim_end()));
        start = split;
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
        first = false;
    }
    wrapped
}

pub(crate) fn wrap_long_lines(text: &str) -> String {
    text.lines()
        .flat_map(|line| wrap_line(line, TUI_SAFE_LINE_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn cap_brief(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= BRIEF_MAX_LINES {
        return text.to_string();
    }
    let omitted = lines.len() - BRIEF_MAX_LINES;
    let kept: Vec<&str> = lines
        .iter()
        .rev()
        .take(BRIEF_MAX_LINES)
        .rev()
        .copied()
        .collect();
    let first_header = kept
        .iter()
        .position(|l| l.starts_with('[') && l.ends_with(']'));
    let clean: Vec<&str> = match first_header {
        Some(idx) if idx > 0 => kept[idx..].to_vec(),
        _ => kept,
    };
    format!(
        "...({omitted} earlier lines omitted)\n\n{}",
        clean.join("\n")
    )
}

/// Format the summary with cache-friendly section ordering: stable (merged)
/// sections first, volatile (always-fresh) sections last, then the brief.
pub(crate) fn format_summary(data: &SectionData) -> String {
    let stable: Vec<String> = [
        section("Session Goal", &data.session_goal),
        section("User Preferences", &data.user_preferences),
        section("Files And Changes", &data.files_and_changes),
        section("Commits", &data.commits),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();

    let volatile: Vec<String> = [
        section("Type Catalog", &data.type_catalog),
        section("Outstanding Context", &data.outstanding_context),
        section("Earlier Turns", &data.turn_summaries),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();

    let all_headers: Vec<String> = stable.into_iter().chain(volatile).collect();
    let mut parts: Vec<String> = Vec::new();
    if !all_headers.is_empty() {
        parts.push(all_headers.join("\n\n"));
    }
    if !data.brief_transcript.is_empty() {
        parts.push(cap_brief(&data.brief_transcript));
    }
    if parts.is_empty() {
        return String::new();
    }
    wrap_long_lines(&parts.join("\n\n---\n\n"))
}
