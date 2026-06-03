use std::collections::HashSet;

/// Compress code output by keeping only significant lines (signatures, type definitions, etc.)
/// The rate parameter (0.0-1.0) controls what fraction of lines to keep.
pub fn compress_code(text: &str, rate: f32) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let target_count = ((lines.len() as f32) * rate).ceil() as usize;
    if target_count >= lines.len() {
        return text.to_owned();
    }

    let mut scored: Vec<(usize, &str, i32)> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let score = score_code_line(line);
            (i, *line, score)
        })
        .collect();

    let head_tail = 3;
    let mut keep_set: HashSet<usize> = (0..head_tail.min(lines.len()))
        .chain(lines.len().saturating_sub(head_tail)..lines.len())
        .collect();

    scored.sort_by_key(|(_, _, score)| -score);

    for (i, _, _) in scored.iter() {
        if keep_set.len() >= target_count {
            break;
        }
        keep_set.insert(*i);
    }

    let mut keep_indices: Vec<usize> = keep_set.into_iter().collect();
    keep_indices.sort_unstable();

    let mut result = String::new();
    let mut last_kept = None;
    for idx in &keep_indices {
        if let Some(prev) = last_kept
            && *idx > prev + 1
        {
            let skipped = *idx - prev - 1;
            result.push_str(&format!("\n... {} lines omitted ...\n", skipped));
        }
        result.push_str(lines[*idx]);
        result.push('\n');
        last_kept = Some(*idx);
    }

    if let Some(last) = last_kept {
        let remaining = lines.len() - last - 1;
        if remaining > 0 {
            result.push_str(&format!("\n... {} lines omitted ...\n", remaining));
        }
    }

    result
}

fn score_code_line(line: &str) -> i32 {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return -5;
    }
    let mut score = 0;

    if trimmed.starts_with("pub ") || trimmed.starts_with("fn ") || trimmed.starts_with("async ") {
        score += 10;
    }
    if trimmed.contains("fn ")
        || trimmed.contains("struct ")
        || trimmed.contains("enum ")
        || trimmed.contains("impl ")
        || trimmed.contains("trait ")
        || trimmed.contains("type ")
        || trimmed.contains("interface ")
        || trimmed.contains("class ")
        || trimmed.contains("def ")
        || trimmed.contains("function ")
    {
        score += 10;
    }
    if trimmed.starts_with("use ")
        || trimmed.starts_with("import ")
        || trimmed.starts_with("from ")
        || trimmed.starts_with("#include")
    {
        score += 8;
    }
    if trimmed.starts_with("const ")
        || trimmed.starts_with("let ")
        || trimmed.starts_with("static ")
    {
        score += 5;
    }
    if trimmed.starts_with("mod ")
        || trimmed.starts_with("package ")
        || trimmed.starts_with("module ")
    {
        score += 8;
    }
    if trimmed.starts_with("//")
        || trimmed.starts_with("#")
        || trimmed.starts_with("--")
        || trimmed.starts_with("/*")
    {
        score -= 3;
    }
    // trimmed.is_empty() already checked at top
    if trimmed == "}" || trimmed == "end" {
        score -= 4;
    }

    score
}