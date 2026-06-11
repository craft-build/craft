use std::collections::HashSet;

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
            let score = super::keywords::score_code_line(line);
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

