/// Compress diff output by keeping hunk headers and added/removed lines, dropping context if over budget.
pub fn compress_diff(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return text.to_owned();
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut in_hunk = false;

    for line in &lines {
        if line.starts_with("@@")
            || line.starts_with("diff ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            kept.push(line);
            in_hunk = true;
            continue;
        }
        if line.starts_with('+') || line.starts_with('-') {
            kept.push(line);
            continue;
        }
        if in_hunk && kept.len() < max_lines {
            continue;
        }
    }

    if kept.len() > max_lines {
        kept.truncate(max_lines);
        kept.push("... diff truncated ...");
    }

    kept.join("\n")
}