use std::collections::HashSet;

pub(crate) fn compress_code(text: &str, rate: f32) -> String {
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
            result.push_str(&format!("\n... {skipped} lines omitted ...\n"));
        }
        result.push_str(lines[*idx]);
        result.push('\n');
        last_kept = Some(*idx);
    }

    if let Some(last) = last_kept {
        let remaining = lines.len() - last - 1;
        if remaining > 0 {
            result.push_str(&format!("\n... {remaining} lines omitted ...\n"));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_head_tail_and_scored_lines() {
        let mut text = String::new();
        for _ in 0..30 {
            text.push_str("1: filler\n");
        }
        text.push_str("11: fn important() {\n");
        for _ in 0..30 {
            text.push_str("40: filler\n");
        }
        let out = compress_code(&text, 0.3);
        assert!(out.contains("fn important()"));
        assert!(out.contains("lines omitted"));
        assert!(out.lines().count() < text.lines().count());
    }

    #[test]
    fn small_input_returned_unchanged() {
        let text = "1: a\n2: b\n3: c\n";
        assert_eq!(compress_code(text, 0.3), text);
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(compress_code("", 0.3), "");
    }
}
