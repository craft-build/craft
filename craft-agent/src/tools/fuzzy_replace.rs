use regex::Regex;
use unicode_normalization::UnicodeNormalization;

pub(super) const NO_MATCH: &str = "old_string not found in file";
pub(super) const EMPTY_OLD_STRING: &str = "old_string cannot be empty";
pub(super) const MULTIPLE_MATCHES: &str = "old_string matches multiple locations; add surrounding context to make it unique, or use occurrence param to select which match";
pub(super) const OCCURRENCE_OUT_OF_RANGE: &str = "occurrence number exceeds number of matches";

const SINGLE_CANDIDATE_THRESHOLD: f64 = 0.0;
const MULTI_CANDIDATE_THRESHOLD: f64 = 0.3;
const CONTEXT_AWARE_LINE_MIN: usize = 3;
const CONTEXT_AWARE_MATCH_RATIO: f64 = 0.5;

use std::collections::HashMap;

type Replacer = fn(&str, &str) -> Vec<String>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Pass {
    Exact = 1,
    LineTrimmed = 2,
    BlockAnchor = 3,
    WhitespaceNormalized = 4,
    IndentationFlexible = 5,
    UnicodeNormalized = 6,
    TrimmedBoundary = 7,
    ContextAware = 8,
    EscapeNormalized = 9,
}

impl Pass {
    pub fn number(self) -> usize {
        self as usize
    }
}

#[derive(Debug)]
pub(super) struct ReplaceResult {
    pub content: String,
    pub pass: Pass,
}

pub(super) fn replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    occurrence: Option<usize>,
) -> Result<ReplaceResult, String> {
    let result = replace_inner(content, old_string, new_string, replace_all, occurrence)?;
    Ok(ReplaceResult {
        content: result.content,
        pass: result.pass,
    })
}

fn replace_inner(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    occurrence: Option<usize>,
) -> Result<ReplaceResult, String> {
    if old_string.is_empty() {
        return Err(EMPTY_OLD_STRING.into());
    }
    if old_string.trim().is_empty() {
        return Err(NO_MATCH.into());
    }

    const PASSES: &[(Replacer, Pass)] = &[
        (exact, Pass::Exact),
        (line_trimmed, Pass::LineTrimmed),
        (block_anchor, Pass::BlockAnchor),
        (whitespace_normalized, Pass::WhitespaceNormalized),
        (indentation_flexible, Pass::IndentationFlexible),
        (unicode_normalized, Pass::UnicodeNormalized),
        (trimmed_boundary, Pass::TrimmedBoundary),
        (context_aware, Pass::ContextAware),
    ];

    let mut any_found = false;
    let mut total_match_count: usize = 0;

    let collect_positions = |candidates: &[String]| -> Vec<(usize, usize)> {
        let mut positions = Vec::new();
        for matched in candidates {
            for (start, _) in content.match_indices(matched.as_str()) {
                positions.push((start, start + matched.len()));
            }
        }
        positions.sort();
        positions.dedup();
        positions
    };

    let apply_at = |start: usize, end: usize, replacement: &str| -> String {
        let mut result = String::with_capacity(content.len() + replacement.len());
        result.push_str(&content[..start]);
        result.push_str(replacement);
        result.push_str(&content[end..]);
        result
    };

    let try_pass = |candidates: Vec<String>,
                    find: &str,
                    replacement: &str,
                    pass: Pass,
                    any_found: &mut bool,
                    total_count: &mut usize|
     -> Option<ReplaceResult> {
        if candidates.is_empty() {
            return None;
        }

        if replace_all {
            let matched = &candidates[0];
            let at: Vec<(usize, usize)> = content
                .match_indices(matched.as_str())
                .map(|(s, _)| (s, s + matched.len()))
                .collect();
            if !at.is_empty() {
                let text = replacement_text(content, matched, find, replacement, &at);
                return Some(ReplaceResult {
                    content: content.replace(matched.as_str(), &text),
                    pass,
                });
            }
            return None;
        }

        let positions = collect_positions(&candidates);
        if positions.is_empty() {
            return None;
        }
        *any_found = true;
        *total_count = positions.len();

        if let Some(occ) = occurrence {
            if occ > 0 && occ <= positions.len() {
                let (start, end) = positions[occ - 1];
                let text =
                    replacement_text(content, &content[start..end], find, replacement, &positions);
                return Some(ReplaceResult {
                    content: apply_at(start, end, &text),
                    pass,
                });
            }
            return None;
        }

        if positions.len() == 1 {
            let (start, end) = positions[0];
            let text =
                replacement_text(content, &content[start..end], find, replacement, &positions);
            return Some(ReplaceResult {
                content: apply_at(start, end, &text),
                pass,
            });
        }

        None
    };

    for &(r, pass) in PASSES {
        let candidates = r(content, old_string);
        if let Some(result) = try_pass(
            candidates,
            old_string,
            new_string,
            pass,
            &mut any_found,
            &mut total_match_count,
        ) {
            return Ok(result);
        }
    }

    let unescaped = unescape(old_string);
    if unescaped != old_string {
        let candidates = escape_normalized(content, &unescaped);
        let escaped_new = unescape(new_string);
        if let Some(result) = try_pass(
            candidates,
            &unescaped,
            &escaped_new,
            Pass::EscapeNormalized,
            &mut any_found,
            &mut total_match_count,
        ) {
            return Ok(result);
        }
    }

    if let Some(occ) = occurrence {
        if any_found {
            return Err(format!(
                "{OCCURRENCE_OUT_OF_RANGE} (found {total_match_count} matches, requested occurrence {occ})"
            ));
        }
        return Err(NO_MATCH.into());
    }

    if any_found {
        Err(MULTIPLE_MATCHES.into())
    } else {
        Err(NO_MATCH.into())
    }
}

fn indent_of(line: &str) -> &str {
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    &line[..end]
}

// Only `old_string` was ever checked against the file, so the drift between the
// two is the sole evidence of what the matcher forgave. Positions correspond
// when the line counts agree, which every matcher but `block_anchor` guarantees;
// otherwise only lines whose content agrees are the same line.
fn indent_drift<'m, 'f>(matched: &'m str, find: &'f str) -> Option<HashMap<&'f str, &'m str>> {
    let file_lines: Vec<&str> = matched.split('\n').collect();
    let model_lines: Vec<&str> = find.split('\n').collect();
    let paired_by_position = file_lines.len() == model_lines.len();
    let mut drift = HashMap::new();
    let mut forgave = false;
    for (file_line, model_line) in file_lines.iter().zip(&model_lines) {
        let same_line = paired_by_position || file_line.trim() == model_line.trim();
        if same_line && !file_line.trim().is_empty() && !model_line.trim().is_empty() {
            let (model_indent, file_indent) = (indent_of(model_line), indent_of(file_line));
            if !drift.contains_key(model_indent) {
                forgave |= model_indent != file_indent;
                drift.insert(model_indent, file_indent);
            }
        }
    }
    forgave.then_some(drift)
}

// A column the model never used is extrapolated from the deepest one it did, so
// a level the replacement adds keeps the shape the model gave it. A column
// shallower than any in the block belongs to a line that leaves the block, a
// new top level definition say, so it stays where the model put it.
fn rebase(drift: &HashMap<&str, &str>, indent: &str) -> String {
    if let Some(&exact_column) = drift.get(indent) {
        return exact_column.to_string();
    }
    let deepest = drift
        .keys()
        .filter(|&&m| m.len() < indent.len() && indent.starts_with(m))
        .max_by_key(|&&m| m.len());
    deepest.map_or_else(
        || indent.to_string(),
        |d| format!("{}{}", drift[d], &indent[d.len()..]),
    )
}

// The model can write `old_string` sloppily and `new_string` in the file's own
// frame, and then there is nothing left to correct. Two independent signs of
// that, either one enough: every column the replacement uses is already a file
// column, or the replacement's base column is the file's one rather than the
// model's. The second matters because a replacement that adds a nesting level
// uses a column neither side has seen, which defeats the first on its own and
// would push the whole block a level deeper.
//
// The bases only tell the frames apart when they differ; when they agree the
// drift is a widening of the inner levels, which the column test still catches
// and reindent has to correct otherwise.
fn written_in_file_frame(drift: &HashMap<&str, &str>, replacement: &str) -> bool {
    // `HashMap` iteration order is arbitrary, so two columns of equal width,
    // easy to hit once tabs and spaces mix, used to leave that order deciding
    // how the file gets reindented. Shallowest first, then by content so a tie
    // always falls the same way.
    let (model_base, file_base) = drift
        .iter()
        .min_by(|(a, _), (b, _)| a.len().cmp(&b.len()).then_with(|| a.cmp(b)))
        .map_or(("", ""), |(&m, &f)| (m, f));

    let mut every_column_known = true;
    let mut replacement_base: Option<&str> = None;
    for line in replacement.split('\n') {
        if !line.trim().is_empty() {
            let indent = indent_of(line);
            every_column_known &= drift.values().any(|&f| f == indent);
            if replacement_base.is_none_or(|r| indent.len() < r.len()) {
                replacement_base = Some(indent);
            }
        }
    }

    every_column_known
        || replacement_base.is_some_and(|r| r == file_base && file_base != model_base)
}

// Corrects exactly what the match forgave in `old_string`, and nothing else.
// Indentation only means something for a block that starts its own line; a
// match landing mid line keeps whatever the model wrote.
fn reindent(matched: &str, find: &str, replacement: &str) -> String {
    let Some(drift) = indent_drift(matched, find) else {
        return replacement.to_string();
    };
    if written_in_file_frame(&drift, replacement) {
        return replacement.to_string();
    }

    replacement
        .split('\n')
        .map(|line| {
            if line.trim().is_empty() {
                line.to_string()
            } else {
                let indent = indent_of(line);
                format!("{}{}", rebase(&drift, indent), &line[indent.len()..])
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn all_start_a_line(content: &str, at: &[(usize, usize)]) -> bool {
    at.iter()
        .all(|&(s, _)| s == 0 || content.as_bytes()[s - 1] == b'\n')
}

fn replacement_text(
    content: &str,
    matched: &str,
    find: &str,
    replacement: &str,
    at: &[(usize, usize)],
) -> String {
    if all_start_a_line(content, at) {
        reindent(matched, find, replacement)
    } else {
        replacement.to_string()
    }
}

fn exact(_content: &str, find: &str) -> Vec<String> {
    vec![find.to_string()]
}

fn line_trimmed(content: &str, find: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = find.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    if search_lines.is_empty() || search_lines.len() > content_lines.len() {
        return vec![];
    }

    let mut results = Vec::new();
    for i in 0..=content_lines.len() - search_lines.len() {
        let all_match = search_lines
            .iter()
            .enumerate()
            .all(|(j, sl)| content_lines[i + j].trim() == sl.trim());
        if all_match {
            results.push(content_lines[i..i + search_lines.len()].join("\n"));
        }
    }
    results
}

fn indentation_flexible(content: &str, find: &str) -> Vec<String> {
    let find_lines: Vec<&str> = find.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if find_lines.is_empty() || find_lines.len() > content_lines.len() {
        return vec![];
    }

    let normalized_find = strip_common_indent(&find_lines);
    let mut results = Vec::new();

    for i in 0..=content_lines.len() - find_lines.len() {
        let block = &content_lines[i..i + find_lines.len()];
        if strip_common_indent(block) == normalized_find {
            results.push(block.join("\n"));
        }
    }
    results
}

fn trimmed_boundary(content: &str, find: &str) -> Vec<String> {
    let trimmed = find.trim();
    if trimmed == find {
        return vec![];
    }

    let mut results = Vec::new();
    if content.contains(trimmed) {
        results.push(trimmed.to_string());
    }

    let find_lines: Vec<&str> = find.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    if find_lines.len() > 1 && find_lines.len() <= content_lines.len() {
        for i in 0..=content_lines.len() - find_lines.len() {
            let block = content_lines[i..i + find_lines.len()].join("\n");
            if block.trim() == trimmed {
                results.push(block);
            }
        }
    }
    results
}

fn block_anchor(content: &str, find: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = find.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    if search_lines.len() < CONTEXT_AWARE_LINE_MIN {
        return vec![];
    }

    let first_trimmed = search_lines[0].trim();
    let last_trimmed = search_lines[search_lines.len() - 1].trim();

    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for (i, line) in content_lines.iter().enumerate() {
        if line.trim() != first_trimmed {
            continue;
        }
        let tail_start = i + 2;
        if let Some(j) = content_lines
            .get(tail_start..)
            .and_then(|s| s.iter().position(|l| l.trim() == last_trimmed))
        {
            candidates.push((i, tail_start + j));
        }
    }

    if candidates.is_empty() {
        return vec![];
    }

    let extract = |start: usize, end: usize| -> String { content_lines[start..=end].join("\n") };

    if candidates.len() == 1 {
        let (start, end) = candidates[0];
        let sim = middle_similarity(&content_lines[start..=end], &search_lines);
        return if sim >= SINGLE_CANDIDATE_THRESHOLD {
            vec![extract(start, end)]
        } else {
            vec![]
        };
    }

    let (best_start, best_end, best_sim) =
        candidates
            .iter()
            .fold((0, 0, -1.0_f64), |(bs, be, bsim), &(s, e)| {
                let sim = middle_similarity(&content_lines[s..=e], &search_lines);
                if sim > bsim {
                    (s, e, sim)
                } else {
                    (bs, be, bsim)
                }
            });

    if best_sim >= MULTI_CANDIDATE_THRESHOLD {
        vec![extract(best_start, best_end)]
    } else {
        vec![]
    }
}

fn whitespace_normalized(content: &str, find: &str) -> Vec<String> {
    let normalized_find = normalize_whitespace(find);
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut results = Vec::new();

    for line in &content_lines {
        if normalize_whitespace(line) == normalized_find {
            results.push(line.to_string());
            continue;
        }
        if let Some(matched) = substring_whitespace_match(line, &normalized_find) {
            results.push(matched);
        }
    }

    let find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() > 1 && find_lines.len() <= content_lines.len() {
        for i in 0..=content_lines.len() - find_lines.len() {
            let block = content_lines[i..i + find_lines.len()].join("\n");
            if normalize_whitespace(&block) == normalized_find {
                results.push(block);
            }
        }
    }

    results
}

fn unicode_normalized(content: &str, find: &str) -> Vec<String> {
    let nfkd_content: String = content.nfkd().collect();
    let nfkd_find: String = find.nfkd().collect();
    if nfkd_content == content && nfkd_find == find {
        return vec![];
    }

    let nfkd_find_trimmed = nfkd_find.trim().to_string();
    if nfkd_find_trimmed.is_empty() {
        return vec![];
    }

    let mut results = Vec::new();

    let content_lines: Vec<&str> = content.split('\n').collect();
    let nfkd_content_lines: Vec<&str> = nfkd_content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();

    if find_lines.len() == 1 {
        for (i, nfkd_line) in nfkd_content_lines.iter().enumerate() {
            if nfkd_line.trim() == nfkd_find_trimmed {
                results.push(content_lines[i].to_string());
                continue;
            }
            if nfkd_line.trim().contains(&nfkd_find_trimmed)
                && nfkd_line.trim() != nfkd_find_trimmed
            {
                let words: Vec<&str> = nfkd_find_trimmed.split(' ').collect();
                if !words.is_empty() {
                    let escaped: Vec<String> = words.iter().map(|w| regex::escape(w)).collect();
                    let pattern = escaped.join(r"[\s\u00a0]+");
                    if let Ok(re) = Regex::new(&pattern)
                        && let Some(m) = re.find(nfkd_line.trim())
                    {
                        let byte_start = m.start();
                        let byte_end = m.end();
                        let trimmed = content_lines[i].trim();
                        if byte_end <= trimmed.len() {
                            results.push(trimmed[byte_start..byte_end].to_string());
                        }
                    }
                }
            }
        }
    } else if find_lines.len() > 1 && find_lines.len() <= content_lines.len() {
        for i in 0..=nfkd_content_lines.len() - find_lines.len() {
            let nfkd_block = nfkd_content_lines[i..i + find_lines.len()].join("\n");
            if nfkd_block.trim() == nfkd_find_trimmed {
                results.push(content_lines[i..i + find_lines.len()].join("\n"));
            }
        }
    }

    results
}

fn escape_normalized(content: &str, unescaped_find: &str) -> Vec<String> {
    let mut results = Vec::new();
    if content.contains(unescaped_find) {
        results.push(unescaped_find.to_string());
    }

    let content_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = unescaped_find.split('\n').collect();
    if find_lines.len() > 1 && find_lines.len() <= content_lines.len() {
        for i in 0..=content_lines.len() - find_lines.len() {
            let block = content_lines[i..i + find_lines.len()].join("\n");
            if unescape(&block) == unescaped_find {
                results.push(block);
            }
        }
    }

    results
}

fn context_aware(content: &str, find: &str) -> Vec<String> {
    let content_lines: Vec<&str> = content.split('\n').collect();
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.last() == Some(&"") {
        find_lines.pop();
    }
    if find_lines.len() < CONTEXT_AWARE_LINE_MIN {
        return vec![];
    }

    let first_trimmed = find_lines[0].trim();
    let last_trimmed = find_lines[find_lines.len() - 1].trim();
    let mut results = Vec::new();

    for (i, line) in content_lines.iter().enumerate() {
        if line.trim() != first_trimmed {
            continue;
        }
        let end = i + find_lines.len() - 1;
        if end >= content_lines.len() {
            continue;
        }
        if content_lines[end].trim() != last_trimmed {
            continue;
        }

        let block = &content_lines[i..=end];
        let (mut matching, mut total) = (0, 0);
        for k in 1..block.len() - 1 {
            let bl = block[k].trim();
            let fl = find_lines[k].trim();
            if !bl.is_empty() || !fl.is_empty() {
                total += 1;
                if bl == fl {
                    matching += 1;
                }
            }
        }

        if total == 0 || matching as f64 / total as f64 >= CONTEXT_AWARE_MATCH_RATIO {
            results.push(block.join("\n"));
        }
    }

    results
}

fn middle_similarity(block: &[&str], search: &[&str]) -> f64 {
    let block_mid = block.len().saturating_sub(2);
    let search_mid = search.len().saturating_sub(2);
    let lines_to_check = block_mid.min(search_mid);
    if lines_to_check == 0 {
        return 1.0;
    }

    let total: f64 = (1..=lines_to_check)
        .map(|j| {
            let a = block[j].trim();
            let b = search[j].trim();
            let max_len = a.len().max(b.len());
            if max_len == 0 {
                return 1.0;
            }
            1.0 - levenshtein(a, b) as f64 / max_len as f64
        })
        .sum();
    total / lines_to_check as f64
}

fn levenshtein(a: &str, b: &str) -> usize {
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let b_chars: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b_chars.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

fn strip_common_indent(lines: &[&str]) -> String {
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                *l
            } else {
                &l[min_indent..]
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws && !result.is_empty() {
                result.push(' ');
            }
            prev_ws = true;
        } else {
            prev_ws = false;
            result.push(ch);
        }
    }
    if result.ends_with(' ') {
        result.pop();
    }
    result
}

fn substring_whitespace_match(line: &str, normalized_find: &str) -> Option<String> {
    let normalized_line = normalize_whitespace(line);
    if !normalized_line.contains(normalized_find) || normalized_line == *normalized_find {
        return None;
    }

    let words: Vec<&str> = normalized_find.split(' ').collect();
    if words.is_empty() {
        return None;
    }

    let escaped: Vec<String> = words.iter().map(|w| regex::escape(w)).collect();
    let pattern = escaped.join(r"\s+");
    let re = Regex::new(&pattern).ok()?;
    re.find(line).map(|m| m.as_str().to_string())
}

fn unescape(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('r') => result.push('\r'),
                Some('\'') => result.push('\''),
                Some('"') => result.push('"'),
                Some('`') => result.push('`'),
                Some('\\') => result.push('\\'),
                Some('$') => result.push('$'),
                Some('\n') => result.push('\n'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const R: &str = "REPLACED";

    fn replace_simple(content: &str, search: &str, replacement: &str) -> Result<String, String> {
        replace(content, search, replacement, false, None).map(|r| r.content)
    }

    #[test_case("fn foo() {}\nfn bar() {}", "fn foo() {}", R ; "exact")]
    #[test_case("fn foo() {}", "\nfn foo() {}\n", R ; "trimmed_boundary")]
    #[test_case("    fn f() {\n        bar();\n    }", "fn f() {\n    bar();\n}", R ; "different_indentation")]
    #[test_case("let   x  =   1;", "let x = 1;", R ; "whitespace_collapsed")]
    #[test_case("fn  foo()  {\n    bar();\n}", "fn foo() {\nbar();\n}", R ; "whitespace_multiline")]
    #[test_case("    let   x  =   compute(a,  b);", "compute(a, b)", R ; "whitespace_substring")]
    #[test_case(
        "let s = \"hello\nworld\";",
        "let s = \"hello\\nworld\";",
        R ;
        "escaped_newline"
    )]
    #[test_case("col1\tcol2\tcol3", "col1\\tcol2\\tcol3", R ; "escaped_tab")]
    #[test_case(
        "fn test() {\n    let x = 1;\n    let y = 2;\n}",
        "fn test() {\n    let x = 99;\n    let y = 2;\n}",
        R ;
        "block_anchor_fuzzy_middle"
    )]
    #[test_case(
        "fn h() {\n    validate();\n    process();\n    save();\n    respond();\n}",
        "fn h() {\n    validate();\n    WRONG();\n    save();\n    respond();\n}",
        R ;
        "context_aware_partial_middle"
    )]
    fn fuzzy_match_succeeds(content: &str, search: &str, replacement: &str) {
        assert!(
            replace_simple(content, search, replacement)
                .unwrap()
                .contains(R)
        );
    }

    #[test_case("fn foo() {}", "MISSING", NO_MATCH ; "no_match")]
    #[test_case("let x = 1;\nlet x = 1;", "let x = 1;", MULTIPLE_MATCHES ; "ambiguous")]
    #[test_case("abc", "", EMPTY_OLD_STRING ; "empty_old_string")]
    #[test_case("a\n\nb", "   ", NO_MATCH ; "whitespace_only_old_string")]
    fn replace_rejects(content: &str, search: &str, expected_err: &str) {
        assert_eq!(
            replace_simple(content, search, "x").unwrap_err(),
            expected_err
        );
    }

    // Two model columns of the same width can map to different file columns, so
    // the base is a real tie. Without a stable order the same edit can reindent
    // one way on one run and another way on the next, hence the repeat.
    #[test]
    fn reindent_base_is_deterministic_when_indent_widths_tie() {
        let content = "if x:\n    a()\n        b()\n";
        let old = "\ta()\n b()";
        let new = "    a()\n      c()";
        for _ in 0..8 {
            assert_eq!(
                replace_simple(content, old, new).unwrap(),
                "if x:\n    a()\n      c()\n"
            );
        }
    }

    #[test]
    fn fuzzy_match_reindents_new_string() {
        let content = "class Foo\n  def bar\n    baz(\n      a,\n    )\n  end\nend\n";
        let old = "def bar\n  baz(\n    a,\n  )\nend";
        let new = "def bar\n  baz(\n    a,\n    b,\n  )\nend";
        assert_eq!(
            replace_simple(content, old, new).unwrap(),
            "class Foo\n  def bar\n    baz(\n      a,\n      b,\n    )\n  end\nend\n"
        );
    }

    #[test]
    fn reindent_ignores_new_string_own_indentation() {
        let content = "class A\n  def f\n    x\n  end\nend\n";
        let old = "def f\n  x\nend";
        let new = "  def f\n    y\n  end";
        assert_eq!(
            replace_simple(content, old, new).unwrap(),
            "class A\n  def f\n    y\n  end\nend\n"
        );
    }

    #[test]
    fn reindent_converts_spaces_to_tabs() {
        let content = "\tfn f() {\n\t\tif x {\n\t\t\tg();\n\t\t}\n\t}";
        let old = "fn f() {\n    if x {\n        g();\n    }\n}";
        let new = "fn f() {\n    if x {\n        h();\n    }\n}";
        assert_eq!(
            replace_simple(content, old, new).unwrap(),
            "\tfn f() {\n\t\tif x {\n\t\t\th();\n\t\t}\n\t}"
        );
    }

    #[test]
    fn reindent_leaves_a_correct_new_string_alone() {
        let content = "def f():\n    a = 1\n    return a\n";
        let old = "    a = 1\n    return a";
        let new = "    a = 1\n    return a\n\n\ndef g():\n    return 2";
        assert_eq!(
            replace_simple(content, old, new).unwrap(),
            "def f():\n    a = 1\n    return a\n\n\ndef g():\n    return 2\n"
        );
    }

    // A replacement that adds a nesting level uses a column the file has never
    // shown, so the "every column is a file column" test cannot recognise the
    // frame on its own and the whole block used to sink a level.
    #[test]
    fn reindent_leaves_a_file_frame_new_string_that_adds_a_level_alone() {
        let content = "def f():\n    if x:\n        a()\n    return 1\n";
        let old = "if x:\na()";
        let new = "    if x:\n        if y:\n            a()";
        assert_eq!(
            replace_simple(content, old, new).unwrap(),
            "def f():\n    if x:\n        if y:\n            a()\n    return 1\n"
        );
    }

    #[test]
    fn reindent_still_rebases_a_model_frame_new_string_that_adds_a_level() {
        let content = "def f():\n    if x:\n        a()\n    return 1\n";
        let old = "if x:\na()";
        let new = "if x:\n    if y:\n        a()";
        assert_eq!(
            replace_simple(content, old, new).unwrap(),
            "def f():\n    if x:\n        if y:\n            a()\n    return 1\n"
        );
    }

    // Bases agree here, so only the columns tell the frames apart: the file's own
    // widths are already in `new_string`, nothing to correct.
    #[test]
    fn reindent_leaves_a_file_frame_new_string_alone_when_only_widths_drifted() {
        let content = "if x:\n    a()\n";
        assert_eq!(
            replace_simple(content, "if x:\n  a()", "if x:\n    a()\n    b()").unwrap(),
            "if x:\n    a()\n    b()\n"
        );
    }

    #[test]
    fn reindent_widens_inner_levels_when_the_base_column_is_shared() {
        let content = "if x:\n    a()\n";
        assert_eq!(
            replace_simple(content, "if x:\n  a()", "if x:\n  a()\n  b()").unwrap(),
            "if x:\n    a()\n    b()\n"
        );
    }

    #[test]
    fn reindent_leaves_a_line_that_exits_the_block_alone() {
        let content = "def f():\n    a = 1\n    return a\n";
        let new = "  a = 1\n  return a\n\n\ndef g():\n  return 2";
        assert_eq!(
            replace_simple(content, "  a = 1\n  return a", new).unwrap(),
            "def f():\n    a = 1\n    return a\n\n\ndef g():\n    return 2\n"
        );
    }

    #[test]
    fn reindent_keeps_the_block_when_a_column_zero_line_is_dropped() {
        let content = "def f():\n    a = 1\n# TODO\n    b = 2\n";
        let old = "  a = 1\n# TODO\n  b = 2";
        assert_eq!(
            replace_simple(content, old, "  a = 1\n  b = 2").unwrap(),
            "def f():\n    a = 1\n    b = 2\n"
        );
    }

    #[test]
    fn reindent_takes_its_widths_from_the_block_not_the_file() {
        let content = "M = \"\"\"\n\tgcc x.c\n\"\"\"\n\ndef g():\n    if x:\n        foo()\n";
        let old = "  if x:\n    foo()";
        let result = replace_simple(content, old, "  if x:\n    foo()\n    baz()").unwrap();
        assert_eq!(
            result,
            "M = \"\"\"\n\tgcc x.c\n\"\"\"\n\ndef g():\n    if x:\n        foo()\n        baz()\n"
        );
    }

    #[test]
    fn reindent_leaves_a_midline_match_alone() {
        assert_eq!(
            replace_simple(
                "    let x = compute(a,  b);",
                "compute(a, b)",
                "compute(c, d)"
            )
            .unwrap(),
            "    let x = compute(c, d);"
        );
    }

    #[test]
    fn reindent_applies_to_every_replaced_occurrence() {
        let content = "  a();\n  b();\nx\n  a();\n  b();\n";
        let result = replace(content, "a();\nb();", "a();\nc();\nb();", true, None).unwrap();
        assert_eq!(
            result.content,
            "  a();\n  c();\n  b();\nx\n  a();\n  c();\n  b();\n"
        );
    }

    #[test]
    fn exact_match_keeps_new_string_indentation() {
        assert_eq!(
            replace_simple("  a\n  b\n", "  a\n  b", "  a\n      b").unwrap(),
            "  a\n      b\n"
        );
    }

    #[test]
    fn occurrence_selects_nth_match() {
        let content = "let x = 1;\nlet y = 2;\nlet x = 3;";
        let result = replace(content, "let x", "REPLACED", false, Some(2)).unwrap();
        assert!(result.content.contains("let y = 2;"));
        assert!(result.content.contains("REPLACED = 3;"));
    }

    #[test]
    fn occurrence_out_of_range_returns_error() {
        let content = "let x = 1;\nlet x = 2;";
        let err = replace(content, "let x", "R", false, Some(5)).unwrap_err();
        assert!(err.contains(OCCURRENCE_OUT_OF_RANGE));
    }

    #[test]
    fn pass_number_returned() {
        let result = replace("fn foo() {}", "fn foo() {}", "R", false, None).unwrap();
        assert_eq!(result.pass, Pass::Exact);

        // Whitespace difference forces a fuzzy pass
        let result = replace("let   x  =   1;", "let x = 1;", "R", false, None).unwrap();
        assert_ne!(result.pass.number(), 1);
    }

    #[test]
    fn unicode_normalization_matches_fullwidth() {
        // U+FF21 is FULLWIDTH LATIN CAPITAL LETTER A, NFKD decomposes to "A"
        let content = "let \u{ff21} = 1;";
        let result = replace_simple(content, "let A = 1;", "REPLACED");
        assert!(result.unwrap().contains("REPLACED"));
    }

    #[test]
    fn block_anchor_picks_best_among_multiple() {
        let content = "fn a() {\n    unrelated();\n}\nfn a() {\n    target();\n}";
        let result = replace_simple(content, "fn a() {\n    target();\n}", R).unwrap();
        assert!(result.contains(R));
        assert!(result.contains("unrelated()"));
    }

    #[test]
    fn leading_whitespace_disambiguates() {
        let result = replace_simple("fn foo() {}\n  fn foo() {}", "  fn foo() {}", R).unwrap();
        assert!(result.starts_with("fn foo() {}"));
        assert!(result.ends_with(R));
    }

    #[test]
    fn context_aware_below_threshold_rejects() {
        let content = "fn f() {\n    a();\n    b();\n    c();\n    d();\n}";
        let search = "fn f() {\n    w();\n    x();\n    y();\n    z();\n}";
        assert!(context_aware(content, search).is_empty());
    }

    #[test_case("trailing\\", "trailing\\" ; "trailing_backslash")]
    #[test_case("\\z", "\\z" ; "unknown_escape_kept")]
    fn unescape_edge_cases(input: &str, expected: &str) {
        assert_eq!(unescape(input), expected);
    }

    #[test]
    fn strip_common_indent_skips_blank_lines() {
        let lines = vec!["    a", "", "    b"];
        let result = strip_common_indent(&lines);
        assert_eq!(result, "a\n\nb");
    }

    #[test_case("aaa\nbbb\nccc\nfn test() {" ; "near_end")]
    #[test_case("fn test() {" ; "last_line")]
    #[test_case("fn test() {\n}" ; "two_lines")]
    fn block_anchor_no_panic(content: &str) {
        let search = "fn test() {\n    body();\n}";
        assert!(block_anchor(content, search).is_empty());
    }

    #[test]
    fn escape_normalized_also_fixes_new_string() {
        let content = r#"print("hello")"#;
        let old = r#"print(\"hello\")"#;
        let new = r#"print(\"world\")"#;
        let result = replace_simple(content, old, new).unwrap();
        assert_eq!(result, r#"print("world")"#);
    }

    #[test]
    fn escape_normalized_new_string_with_replace_all() {
        let content = "say(\"a\")\nsay(\"b\")";
        let old = r#"say(\"a\")"#;
        let new = r#"say(\"x\")"#;
        let result = replace(content, old, new, true, None).unwrap();
        assert_eq!(result.content, "say(\"x\")\nsay(\"b\")");
    }

    #[test]
    fn replace_all_replaces_every_occurrence() {
        let all = replace("aXbXc", "X", "Y", true, None).unwrap().content;
        assert_eq!(all, "aYbYc");
    }
}
