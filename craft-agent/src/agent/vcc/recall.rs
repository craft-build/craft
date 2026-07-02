use std::collections::HashSet;

use craft_providers::{ContentBlock, Message};

const DEFAULT_RECENT: usize = 25;
const PAGE_SIZE: usize = 5;
const MAX_PAGES: usize = 5;
const MAX_SEARCH_RESULTS: usize = 50;

#[derive(Debug, Clone)]
pub(crate) struct RenderedEntry {
    pub index: usize,
    pub role: String,
    pub summary: String,
    pub files: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct SearchHit {
    pub entry: RenderedEntry,
    pub snippet: Option<String>,
    pub score: f64,
}

fn text_of(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_path(args: &serde_json::Value) -> Option<String> {
    for key in ["path", "file_path", "filePath", "file"] {
        if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

use super::util::clip;

pub(crate) fn render_message(msg: &Message, index: usize, full: bool) -> RenderedEntry {
    let role = match msg.role {
        craft_providers::Role::User => "user",
        craft_providers::Role::Assistant => "assistant",
    };
    let mut files = Vec::new();
    let mut tool_summary = String::new();
    for b in &msg.content {
        if let ContentBlock::ToolUse { name, input, .. } = b {
            if let Some(p) = extract_path(input) {
                files.push(p);
            }
            tool_summary.push_str(&format!("{name} "));
        }
    }
    let text = text_of(msg);
    let summary = if full { text.clone() } else { clip(&text, 300) };
    let summary = if tool_summary.is_empty() {
        summary
    } else {
        format!("{tool_summary}\n{summary}")
    };
    RenderedEntry {
        index,
        role: role.to_string(),
        summary,
        files,
    }
}

fn looks_like_regex(query: &str) -> bool {
    query.chars().any(|c| {
        matches!(
            c,
            '|' | '*' | '+' | '?' | '{' | '}' | '(' | ')' | '[' | ']' | '\\' | '^' | '$' | '.'
        )
    })
}

fn safe_regex(pattern: &str) -> regex::Regex {
    if pattern.len() > 256 {
        let escaped = regex::escape(&pattern[..64]);
        return regex::Regex::new(&format!("(?i){escaped}")).unwrap();
    }
    match regex::Regex::new(&format!("(?i){pattern}")) {
        Ok(re) => re,
        Err(_) => {
            let escaped = regex::escape(pattern);
            regex::Regex::new(&format!("(?i){escaped}")).unwrap()
        }
    }
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "of",
    "in", "to", "for", "with", "on", "at", "from", "by", "as", "and", "or", "not", "it", "its",
    "that", "this", "what", "which", "who",
];

fn filter_stopwords(terms: &[String]) -> Vec<String> {
    let meaningful: Vec<String> = terms
        .iter()
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(&t.to_lowercase().as_str()))
        .cloned()
        .collect();
    if meaningful.is_empty() {
        terms.to_vec()
    } else {
        meaningful
    }
}

fn full_text(msg: &Message) -> String {
    let mut text = text_of(msg);
    for b in &msg.content {
        if let ContentBlock::Thinking { thinking, .. } = b {
            text = format!("{thinking}\n{text}");
        }
    }
    text
}

/// Search messages. Regex queries match literally; natural-language queries
/// use OR matching with simple relevance ranking (match count desc).
pub(crate) fn search_entries(
    entries: &[RenderedEntry],
    messages: &[Message],
    query: &str,
) -> Vec<SearchHit> {
    let q = query.trim();
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    if looks_like_regex(q) {
        let re = safe_regex(q);
        for (i, e) in entries.iter().enumerate() {
            let text = messages
                .get(i)
                .map(full_text)
                .unwrap_or_else(|| e.summary.clone());
            let hay = format!("{} {} {}", e.role, text, e.files.join(" "));
            if re.is_match(&hay) {
                let snippet = line_snippet(&text, &re);
                hits.push(SearchHit {
                    entry: e.clone(),
                    snippet,
                    score: 1.0,
                });
                if hits.len() >= MAX_SEARCH_RESULTS {
                    break;
                }
            }
        }
        return hits;
    }

    let raw_terms: Vec<String> = q.split_whitespace().map(String::from).collect();
    let terms = filter_stopwords(&raw_terms);
    let compiled: Vec<regex::Regex> = terms.iter().map(|t| safe_regex(t)).collect();
    let min_match = if terms.len() >= 3 { 2 } else { 1 };

    for (i, e) in entries.iter().enumerate() {
        let text = messages
            .get(i)
            .map(full_text)
            .unwrap_or_else(|| e.summary.clone());
        let hay = format!("{} {} {}", e.role, text, e.files.join(" "));
        let match_count = compiled.iter().filter(|re| re.is_match(&hay)).count();
        if match_count < min_match {
            continue;
        }
        let alt = terms
            .iter()
            .map(|t| regex::escape(t))
            .collect::<Vec<_>>()
            .join("|");
        let snip_re = regex::Regex::new(&format!("(?i)({alt})")).unwrap_or_else(|_| safe_regex(q));
        let snippet = line_snippet(&text, &snip_re);
        hits.push(SearchHit {
            entry: e.clone(),
            snippet,
            score: match_count as f64,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if hits.len() > MAX_SEARCH_RESULTS {
        hits.truncate(MAX_SEARCH_RESULTS);
    }
    hits
}

fn line_snippet(text: &str, re: &regex::Regex) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let match_idx = lines.iter().position(|l| re.is_match(l))?;
    let start = match_idx.saturating_sub(2);
    let end = (match_idx + 3).min(lines.len());
    let mut parts: Vec<String> = Vec::new();
    if start > 0 {
        parts.push(format!("...({start} lines above)"));
    }
    for l in &lines[start..end] {
        parts.push(l.to_string());
    }
    if end < lines.len() {
        parts.push(format!("...({} lines below)", lines.len() - end));
    }
    Some(parts.join("\n"))
}

fn group_segments(hits: &[SearchHit]) -> Vec<Vec<usize>> {
    let mut segments: Vec<Vec<usize>> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        let is_start = h.entry.role == "user";
        if is_start || segments.is_empty() {
            segments.push(vec![i]);
        } else {
            segments.last_mut().unwrap().push(i);
        }
    }
    segments
}

/// Format search/browse output. Browse mode (no query) shows flat recent
/// entries; search mode groups matched hits into segments.
pub(crate) fn format_output(hits: &[SearchHit], query: Option<&str>, page: usize) -> String {
    if hits.is_empty() {
        return match query {
            Some(q) => format!("No matches for \"{q}\" in session history."),
            None => "No entries in session history.".to_string(),
        };
    }
    match query {
        None => {
            let mut lines = vec![format!("Session history ({} entries):", hits.len())];
            for h in hits {
                let file_suffix = if h.entry.files.is_empty() {
                    String::new()
                } else {
                    format!(" files:[{}]", h.entry.files.join(", "))
                };
                lines.push(format!(
                    "#{} [{}]{} {}",
                    h.entry.index, h.entry.role, file_suffix, h.entry.summary
                ));
            }
            lines.join("\n\n")
        }
        Some(q) => {
            let total = hits.len();
            let total_pages = total.div_ceil(PAGE_SIZE).clamp(1, MAX_PAGES);
            let page = page.clamp(1, total_pages);
            let start = (page - 1) * PAGE_SIZE;
            let end = (start + PAGE_SIZE).min(total);
            let page_hits = &hits[start..end];
            let segments = group_segments(page_hits);
            let mut out = vec![format!(
                "Page {page}/{total_pages} ({total} matches for \"{q}\")"
            )];
            for seg in &segments {
                let first = &page_hits[seg[0]];
                let last = &page_hits[seg[seg.len() - 1]];
                let range = if first.entry.index == last.entry.index {
                    format!("#{}", first.entry.index)
                } else {
                    format!("#{}-#{}", first.entry.index, last.entry.index)
                };
                out.push(format!("--- {range} ---"));
                for &i in seg {
                    let h = &page_hits[i];
                    let marker = ">";
                    let body = h.snippet.clone().unwrap_or_else(|| h.entry.summary.clone());
                    out.push(format!(
                        "{marker} #{} [{}] {}",
                        h.entry.index, h.entry.role, body
                    ));
                }
            }
            if page < total_pages {
                out.push(format!("--- Use page:{} for more results ---", page + 1));
            }
            out.join("\n")
        }
    }
}

/// Load all message records from a session JSONL file, returning rendered
/// entries (parallel to raw messages).
pub(crate) fn load_session(
    path: &std::path::Path,
) -> std::io::Result<(Vec<RenderedEntry>, Vec<Message>)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();
    let mut messages = Vec::new();
    let mut index = 0usize;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("t").and_then(|t| t.as_str()) != Some("msg") {
            continue;
        }
        let Some(d) = v.get("d") else { continue };
        let msg: Message = match serde_json::from_value(d.clone()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        entries.push(render_message(&msg, index, false));
        messages.push(msg);
        index += 1;
    }
    Ok((entries, messages))
}

/// Run a recall query against a session file. Returns formatted output.
pub(crate) fn run(
    path: &std::path::Path,
    query: Option<&str>,
    page: usize,
    expand: Option<&[usize]>,
) -> std::io::Result<String> {
    let (entries, messages) = load_session(path)?;
    if let Some(expand_idxs) = expand {
        let available: HashSet<usize> = entries.iter().map(|e| e.index).collect();
        let invalid: Vec<usize> = expand_idxs
            .iter()
            .filter(|i| !available.contains(i))
            .copied()
            .collect();
        if !invalid.is_empty() {
            return Ok(format!(
                "Cannot expand indices outside session history: {}",
                invalid
                    .iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let by_index: std::collections::HashMap<usize, &Message> = messages
            .iter()
            .zip(entries.iter())
            .map(|(m, e)| (e.index, m))
            .collect();
        let mut out = vec!["Expanded entries:".to_string()];
        for &idx in expand_idxs {
            if let Some(m) = by_index.get(&idx) {
                let full = render_message(m, idx, true);
                out.push(format!("#{} [{}] {}", full.index, full.role, full.summary));
            }
        }
        return Ok(out.join("\n\n"));
    }
    match query {
        Some(q) if !q.trim().is_empty() => {
            let hits = search_entries(&entries, &messages, q);
            Ok(format_output(&hits, Some(q), page))
        }
        _ => {
            let start = entries.len().saturating_sub(DEFAULT_RECENT);
            let recent: Vec<SearchHit> = entries[start..]
                .iter()
                .map(|e| SearchHit {
                    entry: e.clone(),
                    snippet: None,
                    score: 0.0,
                })
                .collect();
            Ok(format_output(&recent, None, 1))
        }
    }
}
