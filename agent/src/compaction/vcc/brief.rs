use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use super::extract::causal::{CausalChain, extract_causal_chain};
use super::normalize::NormalizedBlock;
use super::util::{
    clip, collapse_skill_text, compress_bash, extract_path, first_line, truncate_tokens,
};

const TRUNCATE_USER: usize = 256;
const TRUNCATE_ASSISTANT: usize = 200;
const TOOL_CALLS_PER_TURN: usize = 8;

static SELF_TALK_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:hmm|wait|actually|oh|okay|ok|well|so)[,.!\s-]+").unwrap()
});
static REF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\(#(\d+)\)$").unwrap());

static WRITE_TOOLS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    ["Edit", "Write", "edit", "write", "MultiEdit"]
        .into_iter()
        .collect()
});

#[derive(Debug, Clone)]
pub(crate) struct BriefLine {
    pub header: String,
    pub lines: Vec<String>,
}

fn tool_one_liner(name: &str, args: &serde_json::Value) -> String {
    let field = match name {
        "Read" | "read" | "Edit" | "edit" | "Write" | "write" | "Glob" | "glob" | "Grep"
        | "grep" => args
            .get("file_path")
            .or_else(|| args.get("pattern"))
            .and_then(|v| v.as_str()),
        _ => None,
    };
    if let Some(f) = field {
        return format!("* {name} \"{f}\"");
    }
    if let Some(p) = extract_path(args) {
        return format!("* {name} \"{p}\"");
    }
    if name.eq_ignore_ascii_case("bash") {
        return format!("* {name} \"???\"");
    }
    if let Some(q) = args.get("query").and_then(|v| v.as_str()) {
        return format!("* {name} \"{}\"", clip(q, 60));
    }
    format!("* {name}")
}

fn is_noise_user(text: &str) -> bool {
    text.trim().is_empty()
}

fn ref_suffix(source_index: usize) -> String {
    format!(" (#{source_index})")
}

pub(crate) fn build_brief_sections(blocks: &[NormalizedBlock]) -> Vec<BriefLine> {
    let mut sections: Vec<BriefLine> = Vec::new();
    let mut last_header = String::new();

    let push = |header: &str, line: String, sections: &mut Vec<BriefLine>, last: &mut String| {
        if header == last.as_str() && !sections.is_empty() {
            sections.last_mut().unwrap().lines.push(line);
        } else {
            sections.push(BriefLine {
                header: header.to_string(),
                lines: vec![line],
            });
            *last = header.to_string();
        }
    };

    for b in blocks {
        match b {
            NormalizedBlock::User { text, source_index } => {
                if is_noise_user(text) {
                    continue;
                }
                let text_t = truncate_tokens(&collapse_skill_text(text), TRUNCATE_USER);
                if !text_t.is_empty() {
                    push(
                        "[user]",
                        format!("{text_t}{}", ref_suffix(*source_index)),
                        &mut sections,
                        &mut last_header,
                    );
                }
                last_header = "[user]".to_string();
            }
            NormalizedBlock::Bash {
                command,
                source_index,
                ..
            } => {
                let cmd = compress_bash(command);
                if !cmd.is_empty() {
                    push(
                        "[user]",
                        format!("$ {cmd}{}", ref_suffix(*source_index)),
                        &mut sections,
                        &mut last_header,
                    );
                }
                last_header = "[user]".to_string();
            }
            NormalizedBlock::Assistant { text, source_index } => {
                let mut raw = text.clone();
                for _ in 0..2 {
                    let stripped = SELF_TALK_PREFIX_RE.replace(&raw, "");
                    if stripped == raw.as_str() {
                        break;
                    }
                    raw = stripped.into_owned();
                }
                let text_t = truncate_tokens(&raw, TRUNCATE_ASSISTANT);
                if !text_t.is_empty() {
                    push(
                        "[assistant]",
                        format!("{text_t}{}", ref_suffix(*source_index)),
                        &mut sections,
                        &mut last_header,
                    );
                }
            }
            NormalizedBlock::ToolCall {
                name,
                args,
                source_index,
            } => {
                if name.trim().is_empty() {
                    continue;
                }
                let summary = format!(
                    "{}{}",
                    tool_one_liner(name, args),
                    ref_suffix(*source_index)
                );
                push("[assistant]", summary, &mut sections, &mut last_header);
            }
            NormalizedBlock::ToolResult {
                name,
                text,
                is_error,
                source_index,
            } => {
                if *is_error {
                    let body = first_line(text, 150);
                    if !body.is_empty() && body != "(no output)" {
                        let header = format!("[tool_error] {name}{}", ref_suffix(*source_index));
                        push(&header, body, &mut sections, &mut last_header);
                    }
                }
            }
            NormalizedBlock::Thinking { .. } => {}
        }
    }

    collapse_identical_tool_lines(&mut sections);
    cap_tool_calls_per_turn(&mut sections);
    collapse_tool_error_sections(&mut sections);
    sections
}

fn collapse_identical_tool_lines(sections: &mut [BriefLine]) {
    for sec in sections.iter_mut() {
        if sec.header != "[assistant]" {
            continue;
        }
        let mut out: Vec<String> = Vec::new();
        for line in &sec.lines {
            if !line.starts_with("* ") {
                out.push(line.clone());
                continue;
            }
            let (base, refn) = split_ref(line);
            let last = out.last().cloned();
            if let Some(last) = last {
                if let Some(caps) = parse_collapsed(&last)
                    && caps.0 == base
                {
                    let refs = format!("{}, #{}", caps.1, refn);
                    let new_last = format!("{base} ({refs}) x{}", caps.2 + 1);
                    *out.last_mut().unwrap() = new_last;
                    continue;
                }
                if REF_RE.is_match(&last) {
                    let last_base = REF_RE.replace_all(&last, "").to_string();
                    if last_base.trim_end() == base {
                        let prev_ref = REF_RE
                            .captures(&last)
                            .map(|c| c[1].to_string())
                            .unwrap_or_default();
                        *out.last_mut().unwrap() = format!("{base} (#{prev_ref}, #{refn}) x2");
                        continue;
                    }
                }
            }
            out.push(line.clone());
        }
        sec.lines = out;
    }
}

fn split_ref(line: &str) -> (String, String) {
    if let Some(c) = REF_RE.captures(line) {
        let idx = c.get(0).unwrap().start();
        (line[..idx].trim_end().to_string(), c[1].to_string())
    } else {
        (line.to_string(), String::new())
    }
}

fn parse_collapsed(line: &str) -> Option<(String, String, u32)> {
    let re = Regex::new(r"^(.*) \((#[\d, #]+)\) x(\d+)$").unwrap();
    let caps = re.captures(line)?;
    Some((
        caps[1].to_string(),
        caps[2].to_string(),
        caps[3].parse().unwrap_or(1),
    ))
}

fn cap_tool_calls_per_turn(sections: &mut [BriefLine]) {
    for sec in sections.iter_mut() {
        if sec.header != "[assistant]" {
            continue;
        }
        let tool_idxs: Vec<usize> = sec
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("* "))
            .map(|(i, _)| i)
            .collect();
        if tool_idxs.len() <= TOOL_CALLS_PER_TURN {
            continue;
        }
        let drop_count = tool_idxs.len() - TOOL_CALLS_PER_TURN;
        let drop_set: HashSet<usize> = tool_idxs[..drop_count].iter().copied().collect();
        let first_kept = tool_idxs[drop_count];
        let mut next: Vec<String> = Vec::new();
        let mut inserted = false;
        for (i, line) in sec.lines.iter().enumerate() {
            if drop_set.contains(&i) {
                continue;
            }
            if !inserted && i == first_kept {
                next.push(format!(
                    "* ({drop_count} earlier tool-call entries omitted)"
                ));
                inserted = true;
            }
            next.push(line.clone());
        }
        sec.lines = next;
    }
}

fn collapse_tool_error_sections(sections: &mut Vec<BriefLine>) {
    let re = Regex::new(r"^\[tool_error\]\s+(\S+?)(?:\s*\(#(\d+)\))?$").unwrap();
    let collapsed_re =
        Regex::new(r"^\[tool_error\]\s+(\S+?)\s*\(((?:#\d+(?:,\s*)?)+)\)(?:\s*x(\d+))?$").unwrap();
    let mut out: Vec<BriefLine> = Vec::new();
    for sec in sections.iter() {
        if sec.lines.len() != 1 {
            out.push(sec.clone());
            continue;
        }
        let body = &sec.lines[0];
        let tool = re.captures(&sec.header).map(|c| c[1].to_string());
        let refn = re.captures(&sec.header).map(|c| c[2].to_string());
        if let Some(tool) = &tool
            && let Some(prev) = out.last_mut()
            && let Some(caps) = collapsed_re.captures(&prev.header)
            && &caps[1] == tool
            && prev.lines.len() == 1
            && prev.lines[0] == *body
        {
            let refs = format!("{}, #{}", &caps[2], refn.as_deref().unwrap_or(""));
            let count: u32 = caps[3].parse::<u32>().unwrap_or(1) + 1;
            prev.header = format!("[tool_error] {tool} ({refs}) x{count}");
            continue;
        }
        out.push(sec.clone());
    }
    *sections = out;
}

pub(crate) fn stringify_brief(sections: &[BriefLine]) -> String {
    let mut out: Vec<String> = Vec::new();
    for (i, sec) in sections.iter().enumerate() {
        if i > 0 {
            let prev = &sections[i - 1];
            let prev_is_tools =
                prev.header == "[assistant]" && prev.lines.iter().all(|l| l.starts_with("* "));
            let cur_is_tools =
                sec.header == "[assistant]" && sec.lines.iter().all(|l| l.starts_with("* "));
            if !(prev_is_tools && cur_is_tools) {
                out.push(String::new());
            }
        }
        out.push(sec.header.clone());
        for line in &sec.lines {
            out.push(line.clone());
        }
    }
    out.join("\n")
}

fn shorten_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() > 2 {
        parts[parts.len() - 2..].join("/")
    } else {
        p.to_string()
    }
}

pub(crate) struct TurnInfo {
    pub summary: String,
}

fn synthesize_turn_summary(
    user_text: Option<&str>,
    tool_actions: &[String],
    chain: &CausalChain,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(u) = user_text
        && u.len() > 3
    {
        parts.push(clip(u, 50));
    }
    let has_causal = chain.cause.is_some() || chain.resolution.is_some();
    if has_causal {
        if let Some(c) = &chain.cause {
            parts.push(c.clone());
        }
        if let Some(r) = &chain.resolution {
            parts.push(r.clone());
        }
    }
    let mut unique: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for a in tool_actions {
        if seen.insert(a.clone()) {
            unique.push(a.clone());
        }
    }
    unique.truncate(5);
    if !unique.is_empty() {
        let edits: Vec<&String> = unique.iter().filter(|a| a.starts_with("edited")).collect();
        let others: Vec<&String> = unique.iter().filter(|a| !a.starts_with("edited")).collect();
        if !edits.is_empty() && others.len() <= 2 {
            parts.push(unique.join(", "));
        } else if !edits.is_empty() {
            parts.push(
                edits
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            if !others.is_empty() {
                parts.push(format!("+{} more", others.len()));
            }
        } else {
            parts.push(unique.join(", "));
        }
    }
    if parts.is_empty() {
        return "(no actions)".to_string();
    }
    parts.join(" \u{2192} ")
}

pub(crate) fn identify_turns(blocks: &[NormalizedBlock]) -> Vec<TurnInfo> {
    let mut turns: Vec<TurnInfo> = Vec::new();
    let mut current_user_text: Option<String> = None;
    let mut tool_actions: Vec<String> = Vec::new();
    let mut assistant_texts: Vec<String> = Vec::new();

    let flush = |turns: &mut Vec<TurnInfo>,
                 user_text: &mut Option<String>,
                 actions: &mut Vec<String>,
                 texts: &mut Vec<String>| {
        if user_text.is_none() && actions.is_empty() {
            return;
        }
        let combined = texts.join(" ");
        let chain = extract_causal_chain(&combined);
        turns.push(TurnInfo {
            summary: synthesize_turn_summary(user_text.as_deref(), actions, &chain),
        });
        *user_text = None;
        actions.clear();
        texts.clear();
    };

    for b in blocks {
        match b {
            NormalizedBlock::User { text, .. } => {
                flush(
                    &mut turns,
                    &mut current_user_text,
                    &mut tool_actions,
                    &mut assistant_texts,
                );
                current_user_text = Some(truncate_tokens(&collapse_skill_text(text), 12));
            }
            NormalizedBlock::Bash { command, .. } => {
                flush(
                    &mut turns,
                    &mut current_user_text,
                    &mut tool_actions,
                    &mut assistant_texts,
                );
                current_user_text = Some(format!("$ {}", compress_bash(command)));
            }
            NormalizedBlock::Assistant { text, .. } if !text.trim().is_empty() => {
                assistant_texts.push(text.trim().to_string());
            }
            NormalizedBlock::ToolCall { name, args, .. } => {
                if name.trim().is_empty() {
                    continue;
                }
                let path = extract_path(args);
                let is_write = WRITE_TOOLS.contains(name.as_str());
                if is_write && let Some(p) = path {
                    tool_actions.push(format!("edited {}", shorten_path(&p)));
                } else if let Some(p) = path {
                    tool_actions.push(format!("{} {}", name.to_lowercase(), shorten_path(&p)));
                } else if name.eq_ignore_ascii_case("bash") {
                    let raw = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    let cmd = compress_bash(raw);
                    if !cmd.is_empty() {
                        tool_actions.push(format!("ran {cmd}"));
                    }
                } else {
                    tool_actions.push(name.to_lowercase());
                }
            }
            _ => {}
        }
    }
    flush(
        &mut turns,
        &mut current_user_text,
        &mut tool_actions,
        &mut assistant_texts,
    );
    turns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stringify_brief_emits_sections() {
        let sections = vec![
            BriefLine {
                header: "[user]".into(),
                lines: vec!["do it (#0)".into()],
            },
            BriefLine {
                header: "[assistant]".into(),
                lines: vec!["* bash \"ls\" (#1)".into()],
            },
        ];
        let s = stringify_brief(&sections);
        assert!(s.contains("[user]\ndo it (#0)"));
        assert!(s.contains("[assistant]\n* bash \"ls\" (#1)"));
    }

    #[test]
    fn identify_turns_produces_summary() {
        let blocks = vec![
            NormalizedBlock::User {
                text: "fix the bug".into(),
                source_index: 0,
            },
            NormalizedBlock::ToolCall {
                name: "Edit".into(),
                args: serde_json::json!({"file_path": "src/a.rs"}),
                source_index: 1,
            },
        ];
        let turns = identify_turns(&blocks);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].summary.contains("fix the bug"));
        assert!(turns[0].summary.contains("edited src/a.rs"));
    }
}
